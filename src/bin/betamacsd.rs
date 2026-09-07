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
#[path = "../dnsfilter.rs"]
mod dnsfilter;

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::Digest;

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

/// Which agent signal set the current `exposure_penalty_until` deadline.
/// Both the exposure-budget trip and the same-tab focus limit fold into one
/// timed lockout deadline; this remembers WHICH so the quarantine reason
/// reported to the HUD is accurate ("too many exposures" vs "too much
/// scrolling") rather than a generic "timed penalty".
#[derive(Clone, Copy, PartialEq, Eq)]
enum PenaltySource {
    Exposure,
    Focus,
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
    /// Which signal owns the current `exposure_penalty_until` (whichever set
    /// the standing deadline). None when no timed penalty is active.
    penalty_source: Option<PenaltySource>,
    /// The agent detected the wall clock being CHANGED under a running
    /// instance — treated as tamper (quarantine), the same as a shut-down
    /// censor, until it clears.
    clock_tamper: bool,
    /// Debounce for the best-effort clock resync triggered by a boot-wrong
    /// report, so a persistent discrepancy doesn't resync every heartbeat.
    last_clock_resync: Option<Instant>,
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
            penalty_source: None,
            clock_tamper: false,
            last_clock_resync: None,
        }
    }
}

/// Best-effort clock resync when the agent reports the machine booted with
/// the wrong time. Root-only (this daemon is root): enabling macOS network
/// time both corrects it now and keeps it corrected, then a direct SNTP step
/// nudges an immediate correction. Never fatal — a wrong boot time is
/// announced and fixed, not punished (a mid-run change is what quarantines).
fn resync_clock() {
    match std::process::Command::new("/usr/sbin/systemsetup")
        .args(["-setusingnetworktime", "on"])
        .output()
    {
        Ok(o) if o.status.success() => {
            tracing::warn!("clock resync: enabled network time after boot-wrong report")
        }
        Ok(o) => tracing::warn!(
            "clock resync: systemsetup failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::warn!("clock resync: systemsetup spawn failed: {e}"),
    }
    let _ = std::process::Command::new("/usr/bin/sntp")
        .args(["-sS", "time.apple.com"])
        .output();
}

/// Root-owned earned-time balance (docs/earned-time.md part B). The child
/// cannot edit it; the agent only proposes earned deltas (capped here).
#[derive(Serialize, Deserialize, Default, Clone)]
struct EarnedLedger {
    /// Local YYYY-MM-DD the daily total belongs to (reset on rollover).
    date: String,
    earned_today_min: f64,
    balance_min: f64,
    /// Parent-verified chores (docs/chores.md). Absent in pre-chores
    /// ledgers, hence the default.
    #[serde(default)]
    chores: ChoreLedger,
}

// ------------------------------------------------------------------ chores
//
// Parent-verified external tasks (docs/chores.md). The bank (tasks.json)
// carries the definitions; the agent relays the `chores` policy module in
// its earn report; this daemon owns everything the child must not be able
// to fake: claims, verifications, the PIN hash (root-only file), the PIN
// attempt counter, and the credit. A `required` chore holds the earned-time
// gate (earning mode) on its due day until verified; a `bonus` chore
// credits minutes on verification, capped.

/// A chore as defined in the bank. Parsed here with serde defaults so the
/// daemon needs nothing from settings.rs (kept lean on purpose).
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ChoreDef {
    id: String,
    #[serde(default)]
    name: String,
    /// "bonus" (default) | "required"
    #[serde(default)]
    kind: String,
    /// "daily" (default) | "weekly" | "once"
    #[serde(default)]
    repeat: String,
    #[serde(default)]
    days: Vec<String>,
    #[serde(default)]
    minutes: u32,
}

impl ChoreDef {
    fn required(&self) -> bool {
        self.kind == "required"
    }
    /// Is the chore due on `day` (mon..sun)? Weekly/once chores are due every
    /// day of their period; daily ones on their listed days (empty = all).
    fn due_on(&self, day: &str) -> bool {
        match self.repeat.as_str() {
            "weekly" | "once" => true,
            _ => self.days.is_empty() || self.days.iter().any(|d| d.eq_ignore_ascii_case(day)),
        }
    }
    /// The period key a claim/verification belongs to.
    fn period(&self, now: &LocalNow) -> String {
        match self.repeat.as_str() {
            "weekly" => now.week.clone(),
            "once" => "once".to_string(),
            _ => now.date.clone(),
        }
    }
}

#[derive(Deserialize, Default)]
struct BankChores {
    #[serde(default)]
    chores: Vec<ChoreDef>,
}

/// The `chores` policy module as relayed by the agent (docs/chores.md).
#[derive(Clone, Debug, PartialEq, Default)]
struct ChorePolicy {
    enabled: bool,
    bonus_daily_cap_min: f64,
    claim_ttl_min: f64,
    /// HHMM local, e.g. 900 for "09:00".
    required_hold_from: u32,
    verify_max_attempts: u32,
    verify_lockout_sec: u64,
}

impl ChorePolicy {
    fn from_msg(v: Option<&serde_json::Value>) -> Self {
        let Some(v) = v else { return Self::default() };
        let num = |k: &str, d: f64| v.get(k).and_then(|x| x.as_f64()).unwrap_or(d);
        let hold = v
            .get("requiredHoldFrom")
            .and_then(|x| x.as_str())
            .and_then(parse_hhmm)
            .unwrap_or(0);
        Self {
            enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
            bonus_daily_cap_min: num("bonusDailyCapMin", 0.0).max(0.0),
            claim_ttl_min: num("claimTtlMin", 0.0).max(0.0),
            required_hold_from: hold,
            verify_max_attempts: num("verifyMaxAttempts", 5.0).max(1.0) as u32,
            verify_lockout_sec: num("verifyLockoutSec", 600.0).max(0.0) as u64,
        }
    }
}

/// "HH:MM" -> HHMM as a number (09:30 -> 930).
fn parse_hhmm(t: &str) -> Option<u32> {
    let (h, m) = t.split_once(':')?;
    Some(h.trim().parse::<u32>().ok()? * 100 + m.trim().parse::<u32>().ok()?)
}

/// A claim the child made, awaiting a parent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct ChoreClaim {
    id: String,
    period: String,
    claimed_at: u64,
}

/// A verification a parent made (with the minutes actually credited).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct ChoreDone {
    id: String,
    period: String,
    at: u64,
    minutes: f64,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
struct ChoreLedger {
    #[serde(default)]
    claims: Vec<ChoreClaim>,
    #[serde(default)]
    verified: Vec<ChoreDone>,
    /// Bonus minutes credited today (reset on rollover).
    #[serde(default)]
    bonus_today_min: f64,
    #[serde(default)]
    pin_failures: u32,
    #[serde(default)]
    pin_locked_until: Option<u64>,
}

/// The daemon's view of local time for chore periods. From `/bin/date` (the
/// OS clock — a mid-run clock change is already a tamper full-block).
#[derive(Clone, Debug, PartialEq, Default)]
struct LocalNow {
    /// YYYY-MM-DD
    date: String,
    /// ISO week, YYYY-Www
    week: String,
    /// mon..sun
    day: String,
    /// HHMM
    hhmm: u32,
}

