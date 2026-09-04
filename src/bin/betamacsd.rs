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
use serde::{Deserialize, Serialize};

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
#[derive(Clone)]
struct AgentState {
    last_seen: Option<Instant>,
    pid: u32,
    capture_ok: bool,
    config_epoch: u64,
    /// False when policy disables censoring — healthy by policy.
    enabled: bool,
    /// The agent has posed an activity challenge that has gone unanswered
    /// past its window — treated as "unprotected" (quarantine after grace).
    challenge_overdue: bool,
    /// When the exposure budget was exceeded, the deadline until which a
    /// TIMED network quarantine is held regardless of current activity.
    /// Computed here from the heartbeat's requested penalty so the lockout
    /// survives the agent being killed.
    exposure_penalty_until: Option<Instant>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            last_seen: None,
            pid: 0,
            capture_ok: true,
            config_epoch: 0,
            enabled: true,
            challenge_overdue: false,
            exposure_penalty_until: None,
        }
    }
}

/// Root-owned earned-time balance (docs/earned-time.md part B). The child
/// cannot edit it; the agent only proposes earned deltas (capped here).
#[derive(Serialize, Deserialize, Default, Clone)]
struct EarnedLedger {
    /// Local YYYY-MM-DD the daily total belongs to (reset on rollover).
    date: String,
    earned_today_min: f64,
    balance_min: f64,
}

/// The earned-time gate: owns the ledger and the latest policy snapshot the
/// agent resolved (the agent knows the schedule and config; the daemon owns
/// the balance the child can't fake, and drives the pf earning-mode gate).
struct EarnedGate {
    ledger: EarnedLedger,
    ledger_path: PathBuf,
    /// The gate is OPEN unless this exists — a delivered, root-owned task
    /// bank is the per-device marker of a managed (kid) device, gated by
    /// the `ext:betamacs-tasks` entitlement. So earned-time (like
    /// challenges) applies only to entitled devices even from a fleet-wide
    /// config; an un-provisioned Mac (no bank) is never gated.
    tasks_path: PathBuf,
    gate_active: bool,
    spend_ratio: f64,
    daily_cap_min: f64,
    max_bank_min: f64,
    allow_hosts: Vec<String>,
    last_report: Option<Instant>,
    last_tick: Instant,
}

impl EarnedGate {
    fn new(paths: &Paths) -> Self {
        let ledger_path = paths.managed_dir.join("earned-ledger.json");
        let ledger = std::fs::read_to_string(&ledger_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            ledger,
            ledger_path,
            tasks_path: paths.managed_dir.join("tasks.json"),
            gate_active: false,
            spend_ratio: 1.0,
            daily_cap_min: 0.0,
            max_bank_min: 0.0,
            allow_hosts: Vec::new(),
            last_report: None,
            last_tick: Instant::now(),
        }
    }

