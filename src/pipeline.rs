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
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::Frame;
use crate::capture_sck::SckCapturer;
use crate::censor_fx;
use crate::detect::{Detection, Detector, TileCache};
use crate::overlay::{CensorRegion, OverlayHandle};
use crate::settings::{CensorMode, CensorSettings, Effective, ExposureMetric};

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
        highlight: None,
    }
}

/// An on-screen censor box. Marginal calls always err toward covering:
/// borderline detections (within the margin band above the threshold) get
/// a box immediately, but stay provisional — dropped after the debounce
/// window instead of the full hold — until re-sighted `debounce_count`
/// times. Flicker thus costs a brief extra box, never a brief exposure.
struct HeldBox {
    last_seen: Instant,
    sightings: u32,
    region: CensorRegion,
}

/// Rolling accumulator for the exposure budget: timestamped metric
/// increments, plus warn/block cooldown state, so the pipeline can tell
/// "too much censoring, too fast" over a window from an occasional box.
struct ExposureTracker {
    events: VecDeque<(Instant, f32)>,
    last_tick: Instant,
    last_warn: Option<Instant>,
    last_block: Option<Instant>,
}

impl ExposureTracker {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            last_tick: Instant::now(),
            last_warn: None,
            last_block: None,
        }
    }

    fn record(&mut self, amount: f32) {
        if amount > 0.0 {
            self.events.push_back((Instant::now(), amount));
        }
    }

    fn sum_within(&self, window: Duration) -> f32 {
        let now = Instant::now();
        self.events
            .iter()
            .filter(|(t, _)| now.duration_since(*t) <= window)
            .map(|(_, v)| *v)
            .sum()
    }

    fn prune(&mut self, keep: Duration) {
        let now = Instant::now();
        while let Some((t, _)) = self.events.front() {
            if now.duration_since(*t) > keep {
                self.events.pop_front();
            } else {
                break;
            }
        }
    }
}

/// FNV-1a over a sparse pixel grid — cheap "did the screen change" check
/// for the staleness watchdog, not a perceptual hash.
fn frame_hash(img: &image::RgbaImage) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let (w, ht) = img.dimensions();
    for y in (0..ht).step_by(16) {
        for x in (0..w).step_by(16) {
            for &b in &img.get_pixel(x, y).0[..3] {
                h = (h ^ b as u64).wrapping_mul(0x100000001b3);
            }
        }
    }
    h
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