impl LocalNow {
    fn read() -> Self {
        let out = std::process::Command::new("/bin/date")
            .args(["+%F %G-W%V %u %H%M"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let mut it = out.split_whitespace();
        let date = it.next().unwrap_or("").to_string();
        let week = it.next().unwrap_or("").to_string();
        let dow: usize = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let hhmm: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let day = ["", "mon", "tue", "wed", "thu", "fri", "sat", "sun"]
            .get(dow)
            .copied()
            .unwrap_or("")
            .to_string();
        Self { date, week, day, hhmm }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Outcome of a `chore-verify`, on the wire as `result`.
#[derive(Clone, Debug, PartialEq)]
enum ChoreVerify {
    /// Verified; `minutes` were credited (0 for required chores).
    Ok { minutes: f64 },
    WrongPin { attempts_left: u32 },
    /// PIN entry refused for `secs` more seconds.
    Locked { secs: u64 },
    /// No PIN delivered with the bank, module off, unknown id, etc.
    Refused(&'static str),
}

/// Check `pin` against a `sha256$<salt>$<digest>` line as written by
/// `publish.sh tasks` (same digest as challenge answers: salt || 0 || pin).
fn pin_matches(stored: &str, pin: &str) -> bool {
    let mut parts = stored.trim().split('$');
    if parts.next() != Some("sha256") {
        return false;
    }
    let (Some(salt), Some(digest)) = (parts.next(), parts.next()) else {
        return false;
    };
    let mut h = sha2::Sha256::new();
    h.update(salt.as_bytes());
    h.update([0u8]);
    h.update(pin.trim().as_bytes());
    format!("{:x}", h.finalize()) == digest
}

/// Split the PIN hash out of a delivered bank: `chorePinHash` goes to the
/// root-only `chore-pin` file, and `tasks.json` is written without it. A bank
/// with no PIN removes any stale `chore-pin`, so an old PIN never outlives
/// the bank that set it. A bank that isn't a JSON object is written as-is.
fn install_bank(managed_dir: &Path, artifact: &[u8]) -> Result<()> {
    let pin_path = managed_dir.join("chore-pin");
    let tmp = managed_dir.join("tasks.json.tmp");
    let mut value: serde_json::Value = match serde_json::from_slice(artifact) {
        Ok(v) => v,
        Err(_) => {
            std::fs::write(&tmp, artifact)?;
            std::fs::rename(&tmp, managed_dir.join("tasks.json"))?;
            return Ok(());
        }
    };
    let pin = value
        .as_object_mut()
        .and_then(|o| o.remove("chorePinHash"))
        .and_then(|v| v.as_str().map(str::to_string));
    match pin {
        Some(hash) => {
            use std::os::unix::fs::OpenOptionsExt;
            let ptmp = managed_dir.join("chore-pin.tmp");
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&ptmp)?;
            f.write_all(hash.as_bytes())?;
            f.write_all(b"\n")?;
            std::fs::set_permissions(&ptmp, std::fs::Permissions::from_mode(0o600))?;
            std::fs::rename(&ptmp, &pin_path)?;
        }
        None => {
            let _ = std::fs::remove_file(&pin_path);
        }
    }
    let bytes = serde_json::to_vec_pretty(&value)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, managed_dir.join("tasks.json"))?;
    Ok(())
}

/// The site-filter policy snapshot the agent resolved from the config
/// (docs/site-filter.md). Enforced here through the local DNS filter + pf.
#[derive(Clone, PartialEq, Default, Debug)]
struct FilterPolicy {
    enabled: bool,
    audit_only: bool,
    /// Reachable in earning mode, on top of the earn sources.
    allow: Vec<String>,
    /// Never reachable while the filter is on.
    block: Vec<String>,
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
    filter: FilterPolicy,
    last_report: Option<Instant>,
    last_tick: Instant,
    /// Chore policy snapshot (from the agent's earn report) and the bank's
    /// chore definitions (re-read from tasks.json when it changes).
    chores: ChorePolicy,
    chore_defs: Vec<ChoreDef>,
    chore_defs_mtime: Option<std::time::SystemTime>,
    /// Root-only `sha256$salt$digest` of the parent PIN (docs/chores.md).
    pin_path: PathBuf,
    /// Tests pin local time; production reads /bin/date.
    now_override: Option<LocalNow>,
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
            filter: FilterPolicy::default(),
            last_report: None,
            last_tick: Instant::now(),
            chores: ChorePolicy::default(),
            chore_defs: Vec::new(),
            chore_defs_mtime: None,
            pin_path: paths.managed_dir.join("chore-pin"),
            now_override: None,
        }
    }

    fn local_now(&self) -> LocalNow {
        self.now_override.clone().unwrap_or_else(LocalNow::read)
    }

    /// Reset the daily counters when the local date moves on, and drop
    /// chore state from past periods. Idempotent within a day.
    fn rollover(&mut self, now: &LocalNow) {
        if now.date.is_empty() || self.ledger.date == now.date {
            return;
        }
        self.ledger.date = now.date.clone();
        self.ledger.earned_today_min = 0.0;
        self.ledger.chores.bonus_today_min = 0.0;
        let defs = self.chore_defs.clone();
        let current = |id: &str, period: &str| {
            defs.iter()
                .find(|d| d.id == id)
                .is_some_and(|d| d.period(now) == period)
        };
        self.ledger.chores.claims.retain(|c| current(&c.id, &c.period));
        self.ledger
            .chores
            .verified
            .retain(|v| v.period == "once" || current(&v.id, &v.period));
    }

    /// Bank `minutes` of credit into the balance under the earned-time daily
    /// cap and bank ceiling. Returns what was actually credited.
    fn bank(&mut self, minutes: f64) -> f64 {
        let mut add = minutes.max(0.0);
        if self.daily_cap_min > 0.0 {
            add = add.min((self.daily_cap_min - self.ledger.earned_today_min).max(0.0));
        }
        if add > 0.0 {
            self.ledger.earned_today_min += add;
            self.ledger.balance_min += add;
            if self.max_bank_min > 0.0 {
                self.ledger.balance_min = self.ledger.balance_min.min(self.max_bank_min);
            }
        }
        add
    }

    /// (Re)load the chore definitions when the bank changed on disk.
    fn reload_chores(&mut self) {
        let mtime = std::fs::metadata(&self.tasks_path)
            .and_then(|m| m.modified())
            .ok();
        if mtime.is_none() {
            self.chore_defs.clear();
            self.chore_defs_mtime = None;
            return;
        }
        if mtime == self.chore_defs_mtime {
            return;
        }
        self.chore_defs_mtime = mtime;
        self.chore_defs = std::fs::read_to_string(&self.tasks_path)
            .ok()
            .and_then(|s| serde_json::from_str::<BankChores>(&s).ok())
            .map(|b| b.chores)
            .unwrap_or_default();
        tracing::info!("chores: loaded {} definition(s) from the bank", self.chore_defs.len());
    }

    fn chore(&self, id: &str) -> Option<&ChoreDef> {
        self.chore_defs.iter().find(|d| d.id == id)
    }

    fn is_verified(&self, id: &str, period: &str) -> bool {
        self.ledger
            .chores
            .verified
            .iter()
            .any(|v| v.id == id && v.period == period)
    }

    /// Drop claims past the TTL or from another period.
    fn expire_claims(&mut self, now: &LocalNow) {
        let ttl = (self.chores.claim_ttl_min * 60.0) as u64;
        let t = unix_now();
        let defs = self.chore_defs.clone();
        self.ledger.chores.claims.retain(|c| {
            let Some(d) = defs.iter().find(|d| d.id == c.id) else { return false };
            d.period(now) == c.period && (ttl == 0 || t.saturating_sub(c.claimed_at) < ttl)
        });
    }

    /// Chores whose absence holds the gate right now: enabled, required,
    /// due today, not verified for the current period, and past the hold
    /// hour. Empty when the module is off or nothing is outstanding.
    fn required_outstanding(&self, now: &LocalNow) -> Vec<String> {
        if !self.chores.enabled || now.hhmm < self.chores.required_hold_from {
            return Vec::new();
        }
        self.chore_defs
            .iter()
            .filter(|d| d.required() && d.due_on(&now.day))
            .filter(|d| !self.is_verified(&d.id, &d.period(now)))
            .map(|d| d.id.clone())
            .collect()
    }

    /// Claims awaiting a parent (current period, unexpired).
    fn pending(&self, now: &LocalNow) -> Vec<String> {
        let ttl = (self.chores.claim_ttl_min * 60.0) as u64;
        let t = unix_now();
        self.ledger
            .chores
            .claims
            .iter()
            .filter(|c| self.chore(&c.id).is_some_and(|d| d.period(now) == c.period))
            .filter(|c| ttl == 0 || t.saturating_sub(c.claimed_at) < ttl)
            .map(|c| c.id.clone())
            .collect()
    }

    /// Seconds of PIN lockout remaining, 0 when open.
    fn pin_locked_secs(&self) -> u64 {
        self.ledger
            .chores
            .pin_locked_until
            .map(|u| u.saturating_sub(unix_now()))
            .unwrap_or(0)
    }

    /// The child marks a chore done. Idempotent per period. Errors are wire
    /// strings for the agent's dialog.
    fn chore_claim(&mut self, id: &str) -> Result<(), &'static str> {
        self.reload_chores();
        let now = self.local_now();
        self.rollover(&now);
        self.expire_claims(&now);
        if !self.chores.enabled {
            return Err("chores are not enabled");
        }
        let Some(def) = self.chore(id).cloned() else { return Err("unknown chore") };
        let period = def.period(&now);
        if self.is_verified(id, &period) {
            return Err("already verified");
        }
        if !self.ledger.chores.claims.iter().any(|c| c.id == id && c.period == period) {
            self.ledger.chores.claims.push(ChoreClaim {
                id: id.to_string(),
                period,
                claimed_at: unix_now(),
            });
            self.persist();
        }
        Ok(())
    }

    /// The child (or a parent pressing Cancel) withdraws a claim.
    fn chore_reject(&mut self, id: &str) {
        let before = self.ledger.chores.claims.len();
        self.ledger.chores.claims.retain(|c| c.id != id);
        if self.ledger.chores.claims.len() != before {
            self.persist();
        }
    }

    /// A parent verifies a chore with the PIN. A claim is not required (the
    /// PIN is the parent's word), but a chore already verified this period
    /// is refused so it can't be credited twice. Wrong PINs count toward a
    /// lockout that survives agent restarts (it lives in the ledger).
    fn chore_verify(&mut self, id: &str, pin: &str) -> ChoreVerify {
        self.reload_chores();
        let now = self.local_now();
        self.rollover(&now);
        self.expire_claims(&now);
        if !self.chores.enabled {
            return ChoreVerify::Refused("chores are not enabled");
        }
        let Some(def) = self.chore(id).cloned() else {
            return ChoreVerify::Refused("unknown chore");
        };
        let period = def.period(&now);
        if self.is_verified(id, &period) {
            return ChoreVerify::Refused("already verified");
        }
        let Ok(stored) = std::fs::read_to_string(&self.pin_path) else {
            return ChoreVerify::Refused("no PIN delivered with the task bank");
        };
        let locked = self.pin_locked_secs();
        if locked > 0 {
            return ChoreVerify::Locked { secs: locked };
        }
        if !pin_matches(&stored, pin) {
            let c = &mut self.ledger.chores;
            c.pin_failures += 1;
            let out = if c.pin_failures >= self.chores.verify_max_attempts {
                c.pin_failures = 0;
                c.pin_locked_until = Some(unix_now() + self.chores.verify_lockout_sec);
                ChoreVerify::Locked { secs: self.chores.verify_lockout_sec }
            } else {
                ChoreVerify::WrongPin {
                    attempts_left: self.chores.verify_max_attempts - c.pin_failures,
                }
            };
            self.persist();
            return out;
        }
        self.ledger.chores.pin_failures = 0;
        self.ledger.chores.pin_locked_until = None;
        self.ledger.chores.claims.retain(|c| c.id != id);
        let mut minutes = 0.0;
        if !def.required() && def.minutes > 0 {
            let mut want = def.minutes as f64;
            if self.chores.bonus_daily_cap_min > 0.0 {
                want = want
                    .min((self.chores.bonus_daily_cap_min - self.ledger.chores.bonus_today_min).max(0.0));
            }
            minutes = self.bank(want);
            self.ledger.chores.bonus_today_min += minutes;
        }
        self.ledger.chores.verified.push(ChoreDone {
            id: id.to_string(),
            period,
            at: unix_now(),
            minutes,
        });
        self.persist();
        tracing::info!("chores: \"{id}\" verified (+{minutes:.0} min)");
        ChoreVerify::Ok { minutes }
    }