    fn today() -> String {
        std::process::Command::new("/bin/date")
            .args(["+%F"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    fn persist(&self) {
        if let Ok(s) = serde_json::to_string(&self.ledger) {
            let tmp = self.ledger_path.with_extension("json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, &self.ledger_path);
            }
        }
    }

    /// Apply an agent earn report: store the policy snapshot and bank `secs`
    /// of earned credit, capped by the daily cap and the bank ceiling.
    fn apply_report(
        &mut self,
        secs: u32,
        gate_active: bool,
        spend_ratio: f64,
        daily_cap_min: f64,
        max_bank_min: f64,
        allow_hosts: Vec<String>,
    ) {
        let today = Self::today();
        if !today.is_empty() && self.ledger.date != today {
            self.ledger.date = today;
            self.ledger.earned_today_min = 0.0;
        }
        self.gate_active = gate_active;
        self.spend_ratio = spend_ratio.max(0.0);
        self.daily_cap_min = daily_cap_min.max(0.0);
        self.max_bank_min = max_bank_min.max(0.0);
        self.allow_hosts = allow_hosts;
        self.last_report = Some(Instant::now());

        let mut add = secs as f64 / 60.0;
        if self.daily_cap_min > 0.0 {
            add = add.min((self.daily_cap_min - self.ledger.earned_today_min).max(0.0));
        }
        if add > 0.0 {
            self.ledger.earned_today_min += add;
            self.ledger.balance_min += add;
            if self.max_bank_min > 0.0 {
                self.ledger.balance_min = self.ledger.balance_min.min(self.max_bank_min);
            }
            self.persist();
        }
    }

    /// Watch-loop tick. `full_blocked` means a full quarantine reason
    /// (tamper/exposure/challenge) is already active and supersedes this.
    /// Returns the earning-mode allowlist when the internet should be gated
    /// to only the earn sources (gate active + balance depleted), else None.
    fn tick(&mut self, full_blocked: bool) -> Option<Vec<String>> {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;

        // Open unless provisioned: no task bank => not a managed kid device
        // (lacks the ext:betamacs-tasks grant), so never gate, even if a
        // fleet-wide config enables earned-time. Banked balance is left
        // untouched so it survives if the device is later provisioned.
        if !self.tasks_path.exists() {
            return None;
        }
        // A stale snapshot (agent gone) isn't trusted for gating — the
        // heartbeat watchdog covers a dead agent with a full block.
        let fresh = self
            .last_report
            .is_some_and(|t| t.elapsed() < Duration::from_secs(60));
        if !self.gate_active || !fresh || full_blocked {
            return None;
        }
        if self.ledger.balance_min > 0.0 {
            // Spending: time online inside a gate window burns balance.
            let spent = elapsed.as_secs_f64() / 60.0 * self.spend_ratio;
            if spent > 0.0 {
                self.ledger.balance_min = (self.ledger.balance_min - spent).max(0.0);
                self.persist();
            }
            None
        } else {
            Some(self.allow_hosts.clone()) // depleted → earning-mode lockout
        }
    }
}

/// Layer-4 local enforcement (docs/managed-mode.md): when the censor is
/// detectably not protecting an active session — Screen Recording
/// revoked, agent killed/silenced beyond what repair fixes — for longer
/// than the grace period, load a pf ruleset that blocks all traffic
/// except loopback, DHCP, DNS, SSH-in (recovery), and the otactl
/// origins (management keeps working). pfctl is root-only, so a
/// standard user cannot lift it; the rules cover every interface, so
/// tethering or another Wi-Fi doesn't escape. Cleared automatically the
/// moment health returns. The anchor lives under com.apple/* because
/// the stock /etc/pf.conf evaluates that tree — no config edits.
/// What the pf anchor is currently loaded with. `Full` blocks everything
/// but management (tamper/exposure/challenge). `Earning` additionally allows
/// the earn-source hosts, so a child with a depleted balance can still reach
/// the approved sites to earn more time.
#[derive(Clone, PartialEq)]
enum QMode {
    Full,
    Earning(Vec<String>),
}

struct Quarantine {
    engaged: Option<QMode>,
    unhealthy_since: Option<Instant>,
    /// BETAMACSD_NO_QUARANTINE=1, non-root, or a test prefix disables.
    armed: bool,
    /// BETAMACSD_QUARANTINE_DRYRUN=1: full logic, log instead of pfctl.
    dry_run: bool,
    /// Default 180s; BETAMACSD_QUARANTINE_GRACE_SECS overrides.
    grace: Duration,
    rules_path: PathBuf,
}

const PF_ANCHOR: &str = "com.apple/250.BetamacsQuarantine";
const QUARANTINE_GRACE: Duration = Duration::from_secs(180);
const HEARTBEAT_FRESH: Duration = Duration::from_secs(60);
/// Management hosts that stay reachable under quarantine.
const ALLOWED_HOSTS: [&str; 2] = [
    "otactl-device.docker.newton.haus",
    "otactl.docker.newton.haus",
];

impl Quarantine {
    fn new(paths: &Paths) -> Self {
        let dry_run = std::env::var_os("BETAMACSD_QUARANTINE_DRYRUN").is_some();
        let armed = (dry_run
            || (unsafe { libc_geteuid() } == 0
                && std::env::var_os("BETAMACSD_PREFIX").is_none()))
            && std::env::var_os("BETAMACSD_NO_QUARANTINE").is_none();
        if !armed {
            tracing::info!("network quarantine disarmed (env/uid/prefix)");
        }
        let grace = std::env::var("BETAMACSD_QUARANTINE_GRACE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(QUARANTINE_GRACE);
        Self {
            engaged: None,
            unhealthy_since: None,
            armed,
            dry_run,
            grace,
            rules_path: paths.managed_dir.join("quarantine.rules"),
        }
    }

    /// Does a FULL-block reason apply right now (tamper/exposure/challenge)?
    /// Healthy means: no active console session, censoring disabled by
    /// policy, or a fresh heartbeat with working capture — and no unanswered
    /// challenge. A tripped exposure budget forces a full block for its
    /// fixed duration regardless of health. Pure decision; `apply` does pf.
    fn want_full(&mut self, agent: &AgentState) -> bool {
        if !self.armed {
            return false;
        }
        if agent
            .exposure_penalty_until
            .is_some_and(|until| until > Instant::now())
        {
            self.unhealthy_since = None;
            return true;
        }
        let session_active = std::fs::metadata("/dev/console")
            .map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.uid() != 0
            })
            .unwrap_or(false);
        let healthy = (!session_active
            || !agent.enabled
            || (agent
                .last_seen
                .is_some_and(|t| t.elapsed() < HEARTBEAT_FRESH)
                && agent.capture_ok))
            && !agent.challenge_overdue;
        if healthy {
            self.unhealthy_since = None;
            return false;
        }
        let since = *self.unhealthy_since.get_or_insert_with(Instant::now);
        since.elapsed() >= self.grace
    }

    /// Reconcile the pf anchor to the desired mode: `None` releases,
    /// `Full`/`Earning` load the matching ruleset. A no-op when already in
    /// the desired mode.
    fn apply(&mut self, desired: Option<QMode>) {
        if !self.armed || desired == self.engaged {
            return;
        }
        match &desired {
            None => self.release_pf(),
            Some(mode) => {
                let extra = match mode {
                    QMode::Full => &[][..],
                    QMode::Earning(hosts) => hosts.as_slice(),
                };
                self.load_pf(mode.clone(), extra);
            }
        }
    }

    /// Resolve hostnames to a comma-joined IP list for a pf `to { ... }`.
    fn resolve(hosts: &[&str]) -> Vec<String> {
        hosts
            .iter()
            .flat_map(|h| {
                use std::net::ToSocketAddrs;
                format!("{h}:443")
                    .to_socket_addrs()
                    .map(|a| a.map(|s| s.ip().to_string()).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .collect()
    }

    fn build_rules(&self, extra_hosts: &[String]) -> String {
        let mgmt = Self::resolve(&ALLOWED_HOSTS);
        let mut passes = String::new();
        if mgmt.is_empty() {
            tracing::warn!("could not resolve management hosts; quarantine allows DNS only");
        } else {
            passes += &format!(
                "pass out quick proto tcp from any to {{ {} }} port 443\n",
                mgmt.join(", "),
            );
        }
        let earn = Self::resolve(&extra_hosts.iter().map(String::as_str).collect::<Vec<_>>());
        if !earn.is_empty() {
            passes += &format!(
                "pass out quick proto tcp from any to {{ {} }} port {{ 80, 443 }}\n",
                earn.join(", "),
            );
        }
        format!(
            "# betamacs quarantine — loaded by betamacsd when the censor is\n\
             # unprotected, or the earned-time gate is depleted. Removed on recovery.\n\
             pass quick on lo0 all\n\
             pass out quick proto udp from any port 68 to any port 67\n\
             pass out quick proto {{ udp, tcp }} from any to any port 53\n\
             {passes}\
             pass in quick proto tcp from any to any port 22\n\
             block drop quick all\n",
        )
    }

    fn load_pf(&mut self, mode: QMode, extra_hosts: &[String]) {
        let label = match &mode {
            QMode::Full => "full".to_string(),
            QMode::Earning(h) => format!("earning-mode (allow {})", h.join(", ")),
        };
        let rules = self.build_rules(extra_hosts);
        if self.dry_run {
            tracing::warn!("DRY RUN: would load pf anchor {PF_ANCHOR} [{label}]:\n{rules}");
            self.engaged = Some(mode);
            return;
        }
        if let Err(e) = std::fs::write(&self.rules_path, &rules) {
            tracing::error!("could not write quarantine rules: {e}");
            return;
        }
        let _ = std::process::Command::new("/sbin/pfctl").arg("-E").output();
        match std::process::Command::new("/sbin/pfctl")
            .args(["-a", PF_ANCHOR, "-f"])
            .arg(&self.rules_path)
            .output()
        {
            Ok(out) if out.status.success() => {
                self.engaged = Some(mode);
                tracing::warn!("network quarantine ENGAGED [{label}] (pf anchor {PF_ANCHOR})");
            }
            Ok(out) => tracing::error!(
                "pfctl load failed: {}",
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
            Err(e) => tracing::error!("pfctl spawn failed: {e}"),
        }
    }

    fn release_pf(&mut self) {
        if self.dry_run {
            tracing::warn!("DRY RUN: would flush pf anchor {PF_ANCHOR}");
            self.engaged = None;
            return;
        }
        match std::process::Command::new("/sbin/pfctl")
            .args(["-a", PF_ANCHOR, "-F", "all"])
            .output()
        {
            Ok(out) if out.status.success() => {
                self.engaged = None;
                tracing::warn!("network quarantine released");
            }
            Ok(out) => tracing::error!(
                "pfctl flush failed: {}",
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
            Err(e) => tracing::error!("pfctl spawn failed: {e}"),
        }
    }
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

    ensure_managed_layout(&paths);

    let verifier = match envelope::Verifier::from_bundled_root(&paths.app) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("no pinned root — envelopes will be refused: {e}");
            None
        }
    };

    let agent: Arc<Mutex<AgentState>> = Arc::default();
    let earned: Arc<Mutex<EarnedGate>> = Arc::new(Mutex::new(EarnedGate::new(&paths)));

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
        let earned = earned.clone();
        let managed_dir = paths.managed_dir.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let agent = agent.clone();
                        let earned = earned.clone();
                        let managed_dir = managed_dir.clone();
                        let verifier = verifier.clone();
                        std::thread::spawn(move || {
                            handle_client(stream, &agent, &earned, &managed_dir, verifier.as_ref())
                        });
                    }
                    Err(e) => tracing::warn!("accept failed: {e}"),
                }
            }
        });
    }

    // Watchdog loop.
    let mut quarantine = Quarantine::new(&paths);
    let mut last_integrity = Instant::now() - Duration::from_secs(3600);
    loop {
        std::thread::sleep(Duration::from_secs(15));
        watch_agent(&agent);
        // Full-block reasons (tamper/exposure/challenge) take precedence over
        // the earned-time gate; a depleted gate falls back to earning-mode.
        let want_full = quarantine.want_full(&agent.lock().unwrap().clone());
        let earning = earned.lock().unwrap().tick(want_full);
        let desired = if want_full {
            Some(QMode::Full)
        } else {
            earning.map(QMode::Earning)
        };
        quarantine.apply(desired);
        if last_integrity.elapsed() >= Duration::from_secs(600) {
            last_integrity = Instant::now();
            check_integrity(&paths);
        }
    }
}

