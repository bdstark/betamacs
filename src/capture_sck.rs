//! Native ScreenCaptureKit capture: one persistent `SCStream` per display.
//!
//! Advantages over the old polling capture (still used by probe/demo):
//!   - frames are only delivered when screen content actually changes, so
//!     idle CPU cost is ~zero and static screens skip detection entirely
//!   - a per-stream content filter can exclude *our* windows from *our*
//!     capture only, which makes `--censor-captures` flicker-free: other
//!     apps' captures see the black boxes, our detector sees beneath them
//!
//! Threading: SCK delivers sample buffers on a private dispatch queue; the
//! delegate converts them to plain `Frame`s (BGRA -> RGBA) and sends them
//! over an mpsc channel to the pipeline thread.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use image::RgbaImage;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
};
use dispatch2::{DispatchQueue, DispatchRetained};

use crate::capture::Frame;

/// kCVPixelFormatType_32BGRA ('BGRA').
const PIXEL_FORMAT_BGRA: u32 = 0x42475241;

#[derive(Clone)]
struct DisplayInfo {
    id: u32,
    name: String,
    origin: (i32, i32),
    logical_size: (u32, u32),
}

struct OutputIvars {
    tx: mpsc::Sender<Frame>,
    display: DisplayInfo,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "BetamacsStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output_sample_buffer_of_type(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            output_type: SCStreamOutputType,
        ) {
            if output_type != SCStreamOutputType::Screen {
                return;
            }
            // Frames without an image buffer (idle/status-only samples)
            // carry no new content.
            let Some(image_buffer) = (unsafe { sample_buffer.image_buffer() }) else {
                return;
            };
            let ivars = self.ivars();
            let pixel_buffer = image_buffer.as_ref();
            let image = unsafe {
                CVPixelBufferLockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
                let image = bgra_to_rgba(
                    CVPixelBufferGetBaseAddress(pixel_buffer) as *const u8,
                    CVPixelBufferGetBytesPerRow(pixel_buffer),
                    CVPixelBufferGetWidth(pixel_buffer),
                    CVPixelBufferGetHeight(pixel_buffer),
                );
                CVPixelBufferUnlockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
                image
            };
            let Some(image) = image else { return };
            let _ = ivars.tx.send(Frame {
                monitor_id: ivars.display.id,
                monitor_name: ivars.display.name.clone(),
                origin: ivars.display.origin,
                logical_size: ivars.display.logical_size,
                image,
            });
        }
    }
);

impl StreamOutput {
    fn new(tx: mpsc::Sender<Frame>, display: DisplayInfo) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { tx, display });
        unsafe { msg_send![super(this), init] }
    }
}

/// SAFETY: base points at `height` rows of `bytes_per_row` bytes of BGRA.
unsafe fn bgra_to_rgba(
    base: *const u8,
    bytes_per_row: usize,
    width: usize,
    height: usize,
) -> Option<RgbaImage> {
    if base.is_null() || width == 0 || height == 0 {
        return None;
    }
    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        let row = unsafe { std::slice::from_raw_parts(base.add(y * bytes_per_row), width * 4) };
        let out = &mut rgba[y * width * 4..(y + 1) * width * 4];
        for x in 0..width {
            out[x * 4] = row[x * 4 + 2];
            out[x * 4 + 1] = row[x * 4 + 1];
            out[x * 4 + 2] = row[x * 4];
            out[x * 4 + 3] = 255;
        }
    }
    RgbaImage::from_raw(width as u32, height as u32, rgba)
}

/// Fetch shareable content, blocking. The completion handler runs on an SCK
/// queue; the retained pointer is smuggled across as usize because objc2
/// types are not `Send`.
fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel::<Result<usize, String>>();
    let block = block2::RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if content.is_null() {
                let msg = unsafe { error.as_ref() }
                    .map(|e| e.localizedDescription().to_string())
                    .unwrap_or_else(|| "unknown error".into());
                Err(msg)
            } else {
                let retained = unsafe { Retained::retain(content) }.unwrap();
                Ok(Retained::into_raw(retained) as usize)
            };
            let _ = tx.send(result);
        },
    );
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    let ptr = rx
        .recv_timeout(Duration::from_secs(10))
        .context("timed out fetching shareable content (screen recording permission?)")?
        .map_err(|e| anyhow::anyhow!("shareable content: {e}"))?;
    Ok(unsafe { Retained::from_raw(ptr as *mut SCShareableContent) }.unwrap())
}

pub struct SckCapturer {
    rx: mpsc::Receiver<Frame>,
    tx: mpsc::Sender<Frame>,
    /// (displayID, stream) pairs.
    streams: Vec<(u32, Retained<SCStream>)>,
    // Keep delegates and queue alive for the life of the streams.
    _outputs: Vec<Retained<StreamOutput>>,
    _queue: DispatchRetained<DispatchQueue>,
    /// True once our own application is excluded from the stream filters
    /// (only needed in `--censor-captures` mode).
    self_excluded: bool,
}

impl SckCapturer {
    /// Start one stream per display at the given max frame rate.
    pub fn new(fps: f32) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let queue = DispatchQueue::new("betamacs.capture", None);
        let content = shareable_content()?;