    /// The chore snapshot for the `status` / `chores` replies.
    fn chores_status(&mut self) -> serde_json::Value {
        self.reload_chores();
        let now = self.local_now();
        let today_verified: Vec<&str> = self
            .ledger
            .chores
            .verified
            .iter()
            .filter(|v| self.chore(&v.id).is_some_and(|d| d.period(&now) == v.period))
            .map(|v| v.id.as_str())
            .collect();
        let defs: Vec<serde_json::Value> = self
            .chore_defs
            .iter()
            .map(|d| {
                let kind = if d.required() { "required" } else { "bonus" };
                let repeat = if d.repeat.is_empty() { "daily" } else { d.repeat.as_str() };
                serde_json::json!({
                    "id": d.id, "name": d.name, "kind": kind, "repeat": repeat,
                    "minutes": d.minutes, "due": d.due_on(&now.day),
                })
            })
            .collect();
        serde_json::json!({
            "enabled": self.chores.enabled && self.tasks_path.exists(),
            "pinSet": self.pin_path.exists(),
            "outstanding": self.required_outstanding(&now),
            "pending": self.pending(&now),
            "verified": today_verified,
            "pinLockedSecs": self.pin_locked_secs(),
            "chores": defs,
        })
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
        filter: FilterPolicy,
        chores: ChorePolicy,
    ) {
        self.reload_chores();
        let now = self.local_now();
        self.rollover(&now);
        self.chores = chores;
        self.gate_active = gate_active;
        self.spend_ratio = spend_ratio.max(0.0);
        self.daily_cap_min = daily_cap_min.max(0.0);
        self.max_bank_min = max_bank_min.max(0.0);
        self.allow_hosts = allow_hosts;
        self.filter = filter;
        self.last_report = Some(Instant::now());

        if self.bank(secs as f64 / 60.0) > 0.0 {
            self.persist();
        }
    }

    /// Watch-loop tick. `full_blocked` means a full quarantine reason
    /// (tamper/exposure/challenge) is already active and supersedes this.
    /// Returns the earning-mode allowlist and why, when the internet should
    /// be gated to only the earn sources: gate active and either an
    /// outstanding required chore (`QReason::Chores`) or a depleted balance
    /// (`QReason::EarnedGate`). None when open.
    fn tick(&mut self, full_blocked: bool) -> Option<(Vec<String>, QReason)> {
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
        // An unverified required chore holds the gate regardless of balance
        // (and no balance is spent while it does — the child is only on the
        // earn sites). Nothing is due before the hold hour.
        self.reload_chores();
        let now = self.local_now();
        self.rollover(&now);
        if !self.required_outstanding(&now).is_empty() {
            return Some((self.allow_hosts.clone(), QReason::Chores));
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
            Some((self.allow_hosts.clone(), QReason::EarnedGate)) // depleted → earning-mode lockout
        }
    }

    /// The site-filter policy to enforce right now: None when the module is
    /// off, the agent's snapshot is stale, or this is not a provisioned kid
    /// device (same task-bank marker as the gate — a fleet-wide config never
    /// redirects the parent Mac's DNS).
    fn filter_policy(&self) -> Option<FilterPolicy> {
        if !self.tasks_path.exists() || !self.filter.enabled {
            return None;
        }
        let fresh = self
            .last_report
            .is_some_and(|t| t.elapsed() < Duration::from_secs(60));
        fresh.then(|| self.filter.clone())
    }
}

/// What the watchdog wants loaded: a pf ruleset and a DNS-filter mode.
/// Orthogonal on purpose — the blocklist needs DNS pinned but no lockdown,
/// the legacy earning mode needs a lockdown but no DNS.
#[derive(Clone, PartialEq, Debug)]
struct Desired {
    pf: PfMode,
    dns: dnsfilter::Mode,
}

impl Desired {
    const OPEN: Desired = Desired { pf: PfMode::Open, dns: dnsfilter::Mode::Off };
}

/// Compose the pf + DNS state from the watchdog's three inputs. Pure —
/// unit-tested. `earning` is the earn-source allowlist when the gate is
/// depleted; `filter` the site-filter policy when it applies.
fn compose(want_full: bool, earning: Option<Vec<String>>, filter: Option<FilterPolicy>) -> Desired {
    use dnsfilter::Mode;
    if want_full {
        return Desired { pf: PfMode::Full, dns: Mode::Off };
    }
    match (earning, filter) {
        // Depleted balance + site filter: only the earn sources and the
        // configured allowlist resolve; pf passes only what they resolve to.
        (Some(hosts), Some(f)) if !f.audit_only => {
            let mut allow = hosts.clone();
            allow.extend(f.allow.iter().cloned());
            allow.extend(ALLOWED_HOSTS.iter().map(|h| h.to_string()));
            Desired { pf: PfMode::EarningTable(hosts), dns: Mode::Allow { allow, block: f.block } }
        }
        // Audit never enforces: legacy gate, DNS only observed.
        (Some(hosts), Some(_)) => Desired { pf: PfMode::EarningStatic(hosts), dns: Mode::Audit },
        (Some(hosts), None) => Desired { pf: PfMode::EarningStatic(hosts), dns: Mode::Off },
        (None, Some(f)) if f.audit_only => Desired { pf: PfMode::Open, dns: Mode::Audit },
        (None, Some(f)) if !f.block.is_empty() => {
            Desired { pf: PfMode::DnsLock, dns: Mode::Block { block: f.block } }
        }
        _ => Desired::OPEN,
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
/// What the pf anchor is loaded with. `Full` blocks everything but
/// management (tamper/exposure/challenge). The two earning modes additionally
/// allow the earn-source hosts so a child with a depleted balance can still
/// reach the approved sites: `EarningStatic` by resolving their apex IPs
/// (legacy; misses CDNs), `EarningTable` by passing a pf table the DNS filter
/// fills with whatever the allowlisted names resolve to (docs/site-filter.md).
/// `DnsLock` is not a lockdown: it only pins DNS to the local filter so the
/// blocklist can't be resolved around.
#[derive(Clone, PartialEq, Debug)]
enum PfMode {
    Open,
    Full,
    EarningStatic(Vec<String>),
    EarningTable(Vec<String>),
    DnsLock,
}

/// Why the daemon is (or would be) fully blocking the network — the single
/// source of truth for the "why is the internet off" question surfaced in the
/// `status` reply and the HUD. `None` means no full block is in effect. Each
/// variant maps to a stable wire string via `as_str`.
///
/// Timed variants (Exposure/Focus) carry a countdown via
/// `AgentState::exposure_penalty_until`; EarnedGate is a legitimate
/// no-countdown gate (spend earned time to lift it); the rest are health/tamper
/// blocks that clear the moment the underlying condition clears.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum QReason {
    #[default]
    None,
    Exposure,
    Focus,
    Challenge,
    EarnedGate,
    /// A required chore is unverified on its due day (docs/chores.md).
    Chores,
    ClockTamper,
    CaptureUnhealthy,
    HeartbeatStale,
    SessionHealth,
}

impl QReason {
    fn as_str(self) -> &'static str {
        match self {
            QReason::None => "none",
            QReason::Exposure => "exposure",
            QReason::Focus => "focus",
            QReason::Challenge => "challenge",
            QReason::EarnedGate => "earned-gate",
            QReason::Chores => "chores",
            QReason::ClockTamper => "clock-tamper",
            QReason::CaptureUnhealthy => "capture-unhealthy",
            QReason::HeartbeatStale => "heartbeat-stale",
            QReason::SessionHealth => "session/health",
        }
    }
}

struct Quarantine {
    engaged: Desired,
    unhealthy_since: Option<Instant>,
    /// BETAMACSD_NO_QUARANTINE=1, non-root, or a test prefix disables.
    armed: bool,
    /// BETAMACSD_QUARANTINE_DRYRUN=1: full logic, log instead of pfctl.
    dry_run: bool,
    /// Default 180s; BETAMACSD_QUARANTINE_GRACE_SECS overrides.
    grace: Duration,
    rules_path: PathBuf,
    /// The local DNS filter (docs/site-filter.md) and the system-resolver
    /// redirect that feeds it; `pf_hook` adds learned IPs to the allow table.
    filter: dnsfilter::Filter,
    sysdns: dnsfilter::SystemDns,
    pf_hook: dnsfilter::PfHook,
    /// Manual DNS servers replaced by the redirect (used as upstreams).
    saved_manual: Vec<String>,
    /// Upstreams baked into the loaded ruleset (reload when they change).
    loaded_upstreams: Vec<SocketAddr>,
}

