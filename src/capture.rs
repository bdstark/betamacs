//! Multi-monitor screen capture.
//!
//! Currently uses `xcap` (backed by ScreenCaptureKit on macOS) in a polling
//! model: each tick we grab a full screenshot of every monitor. If we later
//! need higher frame rates, the upgrade path is a persistent
//! ScreenCaptureKit stream per display.

use anyhow::{Context, Result};
use image::RgbaImage;
use xcap::Monitor;

/// A captured frame from one monitor, plus enough geometry to map
/// detection boxes back onto screen coordinates.
pub struct Frame {
    pub monitor_id: u32,
    pub monitor_name: String,
    /// Monitor origin in the global virtual-desktop coordinate space (points).
    pub origin: (i32, i32),
    /// Logical size of the monitor in points.
    pub logical_size: (u32, u32),
    /// The captured pixels (physical resolution, i.e. logical * scale on retina).
    pub image: RgbaImage,
}

impl Frame {
    /// Scale from captured-pixel coordinates to logical screen points.
    pub fn pixel_to_point_scale(&self) -> f32 {
        if self.image.width() == 0 {
            return 1.0;
        }
        self.logical_size.0 as f32 / self.image.width() as f32
    }
}

/// Capture a single frame from every connected monitor.
pub fn capture_all() -> Result<Vec<Frame>> {
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    let mut frames = Vec::with_capacity(monitors.len());
    for monitor in monitors {
        let id = monitor.id()?;
        let name = monitor.name()?;
        let image = monitor
            .capture_image()
            .with_context(|| format!("failed to capture monitor {name}"))?;
        frames.push(Frame {
            monitor_id: id,
            monitor_name: name,
            origin: (monitor.x()?, monitor.y()?),
            logical_size: (monitor.width()?, monitor.height()?),
            image,
        });
    }
    Ok(frames)
}
