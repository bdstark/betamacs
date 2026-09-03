//! betamacsd: the root watchdog daemon for managed betamacs installs
//! (docs/managed-mode.md). A standard user cannot stop it; it is the
//! only writer of the root-owned managed config directory.
//!
//! Responsibilities:
//!   1. Envelope custody — accept signed config envelopes on a unix
//!      socket from anyone (they are self-authenticating), verify
//!      signature/chain/artifact-hash/epoch, persist root-owned.
//!   2. Heartbeat watch — the per-user agent reports health every few
//!      seconds; silence with a live process means it was stopped
//!      (SIGSTOP/debugger) and gets a SIGCONT; silence without a
//!      process is left to launchd KeepAlive but logged.
//!   3. Integrity repair — the LaunchAgent/LaunchDaemon plists are
//!      rewritten if missing or altered; app-bundle code signature is
//!      spot-checked and failures reported.
//!
//! Test mode: BETAMACSD_PREFIX rebases every path (socket included)
//! into a directory so the daemon can be exercised without root.

#[path = "../envelope.rs"]
mod envelope;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

const AGENT_LABEL: &str = "com.bdstark.betamacs";
const APP_PATH: &str = "/Applications/betamacs.app";

struct Paths {
    socket: PathBuf,
    managed_dir: PathBuf,
    agent_plist: PathBuf,
    daemon_plist: PathBuf,
    app: PathBuf,
}

impl Paths {
    fn new() -> Self {
        let prefix = std::env::var_os("BETAMACSD_PREFIX").map(PathBuf::from);
        let root = |p: &str| match &prefix {
            Some(pre) => pre.join(p.trim_start_matches('/')),
            None => PathBuf::from(p),
        };
        Self {
            socket: root("/var/run/betamacsd.sock"),
            managed_dir: root("/Library/Application Support/betamacs"),
            agent_plist: root("/Library/LaunchAgents/com.bdstark.betamacs.plist"),
            daemon_plist: root("/Library/LaunchDaemons/com.bdstark.betamacsd.plist"),
            app: root(APP_PATH),
        }
    }
}

/// Last heartbeat seen from the agent.
#[derive(Default, Clone)]
struct AgentState {
    last_seen: Option<Instant>,
    pid: u32,
    capture_ok: bool,
    config_epoch: u64,
}

fn main() -> Result<()> {
    let paths = Paths::new();
    std::fs::create_dir_all(&paths.managed_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.managed_dir.join("betamacsd.log"))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "betamacsd=info".into()),
        )
        .with_ansi(false)
        .with_writer(Arc::new(log))
        .init();
    tracing::info!(
        "betamacsd {} starting (prefix: {:?})",
        env!("CARGO_PKG_VERSION"),
        std::env::var_os("BETAMACSD_PREFIX"),
    );

    let verifier = match envelope::Verifier::from_bundled_root(&paths.app) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("no pinned root — envelopes will be refused: {e}");
            None
        }
    };

    let agent: Arc<Mutex<AgentState>> = Arc::default();

    // Socket listener thread.
    let _ = std::fs::remove_file(&paths.socket);
    if let Some(parent) = paths.socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("bind {}", paths.socket.display()))?;
    // World-writable socket: heartbeats are advisory and envelopes are
    // signature-verified, so the sender's identity is irrelevant.
    std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o666))?;
    {
        let agent = agent.clone();
        let managed_dir = paths.managed_dir.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let agent = agent.clone();
                        let managed_dir = managed_dir.clone();
                        let verifier = verifier.clone();
                        std::thread::spawn(move || {
                            handle_client(stream, &agent, &managed_dir, verifier.as_ref())
                        });
                    }
                    Err(e) => tracing::warn!("accept failed: {e}"),
                }
            }
        });
    }

    // Watchdog loop.
    let mut last_integrity = Instant::now() - Duration::from_secs(3600);
    loop {
        std::thread::sleep(Duration::from_secs(15));
        watch_agent(&agent);
        if last_integrity.elapsed() >= Duration::from_secs(600) {
            last_integrity = Instant::now();
            check_integrity(&paths);
        }
    }
}

