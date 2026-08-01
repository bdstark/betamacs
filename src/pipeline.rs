//! Continuous capture -> detect -> censor loop (runs on a worker thread).
//!
//! Capture is change-driven (ScreenCaptureKit only delivers a frame when a
//! display's content changes), which shapes the censor-box lifetime rules:
//!
//!   - a box is placed/refreshed whenever a frame shows censorable content
//!   - a box is removed only when a *new frame* for that monitor shows no
//!     content there AND the last sighting is older than `hold_ms` (grace
//!     for detection wobble on moving content)
//!   - if no frames arrive the screen hasn't changed, so boxes stay put —
//!     a static image must remain covered indefinitely

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::Frame;
use crate::capture_sck::SckCapturer;
use crate::config::Config;
use crate::detect::{Detection, Detector};
use crate::overlay::{CensorRegion, OverlayHandle};

/// Convert a detection on a captured frame into a censor box in global
/// logical screen points, padded by `box_padding` on each side.
pub fn detection_to_region(frame: &Frame, det: &Detection, box_padding: f32) -> CensorRegion {
    let scale = frame.pixel_to_point_scale();
    let (x, y, w, h) = det.bbox;
    let (pad_w, pad_h) = (w * box_padding, h * box_padding);
    CensorRegion {
        x: frame.origin.0 as f32 + (x - pad_w) * scale,
        y: frame.origin.1 as f32 + (y - pad_h) * scale,
        width: (w + 2.0 * pad_w) * scale,
        height: (h + 2.0 * pad_h) * scale,
    }
}

pub fn run(config: Config, overlay: OverlayHandle) -> Result<()> {
    let mut detector = Detector::new(&config.model_path, config.input_size)?;
    let mut capturer = SckCapturer::new(config.capture_fps)?;
    let hold = Duration::from_millis(config.hold_ms);
    // Per-monitor: when censorable content was last seen, and where.
    let mut held: HashMap<u32, (Instant, Vec<CensorRegion>)> = HashMap::new();
    let mut last_exclusion_attempt: Option<Instant> = None;
    // Frame-rate accounting, logged every 10s to spot busy displays.
    let mut frame_counts: HashMap<u32, u32> = HashMap::new();
    let mut last_stats = Instant::now();

    tracing::info!(
        "pipeline running: model {}, change-driven capture at <= {:.1} fps",
        config.model_path.display(),
        config.capture_fps,
    );

    loop {
        // Block for the next changed frame, then drain the queue keeping
        // only the newest frame per monitor (coalescing under load).
        let mut latest: HashMap<u32, Frame> = HashMap::new();
        if let Some(frame) = capturer.recv_timeout(Duration::from_millis(500)) {
            latest.insert(frame.monitor_id, frame);
            while let Some(frame) = capturer.try_recv() {
                latest.insert(frame.monitor_id, frame);
            }
        }

        for frame_id in latest.keys() {
            *frame_counts.entry(*frame_id).or_default() += 1;
        }
        if last_stats.elapsed() > Duration::from_secs(10) {
            if !frame_counts.is_empty() {
                tracing::info!("frames in last 10s: {frame_counts:?}");
            }
            frame_counts.clear();
            last_stats = Instant::now();
        }

        let mut changed = false;
        for (monitor_id, frame) in &latest {
            let tick = Instant::now();
            let detections = match detector.detect_tiled(
                &frame.image,
                config.tile_grid,
                config.tile_overlap,
                config.confidence_threshold,
                config.iou_threshold,
            ) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("detection failed on {}: {e}", frame.monitor_name);
                    continue;
                }
            };
            let flagged: Vec<&Detection> = detections
                .iter()
                .filter(|d| config.censored_classes.contains(&d.class))
                .collect();
            if !flagged.is_empty() {
                let regions = flagged
                    .iter()
                    .map(|d| detection_to_region(frame, d, config.box_padding))
                    .collect::<Vec<_>>();
                let is_new = held
                    .get(monitor_id)
                    .is_none_or(|(_, prev)| *prev != regions);
                if is_new {
                    tracing::info!(
                        "{}: censoring {} region(s) in {:?}: {:?}",
                        frame.monitor_name,
                        regions.len(),
                        tick.elapsed(),
                        flagged
                            .iter()
                            .map(|d| format!("{} {:.0}%", d.class, d.confidence * 100.0))
                            .collect::<Vec<_>>(),
                    );
                }
                held.insert(*monitor_id, (Instant::now(), regions));
                changed = true;
            } else if let Some((last_seen, _)) = held.get(monitor_id) {
                // Content gone from this fresh frame; release after the
                // wobble grace period.
                if last_seen.elapsed() >= hold {
                    tracing::info!("{}: clear, releasing censor boxes", frame.monitor_name);
                    held.remove(monitor_id);
                    changed = true;
                }
            }
        }

        if changed {
            let all: Vec<CensorRegion> =
                held.values().flat_map(|(_, r)| r.iter().cloned()).collect();
            overlay.set_regions(all)?;
        }

        // In --censor-captures mode the boxes are visible to capture, so our
        // own streams must exclude this app or the detector goes blind under
        // its boxes. The app only becomes excludable once it has an
        // on-screen window, so retry after boxes first appear.
        if config.censor_in_captures
            && !capturer.self_excluded()
            && !held.is_empty()
            && last_exclusion_attempt.is_none_or(|t| t.elapsed() > Duration::from_secs(2))
        {
            last_exclusion_attempt = Some(Instant::now());
            match capturer.exclude_self() {
                Ok(true) => {}
                Ok(false) => tracing::debug!("self-exclusion: app not yet in shareable content"),
                Err(e) => tracing::warn!("self-exclusion failed: {e}"),
            }
        }
    }
}
