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
use crate::censor_fx;
use crate::detect::{Detection, Detector};
use crate::overlay::{CensorRegion, OverlayHandle};
use crate::settings::{CensorMode, CensorSettings, Effective};

/// Crop a censor region out of its monitor's frame and process it per the
/// censor mode. Returns None for modes that don't need source pixels.
fn region_content(
    frame: &Frame,
    region: &CensorRegion,
    censor: &CensorSettings,
) -> Option<censor_fx::RegionContent> {
    if !matches!(censor.mode, CensorMode::Blur | CensorMode::Mosaic) {
        return None;
    }
    let scale = frame.pixel_to_point_scale();
    let (img_w, img_h) = (frame.image.width(), frame.image.height());
    let x = (((region.x - frame.origin.0 as f32) / scale).max(0.0) as u32).min(img_w - 1);
    let y = (((region.y - frame.origin.1 as f32) / scale).max(0.0) as u32).min(img_h - 1);
    let w = ((region.width / scale) as u32).clamp(1, img_w - x);
    let h = ((region.height / scale) as u32).clamp(1, img_h - y);
    let crop = image::imageops::crop_imm(&frame.image, x, y, w, h).to_image();
    Some(match censor.mode {
        CensorMode::Blur => censor_fx::blur(&crop, &censor.blur),
        CensorMode::Mosaic => censor_fx::mosaic(&crop, &censor.mosaic, scale),
        _ => unreachable!(),
    })
}

/// Convert a detection on a captured frame into a censor box in global
/// logical screen points, scaled by the censor module's x/y percentages.
pub fn detection_to_region(
    frame: &Frame,
    det: &Detection,
    x_scale_pct: f32,
    y_scale_pct: f32,
    text_seed: u64,
) -> CensorRegion {
    let scale = frame.pixel_to_point_scale();
    let (x, y, w, h) = det.bbox;
    let new_w = w * (x_scale_pct / 100.0).max(0.0);
    let new_h = h * (y_scale_pct / 100.0).max(0.0);
    // Clamp to the monitor so scaled-up boxes never hang off-screen (or
    // onto a neighboring display).
    let (ox, oy) = (frame.origin.0 as f32, frame.origin.1 as f32);
    let (mw, mh) = (frame.logical_size.0 as f32, frame.logical_size.1 as f32);
    let x1 = (ox + (x + w / 2.0 - new_w / 2.0) * scale).max(ox);
    let y1 = (oy + (y + h / 2.0 - new_h / 2.0) * scale).max(oy);
    let x2 = (x1 + new_w * scale).min(ox + mw);
    let y2 = (y1 + new_h * scale).min(oy + mh);
    CensorRegion {
        x: x1,
        y: y1,
        width: (x2 - x1).max(1.0),
        height: (y2 - y1).max(1.0),
        trigger: det.class,
        text_seed,
        content: None,
    }
}

/// A borderline detection waiting for confirmation before it may create a
/// box.
struct PendingRegion {
    last_seen: Instant,
    sightings: u32,
    region: CensorRegion,
}

