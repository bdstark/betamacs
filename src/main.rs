mod capture;
mod config;
mod detect;
mod overlay;
mod pipeline;

use std::time::{Duration, Instant};

use anyhow::Result;
use config::Config;
use overlay::{CensorRegion, OverlayApp};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "betamacs=info,ort=error".into()),
        )
        .init();

    // Usage: betamacs [run|probe|demo] [320n|640m|path/to/model.onnx] [--censor-captures]
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let censor_in_captures = args.iter().any(|a| a == "--censor-captures");
    args.retain(|a| a != "--censor-captures");
    let (mode, model) = match args.first().map(String::as_str) {
        Some(m @ ("run" | "probe" | "demo")) => (m, args.get(1)),
        Some(_) => ("run", args.first()),
        None => ("run", None),
    };
    let mut config = match model {
        Some(model) => Config::default().with_model(model),
        None => Config::default(),
    };
    config.censor_in_captures = censor_in_captures;

    match mode {
        "probe" => probe(config),
        "demo" => demo(),
        _ => run(config),
    }
}

/// Continuous censoring: overlay event loop on the main thread (macOS
/// requirement), capture/detect pipeline on a worker thread.
fn run(config: Config) -> Result<()> {
    anyhow::ensure!(
        config.model_path.exists(),
        "model not found at {} — run scripts/fetch-model.sh",
        config.model_path.display()
    );
    if config.censor_in_captures {
        tracing::warn!(
            "--censor-captures: boxes will appear in screenshots/shares, but \
             will blink roughly every {}ms as the detector re-checks beneath \
             them (proper SCK exclusion filter is on the roadmap)",
            config.hold_ms
        );
    }
    let (event_loop, handle, mut app) = OverlayApp::new(!config.censor_in_captures)?;
    std::thread::spawn(move || {
        if let Err(e) = pipeline::run(config, handle) {
            tracing::error!("pipeline exited: {e}");
            std::process::exit(1);
        }
    });
    event_loop
        .run_app(&mut app)
        .map_err(|e| anyhow::anyhow!("event loop failed: {e}"))
}

/// Show a fixed censor box on the primary monitor for a few seconds and
/// verify it is (a) visible to the user but (b) invisible to screen
/// capture, by comparing captures taken before and while it is shown.
fn demo() -> Result<()> {
    let (event_loop, handle, mut app) = OverlayApp::new(true)?;
    std::thread::spawn(move || -> () {
        let check = || -> Result<f32> {
            let frames = capture::capture_all()?;
            let frame = &frames[0];
            let scale = frame.pixel_to_point_scale();
            let (px, py) = (
                ((300.0 - frame.origin.0 as f32) / scale) as u32,
                ((300.0 - frame.origin.1 as f32) / scale) as u32,
            );
            let (pw, ph) = ((400.0 / scale) as u32, (300.0 / scale) as u32);
            let mut sum = 0u64;
            let mut n = 0u64;
            for y in (py..py + ph).step_by(8) {
                for x in (px..px + pw).step_by(8) {
                    if x < frame.image.width() && y < frame.image.height() {
                        let p = frame.image.get_pixel(x, y);
                        sum += (p[0] as u64 + p[1] as u64 + p[2] as u64) / 3;
                        n += 1;
                    }
                }
            }
            Ok(sum as f32 / n.max(1) as f32)
        };

        let before = check().map(|b| {
            tracing::info!("mean brightness under box region before showing: {b:.1}");
            b
        });
        tracing::info!("showing demo censor box at (300, 300) 400x300 for 8s");
        let region = CensorRegion {
            x: 300.0,
            y: 300.0,
            width: 400.0,
            height: 300.0,
        };
        if let Err(e) = handle.set_regions(vec![region]) {
            tracing::error!("{e}");
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(1500));
        match (before, check()) {
            (Ok(before), Ok(during)) => {
                tracing::info!("mean brightness while box shown: {during:.1}");
                if during < 8.0 && before > 16.0 {
                    tracing::error!(
                        "overlay LEAKED into capture (region went dark) — \
                         content_protected is not excluding it"
                    );
                } else if (before - during).abs() < 16.0 {
                    tracing::info!("overlay correctly excluded from capture");
                } else {
                    tracing::warn!(
                        "region changed brightness ({before:.1} -> {during:.1}); \
                         screen content may have moved — rerun on a static screen"
                    );
                }
            }
            (Err(e), _) | (_, Err(e)) => tracing::error!("capture check failed: {e}"),
        }
        let start = Instant::now();
        std::thread::sleep(Duration::from_secs(8).saturating_sub(start.elapsed()));
        std::process::exit(0);
    });
    event_loop
        .run_app(&mut app)
        .map_err(|e| anyhow::anyhow!("event loop failed: {e}"))
}

/// One-shot capture + detection pass with timings, no overlay.
fn probe(config: Config) -> Result<()> {
    let start = Instant::now();
    let frames = capture::capture_all()?;
    tracing::info!(
        "captured {} monitor(s) in {:?}",
        frames.len(),
        start.elapsed()
    );
    for frame in &frames {
        tracing::info!(
            "  {} (id {}): {}x{} px, origin {:?}, logical {:?}",
            frame.monitor_name,
            frame.monitor_id,
            frame.image.width(),
            frame.image.height(),
            frame.origin,
            frame.logical_size,
        );
    }

    if !config.model_path.exists() {
        tracing::warn!(
            "model not found at {} — run scripts/fetch-model.sh; skipping detection",
            config.model_path.display()
        );
        return Ok(());
    }

    let mut detector = detect::Detector::new(&config.model_path, config.input_size)?;
    for frame in &frames {
        let start = Instant::now();
        let detections = detector.detect_tiled(
            &frame.image,
            config.tile_grid,
            config.tile_overlap,
            config.confidence_threshold,
            config.iou_threshold,
        )?;
        tracing::info!(
            "{}: {} detection(s) in {:?}",
            frame.monitor_name,
            detections.len(),
            start.elapsed()
        );
        for det in &detections {
            let censored = config.censored_classes.contains(&det.class);
            tracing::info!(
                "  {} {:.0}% at {:?}{}",
                det.class,
                det.confidence * 100.0,
                det.bbox,
                if censored { "  [would censor]" } else { "" }
            );
        }
    }
    Ok(())
}