pub fn run(
    shared: Arc<RwLock<Effective>>,
    overlay: OverlayHandle,
    health: Arc<crate::heartbeat::Health>,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    let initial = shared.read().unwrap().clone();
    let (model_path, input_size) = initial.detection.model_path();
    // Detector pool: [0] always exists; extras are built lazily so frames
    // from multiple monitors can be scanned in parallel.
    let mut detectors = vec![Detector::new(&model_path, input_size)?];
    let mut loaded_model = initial.detection.model.clone();
    let mut consecutive_failures = 0u32;
    let mut capturer = SckCapturer::new(initial.detection.capture_fps)?;
    // Per-monitor held boxes, each with its own last-seen time so one
    // region dropping out (confidence wobble) doesn't take others with it.
    let mut held: HashMap<u32, Vec<HeldBox>> = HashMap::new();
    // Per-monitor tile caches so detect_tiled can skip unchanged tiles.
    let mut tile_caches: HashMap<u32, TileCache> = HashMap::new();
    // Per-monitor debug highlights (flagged but not blocked), replaced
    // wholesale on each processed frame — same change-driven lifetime as
    // censor boxes: no frame means the screen (and they) stay put.
    let mut highlights: HashMap<u32, Vec<CensorRegion>> = HashMap::new();
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
    // Staleness watchdog state: when each display last delivered a frame,
    // and the previous probe hash for displays that have gone silent.
    const WATCHDOG_INTERVAL: Duration = Duration::from_secs(30);
    let mut last_frame_at: HashMap<u32, Instant> = HashMap::new();
    let mut probe_hashes: HashMap<u32, u64> = HashMap::new();
    let mut streams_started = Instant::now();
    let mut last_watchdog = Instant::now();

    tracing::info!(
        "pipeline running: model {}, change-driven capture at <= {:.1} fps",
        model_path.display(),
        initial.detection.capture_fps,
    );
    let menubar_status = |capturer: &SckCapturer, model: &str, overlay: &OverlayHandle| {
        let n = capturer.display_origins().len();
        health.streams.store(n as u32, Ordering::Relaxed);
        health.capture_ok.store(true, Ordering::Relaxed);
        let _ = overlay.set_status(format!("monitoring {n} display(s) · model {model}"));
    };
    menubar_status(&capturer, &loaded_model, &overlay);

    let mut was_enabled = true;
    let mut exposure = ExposureTracker::new();
    loop {
        let cfg = shared.read().unwrap().clone();

        // Master switch: when policy disables censoring, drain frames but
        // do no work, clear anything on screen, and say so in the
        // heartbeat so the daemon knows this is policy, not tampering.
        health
            .enabled
            .store(cfg.detection.enabled, Ordering::Relaxed);
        if !cfg.detection.enabled {
            if was_enabled {
                was_enabled = false;
                held.clear();
                highlights.clear();
                health.boxes.store(0, Ordering::Relaxed);
                overlay.set_regions(Vec::new())?;
                let _ = overlay.set_status("censoring disabled by policy".into());
                tracing::warn!("censoring disabled by policy");
            }
            while capturer.try_recv().is_some() {}
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        if !was_enabled {
            was_enabled = true;
            tracing::info!("censoring re-enabled by policy");
            menubar_status(&capturer, &loaded_model, &overlay);
        }

        // Hot-swap the detector on model change.
        if cfg.detection.model != loaded_model {
            let (path, size) = cfg.detection.model_path();
            match Detector::new(&path, size) {
                Ok(d) => {
                    tracing::info!("switched detector to {}", path.display());
                    detectors = vec![d];
                    tile_caches.clear();
                    loaded_model = cfg.detection.model.clone();
                    menubar_status(&capturer, &loaded_model, &overlay);
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
                Ok(new_capturer) => {
                    capturer = new_capturer;
                    last_frame_at.clear();
                    probe_hashes.clear();
                    streams_started = Instant::now();
                    menubar_status(&capturer, &loaded_model, &overlay);
                }
                Err(e) => {
                    health.capture_ok.store(false, Ordering::Relaxed);
                    tracing::error!("stream rebuild failed: {e}");
                }
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
            // A delivered frame proves the stream is alive; drop any probe
            // baseline so the watchdog starts fresh next time it goes quiet.
            last_frame_at.insert(*frame_id, Instant::now());
            probe_hashes.remove(frame_id);
        }
        if last_stats.elapsed() > Duration::from_secs(10) {
            if !frame_counts.is_empty() {
                tracing::info!("frames in last 10s: {frame_counts:?}");
            }
            frame_counts.clear();
            last_stats = Instant::now();
        }

        // Staleness watchdog, backing up the display-sleep rebuild above:
        // change-driven capture means a healthy stream is silent on a
        // static screen, so silence alone proves nothing. For displays
        // silent a whole interval, take a cheap polled capture (matched to
        // the stream by display origin); if two consecutive probes differ
        // while the stream stayed silent, the screen is changing but frames
        // aren't arriving — the stream is dead, rebuild them all.
        if !displays_asleep && last_watchdog.elapsed() >= WATCHDOG_INTERVAL {
            last_watchdog = Instant::now();
            let silent: Vec<(u32, (i32, i32))> = capturer
                .display_origins()
                .into_iter()
                .filter(|(id, _)| {
                    last_frame_at
                        .get(id)
                        .map_or(streams_started.elapsed(), |t| t.elapsed())
                        >= WATCHDOG_INTERVAL
                })
                .collect();
            let mut stale = false;
            if !silent.is_empty() {
                match crate::capture::capture_all() {
                    Ok(probes) => {
                        for (id, origin) in &silent {
                            let Some(probe) = probes.iter().find(|p| p.origin == *origin) else {
                                continue;
                            };
                            let hash = frame_hash(&probe.image);
                            if let Some(prev) = probe_hashes.insert(*id, hash)
                                && prev != hash
                            {
                                tracing::warn!(
                                    "display {id}: content changed while its stream was \
                                     silent — stream is stale"
                                );
                                stale = true;
                            }
                        }
                    }
                    Err(e) => tracing::debug!("watchdog probe capture failed: {e}"),
                }
            }
            if stale {
                tracing::warn!("rebuilding capture streams (staleness watchdog)");
                match SckCapturer::new(cfg.detection.capture_fps) {
                    Ok(new_capturer) => {
                        capturer = new_capturer;
                        last_frame_at.clear();
                        probe_hashes.clear();
                        streams_started = Instant::now();
                        menubar_status(&capturer, &loaded_model, &overlay);
                    }
                    Err(e) => {
                        health.capture_ok.store(false, Ordering::Relaxed);
                        tracing::error!("stream rebuild failed: {e}");
                    }
                }
            }
        }

        let hold = Duration::from_millis(cfg.detection.hold_ms);
        let mut changed = false;
        // Count fresh censor boxes this cycle for the "events" exposure metric.
        let mut new_boxes = 0u32;
        // With highlighting on, detect down to the highlight floor so
        // sub-threshold regions are visible; blocking still requires
        // the full confidence threshold below.
        let detect_floor = if cfg.detection.highlight_enabled {
            cfg.detection
                .highlight_floor
                .min(cfg.detection.confidence_threshold)
                .max(0.05)
        } else {
            cfg.detection.confidence_threshold
        };
        // Grow the pool to one detector per fresh frame; on build failure
        // fall back to scanning them serially with detectors[0].
        if latest.len() > detectors.len() {
            let (path, size) = cfg.detection.model_path();
            while detectors.len() < latest.len() {
                match Detector::new(&path, size) {
                    Ok(d) => detectors.push(d),
                    Err(e) => {
                        tracing::warn!("extra detector build failed, scanning serially: {e}");
                        break;
                    }
                }
            }
        }
        let tick = Instant::now();
        let jobs: Vec<(u32, &Frame, TileCache)> = latest
            .iter()
            .map(|(id, frame)| (*id, frame, tile_caches.remove(id).unwrap_or_default()))
            .collect();
        let detect = |detector: &mut Detector, frame: &Frame, cache: &mut TileCache| {
            detector.detect_tiled(
                &frame.image,
                cfg.detection.tile_grid,
                0.2,
                detect_floor,
                cfg.detection.iou_threshold,
                cache,
            )
        };
        let results: Vec<(u32, Result<Vec<Detection>>, TileCache)> =
            if jobs.len() > 1 && detectors.len() >= jobs.len() {
                std::thread::scope(|scope| {
                    jobs.into_iter()
                        .zip(detectors.iter_mut())
                        .map(|((id, frame, mut cache), detector)| {
                            let detect = &detect;
                            scope.spawn(move || {
                                let result = detect(detector, frame, &mut cache);
                                (id, result, cache)
                            })
                        })
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(|handle| handle.join().unwrap())
                        .collect()
                })
            } else {
                jobs.into_iter()
                    .map(|(id, frame, mut cache)| {
                        let result = detect(&mut detectors[0], frame, &mut cache);
                        (id, result, cache)
                    })
                    .collect()
            };
        for (monitor_id, result, cache) in results {
            tile_caches.insert(monitor_id, cache);
            let frame = &latest[&monitor_id];
            let detections = match result {
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
                                detectors = vec![d];
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
                        && d.confidence >= cfg.detection.confidence_threshold
                        && d.bbox.2 >= cfg.detection.min_region_px
                        && d.bbox.3 >= cfg.detection.min_region_px
                })
                .collect();

            // Flagged-but-not-blocked: trigger-enabled detections that
            // missed the confidence threshold or the size floor become
            // outlined annotations with their parameters.
            if cfg.detection.highlight_enabled {
                let boxes: Vec<CensorRegion> = detections
                    .iter()
                    .filter(|d| {
                        cfg.detection.triggers.get(d.class).copied().unwrap_or(false)
                            && !(d.confidence >= cfg.detection.confidence_threshold
                                && d.bbox.2 >= cfg.detection.min_region_px
                                && d.bbox.3 >= cfg.detection.min_region_px)
                    })
                    .map(|d| {
                        let reason = if d.confidence < cfg.detection.confidence_threshold {
                            format!(
                                "below {:.0}% threshold",
                                cfg.detection.confidence_threshold * 100.0
                            )
                        } else {
                            format!("below {:.0}px size floor", cfg.detection.min_region_px)
                        };
                        let mut region = detection_to_region(frame, d, 100.0, 100.0, 0);
                        region.highlight = Some(format!(
                            "{} {:.0}% {}×{}px — {}",
                            d.class,
                            d.confidence * 100.0,
                            d.bbox.2 as i32,
                            d.bbox.3 as i32,
                            reason,
                        ));
                        region
                    })
                    .collect();
                let entry = highlights.entry(monitor_id).or_default();
                if *entry != boxes {
                    *entry = boxes;
                    changed = true;
                }
            } else if highlights.remove(&monitor_id).is_some_and(|h| !h.is_empty()) {
                changed = true;
            }
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
            let entry = held.entry(monitor_id).or_default();
            let before: Vec<CensorRegion> = entry.iter().map(|b| b.region.clone()).collect();
            let mut claimed = vec![false; entry.len()];
            let strong_threshold =
                cfg.detection.confidence_threshold + cfg.detection.borderline_margin.max(0.0);
            let debounce_count = cfg.detection.debounce_count;
            for (region, confidence) in regions {
                let best = entry
                    .iter()
                    .enumerate()
                    .filter(|(j, b)| !claimed[*j] && b.region.trigger == region.trigger)
                    .map(|(j, b)| {
                        let h = &b.region;
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
                        region.text_seed = entry[j].region.text_seed;
                        // Dead-band: detection coordinates jitter slightly
                        // frame to frame; keep the existing geometry unless
                        // the box genuinely moved or resized.
                        let existing = entry[j].region.clone();
                        let stable = (existing.x - region.x).abs() < 4.0
                            && (existing.y - region.y).abs() < 4.0
                            && (existing.width - region.width).abs() < 4.0
                            && (existing.height - region.height).abs() < 4.0;
                        let b = &mut entry[j];
                        b.last_seen = now;
                        b.region = if stable { existing } else { region };
                        if b.sightings < debounce_count {
                            b.sightings += 1;
                            if b.sightings >= debounce_count {
                                tracing::info!(
                                    "{}: borderline {} confirmed after {} sightings",
                                    frame.monitor_name,
                                    b.region.trigger,
                                    b.sightings,
                                );
                            }
                        }
                    }
                    None => {
                        // A new box, covered immediately either way. Strong
                        // detections are confirmed at birth; borderline ones
                        // (within the margin band above the threshold) stay
                        // provisional — short-lived unless re-sighted — so a
                        // one-frame flicker costs a brief extra box rather
                        // than a brief exposure.
                        let strong = confidence >= strong_threshold;
                        if !strong {
                            tracing::debug!(
                                "{}: borderline {} at {:.0}% covered provisionally",
                                frame.monitor_name,
                                region.trigger,
                                confidence * 100.0,
                            );
                        }
                        entry.push(HeldBox {
                            last_seen: now,
                            sightings: if strong { debounce_count } else { 1 },
                            region,
                        });
                        claimed.push(true);
                        new_boxes += 1;
                    }
                }
            }
            let debounce_window = Duration::from_millis(cfg.detection.debounce_window_ms.max(1));
            entry.retain(|b| {
                let lifetime = if b.sightings >= debounce_count {
                    hold
                } else {
                    debounce_window
                };
                b.last_seen.elapsed() < lifetime
            });
            let after: Vec<CensorRegion> = entry.iter().map(|b| b.region.clone()).collect();
            if entry.is_empty() {
                held.remove(&monitor_id);
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
                for b in entry.iter_mut() {
                    if censor_changed || b.region.content.is_none() || frame_is_fresh {
                        b.region.content = region_content(frame, &b.region, &cfg.censor);
                        changed = true;
                    }
                }
            }
        } else if censor_changed {
            // Leaving blur/mosaic: strip stale content so boxes render
            // their mode-appropriate interior.
            for entry in held.values_mut() {
                for b in entry.iter_mut() {
                    b.region.content = None;
                }
            }
            changed = true;
        }

        if changed {
            let mut all: Vec<CensorRegion> = held
                .values()
                .flat_map(|regions| regions.iter().map(|b| b.region.clone()))
                .collect();
            health.boxes.store(all.len() as u32, Ordering::Relaxed);
            all.extend(highlights.values().flatten().cloned());
            overlay.set_regions(all)?;
        }

        // Exposure budget: accumulate how much the censor is firing over a
        // rolling window and escalate — a warning popup at the soft limit,
        // and at the hard limit a one-shot request for a timed network
        // lockout that betamacsd enforces (like tamper/uninstall, but for a
        // fixed period). Policy only; disabled by default.
        if cfg.exposure.enabled {
            let ex = &cfg.exposure;
            let now = Instant::now();
            let dt = now
                .saturating_duration_since(exposure.last_tick)
                .min(Duration::from_secs(5))
                .as_secs_f32();
            exposure.last_tick = now;
            let box_count: usize = held.values().map(|v| v.len()).sum();
            let increment = match ex.metric {
                ExposureMetric::Events => new_boxes as f32,
                ExposureMetric::ActiveSeconds => {
                    if box_count > 0 {
                        dt
                    } else {
                        0.0
                    }
                }
                ExposureMetric::BoxSeconds => box_count as f32 * dt,
                ExposureMetric::AreaSeconds => {
                    let box_area: f32 = held
                        .values()
                        .flatten()
                        .map(|b| b.region.width * b.region.height)
                        .sum();
                    let screen_area: f32 = last_frames
                        .values()
                        .map(|f| f.logical_size.0 as f32 * f.logical_size.1 as f32)
                        .sum();
                    if screen_area > 0.0 {
                        (box_area / screen_area).min(1.0) * dt
                    } else {
                        0.0
                    }
                }
            };
            exposure.record(increment);
            let keep =
                Duration::from_secs(ex.warn_window_sec.max(ex.block_window_sec) as u64 + 5);
            exposure.prune(keep);
            // Publish the current block-window gauge for the status HUD.
            let block_sum = exposure.sum_within(Duration::from_secs(ex.block_window_sec as u64));
            health
                .exposure_recent
                .store(block_sum.round().max(0.0) as u32, Ordering::Relaxed);
            health
                .exposure_block
                .store(ex.block_threshold.round().max(0.0) as u32, Ordering::Relaxed);

            let over_block = ex.block_threshold > 0.0
                && exposure.sum_within(Duration::from_secs(ex.block_window_sec as u64))
                    >= ex.block_threshold
                && exposure
                    .last_block
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(ex.penalty_sec as u64));
            if over_block {
                exposure.last_block = Some(now);
                exposure.events.clear();
                health
                    .exposure_penalty_secs
                    .store(ex.penalty_sec, Ordering::Relaxed);
                health.exposure_over_budget.store(true, Ordering::Relaxed);
                tracing::warn!(
                    "exposure budget exceeded ({:?}) — requesting {}s network lockout",
                    ex.metric,
                    ex.penalty_sec,
                );
            } else if ex.warn_threshold > 0.0
                && exposure.sum_within(Duration::from_secs(ex.warn_window_sec as u64))
                    >= ex.warn_threshold
                && exposure
                    .last_warn
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(ex.warn_cooldown_sec as u64))
            {
                exposure.last_warn = Some(now);
                tracing::info!("exposure over warn threshold — showing prompt");
                crate::prompt::warn_exposure(
                    "Are you looking at appropriate content?\n\nA lot of flagged content has been detected. Please make a better choice.",
                );
            }
        } else {
            // Exposure disabled: clear the HUD gauge.
            health.exposure_block.store(0, Ordering::Relaxed);
            health.exposure_recent.store(0, Ordering::Relaxed);
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