fn handle_client(
    stream: UnixStream,
    agent: &Mutex<AgentState>,
    managed_dir: &Path,
    verifier: Option<&envelope::Verifier>,
) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    // One JSON message per line; a connection may send several
    // heartbeats or a single envelope.
    while {
        line.clear();
        matches!(reader.read_line(&mut line), Ok(n) if n > 0)
    } {
        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("unparseable message: {e}");
                continue;
            }
        };
        match msg.get("type").and_then(|t| t.as_str()) {
            Some("heartbeat") => {
                let mut a = agent.lock().unwrap();
                a.last_seen = Some(Instant::now());
                a.pid = msg.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                a.capture_ok = msg.get("captureOk").and_then(|v| v.as_bool()).unwrap_or(true);
                a.config_epoch = msg.get("configEpoch").and_then(|v| v.as_u64()).unwrap_or(0);
                if !a.capture_ok {
                    tracing::warn!("agent reports capture unhealthy (Screen Recording revoked?)");
                }
            }
            Some("envelope") => {
                let reply = match apply_envelope(&line, managed_dir, verifier) {
                    Ok(epoch) => {
                        tracing::info!("accepted config envelope, epoch {epoch}");
                        "{\"ok\":true}\n".to_string()
                    }
                    Err(e) => {
                        tracing::warn!("envelope refused: {e:#}");
                        format!("{{\"ok\":false,\"error\":{}}}\n", serde_json::json!(e.to_string()))
                    }
                };
                let mut stream = reader.into_inner();
                let _ = stream.write_all(reply.as_bytes());
                return;
            }
            Some("app") => {
                let reply = match apply_app_envelope(&line, managed_dir, verifier) {
                    Ok(version) => {
                        tracing::info!("installed betamacs {version}; restarting daemon to match");
                        // KeepAlive respawns us from the new bundle.
                        std::thread::spawn(|| {
                            std::thread::sleep(Duration::from_secs(2));
                            std::process::exit(0);
                        });
                        "{\"ok\":true}\n".to_string()
                    }
                    Err(e) => {
                        tracing::warn!("app envelope refused: {e:#}");
                        format!("{{\"ok\":false,\"error\":{}}}\n", serde_json::json!(e.to_string()))
                    }
                };
                let mut stream = reader.into_inner();
                let _ = stream.write_all(reply.as_bytes());
                return;
            }
            Some("status") => {
                let a = agent.lock().unwrap().clone();
                let reply = format!(
                    "{{\"ok\":true,\"agentPid\":{},\"heartbeatAgeSecs\":{},\"captureOk\":{},\"configEpoch\":{}}}\n",
                    a.pid,
                    a.last_seen.map(|t| t.elapsed().as_secs() as i64).unwrap_or(-1),
                    a.capture_ok,
                    a.config_epoch,
                );
                let mut stream = reader.into_inner();
                let _ = stream.write_all(reply.as_bytes());
                return;
            }
            other => tracing::debug!("unknown message type {other:?}"),
        }
    }
}

/// Verify and persist a config envelope; returns the accepted epoch.
fn apply_envelope(
    raw: &str,
    managed_dir: &Path,
    verifier: Option<&envelope::Verifier>,
) -> Result<u64> {
    let verifier = verifier.context("no pinned otactl root installed")?;
    let env: envelope::Envelope = serde_json::from_str(raw).context("malformed envelope")?;
    let epoch_path = managed_dir.join("epoch");
    let last_epoch: u64 = read_epoch(&epoch_path);
    let verified = verifier.verify(&env, last_epoch, envelope::CONFIG_APP)?;

    // Persist artifact + envelope atomically-ish, then bump the epoch
    // high-water last so a crash never leaves epoch ahead of config.
    let tmp = managed_dir.join("package.json.tmp");
    std::fs::write(&tmp, &verified.artifact)?;
    std::fs::rename(&tmp, managed_dir.join("package.json"))?;
    std::fs::write(managed_dir.join("envelope.json"), raw)?;
    std::fs::write(&epoch_path, format!("{}\n", verified.epoch))?;
    Ok(verified.epoch)
}

