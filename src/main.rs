mod capture;
mod capture_sck;
mod censor_fx;
mod detect;
mod overlay;
mod pipeline;
mod server;
mod settings;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use overlay::{CensorRegion, OverlayApp};
use settings::{CensorSettings, Effective, Package};

fn main() -> Result<()> {
    // Launched from the .app bundle (e.g. as a login item) the working
    // directory is "/" and stderr goes nowhere, so relative paths are
    // pinned to a per-user data dir and the log goes to a file there.
    let log_file = bundle_resources()
        .map(|res| enter_data_dir(&res))
        .transpose()?;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "betamacs=info,ort=error".into());
    match log_file {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(Arc::new(file))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }

    // A silent thread panic would leave the app half-dead with no trace in
    // the log; record it and exit loudly so a supervisor can restart us.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("panic: {info}");
        std::process::exit(101);
    }));

    // Usage: betamacs [run|probe|demo] [320n|640m|path/to/model.onnx] [--censor-captures]
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let censor_in_captures = args.iter().any(|a| a == "--censor-captures");
    args.retain(|a| a != "--censor-captures");
    let (mode, model) = match args.first().map(String::as_str) {
        Some(m @ ("run" | "probe" | "demo")) => (m, args.get(1).cloned()),
        Some(_) => ("run", args.first().cloned()),
        None => ("run", None),
    };

    match mode {
        "probe" => probe(model),
        "demo" => demo(),
        _ => run(model, censor_in_captures),
    }
}

/// Contents/Resources of the .app we are running from, or None when the
/// executable is not inside a bundle (plain `cargo run`).
fn bundle_resources() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents")
        .then(|| contents.join("Resources"))
}

/// Make the per-user data dir the working directory so the existing
/// relative paths (config/, models/) keep working, with models/ a symlink
/// into the bundle. Returns the opened log file.
fn enter_data_dir(resources: &std::path::Path) -> Result<std::fs::File> {
    let data = std::env::home_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?
        .join("Library/Application Support/betamacs");
    std::fs::create_dir_all(&data)?;
    std::env::set_current_dir(&data)?;

    let models = data.join("models");
    match std::fs::symlink_metadata(&models) {
        // Re-point at the current bundle in case the app moved.
        Ok(m) if m.file_type().is_symlink() => std::fs::remove_file(&models)?,
        Ok(_) => {} // a real directory the user manages; leave it
        Err(_) => {}
    }
    if std::fs::symlink_metadata(&models).is_err() {
        std::os::unix::fs::symlink(resources.join("models"), &models)?;
    }

    let log_path = data.join("betamacs.log");
    if let Ok(m) = std::fs::metadata(&log_path)
        && m.len() > 5 * 1024 * 1024
    {
        let _ = std::fs::rename(&log_path, data.join("betamacs.log.old"));
    }
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?)
}

/// Continuous censoring: overlay event loop on the main thread (macOS
/// requirement), capture/detect pipeline and settings server on worker
/// threads.
fn run(model_override: Option<String>, censor_in_captures: bool) -> Result<()> {
    let package_path = PathBuf::from("config/package.json");
    let package = if package_path.exists() {
        Package::load(&package_path)?
    } else {
        let starter = Package::starter();
        starter.save(&package_path)?;
        tracing::info!("wrote starter package to {}", package_path.display());
        starter
    };

    let mut effective = package.resolve();
    // CLI flags act as runtime-only overrides on top of the package.
    if let Some(model) = model_override {
        effective.detection.model = model;
    }
    if censor_in_captures {
        effective.censor.censor_in_captures = true;
    }
    let (model_path, _) = effective.detection.model_path();
    anyhow::ensure!(
        model_path.exists(),
        "model not found at {} — run scripts/fetch-model.sh",
        model_path.display()
    );

    let (event_loop, handle, mut app) = OverlayApp::new(effective.censor.clone())?;
    let shared = Arc::new(RwLock::new(effective));

    let token = server::load_or_create_token(&PathBuf::from("config/api-token"))?;
    let port: u16 = std::env::var("BETAMACS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    tracing::info!("settings UI: http://127.0.0.1:{port}/  (token: {token})");
    server::spawn(
        Arc::new(server::ServerState {
            package: Mutex::new(package),
            effective: shared.clone(),
            overlay: handle.clone(),
            package_path,
            token,
        }),
        port,
        std::env::var("BETAMACS_WEBAPP_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                bundle_resources()
                    .map(|r| r.join("webapp"))
                    .unwrap_or_else(|| PathBuf::from("webapp/dist"))
            }),
    );

    std::thread::spawn(move || {
        if let Err(e) = pipeline::run(shared, handle) {
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
/// Styled from config/package.json when present, so the demo doubles as a
/// style preview.
fn demo() -> Result<()> {
    let style = Package::load(&PathBuf::from("config/package.json"))
        .map(|p| p.resolve().censor)
        .unwrap_or_else(|_| CensorSettings::default());
    let (event_loop, handle, mut app) = OverlayApp::new(style)?;
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
            trigger: "DEMO_TRIGGER",
            text_seed: 0,
            content: None,
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
                         content protection is not excluding it"
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
fn probe(model_override: Option<String>) -> Result<()> {
    let mut effective = Effective::default();
    if let Some(model) = model_override {
        effective.detection.model = model;
    }
    let d = &effective.detection;

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

    let (model_path, input_size) = d.model_path();
    if !model_path.exists() {
        tracing::warn!(
            "model not found at {} — run scripts/fetch-model.sh; skipping detection",
            model_path.display()
        );
        return Ok(());
    }

    let mut detector = detect::Detector::new(&model_path, input_size)?;
    for frame in &frames {
        let start = Instant::now();
        let detections = detector.detect_tiled(
            &frame.image,
            d.tile_grid,
            0.2,
            d.confidence_threshold,
            d.iou_threshold,
        )?;
        tracing::info!(
            "{}: {} detection(s) in {:?}",
            frame.monitor_name,
            detections.len(),
            start.elapsed()
        );
        for det in &detections {
            let censored = d.triggers.get(det.class).copied().unwrap_or(false);
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
