//! Continuous capture -> detect -> censor loop (runs on a worker thread).
//!
//! Settings are read from the shared `Effective` state every cycle, so
//! threshold/trigger/scale changes pushed through the config API apply
//! live; a model change hot-swaps the detector. Capture fps and tile
//! layout of the SCK streams need a restart.
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
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::Frame;
use crate::capture_sck::SckCapturer;
use crate::detect::{Detection, Detector};
use crate::overlay::{CensorRegion, OverlayHandle};
use crate::settings::Effective;

/// Convert a detection on a captured frame into a censor box in global
/// logical screen points, scaled by the censor module's x/y percentages.
pub fn detection_to_region(
    frame: &Frame,
    det: &Detection,
    x_scale_pct: f32,
    y_scale_pct: f32,
) -> CensorRegion {
    let scale = frame.pixel_to_point_scale();
    let (x, y, w, h) = det.bbox;
    let new_w = w * (x_scale_pct / 100.0).max(0.0);
    let new_h = h * (y_scale_pct / 100.0).max(0.0);
    CensorRegion {
        x: frame.origin.0 as f32 + (x + w / 2.0 - new_w / 2.0) * scale,
        y: frame.origin.1 as f32 + (y + h / 2.0 - new_h / 2.0) * scale,
        width: new_w * scale,
        height: new_h * scale,
    }
}

pub fn run(shared: Arc<RwLock<Effective>>, overlay: OverlayHandle) -> Result<()> {
    let initial = shared.read().unwrap().clone();
    let (model_path, input_size) = initial.detection.model_path();
    let mut detector = Detector::new(&model_path, input_size)?;
    let mut loaded_model = initial.detection.model.clone();
    let mut capturer = SckCapturer::new(initial.detection.capture_fps)?;
    // Per-monitor: when censorable content was last seen, and where.
    let mut held: HashMap<u32, (Instant, Vec<CensorRegion>)> = HashMap::new();
    let mut last_exclusion_attempt: Option<Instant> = None;
    // Frame-rate accounting, logged every 10s to spot busy displays.
    let mut frame_counts: HashMap<u32, u32> = HashMap::new();
    let mut last_stats = Instant::now();

    tracing::info!(
        "pipeline running: model {}, change-driven capture at <= {:.1} fps",
        model_path.display(),
        initial.detection.capture_fps,
    );

    loop {
        let cfg = shared.read().unwrap().clone();

        // Hot-swap the detector on model change.
        if cfg.detection.model != loaded_model {
            let (path, size) = cfg.detection.model_path();
            match Detector::new(&path, size) {
                Ok(d) => {
                    tracing::info!("switched detector to {}", path.display());
                    detector = d;
                    loaded_model = cfg.detection.model.clone();
                }
                Err(e) => tracing::error!("model switch to {} failed: {e}", path.display()),
            }
        }

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

        let hold = Duration::from_millis(cfg.detection.hold_ms);
        let mut changed = false;
        for (monitor_id, frame) in &latest {
            let tick = Instant::now();
            let detections = match detector.detect_tiled(
                &frame.image,
                cfg.detection.tile_grid,
                0.2,
                cfg.detection.confidence_threshold,
                cfg.detection.iou_threshold,
            ) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("detection failed on {}: {e}", frame.monitor_name);
                    continue;
                }
            };
            let flagged: Vec<&Detection> = detections
                .iter()
                .filter(|d| {
                    cfg.detection.triggers.get(d.class).copied().unwrap_or(false)
                        && d.bbox.2 >= cfg.detection.min_region_px
                        && d.bbox.3 >= cfg.detection.min_region_px
                })
                .collect();
            if !flagged.is_empty() {
                let regions = flagged
                    .iter()
                    .map(|d| {
                        detection_to_region(
                            frame,
                            d,
                            cfg.censor.x_scale_pct,
                            cfg.censor.y_scale_pct,
                        )
                    })
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

        // In censor-in-captures mode the boxes are visible to capture, so
        // our own streams must exclude this app or the detector goes blind
        // under its boxes. The app only becomes excludable once it has an
        // on-screen window, so retry after boxes first appear.
        if cfg.censor.censor_in_captures
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