fn read_epoch(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Verify and install a betamacs.app update: unzip, code-signature and
/// version checks, root-owned swap of /Applications/betamacs.app, agent
/// kickstart. Returns the installed version; the caller restarts the
/// daemon so the new bundle's betamacsd takes over.
fn apply_app_envelope(
    raw: &str,
    managed_dir: &Path,
    verifier: Option<&envelope::Verifier>,
) -> Result<String> {
    let verifier = verifier.context("no pinned otactl root installed")?;
    let env: envelope::Envelope = serde_json::from_str(raw).context("malformed envelope")?;
    if let Some(format) = env.manifest.format.as_deref()
        && !format.is_empty()
        && format != "macos-app-zip"
    {
        anyhow::bail!("unexpected artifact format {format:?}");
    }
    let epoch_path = managed_dir.join("epoch-app");
    let verified = verifier.verify(&env, read_epoch(&epoch_path), envelope::APP_APP)?;
    let version = verified.version.clone();

    // Unpack into a staging dir inside the managed (root-owned) tree.
    let staging = managed_dir.join("staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let zip = staging.join("betamacs.zip");
    std::fs::write(&zip, &verified.artifact)?;
    run("/usr/bin/ditto", &["-x", "-k"], &[&zip, &staging.join("unpacked")])?;

    // The bundle root is wherever the .app is: directly, or one dir deep.
    let unpacked = staging.join("unpacked");
    let bundle = find_app_bundle(&unpacked)
        .context("no betamacs.app in the archive")?;

    // Gate on the code signature like the Hausmeister updater: the new
    // bundle must verify, and — when an install exists — carry the same
    // team as the running one.
    run("/usr/bin/codesign", &["--verify", "--strict"], &[&bundle])?;
    let app = Paths::new().app;
    if app.exists() {
        let (old_team, new_team) = (codesign_team(&app)?, codesign_team(&bundle)?);
        anyhow::ensure!(
            old_team == new_team,
            "new bundle team {new_team:?} does not match installed {old_team:?}",
        );
    }
    let plist_version = run_capture(
        "/usr/libexec/PlistBuddy",
        &["-c", "Print :CFBundleShortVersionString"],
        &[&bundle.join("Contents/Info.plist")],
    )?;
    anyhow::ensure!(
        plist_version.trim() == version,
        "bundle says version {:?}, manifest {version:?}",
        plist_version.trim(),
    );

    // Root-owned swap with rollback, then the agent restarts into it.
    // (chown only when actually root, so prefix test runs still work.)
    if unsafe { libc_geteuid() } == 0 {
        run("/usr/sbin/chown", &["-R", "root:wheel"], &[&bundle])?;
    }
    run("/bin/chmod", &["-R", "go-w"], &[&bundle])?;
    let old = app.with_extension("app.old");
    let _ = std::fs::remove_dir_all(&old);
    let had_existing = app.exists();
    if had_existing {
        std::fs::rename(&app, &old).context("move current app aside")?;
    }
    if let Err(e) = std::fs::rename(&bundle, &app) {
        if had_existing {
            let _ = std::fs::rename(&old, &app);
        }
        return Err(anyhow::Error::from(e).context("move new app into place"));
    }
    let _ = std::fs::remove_dir_all(&old);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::write(&epoch_path, format!("{}\n", verified.epoch))?;

    // Restart the console user's agent into the new bundle.
    if let Ok(meta) = std::fs::metadata("/dev/console") {
        use std::os::unix::fs::MetadataExt;
        let uid = meta.uid();
        let _ = std::process::Command::new("/bin/launchctl")
            .args(["kickstart", "-k", &format!("gui/{uid}/{AGENT_LABEL}")])
            .status();
    }
    Ok(version)
}

fn find_app_bundle(dir: &Path) -> Option<PathBuf> {
    let is_app = |p: &PathBuf| p.extension().is_some_and(|e| e == "app");
    let entries = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .map(|r| r.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    };
    let top = entries(dir);
    if let Some(app) = top.iter().find(|p| is_app(p)) {
        return Some(app.clone());
    }
    let dirs: Vec<&PathBuf> = top.iter().filter(|p| p.is_dir()).collect();
    if let [only] = dirs.as_slice() {
        return entries(only).into_iter().find(|p| is_app(p));
    }
    None
}

fn codesign_team(app: &Path) -> Result<String> {
    let out = std::process::Command::new("/usr/bin/codesign")
        .args(["-dv"])
        .arg(app)
        .output()?;
    // codesign writes details to stderr.
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines()
        .find_map(|l| l.strip_prefix("TeamIdentifier="))
        .map(str::to_string)
        .context("no TeamIdentifier in codesign output")
}

fn run(tool: &str, args: &[&str], paths: &[&Path]) -> Result<()> {
    let mut cmd = std::process::Command::new(tool);
    cmd.args(args);
    for p in paths {
        cmd.arg(p);
    }
    let out = cmd.output().with_context(|| format!("spawn {tool}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{tool} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim(),
    );
    Ok(())
}

fn run_capture(tool: &str, args: &[&str], paths: &[&Path]) -> Result<String> {
    let mut cmd = std::process::Command::new(tool);
    cmd.args(args);
    for p in paths {
        cmd.arg(p);
    }
    let out = cmd.output().with_context(|| format!("spawn {tool}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{tool} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim(),
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// React to heartbeat state: resume a stopped agent, log a missing one.
fn watch_agent(agent: &Mutex<AgentState>) {
    let a = agent.lock().unwrap().clone();
    let Some(last_seen) = a.last_seen else { return };
    if last_seen.elapsed() < Duration::from_secs(30) || a.pid == 0 {
        return;
    }
    // Heartbeat is stale. Is the process alive but suspended?
    let stat = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &a.pid.to_string()])
        .output();
    match stat {
        Ok(out) if out.status.success() => {
            let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if stat.starts_with('T') {
                // Darwin SIGCONT = 19 (differs from Linux).
                tracing::warn!("agent pid {} is suspended (stat {stat}); resuming", a.pid);
                unsafe {
                    libc_kill(a.pid as i32, 19);
                }
            } else {
                tracing::warn!(
                    "agent pid {} alive (stat {stat}) but heartbeat silent {}s",
                    a.pid,
                    last_seen.elapsed().as_secs(),
                );
            }
        }
        _ => tracing::warn!(
            "agent pid {} gone, heartbeat silent {}s (launchd should relaunch)",
            a.pid,
            last_seen.elapsed().as_secs(),
        ),
    }
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

/// Verify managed files exist with sane ownership; rewrite plists we
/// own, report what we cannot fix.
fn check_integrity(paths: &Paths) {
    for (path, content) in [
        (&paths.agent_plist, agent_plist(&paths.app)),
        (&paths.daemon_plist, daemon_plist(&paths.app, &paths.managed_dir)),
    ] {
        let current = std::fs::read_to_string(path).unwrap_or_default();
        if current != content {
            tracing::warn!("{} missing or altered; rewriting", path.display());
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(path, &content) {
                tracing::error!("could not rewrite {}: {e}", path.display());
            }
        }
    }
    if !paths.app.exists() {
        tracing::error!("{} is missing — cannot repair without an artifact", paths.app.display());
        return;
    }
    let out = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(&paths.app)
        .output();
    if let Ok(out) = out
        && !out.status.success()
    {
        tracing::error!(
            "app bundle failed code-signature verification: {}",
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
}

fn agent_plist(app: &Path) -> String {
    plist_template(
        AGENT_LABEL,
        &app.join("Contents/MacOS/betamacs"),
        "\t<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>\n",
    )
}

fn daemon_plist(app: &Path, managed_dir: &Path) -> String {
    let _ = managed_dir;
    plist_template("com.bdstark.betamacsd", &app.join("Contents/MacOS/betamacsd"), "")
}

fn plist_template(label: &str, program: &Path, extra: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{program}</string>
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
{extra}</dict>
</plist>
"#,
        label = label,
        program = program.display(),
        extra = extra,
    )
}
