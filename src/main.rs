mod capture;
mod capture_sck;
mod censor_fx;
mod challenge;
mod detect;
mod earned;
mod envelope;
mod heartbeat;
mod menubar;
mod overlay;
mod prompt;
mod smappservice;
mod pipeline;
mod server;
mod settings;
mod statusframe;

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

    let log_file_bundled = log_file.is_some();
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

    // Usage: betamacs [run|probe|demo|install-daemon] [320n|640m|model.onnx] [--censor-captures]
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let censor_in_captures = args.iter().any(|a| a == "--censor-captures");
    args.retain(|a| a != "--censor-captures");
    let (mode, model) = match args.first().map(String::as_str) {
        Some(m @ ("run" | "probe" | "demo" | "install-daemon")) => (m, args.get(1).cloned()),
        Some(_) => ("run", args.first().cloned()),
        None => ("run", None),
    };

    if mode == "install-daemon" {
        anyhow::ensure!(
            log_file_bundled,
            "install-daemon only works from the .app bundle (the daemon plist lives inside it)",
        );
        return install_daemon(true);
    }

    // Bundled but launched by hand (Finder, `open`) rather than by launchd:
    // install/refresh the LaunchAgent and hand off to the instance it
    // starts, so copying the .app and opening it once is a full install.
    // On a managed install (docs/managed-mode.md) the root-owned global
    // agent already runs betamacs; a hand launch does nothing. A managed
    // BUILD not yet bootstrapped additionally registers betamacsd via
    // SMAppService — once approved, the daemon roots the whole layout and
    // retires the per-user agent installed as the bridge here.
    if mode == "run" && log_file_bundled && std::env::var_os("BETAMACS_LAUNCHD").is_none() {
        if PathBuf::from("/Library/LaunchAgents/com.bdstark.betamacs.plist").exists() {
            tracing::info!("managed install detected; the global LaunchAgent owns this app");
            return Ok(());
        }
        if bundle_resources().is_some_and(|r| r.join("otactl-root.pem").exists()) {
            if let Err(e) = install_daemon(false) {
                tracing::warn!("daemon registration failed (continuing per-user): {e:#}");
            }
        }
        return self_install().inspect_err(|e| tracing::error!("self-install failed: {e}"));
    }

    match mode {
        "probe" => probe(model),
        "demo" => demo(),
        _ => run(model, censor_in_captures),
    }
}

/// Write ~/Library/LaunchAgents/com.bdstark.betamacs.plist pointing at the
/// running bundle, replace any loaded agent with it, and return so this
/// hand-launched process exits. The plist sets BETAMACS_LAUNCHD, which is
/// what stops the launchd-started child from landing back here.
fn self_install() -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let exe = std::env::current_exe()?;
    let home = std::env::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let uid = std::fs::metadata(&home)?.uid();
    let log = home.join("Library/Application Support/betamacs/launchd.log");
    let agents = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents)?;
    let plist_path = agents.join("com.bdstark.betamacs.plist");
    std::fs::write(
        &plist_path,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.bdstark.betamacs</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>BETAMACS_LAUNCHD</key>
		<string>1</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
            exe = exe.display(),
            log = log.display(),
        ),
    )?;

    // Replace whatever is loaded; bootout fails harmlessly when nothing is.
    let service = format!("gui/{uid}/com.bdstark.betamacs");
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &service])
        .status();
    // launchd tears the booted-out service down asynchronously, and
    // bootstrapping the same label too soon fails with EIO — retry briefly.
    let mut status = None;
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(500));
        }
        let s = std::process::Command::new("launchctl")
            .arg("bootstrap")
            .arg(format!("gui/{uid}"))
            .arg(&plist_path)
            .status()?;
        status = Some(s);
        if s.success() {
            break;
        }
    }
    let status = status.expect("at least one bootstrap attempt");
    anyhow::ensure!(status.success(), "launchctl bootstrap failed: {status}");
    tracing::info!(
        "installed LaunchAgent {} for {} and handed off",
        plist_path.display(),
        exe.display()
    );
    Ok(())
}