/// IoU between two censor regions, for matching a fresh detection to a
/// held box.
fn region_iou(a: &CensorRegion, b: &CensorRegion) -> f32 {
    let x1 = a.x.max(b.x);
    let y1 = a.y.max(b.y);
    let x2 = (a.x + a.width).min(b.x + b.width);
    let y2 = (a.y + a.height).min(b.y + b.height);
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let union = a.width * a.height + b.width * b.height - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

pub fn run(shared: Arc<RwLock<Effective>>, overlay: OverlayHandle) -> Result<()> {
    let initial = shared.read().unwrap().clone();
    let (model_path, input_size) = initial.detection.model_path();
    let mut detector = Detector::new(&model_path, input_size)?;
    let mut loaded_model = initial.detection.model.clone();
    let mut consecutive_failures = 0u32;
    let mut capturer = SckCapturer::new(initial.detection.capture_fps)?;
    // Per-monitor held boxes, each with its own last-seen time so one
    // region dropping out (confidence wobble) doesn't take others with it.
    let mut held: HashMap<u32, Vec<(Instant, CensorRegion)>> = HashMap::new();
    // Per-monitor borderline detections awaiting confirmation.
    let mut pending: HashMap<u32, Vec<PendingRegion>> = HashMap::new();
    // Monotonic seed source for per-box text picks.
    let mut next_text_seed: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut last_exclusion_attempt: Option<Instant> = None;
    let mut displays_were_asleep = capturer.any_display_asleep();
    // Latest frame per monitor, kept so blur/mosaic content can be
    // recomputed when the censor style changes without a fresh frame.
    let mut last_frames: HashMap<u32, Frame> = HashMap::new();
    let mut prev_censor = initial.censor.clone();
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

        // SCK streams go stale across display sleep: they keep delivering
        // frames, but of pre-sleep content, so detection silently sees an
        // outdated screen. Rebuild the streams when displays wake.
        let displays_asleep = capturer.any_display_asleep();
        if displays_were_asleep && !displays_asleep {
            tracing::info!("display(s) woke; rebuilding capture streams");
            match SckCapturer::new(cfg.detection.capture_fps) {
                Ok(new_capturer) => capturer = new_capturer,
                Err(e) => tracing::error!("stream rebuild failed: {e}"),
            }
        }
        displays_were_asleep = displays_asleep;

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
                Ok(d) => {
                    consecutive_failures = 0;
                    d
                }
                Err(e) => {
                    tracing::error!("detection failed on {}: {e}", frame.monitor_name);
                    consecutive_failures += 1;
                    // The ONNX session can wedge (e.g. "GetElementType is
                    // not implemented"); rebuild it after repeat failures.
                    if consecutive_failures >= 3 {
                        let (path, size) = cfg.detection.model_path();
                        match Detector::new(&path, size) {
                            Ok(d) => {
                                tracing::warn!("rebuilt detector session after repeated failures");
                                detector = d;
                                consecutive_failures = 0;
                            }
                            Err(e) => tracing::error!("detector rebuild failed: {e}"),
                        }
                    }
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
            let regions: Vec<(CensorRegion, f32)> = flagged
                .iter()
                .map(|d| {
                    next_text_seed = next_text_seed.wrapping_add(0x9e3779b97f4a7c15);
                    (
                        detection_to_region(
                            frame,
                            d,
                            cfg.censor.x_scale_pct,
                            cfg.censor.y_scale_pct,
                            next_text_seed,
                        ),
                        d.confidence,
                    )
                })
                .collect();

            // Per-region hold with movement tracking: each fresh detection
            // claims the best-matching held box — overlapping first, else
            // the nearest with the same trigger (content being dragged) —
            // and updates it in place, so the box jumps with its content
            // instead of leaving a trail. Unclaimed boxes linger `hold_ms`
            // (confidence-wobble grace, per box).
            let now = Instant::now();
            let entry = held.entry(*monitor_id).or_default();
            let monitor_pending = pending.entry(*monitor_id).or_default();
            let before: Vec<CensorRegion> = entry.iter().map(|(_, r)| r.clone()).collect();
            let mut claimed = vec![false; entry.len()];
            let strong_threshold =
                cfg.detection.confidence_threshold + cfg.detection.borderline_margin.max(0.0);
            for (region, confidence) in regions {
                let best = entry
                    .iter()
                    .enumerate()
                    .filter(|(j, (_, h))| !claimed[*j] && h.trigger == region.trigger)
                    .map(|(j, (_, h))| {
                        let iou = region_iou(h, &region);
                        let dx = (h.x + h.width / 2.0) - (region.x + region.width / 2.0);
                        let dy = (h.y + h.height / 2.0) - (region.y + region.height / 2.0);
                        // Overlapping boxes always beat distant ones.
                        (j, if iou > 0.0 { 1e6 + iou } else { -dx.hypot(dy) })
                    })
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(j, _)| j);
                match best {
                    Some(j) => {
                        claimed[j] = true;
                        // The box keeps its text for its whole lifetime.
                        let mut region = region;
                        region.text_seed = entry[j].1.text_seed;
                        // Dead-band: detection coordinates jitter slightly
                        // frame to frame; keep the existing geometry unless
                        // the box genuinely moved or resized.
                        let existing = entry[j].1.clone();
                        let stable = (existing.x - region.x).abs() < 4.0
                            && (existing.y - region.y).abs() < 4.0
                            && (existing.width - region.width).abs() < 4.0
                            && (existing.height - region.height).abs() < 4.0;
                        entry[j] = (now, if stable { existing } else { region });
                    }
                    None => {
                        // A new box. Strong detections censor immediately;
                        // borderline ones (within the margin band above the
                        // threshold) must be sighted debounce_count times
                        // within the debounce window first — this suppresses
                        // one-frame threshold flickers.
                        let strong =
                            confidence >= strong_threshold || cfg.detection.debounce_count <= 1;
                        if strong {
                            entry.push((now, region));
                            claimed.push(true);
                        } else {
                            let slot = monitor_pending.iter_mut().find(|p| {
                                p.region.trigger == region.trigger
                                    && region_iou(&p.region, &region) > 0.1
                            });
                            match slot {
                                Some(p) => {
                                    p.sightings += 1;
                                    p.last_seen = now;
                                    // Keep the original text seed; track the
                                    // latest geometry.
                                    let seed = p.region.text_seed;
                                    p.region = region;
                                    p.region.text_seed = seed;
                                    if p.sightings >= cfg.detection.debounce_count {
                                        tracing::info!(
                                            "{}: borderline {} confirmed after {} sightings",
                                            frame.monitor_name,
                                            p.region.trigger,
                                            p.sightings,
                                        );
                                        entry.push((now, p.region.clone()));
                                        claimed.push(true);
                                        p.sightings = 0; // recycled below by prune
                                        p.last_seen = now - Duration::from_secs(3600);
                                    }
                                }
                                None => {
                                    tracing::debug!(
                                        "{}: borderline {} at {:.0}% pending confirmation",
                                        frame.monitor_name,
                                        region.trigger,
                                        confidence * 100.0,
                                    );
                                    monitor_pending.push(PendingRegion {
                                        last_seen: now,
                                        sightings: 1,
                                        region,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            entry.retain(|(last_seen, _)| last_seen.elapsed() < hold);
            let debounce_window = Duration::from_millis(cfg.detection.debounce_window_ms.max(1));
            monitor_pending.retain(|p| p.sightings > 0 && p.last_seen.elapsed() < debounce_window);
            let after: Vec<CensorRegion> = entry.iter().map(|(_, r)| r.clone()).collect();
            if entry.is_empty() {
                held.remove(monitor_id);
            }

            if after != before {
                changed = true;
                if after.is_empty() {
                    tracing::info!("{}: clear, releasing censor boxes", frame.monitor_name);
                } else {
                    tracing::info!(
                        "{}: censoring {} region(s) in {:?}: {:?}",
                        frame.monitor_name,
                        after.len(),
                        tick.elapsed(),
                        flagged
                            .iter()
                            .map(|d| format!("{} {:.0}%", d.class, d.confidence * 100.0))
                            .collect::<Vec<_>>(),
                    );
                }
            }
        }

        // Attach processed interiors (blur/mosaic). Reprocess a monitor's
        // regions when a fresh frame arrived, a region has no content yet,
        // or the censor style changed.
        let censor_changed = cfg.censor != prev_censor;
        if censor_changed {
            prev_censor = cfg.censor.clone();
        }
        let fresh: std::collections::HashSet<u32> = latest.keys().copied().collect();
        for (monitor_id, frame) in latest {
            last_frames.insert(monitor_id, frame);
        }
        if matches!(cfg.censor.mode, CensorMode::Blur | CensorMode::Mosaic) {
            for (monitor_id, entry) in held.iter_mut() {
                let Some(frame) = last_frames.get(monitor_id) else {
                    continue;
                };
                let frame_is_fresh = fresh.contains(monitor_id);
                for (_, region) in entry.iter_mut() {
                    if censor_changed || region.content.is_none() || frame_is_fresh {
                        region.content = region_content(frame, region, &cfg.censor);
                        changed = true;
                    }
                }
            }
        } else if censor_changed {
            // Leaving blur/mosaic: strip stale content so boxes render
            // their mode-appropriate interior.
            for entry in held.values_mut() {
                for (_, region) in entry.iter_mut() {
                    region.content = None;
                }
            }
            changed = true;
        }

        if changed {
            let all: Vec<CensorRegion> = held
                .values()
                .flat_map(|regions| regions.iter().map(|(_, r)| r.clone()))
                .collect();
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
