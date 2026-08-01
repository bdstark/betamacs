//! Continuous capture -> detect -> censor loop (runs on a worker thread).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::{self, Frame};
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
    let tick = Duration::from_secs_f32(1.0 / config.capture_fps.max(0.1));
    let hold = Duration::from_millis(config.hold_ms);
    // Per-monitor: when censorable content was last seen, and where.
    // Regions linger for `hold_ms` after last sighting to avoid flicker.
    let mut held: HashMap<u32, (Instant, Vec<CensorRegion>)> = HashMap::new();

    tracing::info!(
        "pipeline running: model {}, {:.1} fps target, {} tick budget",
        config.model_path.display(),
        config.capture_fps,
        humantime(tick),
    );

    loop {
        let tick_start = Instant::now();
        match capture::capture_all() {
            Ok(frames) => {
                for frame in &frames {
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
                    let regions: Vec<CensorRegion> = detections
                        .iter()
                        .filter(|d| config.censored_classes.contains(&d.class))
                        .map(|d| detection_to_region(frame, d, config.box_padding))
                        .collect();
                    if !regions.is_empty() {
                        tracing::info!(
                            "{}: censoring {} region(s): {:?}",
                            frame.monitor_name,
                            regions.len(),
                            detections
                                .iter()
                                .filter(|d| config.censored_classes.contains(&d.class))
                                .map(|d| format!("{} {:.0}%", d.class, d.confidence * 100.0))
                                .collect::<Vec<_>>()
                        );
                        held.insert(frame.monitor_id, (Instant::now(), regions));
                    }
                }
                held.retain(|_, (last_seen, _)| last_seen.elapsed() < hold);
                let all: Vec<CensorRegion> =
                    held.values().flat_map(|(_, r)| r.iter().cloned()).collect();
                overlay.set_regions(all)?;
            }
            Err(e) => tracing::warn!("capture failed: {e}"),
        }

        let elapsed = tick_start.elapsed();
        if let Some(rest) = tick.checked_sub(elapsed) {
            std::thread::sleep(rest);
        } else {
            tracing::debug!("tick overran budget: {}", humantime(elapsed));
        }
    }
}

fn humantime(d: Duration) -> String {
    format!("{:.0}ms", d.as_secs_f64() * 1000.0)
}