/// Register betamacsd (the root watchdog) from the bundle's own plist
/// via SMAppService: the privileged bootstrap is then one admin approval
/// in System Settings → Login Items, no sudo. Once running as root, the
/// daemon lays down the rest of the managed layout itself.
fn install_daemon(interactive: bool) -> Result<()> {
    use smappservice::ServiceStatus;
    let status = smappservice::register("com.bdstark.betamacsd.plist")?;
    tracing::info!("betamacsd registration status: {status:?}");
    match status {
        ServiceStatus::Enabled => {
            println!("betamacsd is registered and enabled; the managed layout follows.");
        }
        ServiceStatus::RequiresApproval => {
            println!(
                "betamacsd needs one-time approval: System Settings → General → \
                 Login Items & Extensions → allow \"betamacs\" (admin credentials required)."
            );
            if interactive {
                smappservice::open_login_items_settings();
            }
        }
        other => println!("betamacsd registration state: {other:?}"),
    }
    Ok(())
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
/// Root-owned managed config directory, written only by betamacsd.
const MANAGED_DIR: &str = "/Library/Application Support/betamacs";

/// Read and re-verify the envelope betamacsd persisted. Rollback gating
/// happened at write time (the daemon owns the epoch high-water), so
/// this only proves authenticity and integrity.
fn load_managed(verifier: &envelope::Verifier) -> Result<(Package, u64)> {
    let raw = std::fs::read_to_string(PathBuf::from(MANAGED_DIR).join("envelope.json"))?;
    let env: envelope::Envelope = serde_json::from_str(&raw)?;
    let verified = verifier.verify(&env, 0, envelope::CONFIG_APP)?;
    let package: Package = serde_json::from_slice(&verified.artifact)?;
    Ok((package, verified.epoch))
}

/// Re-verify and apply the managed config whenever betamacsd rewrites
/// the envelope.
fn spawn_managed_watch(
    verifier: envelope::Verifier,
    state: Arc<server::ServerState>,
    overlay: overlay::OverlayHandle,
    health: Arc<heartbeat::Health>,
) {
    std::thread::spawn(move || {
        let path = PathBuf::from(MANAGED_DIR).join("envelope.json");
        let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let mut last = mtime(&path);
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let current = mtime(&path);
            if current == last {
                continue;
            }
            last = current;
            match load_managed(&verifier) {
                Ok((package, epoch)) => {
                    let effective = package.resolve();
                    *state.package.lock().unwrap() = package;
                    *state.effective.write().unwrap() = effective.clone();
                    if let Err(e) = overlay.set_style(effective.censor.clone()) {
                        tracing::warn!("could not push managed style to overlay: {e}");
                    }
                    health
                        .config_epoch
                        .store(epoch, std::sync::atomic::Ordering::Relaxed);
                    tracing::info!("managed config applied (epoch {epoch})");
                }
                Err(e) => tracing::error!("managed config update rejected: {e:#}"),
            }
        }
    });
}

fn run(model_override: Option<String>, censor_in_captures: bool) -> Result<()> {
    // Managed mode (docs/managed-mode.md): a pinned otactl root in the
    // bundle switches the settings source to signed envelopes and makes
    // the local API read-only.
    let verifier = bundle_resources()
        .filter(|r| r.join("otactl-root.pem").exists())
        .map(|r| {
            let pem = std::fs::read(r.join("otactl-root.pem"))?;
            let v = envelope::Verifier::from_pem(&pem)?;
            // An author pin beside the root additionally requires
            // author-signed config (docs/managed-mode.md).
            match std::fs::read_to_string(r.join("author-pubkey.pem")) {
                Ok(author) => v.with_author_key_pem(&author),
                Err(_) => Ok(v),
            }
        })
        .transpose()?;
    let managed = verifier.is_some();

    let mut config_epoch = 0u64;
    let package_path = PathBuf::from("config/package.json");
    let package = if let Some(verifier) = &verifier {
        match load_managed(verifier) {
            Ok((package, epoch)) => {
                tracing::info!("managed config loaded (epoch {epoch})");
                config_epoch = epoch;
                package
            }
            Err(e) => {
                // Fail closed: no valid signed config means built-in
                // defaults (all exposure classes trigger) — never "off".
                tracing::error!("managed config unavailable ({e:#}); running default policy");
                Package::starter()
            }
        }
    } else if package_path.exists() {
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

    // Menu bar status item; the settings link carries the token in the
    // URL fragment so the web UI connects without a manual paste.
    match menubar::MenuBar::new(
        format!("http://127.0.0.1:{port}/#token={token}"),
        std::env::current_dir()?.join("betamacs.log"),
        event_loop.create_proxy(),
    ) {
        Some(mb) => app.set_menubar(mb),
        None => tracing::warn!("menu bar item unavailable (not on main thread?)"),
    }

    let state = Arc::new(server::ServerState {
        package: Mutex::new(package),
        effective: shared.clone(),
        overlay: handle.clone(),
        package_path,
        token,
        managed,
    });
    server::spawn(
        state.clone(),
        port,
        std::env::var("BETAMACS_WEBAPP_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                bundle_resources()
                    .map(|r| r.join("webapp"))
                    .unwrap_or_else(|| PathBuf::from("webapp/dist"))
            }),
    );

    // Health reporting to betamacsd, and live pickup of managed config
    // updates the daemon persists.
    let health = heartbeat::Health::new();
    health
        .config_epoch
        .store(config_epoch, std::sync::atomic::Ordering::Relaxed);
    heartbeat::spawn(health.clone());
    if let Some(verifier) = verifier {
        spawn_managed_watch(verifier, state, handle.clone(), health.clone());
    }

    // Activity-challenge scheduler (osascript prompts; enforced via the
    // heartbeat's challengeOverdue signal). No-op until policy enables it.
    challenge::spawn(shared.clone(), health.clone());
    // Earned-time activity monitor (observation only for now; ledger and
    // enforcement are the daemon's, per docs/earned-time.md). No-op until
    // policy enables it.
    earned::spawn(shared.clone(), health.clone());
    // Live status HUD feed (composes stats each second; the window is shown
    // on demand from the menu bar). Display-only.
    statusframe::spawn(health.clone(), handle.clone());

    std::thread::spawn(move || {
        if let Err(e) = pipeline::run(shared, handle, health) {
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
            highlight: None,
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
            &mut detect::TileCache::default(),
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