const PF_ANCHOR: &str = "com.apple/250.BetamacsQuarantine";
/// pf table (inside the anchor) of IPs the DNS filter learned for allowed names.
const PF_TABLE: &str = "betamacs_allow";
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
        let pf_hook = Self::table_hook(dry_run);
        let sysdns = dnsfilter::SystemDns::new(&paths.managed_dir, dry_run);
        // A previous instance died while DNS was redirected: put it back
        // now; the watchdog re-engages within a tick if still warranted.
        if armed && sysdns.is_engaged() {
            tracing::warn!("system DNS was left redirected by a previous instance; restoring");
            sysdns.restore();
        }
        Self {
            engaged: Desired::OPEN,
            unhealthy_since: None,
            armed,
            dry_run,
            grace,
            rules_path: paths.managed_dir.join("quarantine.rules"),
            filter: dnsfilter::Filter::new(&paths.managed_dir, pf_hook.clone()),
            sysdns,
            pf_hook,
            saved_manual: Vec::new(),
            loaded_upstreams: Vec::new(),
        }
    }

    /// `pfctl -a ANCHOR -t TABLE -T add …` for IPs the DNS filter learned.
    fn table_hook(dry_run: bool) -> dnsfilter::PfHook {
        Arc::new(move |ips: &[IpAddr]| {
            let list: Vec<String> = ips.iter().map(|i| i.to_string()).collect();
            if dry_run {
                tracing::info!("DRY RUN: would add to pf table {PF_TABLE}: {}", list.join(", "));
                return;
            }
            match std::process::Command::new("/sbin/pfctl")
                .args(["-a", PF_ANCHOR, "-t", PF_TABLE, "-T", "add"])
                .args(&list)
                .output()
            {
                Ok(o) if o.status.success() => tracing::debug!("pf table += {}", list.join(", ")),
                Ok(o) => tracing::warn!(
                    "pf table add failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => tracing::warn!("pfctl spawn failed: {e}"),
            }
        })
    }

    /// Which FULL-block reason applies right now, or `QReason::None` when the
    /// censor is healthy. This is the daemon's authoritative "why is the net
    /// off" decision (minus the earned-time gate, which the caller folds in).
    ///
    /// Precedence, highest first:
    ///   1. A timed penalty (exposure budget / same-tab focus) — immediate, no
    ///      grace, supersedes everything; the reason names which one set it.
    ///   2. A health/tamper reason (see `unhealthy_reason`) — but only after it
    ///      has persisted past the grace window (debounced by `unhealthy_since`)
    ///      so a transient blip doesn't quarantine.
    ///
    /// Pure decision; `apply` does the pf work.
    fn want_full(&mut self, agent: &AgentState) -> QReason {
        if !self.armed {
            return QReason::None;
        }
        if agent
            .exposure_penalty_until
            .is_some_and(|until| until > Instant::now())
        {
            self.unhealthy_since = None;
            return match agent.penalty_source {
                Some(PenaltySource::Focus) => QReason::Focus,
                // Default to exposure if the source was somehow not recorded
                // (e.g. a deadline restored across a restart) — never lie by
                // reporting "none" while a timed lockout is in force.
                _ => QReason::Exposure,
            };
        }
        let session_active = std::fs::metadata("/dev/console")
            .map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.uid() != 0
            })
            .unwrap_or(false);
        let reason = Self::unhealthy_reason(agent, session_active);
        if reason == QReason::None {
            self.unhealthy_since = None;
            return QReason::None;
        }
        let since = *self.unhealthy_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= self.grace {
            reason
        } else {
            QReason::None
        }
    }

    /// The non-timed full-block reason that applies to `agent` right now,
    /// ignoring the grace debounce (that is `want_full`'s job). `None` when the
    /// censor is healthy: no active console session, censoring disabled by
    /// policy, or a fresh heartbeat with working capture — and no unanswered
    /// challenge or clock tamper. `session_active` is whether a non-root
    /// console session is logged in (passed in so this stays pure and
    /// unit-testable; `want_full` reads /dev/console for it). Split out so the
    /// reason-selection is unit-testable.
    fn unhealthy_reason(agent: &AgentState, session_active: bool) -> QReason {
        // Tamper first: a mid-run clock change is a hard signal regardless of
        // session/heartbeat state.
        if agent.clock_tamper {
            return QReason::ClockTamper;
        }
        if agent.challenge_overdue {
            return QReason::Challenge;
        }
        // An unprotected console session: someone is logged in and policy has
        // censoring on, yet the agent isn't demonstrably protecting the
        // screen. Pin the specific failure so the HUD can name it.
        if session_active && agent.enabled {
            if !agent.capture_ok {
                return QReason::CaptureUnhealthy; // Screen Recording revoked
            }
            match agent.last_seen {
                Some(t) if t.elapsed() >= HEARTBEAT_FRESH => return QReason::HeartbeatStale,
                None => return QReason::SessionHealth, // agent never checked in
                _ => {}                                // fresh + capture ok → healthy
            }
        }
        QReason::None
    }

    /// Reconcile pf and the DNS filter to `desired`. A no-op when already
    /// there (except that upstream resolvers are re-discovered while DNS is
    /// engaged, and the ruleset reloaded if they moved — network switch).
    fn apply(&mut self, mut desired: Desired) {
        use dnsfilter::Mode;
        if !self.armed {
            return;
        }
        // Engage DNS first so the filter is answering before pf pins port 53.
        if desired.dns != Mode::Off {
            if desired.dns != self.engaged.dns && !self.filter.set_mode(desired.dns.clone()) {
                // Can't bind :53 (another local resolver?) — fall back to the
                // legacy IP gate so the child is still gated, not open.
                tracing::error!("dns filter unavailable; falling back to the static earning gate");
                desired = Desired {
                    pf: match desired.pf {
                        PfMode::EarningTable(h) => PfMode::EarningStatic(h),
                        PfMode::DnsLock => PfMode::Open,
                        other => other,
                    },
                    dns: Mode::Off,
                };
            } else {
                if self.engaged.dns == Mode::Off {
                    self.saved_manual = self.sysdns.engage();
                }
                self.filter.set_upstreams(dnsfilter::discover_upstreams(&self.saved_manual));
            }
        }
        let ups = self.filter.upstreams();
        let pf_changed = desired.pf != self.engaged.pf
            || (Self::uses_upstreams(&desired.pf) && ups != self.loaded_upstreams);
        if pf_changed {
            match &desired.pf {
                PfMode::Open => self.release_pf(),
                mode => self.load_pf(mode.clone(), &ups),
            }
        }
        if desired.dns == Mode::Off && self.engaged.dns != Mode::Off {
            self.sysdns.restore();
            self.saved_manual.clear();
            self.filter.set_mode(Mode::Off);
        }
        self.engaged = desired;
    }

    fn uses_upstreams(pf: &PfMode) -> bool {
        matches!(pf, PfMode::EarningTable(_) | PfMode::DnsLock)
    }

    /// Resolve hostnames to IPs (for a pf `to { ... }` or a table seed).
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

    fn build_rules(&self, pf: &PfMode, upstreams: &[SocketAddr]) -> String {
        let mgmt = Self::resolve(&ALLOWED_HOSTS);
        let mgmt_pass = if mgmt.is_empty() {
            tracing::warn!("could not resolve management hosts; quarantine allows DNS only");
            String::new()
        } else {
            format!(
                "pass out quick proto tcp from any to {{ {} }} port 443\n",
                mgmt.join(", "),
            )
        };
        // Port 53 only to the resolvers the local filter forwards to; any
        // other resolver (and DoT on 853) is dropped so the filter can't be
        // bypassed by pointing an app at a different server.
        let ups: Vec<String> = upstreams.iter().map(|u| u.ip().to_string()).collect();
        let dns_pass = if ups.is_empty() {
            "pass out quick proto { udp, tcp } from any to any port 53\n".to_string()
        } else {
            format!("pass out quick proto {{ udp, tcp }} from any to {{ {} }} port 53\n", ups.join(", "))
        };
        let doh = dnsfilter::DOH_IPS.join(", ");
        match pf {
            PfMode::Open => String::new(),
            PfMode::Full | PfMode::EarningStatic(_) => {
                let mut passes = mgmt_pass;
                if let PfMode::EarningStatic(hosts) = pf {
                    let earn = Self::resolve(&hosts.iter().map(String::as_str).collect::<Vec<_>>());
                    if !earn.is_empty() {
                        passes += &format!(
                            "pass out quick proto tcp from any to {{ {} }} port {{ 80, 443 }}\n",
                            earn.join(", "),
                        );
                    }
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
            PfMode::EarningTable(_) => format!(
                "# betamacs earning-mode lockout (docs/site-filter.md): only what the\n\
                 # allowlisted names resolve to is reachable; the local DNS filter fills\n\
                 # the table. Removed when the balance goes positive.\n\
                 table <{PF_TABLE}> persist\n\
                 pass quick on lo0 all\n\
                 pass out quick proto udp from any port 68 to any port 67\n\
                 {dns_pass}\
                 {mgmt_pass}\
                 pass out quick proto {{ tcp, udp }} from any to <{PF_TABLE}> port {{ 80, 443 }}\n\
                 pass in quick proto tcp from any to any port 22\n\
                 block drop quick all\n",
            ),
            PfMode::DnsLock => format!(
                "# betamacs dns lock (docs/site-filter.md): the blocklist is enforced by\n\
                 # the local DNS filter; these rules only stop resolving around it.\n\
                 pass quick on lo0 all\n\
                 {dns_pass}\
                 block drop out quick proto {{ udp, tcp }} from any to any port {{ 53, 853 }}\n\
                 block drop out quick proto {{ tcp, udp }} from any to {{ {doh} }} port 443\n\
                 block drop out quick proto udp from any to any port 443\n",
            ),
        }
    }

    fn load_pf(&mut self, mode: PfMode, upstreams: &[SocketAddr]) {
        let label = match &mode {
            PfMode::Open => "open".to_string(),
            PfMode::Full => "full".to_string(),
            PfMode::EarningStatic(h) => format!("earning-mode/static (allow {})", h.join(", ")),
            PfMode::EarningTable(h) => format!("earning-mode/dns (seed {})", h.join(", ")),
            PfMode::DnsLock => "dns-lock".to_string(),
        };
        let rules = self.build_rules(&mode, upstreams);
        let loaded = if self.dry_run {
            tracing::warn!("DRY RUN: would load pf anchor {PF_ANCHOR} [{label}]:\n{rules}");
            true
        } else if let Err(e) = std::fs::write(&self.rules_path, &rules) {
            tracing::error!("could not write quarantine rules: {e}");
            false
        } else {
            let _ = std::process::Command::new("/sbin/pfctl").arg("-E").output();
            match std::process::Command::new("/sbin/pfctl")
                .args(["-a", PF_ANCHOR, "-f"])
                .arg(&self.rules_path)
                .output()
            {
                Ok(out) if out.status.success() => {
                    tracing::warn!("network quarantine ENGAGED [{label}] (pf anchor {PF_ANCHOR})");
                    true
                }
                Ok(out) => {
                    tracing::error!(
                        "pfctl load failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim(),
                    );
                    false
                }
                Err(e) => {
                    tracing::error!("pfctl spawn failed: {e}");
                    false
                }
            }
        };
        if !loaded {
            return;
        }
        self.loaded_upstreams = upstreams.to_vec();
        if let PfMode::EarningTable(seed) = &mode {
            // A fresh (or reloaded) anchor means an empty table: forget what
            // the filter already handed over and seed it with the earn hosts'
            // apex IPs so a page already open keeps working.
            self.filter.reset_learned();
            let ips: Vec<IpAddr> = Self::resolve(&seed.iter().map(String::as_str).collect::<Vec<_>>())
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect();
            if !ips.is_empty() {
                (self.pf_hook)(&ips);
            }
        }
    }

    fn release_pf(&mut self) {
        self.loaded_upstreams.clear();
        if self.dry_run {
            tracing::warn!("DRY RUN: would flush pf anchor {PF_ANCHOR}");
            return;
        }
        match std::process::Command::new("/sbin/pfctl")
            .args(["-a", PF_ANCHOR, "-F", "all"])
            .output()
        {
            Ok(out) if out.status.success() => {
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
    // The watchdog loop's latest full-block reason (including the earned-time
    // gate), published here so the `status` handler reports the ACTUAL block
    // state instead of re-deriving it from a subset of signals. Refreshed each
    // watchdog tick; timed countdowns are recomputed live in the handler.
    let quarantine_reason: Arc<Mutex<QReason>> = Arc::new(Mutex::new(QReason::None));
    // The quarantine (pf + DNS filter) lives on the watchdog loop; the DNS
    // filter handle is shared with the `status` handler so the recent-names
    // lists are live, and the engaged mode is published each tick.
    let mut quarantine = Quarantine::new(&paths);
    let filter_status: Arc<Mutex<(String, dnsfilter::Filter)>> =
        Arc::new(Mutex::new(("off".to_string(), quarantine.filter.clone())));

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
        let quarantine_reason = quarantine_reason.clone();
        let filter_status = filter_status.clone();
        let managed_dir = paths.managed_dir.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let agent = agent.clone();
                        let earned = earned.clone();
                        let quarantine_reason = quarantine_reason.clone();
                        let filter_status = filter_status.clone();
                        let managed_dir = managed_dir.clone();
                        let verifier = verifier.clone();
                        std::thread::spawn(move || {
                            handle_client(
                                stream,
                                &agent,
                                &earned,
                                &quarantine_reason,
                                &filter_status,
                                &managed_dir,
                                verifier.as_ref(),
                            )
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
        // Full-block reasons (tamper/exposure/challenge) take precedence over
        // the earned-time gate; a depleted gate falls back to earning-mode.
        let full_reason = quarantine.want_full(&agent.lock().unwrap().clone());
        let want_full = full_reason != QReason::None;
        let (earning, filter) = {
            let mut e = earned.lock().unwrap();
            (e.tick(want_full), e.filter_policy())
        };
        let desired = compose(want_full, earning.as_ref().map(|(h, _)| h.clone()), filter);
        // Publish the effective reason for the status handler: a full block's
        // reason, else why earning-mode engaged (depleted balance or an
        // outstanding required chore), else none. This is the single source
        // of truth the HUD reads.
        *quarantine_reason.lock().unwrap() = if want_full {
            full_reason
        } else if let Some((_, why)) = earning {
            why
        } else {
            QReason::None
        };
        quarantine.apply(desired);
        filter_status.lock().unwrap().0 = quarantine.engaged.dns.as_str().to_string();
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
    quarantine_reason: &Mutex<QReason>,
    filter_status: &Mutex<(String, dnsfilter::Filter)>,
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
                        // Keep the longer standing lockout; the source follows
                        // whichever deadline actually stands, so the reported
                        // reason matches the countdown the HUD shows.
                        if a.exposure_penalty_until.is_none_or(|prev| until >= prev) {
                            a.exposure_penalty_until = Some(until);
                            a.penalty_source = Some(PenaltySource::Exposure);
                        }
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
                        if a.exposure_penalty_until.is_none_or(|prev| until >= prev) {
                            a.exposure_penalty_until = Some(until);
                            a.penalty_source = Some(PenaltySource::Focus);
                        }
                        tracing::warn!("agent reports same-tab focus limit — network lockout for {secs}s");
                    }
                }
                // Clock changed under a running instance: latch tamper so the
                // quarantine holds (like challengeOverdue) until it clears.
                a.clock_tamper = msg
                    .get("clockTamper")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if a.clock_tamper {
                    tracing::warn!("agent reports the clock was changed under a running instance — quarantining");
                }
                // Booted with the wrong time (no running-instance jump): a
                // one-shot, debounced best-effort resync — not a punishment.
                if msg
                    .get("clockBootWrong")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                    && a.last_clock_resync
                        .is_none_or(|t| t.elapsed() > Duration::from_secs(600))
                {
                    a.last_clock_resync = Some(Instant::now());
                    resync_clock();
                }
                // Pin the assigned timezone at device init: the first
                // heartbeat's OS timezone is written root-owned so a later
                // (kid-initiated) timezone change is ignored by schedule
                // evaluation. A config `timezone` still overrides it agent-side.
                if let Some(tz) = msg.get("osTimezone").and_then(|v| v.as_str()) {
                    if !tz.is_empty() {
                        let pin = managed_dir.join("assigned-timezone");
                        if !pin.exists() {
                            match std::fs::write(&pin, tz) {
                                Ok(()) => tracing::info!("pinned assigned timezone at device init: {tz}"),
                                Err(e) => tracing::warn!("could not pin assigned timezone: {e}"),
                            }
                        }
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
                let list = |key: &str| -> Vec<String> {
                    msg.get(key)
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|h| h.as_str().map(str::to_string))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                };
                let allow_hosts = list("allowHosts");
                let filter = FilterPolicy {
                    enabled: msg.get("filterEnabled").and_then(|v| v.as_bool()).unwrap_or(false),
                    audit_only: msg.get("filterAuditOnly").and_then(|v| v.as_bool()).unwrap_or(false),
                    allow: list("filterAllowHosts"),
                    block: list("filterBlockHosts"),
                };
                let chores = ChorePolicy::from_msg(msg.get("chores"));
                earned.lock().unwrap().apply_report(
                    secs, gate_active, spend_ratio, daily_cap, max_bank, allow_hosts, filter,
                    chores,
                );
            }
            // Chore ops (docs/chores.md): one request per connection, JSON
            // reply. The PIN is checked HERE, never by the agent.
            Some("chore-claim") | Some("chore-reject") | Some("chore-verify") | Some("chores") => {
                let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let reply = {
                    let mut e = earned.lock().unwrap();
                    match msg.get("type").and_then(|t| t.as_str()) {
                        Some("chore-claim") => match e.chore_claim(id) {
                            Ok(()) => serde_json::json!({"ok": true}),
                            Err(err) => serde_json::json!({"ok": false, "error": err}),
                        },
                        Some("chore-reject") => {
                            e.chore_reject(id);
                            serde_json::json!({"ok": true})
                        }
                        Some("chore-verify") => {
                            let pin = msg.get("pin").and_then(|v| v.as_str()).unwrap_or("");
                            match e.chore_verify(id, pin) {
                                ChoreVerify::Ok { minutes } => {
                                    serde_json::json!({"ok": true, "result": "verified", "minutes": minutes})
                                }
                                ChoreVerify::WrongPin { attempts_left } => serde_json::json!({
                                    "ok": false, "result": "wrong-pin", "attemptsLeft": attempts_left
                                }),
                                ChoreVerify::Locked { secs } => {
                                    serde_json::json!({"ok": false, "result": "locked", "secs": secs})
                                }
                                ChoreVerify::Refused(err) => {
                                    serde_json::json!({"ok": false, "result": "refused", "error": err})
                                }
                            }
                        }
                        _ => {
                            let mut v = e.chores_status();
                            v["ok"] = serde_json::Value::Bool(true);
                            v
                        }
                    }
                };
                let mut stream = reader.into_inner();
                let _ = stream.write_all(format!("{reply}\n").as_bytes());
                return;
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
                let (earned_balance_min, earned_gate_active, earned_today_min, chores) = {
                    let mut e = earned.lock().unwrap();
                    let mut c = e.chores_status();
                    // The status line carries the summary; `chores` has the definitions.
                    if let Some(o) = c.as_object_mut() {
                        o.remove("chores");
                    }
                    (e.ledger.balance_min, e.gate_active, e.ledger.earned_today_min, c)
                };
                let assigned_tz = std::fs::read_to_string(managed_dir.join("assigned-timezone"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                // The authoritative block state, from the watchdog loop. The
                // countdown is recomputed live here (the watchdog only ticks
                // every 15s) so the HUD's timer is smooth; only timed penalties
                // (exposure/focus) carry one — every other reason is a gate.
                let reason = *quarantine_reason.lock().unwrap();
                let quarantine_active = reason != QReason::None;
                let quarantine_left = match reason {
                    QReason::Exposure | QReason::Focus => quarantine_secs.max(0),
                    _ => 0,
                };
                let (filter_mode, denied, forwarded) = {
                    let f = filter_status.lock().unwrap();
                    let (denied, forwarded) = f.1.recent(20);
                    (f.0.clone(), denied, forwarded)
                };
                let reply = format!(
                    "{{\"ok\":true,\"agentPid\":{},\"heartbeatAgeSecs\":{},\"captureOk\":{},\"configEpoch\":{},\"tasksEpoch\":{},\"enabled\":{},\"challengeOverdue\":{},\"clockTamper\":{},\"assignedTimezone\":\"{}\",\"exposureLockoutSecs\":{},\"earnedBalanceMin\":{:.1},\"earnedGateActive\":{},\"earnedTodayMin\":{:.1},\"quarantine\":{{\"active\":{},\"reason\":\"{}\",\"secsLeft\":{}}},\"siteFilter\":{{\"mode\":{},\"deniedRecent\":{},\"forwardedRecent\":{}}},\"chores\":{}}}\n",
                    a.pid,
                    a.last_seen.map(|t| t.elapsed().as_secs() as i64).unwrap_or(-1),
                    a.capture_ok,
                    a.config_epoch,
                    read_epoch(&managed_dir.join("epoch-tasks")),
                    a.enabled,
                    a.challenge_overdue,
                    a.clock_tamper,
                    assigned_tz,
                    quarantine_secs,
                    earned_balance_min,
                    earned_gate_active,
                    earned_today_min,
                    quarantine_active,
                    reason.as_str(),
                    quarantine_left,
                    serde_json::json!(filter_mode),
                    serde_json::json!(denied),
                    serde_json::json!(forwarded),
                    chores,
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
/// Generation-monotonic rollback refusal: reject an author-signed artifact
/// whose `authoredAt` is older than the last accepted one. This is what
/// replaced the old `notAfter` expiry — it catches a stashed old signing being
/// re-uploaded over a newer one (the epoch alone misses that, since otactl
/// stamps a fresh epoch at upload), WITHOUT ever making a valid config
/// un-appliable. `authoredAt` is fixed-format RFC3339 (validated by envelope),
/// so a lexical compare is chronological, and both values are server-stamped,
/// so it never depends on the device clock. `authored` None (non-authored
/// artifact) is a no-op.
fn check_generation(hw_path: &Path, authored: &Option<String>) -> Result<()> {
    let Some(authored) = authored else { return Ok(()) };
    if let Ok(prev) = std::fs::read_to_string(hw_path) {
        let prev = prev.trim();
        if !prev.is_empty() && authored.as_str() < prev {
            anyhow::bail!(
                "generation rollback refused: authoredAt {authored} is before the accepted {prev}"
            );
        }
    }
    Ok(())
}

fn apply_envelope(
    raw: &str,
    managed_dir: &Path,
    verifier: Option<&envelope::Verifier>,
) -> Result<u64> {
    let verifier = verifier.context("no pinned otactl root installed")?;
    let env: envelope::Envelope = serde_json::from_str(raw).context("malformed envelope")?;
    let epoch_path = managed_dir.join("epoch");
    let authored_path = managed_dir.join("authored");
    let last_epoch: u64 = read_epoch(&epoch_path);
    let verified = verifier.verify(&env, last_epoch, envelope::CONFIG_APP)?;
    check_generation(&authored_path, &verified.authored_at)?;

    // Persist artifact + envelope atomically-ish, then bump the epoch and
    // generation high-waters last so a crash never leaves them ahead of config.
    let tmp = managed_dir.join("package.json.tmp");
    std::fs::write(&tmp, &verified.artifact)?;
    std::fs::rename(&tmp, managed_dir.join("package.json"))?;
    std::fs::write(managed_dir.join("envelope.json"), raw)?;
    std::fs::write(&epoch_path, format!("{}\n", verified.epoch))?;
    if let Some(authored) = &verified.authored_at {
        std::fs::write(&authored_path, format!("{authored}\n"))?;
    }
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
    let authored_path = managed_dir.join("authored-tasks");
    let verified = verifier.verify(&env, read_epoch(&epoch_path), envelope::TASKS_APP)?;
    check_generation(&authored_path, &verified.authored_at)?;

    // Persist artifact (PIN hash split into the root-only chore-pin file,
    // docs/chores.md) then bump the epoch + generation high-waters last, so a
    // crash never leaves them ahead of the bank (mirrors apply_envelope).
    install_bank(managed_dir, &verified.artifact)?;
    std::fs::write(&epoch_path, format!("{}\n", verified.epoch))?;
    if let Some(authored) = &verified.authored_at {
        std::fs::write(&authored_path, format!("{authored}\n"))?;
    }
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
            filter: FilterPolicy::default(),
            last_report: Some(Instant::now()),
            last_tick: Instant::now(),
            chores: ChorePolicy::default(),
            chore_defs: Vec::new(),
            chore_defs_mtime: None,
            pin_path: PathBuf::from(format!("{path}.pin")),
            now_override: Some(LocalNow {
                date: "2026-09-07".into(),
                week: "2026-W37".into(),
                day: "mon".into(),
                hhmm: 1000,
            }),
        }
    }

    #[test]
    fn banks_and_caps_daily() {
        let mut g = gate("/tmp/bm-earn-t1.json");
        // 600s = 10 earned min, but the daily cap is 5.
        g.apply_report(600, true, 1.0, 5.0, 0.0, vec![], FilterPolicy::default(), ChorePolicy::default());
        assert!((g.ledger.balance_min - 5.0).abs() < 1e-6);
        g.apply_report(600, true, 1.0, 5.0, 0.0, vec![], FilterPolicy::default(), ChorePolicy::default()); // cap already hit
        assert!((g.ledger.balance_min - 5.0).abs() < 1e-6);
        let _ = std::fs::remove_file("/tmp/bm-earn-t1.json");
    }

    #[test]
    fn bank_ceiling() {
        let mut g = gate("/tmp/bm-earn-t2.json");
        g.apply_report(6000, true, 1.0, 0.0, 30.0, vec![], FilterPolicy::default(), ChorePolicy::default()); // 100 min, ceiling 30
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
        assert_eq!(
            g.tick(false),
            Some((vec!["khanacademy.org".to_string()], QReason::EarnedGate))
        );
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
    fn filter_policy_gated_like_earned_time() {
        let mut g = gate("/tmp/bm-earn-t6.json");
        let fp = FilterPolicy {
            enabled: true,
            audit_only: false,
            allow: vec!["kastatic.org".into()],
            block: vec!["tiktok.com".into()],
        };
        g.apply_report(0, false, 1.0, 0.0, 0.0, vec![], fp.clone(), ChorePolicy::default());
        assert_eq!(g.filter_policy(), Some(fp.clone()));
        g.last_report = Some(Instant::now() - Duration::from_secs(120)); // stale
        assert!(g.filter_policy().is_none());
        g.last_report = Some(Instant::now());
        std::fs::remove_file(&g.tasks_path).unwrap(); // not a kid device
        assert!(g.filter_policy().is_none());
        let _ = std::fs::remove_file("/tmp/bm-earn-t6.json");
    }

    #[test]
    fn compose_modes() {
        use dnsfilter::Mode;
        let hosts = vec!["khanacademy.org".to_string()];
        let fp = FilterPolicy {
            enabled: true,
            audit_only: false,
            allow: vec!["kastatic.org".into()],
            block: vec!["tiktok.com".into()],
        };
        // Full block wins over everything.
        assert_eq!(compose(true, Some(hosts.clone()), Some(fp.clone())).pf, PfMode::Full);
        // Depleted + filter: DNS allow mode with sources + allowlist + mgmt.
        let d = compose(false, Some(hosts.clone()), Some(fp.clone()));
        assert_eq!(d.pf, PfMode::EarningTable(hosts.clone()));
        match d.dns {
            Mode::Allow { allow, block } => {
                assert!(allow.contains(&"khanacademy.org".to_string()));
                assert!(allow.contains(&"kastatic.org".to_string()));
                assert!(allow.contains(&ALLOWED_HOSTS[0].to_string()));
                assert_eq!(block, vec!["tiktok.com".to_string()]);
            }
            other => panic!("expected allow mode, got {other:?}"),
        }
        // Depleted, no filter: legacy static gate.
        assert_eq!(
            compose(false, Some(hosts.clone()), None),
            Desired { pf: PfMode::EarningStatic(hosts.clone()), dns: Mode::Off }
        );
        // Balance available + blocklist: DNS lock only.
        let d = compose(false, None, Some(fp.clone()));
        assert_eq!(d.pf, PfMode::DnsLock);
        assert_eq!(d.dns, Mode::Block { block: vec!["tiktok.com".into()] });
        // Balance available, filter on but nothing to block: open.
        let empty = FilterPolicy { block: vec![], ..fp.clone() };
        assert_eq!(compose(false, None, Some(empty)), Desired::OPEN);
        // Audit never enforces.
        let audit = FilterPolicy { audit_only: true, ..fp.clone() };
        assert_eq!(
            compose(false, Some(hosts.clone()), Some(audit.clone())),
            Desired { pf: PfMode::EarningStatic(hosts.clone()), dns: Mode::Audit }
        );
        assert_eq!(compose(false, None, Some(audit)), Desired { pf: PfMode::Open, dns: Mode::Audit });
        assert_eq!(compose(false, None, None), Desired::OPEN);
    }

    // ---- chores (docs/chores.md)

    const PIN_LINE: &str = "sha256$00ff$"; // completed in pin_hash()

    fn pin_hash(pin: &str) -> String {
        let mut h = sha2::Sha256::new();
        h.update(b"00ff");
        h.update([0u8]);
        h.update(pin.as_bytes());
        format!("{PIN_LINE}{:x}\n", h.finalize())
    }

    fn chore_gate(path: &str) -> EarnedGate {
        let mut g = gate(path);
        std::fs::write(
            &g.tasks_path,
            r#"{"version":2,"tasks":[],"chores":[
              {"id":"bed","name":"Bed","kind":"required","repeat":"daily"},
              {"id":"dishes","name":"Dishes","kind":"required","repeat":"daily","days":["tue"]},
              {"id":"piano","name":"Piano","kind":"bonus","repeat":"daily","minutes":20},
              {"id":"trash","name":"Trash","kind":"bonus","repeat":"weekly","minutes":30},
              {"id":"garage","name":"Garage","kind":"bonus","repeat":"once","minutes":60}]}"#,
        )
        .unwrap();
        std::fs::write(&g.pin_path, pin_hash("4821")).unwrap();
        g.chores = ChorePolicy {
            enabled: true,
            bonus_daily_cap_min: 45.0,
            claim_ttl_min: 60.0,
            required_hold_from: 900,
            verify_max_attempts: 3,
            verify_lockout_sec: 600,
        };
        g.ledger.date = "2026-09-07".into();
        g
    }

    fn cleanup(g: &EarnedGate) {
        let _ = std::fs::remove_file(&g.ledger_path);
        let _ = std::fs::remove_file(&g.tasks_path);
        let _ = std::fs::remove_file(&g.pin_path);
    }

    #[test]
    fn required_chore_holds_gate_until_verified() {
        let mut g = chore_gate("/tmp/bm-chore-t1.json");
        g.ledger.balance_min = 30.0; // balance doesn't matter
        // Monday 10:00: "bed" is due (every day), "dishes" only on Tuesday.
        assert_eq!(g.tick(false), Some((vec!["khanacademy.org".into()], QReason::Chores)));
        assert_eq!(g.required_outstanding(&g.local_now()), vec!["bed".to_string()]);
        assert!((g.ledger.balance_min - 30.0).abs() < 1e-9, "no spend while held");
        // Before the hold hour nothing is due yet.
        g.now_override.as_mut().unwrap().hhmm = 830;
        assert!(g.tick(false).is_none() || g.ledger.balance_min < 30.0);
        g.now_override.as_mut().unwrap().hhmm = 1000;
        // Verify with the PIN → released, nothing credited (required).
        assert_eq!(g.chore_verify("bed", "4821"), ChoreVerify::Ok { minutes: 0.0 });
        assert!(g.required_outstanding(&g.local_now()).is_empty());
        g.last_tick = Instant::now();
        assert!(g.tick(false).is_none());
        // Same period again is refused (no double verification).
        assert_eq!(g.chore_verify("bed", "4821"), ChoreVerify::Refused("already verified"));
        // Next day it is due again.
        g.now_override.as_mut().unwrap().date = "2026-09-08".into();
        g.now_override.as_mut().unwrap().day = "tue".into();
        let now = g.local_now();
        g.rollover(&now);
        let mut out = g.required_outstanding(&now);
        out.sort();
        assert_eq!(out, vec!["bed".to_string(), "dishes".to_string()]);
        cleanup(&g);
    }

    #[test]
    fn bonus_chore_credits_under_caps() {
        let mut g = chore_gate("/tmp/bm-chore-t2.json");
        g.daily_cap_min = 100.0;
        g.max_bank_min = 0.0;
        assert_eq!(g.chore_claim("piano"), Ok(()));
        assert_eq!(g.pending(&g.local_now()), vec!["piano".to_string()]);
        assert_eq!(g.chore_verify("piano", "4821"), ChoreVerify::Ok { minutes: 20.0 });
        assert!(g.pending(&g.local_now()).is_empty(), "claim consumed");
        assert!((g.ledger.balance_min - 20.0).abs() < 1e-9);
        assert!((g.ledger.earned_today_min - 20.0).abs() < 1e-9, "counts toward the earn cap");
        // Weekly trash: 30 wanted, chore cap 45 leaves 25.
        assert_eq!(g.chore_verify("trash", "4821"), ChoreVerify::Ok { minutes: 25.0 });
        assert!((g.ledger.balance_min - 45.0).abs() < 1e-9);
        // Chore cap exhausted: verified, but nothing credited.
        assert_eq!(g.chore_verify("garage", "4821"), ChoreVerify::Ok { minutes: 0.0 });
        // Next day: piano is due again, trash (weekly) and garage (once) are not.
        g.now_override.as_mut().unwrap().date = "2026-09-08".into();
        g.now_override.as_mut().unwrap().day = "tue".into();
        assert_eq!(g.chore_verify("piano", "4821"), ChoreVerify::Ok { minutes: 20.0 });
        assert_eq!(g.chore_verify("trash", "4821"), ChoreVerify::Refused("already verified"));
        assert_eq!(g.chore_verify("garage", "4821"), ChoreVerify::Refused("already verified"));
        // Ledger round-trips with the chore section.
        let back: EarnedLedger =
            serde_json::from_str(&std::fs::read_to_string(&g.ledger_path).unwrap()).unwrap();
        assert!(back.chores.verified.iter().any(|v| v.id == "garage" && v.period == "once"));
        cleanup(&g);
    }

    #[test]
    fn wrong_pins_lock_out() {
        let mut g = chore_gate("/tmp/bm-chore-t3.json");
        assert_eq!(g.chore_verify("bed", "0000"), ChoreVerify::WrongPin { attempts_left: 2 });
        assert_eq!(g.chore_verify("bed", "1111"), ChoreVerify::WrongPin { attempts_left: 1 });
        assert_eq!(g.chore_verify("bed", "2222"), ChoreVerify::Locked { secs: 600 });
        // Even the right PIN is refused while locked, and the lock persists.
        assert!(matches!(g.chore_verify("bed", "4821"), ChoreVerify::Locked { .. }));
        let back: EarnedLedger =
            serde_json::from_str(&std::fs::read_to_string(&g.ledger_path).unwrap()).unwrap();
        assert!(back.chores.pin_locked_until.is_some());
        // Lock expired → verification works and clears the counter.
        g.ledger.chores.pin_locked_until = Some(unix_now() - 1);
        assert_eq!(g.chore_verify("bed", " 4821 "), ChoreVerify::Ok { minutes: 0.0 });
        assert_eq!(g.ledger.chores.pin_failures, 0);
        cleanup(&g);
    }

    #[test]
    fn claims_expire_and_module_off_refuses() {
        let mut g = chore_gate("/tmp/bm-chore-t4.json");
        assert_eq!(g.chore_claim("piano"), Ok(()));
        assert_eq!(g.chore_claim("piano"), Ok(()), "idempotent");
        assert_eq!(g.ledger.chores.claims.len(), 1);
        assert_eq!(g.chore_claim("nope"), Err("unknown chore"));
        g.ledger.chores.claims[0].claimed_at = unix_now() - 61 * 60; // past the 60-min TTL
        assert!(g.pending(&g.local_now()).is_empty());
        g.chore_reject("piano");
        assert!(g.ledger.chores.claims.is_empty());
        // No PIN file → refused, not a crash; module off → refused.
        std::fs::remove_file(&g.pin_path).unwrap();
        assert!(matches!(g.chore_verify("bed", "4821"), ChoreVerify::Refused(_)));
        g.chores.enabled = false;
        assert!(g.required_outstanding(&g.local_now()).is_empty());
        assert_eq!(g.chore_claim("piano"), Err("chores are not enabled"));
        // With the module off the unverified "bed" no longer holds the gate
        // (a positive balance keeps it open as usual).
        g.ledger.balance_min = 10.0;
        g.last_tick = Instant::now();
        assert!(g.tick(false).is_none());
        cleanup(&g);
    }

    #[test]
    fn install_bank_splits_pin() {
        let dir = PathBuf::from("/tmp/bm-chore-bank");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        install_bank(&dir, br#"{"version":2,"tasks":[],"chores":[],"chorePinHash":"sha256$ab$cd"}"#)
            .unwrap();
        let tasks = std::fs::read_to_string(dir.join("tasks.json")).unwrap();
        assert!(!tasks.contains("chorePinHash"), "hash must not be in the readable bank");
        assert!(tasks.contains("\"version\": 2"));
        assert_eq!(std::fs::read_to_string(dir.join("chore-pin")).unwrap(), "sha256$ab$cd\n");
        let mode = std::fs::metadata(dir.join("chore-pin")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // A bank without a PIN removes the stale one; non-JSON is written as-is.
        install_bank(&dir, br#"{"version":3,"tasks":[]}"#).unwrap();
        assert!(!dir.join("chore-pin").exists());
        install_bank(&dir, b"not json").unwrap();
        assert_eq!(std::fs::read(dir.join("tasks.json")).unwrap(), b"not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pin_hash_format() {
        assert!(pin_matches(&pin_hash("4821"), "4821"));
        assert!(pin_matches(&pin_hash("4821"), " 4821\n"));
        assert!(!pin_matches(&pin_hash("4821"), "4822"));
        assert!(!pin_matches("md5$x$y", "4821"));
        assert!(!pin_matches("garbage", "4821"));
        assert_eq!(parse_hhmm("09:30"), Some(930));
        assert_eq!(parse_hhmm("9"), None);
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

#[cfg(test)]
mod quarantine_tests {
    use super::*;

    // A healthy agent: session-protected, fresh heartbeat, capture ok, no
    // penalties/tamper/challenge. Individual tests flip one field.
    fn healthy_agent() -> AgentState {
        AgentState {
            last_seen: Some(Instant::now()),
            pid: 1234,
            capture_ok: true,
            config_epoch: 1,
            enabled: true,
            challenge_overdue: false,
            exposure_penalty_until: None,
            penalty_source: None,
            clock_tamper: false,
            last_clock_resync: None,
        }
    }

    // An armed quarantine with zero grace, so want_full returns a health
    // reason on the first unhealthy call rather than waiting out the debounce.
    fn quarantine_no_grace() -> Quarantine {
        let hook = Quarantine::table_hook(true);
        let dir = PathBuf::from("/tmp");
        Quarantine {
            engaged: Desired::OPEN,
            unhealthy_since: None,
            armed: true,
            dry_run: true,
            grace: Duration::ZERO,
            rules_path: PathBuf::from("/tmp/bm-quar-test.rules"),
            filter: dnsfilter::Filter::new(&dir, hook.clone()),
            sysdns: dnsfilter::SystemDns::new(&dir, true),
            pf_hook: hook,
            saved_manual: Vec::new(),
            loaded_upstreams: Vec::new(),
        }
    }

    #[test]
    fn rules_per_mode() {
        let q = quarantine_no_grace();
        let ups: Vec<SocketAddr> = vec!["192.0.2.53:53".parse().unwrap()];
        let table = q.build_rules(&PfMode::EarningTable(vec![]), &ups);
        assert!(table.contains("table <betamacs_allow> persist"));
        assert!(table.contains("to { 192.0.2.53 } port 53"));
        assert!(table.contains("to <betamacs_allow> port { 80, 443 }"));
        assert!(table.trim_end().ends_with("block drop quick all"));
        let lock = q.build_rules(&PfMode::DnsLock, &ups);
        assert!(lock.contains("port { 53, 853 }"));
        assert!(!lock.contains("block drop quick all"));
        assert!(lock.contains("1.1.1.1"));
        let full = q.build_rules(&PfMode::Full, &[]);
        assert!(full.contains("to any port 53"));
        assert!(full.trim_end().ends_with("block drop quick all"));
        assert!(q.build_rules(&PfMode::Open, &[]).is_empty());
    }

    // session_active = true throughout: an unprotected logged-in session is
    // the case the health reasons care about.
    #[test]
    fn healthy_is_none() {
        assert_eq!(
            Quarantine::unhealthy_reason(&healthy_agent(), true),
            QReason::None
        );
    }

    #[test]
    fn capture_revoked_is_capture_unhealthy() {
        let mut a = healthy_agent();
        a.capture_ok = false;
        assert_eq!(
            Quarantine::unhealthy_reason(&a, true),
            QReason::CaptureUnhealthy
        );
    }

    #[test]
    fn stale_heartbeat_is_heartbeat_stale() {
        let mut a = healthy_agent();
        a.last_seen = Some(Instant::now() - HEARTBEAT_FRESH - Duration::from_secs(5));
        assert_eq!(
            Quarantine::unhealthy_reason(&a, true),
            QReason::HeartbeatStale
        );
    }

    #[test]
    fn never_seen_is_session_health() {
        let mut a = healthy_agent();
        a.last_seen = None;
        assert_eq!(
            Quarantine::unhealthy_reason(&a, true),
            QReason::SessionHealth
        );
    }

    #[test]
    fn challenge_and_tamper_take_priority() {
        // Challenge and tamper apply even when the session itself is protected
        // (fresh, capture ok) and regardless of session_active.
        let mut a = healthy_agent();
        a.challenge_overdue = true;
        assert_eq!(Quarantine::unhealthy_reason(&a, false), QReason::Challenge);
        a.clock_tamper = true; // tamper outranks challenge
        assert_eq!(Quarantine::unhealthy_reason(&a, true), QReason::ClockTamper);
    }

    #[test]
    fn no_session_is_healthy() {
        // Capture unhealthy but nobody is logged in → not a block.
        let mut a = healthy_agent();
        a.capture_ok = false;
        assert_eq!(Quarantine::unhealthy_reason(&a, false), QReason::None);
    }

    #[test]
    fn disabled_policy_is_healthy() {
        // Censoring off by policy → an unhealthy capture is healthy-by-policy.
        let mut a = healthy_agent();
        a.enabled = false;
        a.capture_ok = false;
        assert_eq!(Quarantine::unhealthy_reason(&a, true), QReason::None);
    }

    #[test]
    fn timed_penalty_names_its_source() {
        let mut q = quarantine_no_grace();
        let mut a = healthy_agent();
        a.exposure_penalty_until = Some(Instant::now() + Duration::from_secs(600));
        a.penalty_source = Some(PenaltySource::Exposure);
        assert_eq!(q.want_full(&a), QReason::Exposure);
        a.penalty_source = Some(PenaltySource::Focus);
        assert_eq!(q.want_full(&a), QReason::Focus);
        // Missing source but a live deadline → default to exposure, never None.
        a.penalty_source = None;
        assert_eq!(q.want_full(&a), QReason::Exposure);
    }

    #[test]
    fn expired_penalty_is_not_a_reason() {
        let mut q = quarantine_no_grace();
        let mut a = healthy_agent();
        a.exposure_penalty_until = Some(Instant::now() - Duration::from_secs(1));
        a.penalty_source = Some(PenaltySource::Exposure);
        // Deadline passed and the session is otherwise healthy → open.
        assert_eq!(q.want_full(&a), QReason::None);
    }

    #[test]
    fn disarmed_is_never_a_reason() {
        let mut q = quarantine_no_grace();
        q.armed = false;
        let mut a = healthy_agent();
        a.clock_tamper = true;
        assert_eq!(q.want_full(&a), QReason::None);
    }

    #[test]
    fn grace_debounces_health_reasons() {
        // A health reason is withheld until it has persisted past the grace
        // window; a timed penalty is immediate (no grace).
        let mut q = quarantine_no_grace();
        q.grace = Duration::from_secs(3600);
        let mut a = healthy_agent();
        a.challenge_overdue = true;
        assert_eq!(q.want_full(&a), QReason::None); // within grace
        // But an exposure penalty bypasses grace entirely.
        a.challenge_overdue = false;
        a.exposure_penalty_until = Some(Instant::now() + Duration::from_secs(600));
        a.penalty_source = Some(PenaltySource::Exposure);
        assert_eq!(q.want_full(&a), QReason::Exposure);
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;

    #[test]
    fn generation_monotonicity() {
        let dir = std::env::temp_dir().join(format!("betamacs-gen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hw = dir.join("authored");

        // No high-water yet: anything is accepted, incl. a non-authored (None).
        assert!(check_generation(&hw, &None).is_ok());
        assert!(check_generation(&hw, &Some("2026-09-04T12:00:00Z".into())).is_ok());

        // Seed the high-water; equal and newer are accepted, older is refused.
        std::fs::write(&hw, "2026-09-04T12:00:00Z\n").unwrap();
        assert!(check_generation(&hw, &Some("2026-09-04T12:00:00Z".into())).is_ok());
        assert!(check_generation(&hw, &Some("2026-09-05T00:00:00Z".into())).is_ok());
        let err = check_generation(&hw, &Some("2026-09-04T11:59:59Z".into())).unwrap_err();
        assert!(err.to_string().contains("generation rollback refused"), "{err}");

        // A non-authored artifact (None) is never gated by the high-water.
        assert!(check_generation(&hw, &None).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