fn handle_client(
    stream: UnixStream,
    agent: &Mutex<AgentState>,
    earned: &Mutex<EarnedGate>,
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
                a.enabled = msg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                a.challenge_overdue = msg
                    .get("challengeOverdue")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // Exposure budget exceeded is edge-triggered: on each such
                // report start (or extend) a timed lockout of the requested
                // length. The daemon owns the deadline so killing the agent
                // can't cut the penalty short.
                if msg
                    .get("exposureOverBudget")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    let secs = msg.get("exposurePenaltySec").and_then(|v| v.as_u64()).unwrap_or(0);
                    if secs > 0 {
                        let until = Instant::now() + Duration::from_secs(secs);
                        a.exposure_penalty_until = Some(match a.exposure_penalty_until {
                            Some(prev) if prev > until => prev, // keep the longer standing lockout
                            _ => until,
                        });
                        tracing::warn!("agent reports exposure over budget — network lockout for {secs}s");
                    }
                }
                // Same-tab focus limit tripped: another timed full-block,
                // held on the same deadline (whichever is longer wins).
                if msg
                    .get("focusOverLimit")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    let secs = msg.get("focusPenaltySec").and_then(|v| v.as_u64()).unwrap_or(0);
                    if secs > 0 {
                        let until = Instant::now() + Duration::from_secs(secs);
                        a.exposure_penalty_until = Some(match a.exposure_penalty_until {
                            Some(prev) if prev > until => prev,
                            _ => until,
                        });
                        tracing::warn!("agent reports same-tab focus limit — network lockout for {secs}s");
                    }
                }
                if !a.capture_ok {
                    tracing::warn!("agent reports capture unhealthy (Screen Recording revoked?)");
                }
            }
            Some("earn") => {
                // Earned-time report from the agent's activity monitor: the
                // agent resolved the schedule/policy; the daemon banks the
                // (capped) credit and owns the balance and the pf gate.
                let secs = msg.get("secs").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let gate_active = msg.get("gateActive").and_then(|v| v.as_bool()).unwrap_or(false);
                let spend_ratio = msg.get("spendRatio").and_then(|v| v.as_f64()).unwrap_or(1.0);
                let daily_cap = msg.get("dailyCapMin").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let max_bank = msg.get("maxBankMin").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let allow_hosts = msg
                    .get("allowHosts")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|h| h.as_str().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                earned.lock().unwrap().apply_report(
                    secs, gate_active, spend_ratio, daily_cap, max_bank, allow_hosts,
                );
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
            Some("tasks") => {
                let reply = match apply_tasks_envelope(&line, managed_dir, verifier) {
                    Ok(epoch) => {
                        tracing::info!("accepted task-bank envelope, epoch {epoch}");
                        "{\"ok\":true}\n".to_string()
                    }
                    Err(e) => {
                        tracing::warn!("task-bank envelope refused: {e:#}");
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
                let quarantine_secs = a
                    .exposure_penalty_until
                    .map(|u| u.saturating_duration_since(Instant::now()).as_secs() as i64)
                    .unwrap_or(0);
                let (earned_balance_min, earned_gate_active, earned_today_min) = {
                    let e = earned.lock().unwrap();
                    (e.ledger.balance_min, e.gate_active, e.ledger.earned_today_min)
                };
                let reply = format!(
                    "{{\"ok\":true,\"agentPid\":{},\"heartbeatAgeSecs\":{},\"captureOk\":{},\"configEpoch\":{},\"tasksEpoch\":{},\"enabled\":{},\"challengeOverdue\":{},\"exposureLockoutSecs\":{},\"earnedBalanceMin\":{:.1},\"earnedGateActive\":{},\"earnedTodayMin\":{:.1}}}\n",
                    a.pid,
                    a.last_seen.map(|t| t.elapsed().as_secs() as i64).unwrap_or(-1),
                    a.capture_ok,
                    a.config_epoch,
                    read_epoch(&managed_dir.join("epoch-tasks")),
                    a.enabled,
                    a.challenge_overdue,
                    quarantine_secs,
                    earned_balance_min,
                    earned_gate_active,
                    earned_today_min,
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

/// Verify and persist a task-bank envelope; returns the accepted epoch.
/// The bank is a separate artifact with its own epoch high-water, so a new
/// question set can't be rolled back independently of config or the app.
/// The daemon only takes custody and enforces (via the heartbeat signals);
/// selection and answer-checking live in the agent, which reads this file
/// like it reads package.json. Answers in the bank are stored hashed, so a
/// world-readable tasks.json is not a cheat sheet.
fn apply_tasks_envelope(
    raw: &str,
    managed_dir: &Path,
    verifier: Option<&envelope::Verifier>,
) -> Result<u64> {
    let verifier = verifier.context("no pinned otactl root installed")?;
    let env: envelope::Envelope = serde_json::from_str(raw).context("malformed envelope")?;
    let epoch_path = managed_dir.join("epoch-tasks");
    let verified = verifier.verify(&env, read_epoch(&epoch_path), envelope::TASKS_APP)?;

    // Persist artifact then bump the epoch high-water last, so a crash never
    // leaves the epoch ahead of the bank (mirrors apply_envelope).
    let tmp = managed_dir.join("tasks.json.tmp");
    std::fs::write(&tmp, &verified.artifact)?;
    std::fs::rename(&tmp, managed_dir.join("tasks.json"))?;
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

/// Running as root with an incomplete managed layout (the SMAppService
/// bootstrap path, docs/managed-mode.md): finish what the sudo installer
/// would have done — take root ownership of the bundle, install the
/// global LaunchAgent, and migrate the console session off any per-user
/// agent that served as the bridge.
fn ensure_managed_layout(paths: &Paths) {
    if unsafe { libc_geteuid() } != 0 {
        return;
    }
    use std::os::unix::fs::MetadataExt;
    if let Ok(meta) = std::fs::metadata(&paths.app)
        && meta.uid() != 0
    {
        tracing::info!("taking root ownership of {}", paths.app.display());
        let _ = run("/usr/sbin/chown", &["-R", "root:wheel"], &[&paths.app]);
        let _ = run("/bin/chmod", &["-R", "go-w"], &[&paths.app]);
    }
    if paths.agent_plist.exists() {
        return;
    }
    tracing::info!("installing global LaunchAgent {}", paths.agent_plist.display());
    if let Some(parent) = paths.agent_plist.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&paths.agent_plist, agent_plist(&paths.app)) {
        tracing::error!("could not write {}: {e}", paths.agent_plist.display());
        return;
    }
    let Ok(console) = std::fs::metadata("/dev/console") else {
        return;
    };
    let uid = console.uid();
    // Same label, new plist: drop the per-user registration and its file,
    // then load the global agent into the live session.
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &format!("gui/{uid}/{AGENT_LABEL}")])
        .status();
    if let Ok(out) = std::process::Command::new("/usr/bin/stat")
        .args(["-f", "%Su", "/dev/console"])
        .output()
    {
        let user = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !user.is_empty() && user != "root" {
            let _ = std::fs::remove_file(format!(
                "/Users/{user}/Library/LaunchAgents/{AGENT_LABEL}.plist"
            ));
        }
    }
    let _ = std::process::Command::new("/bin/launchctl")
        .arg("bootstrap")
        .arg(format!("gui/{uid}"))
        .arg(&paths.agent_plist)
        .status();
}

/// Verify managed files exist with sane ownership; rewrite plists we
/// own, report what we cannot fix. The agent plist is always maintained;
/// the /Library/LaunchDaemons plist only when a script install created
/// it — the SMAppService path runs the daemon from the bundle's own
/// plist and must not gain a second registration under the same label.
fn check_integrity(paths: &Paths) {
    let daemon_exists = paths.daemon_plist.exists();
    for (path, content, create) in [
        (&paths.agent_plist, agent_plist(&paths.app), true),
        (
            &paths.daemon_plist,
            daemon_plist(&paths.app, &paths.managed_dir),
            daemon_exists,
        ),
    ] {
        let current = std::fs::read_to_string(path).unwrap_or_default();
        if current != content && (create || !current.is_empty()) {
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

#[cfg(test)]
mod earned_tests {
    use super::*;

    // A gate with a task bank present (a provisioned kid device). `path` is
    // unique per test; the bank sits beside it.
    fn gate(path: &str) -> EarnedGate {
        let tasks = format!("{path}.tasks");
        std::fs::write(&tasks, "{}").unwrap();
        EarnedGate {
            ledger: EarnedLedger::default(),
            ledger_path: PathBuf::from(path),
            tasks_path: PathBuf::from(tasks),
            gate_active: true,
            spend_ratio: 1.0,
            daily_cap_min: 0.0,
            max_bank_min: 0.0,
            allow_hosts: vec!["khanacademy.org".into()],
            last_report: Some(Instant::now()),
            last_tick: Instant::now(),
        }
    }

    #[test]
    fn banks_and_caps_daily() {
        let mut g = gate("/tmp/bm-earn-t1.json");
        // 600s = 10 earned min, but the daily cap is 5.
        g.apply_report(600, true, 1.0, 5.0, 0.0, vec![]);
        assert!((g.ledger.balance_min - 5.0).abs() < 1e-6);
        g.apply_report(600, true, 1.0, 5.0, 0.0, vec![]); // cap already hit
        assert!((g.ledger.balance_min - 5.0).abs() < 1e-6);
        let _ = std::fs::remove_file("/tmp/bm-earn-t1.json");
    }

    #[test]
    fn bank_ceiling() {
        let mut g = gate("/tmp/bm-earn-t2.json");
        g.apply_report(6000, true, 1.0, 0.0, 30.0, vec![]); // 100 min, ceiling 30
        assert!((g.ledger.balance_min - 30.0).abs() < 1e-6);
        let _ = std::fs::remove_file("/tmp/bm-earn-t2.json");
    }

    #[test]
    fn spends_then_locks_to_earning_mode() {
        let mut g = gate("/tmp/bm-earn-t3.json");
        g.ledger.balance_min = 1.0;
        g.last_tick = Instant::now() - Duration::from_secs(30);
        // Gate active, balance > 0 → spend, no lockout.
        assert!(g.tick(false).is_none());
        assert!(g.ledger.balance_min < 1.0 && g.ledger.balance_min > 0.0);
        // Depleted → earning-mode allowlist.
        g.ledger.balance_min = 0.0;
        g.last_tick = Instant::now();
        assert_eq!(g.tick(false), Some(vec!["khanacademy.org".to_string()]));
        // A full-block reason supersedes the earned gate.
        assert!(g.tick(true).is_none());
        let _ = std::fs::remove_file("/tmp/bm-earn-t3.json");
    }

    #[test]
    fn no_earning_when_inactive_or_stale() {
        let mut g = gate("/tmp/bm-earn-t4.json");
        g.ledger.balance_min = 0.0;
        g.gate_active = false;
        assert!(g.tick(false).is_none()); // outside a schedule window
        g.gate_active = true;
        g.last_report = Some(Instant::now() - Duration::from_secs(120)); // agent gone
        assert!(g.tick(false).is_none());
        let _ = std::fs::remove_file("/tmp/bm-earn-t4.json");
        let _ = std::fs::remove_file("/tmp/bm-earn-t4.json.tasks");
    }

    #[test]
    fn open_without_task_bank() {
        // No bank (unprovisioned Mac, e.g. the parent's) => never gated,
        // even with the gate active and a depleted balance.
        let mut g = gate("/tmp/bm-earn-t5.json");
        std::fs::remove_file(&g.tasks_path).unwrap(); // no bank delivered
        g.ledger.balance_min = 0.0;
        assert!(g.tick(false).is_none());
        let _ = std::fs::remove_file("/tmp/bm-earn-t5.json");
    }
}