        let mut streams = Vec::new();
        let mut outputs = Vec::new();
        let displays = unsafe { content.displays() };
        for display in displays.iter() {
            let (stream, output) = start_stream(&display, None, fps, tx.clone(), &queue)?;
            streams.push((unsafe { display.displayID() }, stream));
            outputs.push(output);
        }
        anyhow::ensure!(!streams.is_empty(), "no displays found to capture");
        tracing::info!("started {} ScreenCaptureKit stream(s)", streams.len());
        Ok(Self {
            rx,
            tx,
            streams,
            _outputs: outputs,
            _queue: queue,
            self_excluded: false,
        })
    }

    /// Wait up to `timeout` for the next changed-content frame.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Frame> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Drain any additionally queued frames without blocking.
    pub fn try_recv(&self) -> Option<Frame> {
        self.rx.try_recv().ok()
    }

    pub fn self_excluded(&self) -> bool {
        self.self_excluded
    }

    /// Update every stream's filter to exclude this process's windows from
    /// our own capture (used in `--censor-captures` mode, where the boxes
    /// are visible to other apps' captures and must not blind our
    /// detector). Our app only appears in shareable content once it has at
    /// least one on-screen window, so this is retried by the pipeline until
    /// it succeeds.
    pub fn exclude_self(&mut self) -> Result<bool> {
        let content = shareable_content()?;
        let pid = std::process::id() as i32;
        let apps = unsafe { content.applications() };
        let Some(our_app) = apps.iter().find(|a| unsafe { a.processID() } == pid) else {
            return Ok(false);
        };
        let our_app_array = NSArray::from_retained_slice(&[our_app]);
        let displays = unsafe { content.displays() };
        for (display_id, stream) in &self.streams {
            let Some(display) = displays
                .iter()
                .find(|d| unsafe { d.displayID() } == *display_id)
            else {
                tracing::warn!("display {display_id} no longer in shareable content");
                continue;
            };
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &our_app_array,
                    &NSArray::new(),
                )
            };
            unsafe { stream.updateContentFilter_completionHandler(&filter, None) };
        }
        self.self_excluded = true;
        tracing::info!("own windows now excluded from capture streams");
        Ok(true)
    }

    /// Sender clone, for tests/tools that want to inject frames.
    #[allow(dead_code)]
    pub fn sender(&self) -> mpsc::Sender<Frame> {
        self.tx.clone()
    }

    /// True if any captured display is currently asleep. Streams go stale
    /// across display sleep (they keep delivering frames of pre-sleep
    /// content), so the pipeline rebuilds the capturer on the wake
    /// transition.
    pub fn any_display_asleep(&self) -> bool {
        self.streams
            .iter()
            .any(|(id, _)| objc2_core_graphics::CGDisplayIsAsleep(*id))
    }
}

impl Drop for SckCapturer {
    fn drop(&mut self) {
        for (_, stream) in &self.streams {
            unsafe { stream.stopCaptureWithCompletionHandler(None) };
        }
    }
}

fn start_stream(
    display: &SCDisplay,
    exclude_apps: Option<&NSArray<SCRunningApplication>>,
    fps: f32,
    tx: mpsc::Sender<Frame>,
    queue: &DispatchQueue,
) -> Result<(Retained<SCStream>, Retained<StreamOutput>)> {
    let (id, frame_rect, width, height) = unsafe {
        (
            display.displayID(),
            display.frame(),
            display.width() as u32,
            display.height() as u32,
        )
    };
    let info = DisplayInfo {
        id,
        name: format!("Display {id}"),
        origin: (frame_rect.origin.x as i32, frame_rect.origin.y as i32),
        logical_size: (width, height),
    };

    let filter = unsafe {
        match exclude_apps {
            Some(apps) => SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                display,
                apps,
                &NSArray::new(),
            ),
            None => SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                display,
                &NSArray::new(),
            ),
        }
    };

    let config = unsafe {
        let config = SCStreamConfiguration::new();
        // Capture at logical (point) resolution: the detector downscales to
        // 320/640 anyway, and this halves conversion cost on retina.
        config.setWidth(width as usize);
        config.setHeight(height as usize);
        config.setPixelFormat(PIXEL_FORMAT_BGRA);
        config.setMinimumFrameInterval(CMTime::new(1, fps.max(0.1) as i32));
        config.setShowsCursor(false);
        config.setQueueDepth(3);
        config
    };

    let output = StreamOutput::new(tx, info);
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None)
    };
    unsafe {
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(queue),
            )
            .map_err(|e| anyhow::anyhow!("addStreamOutput failed: {e}"))?;
        stream.startCaptureWithCompletionHandler(Some(&block2::RcBlock::new(
            |error: *mut NSError| {
                if let Some(e) = error.as_ref() {
                    tracing::error!("stream failed to start: {}", e.localizedDescription());
                }
            },
        )));
    }
    tracing::info!(
        "capturing {} ({}x{} pts at {:?})",
        format!("Display {id}"),
        width,
        height,
        (frame_rect.origin.x, frame_rect.origin.y),
    );
    Ok((stream, output))
}
