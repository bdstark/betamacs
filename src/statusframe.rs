//! The live status frame's data feed. A background thread composes a
//! human-readable snapshot once a second from the agent's own Health plus
//! the daemon's `status` reply, and pushes it to the overlay event loop,
//! which updates the native HUD window on the main thread. Display-only.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::heartbeat::{DAEMON_SOCKET, Health};
use crate::overlay::OverlayHandle;

const REFRESH: Duration = Duration::from_secs(1);

/// One request/one reply to the daemon's status endpoint. None when there
/// is no daemon (unmanaged) or it is unreachable.
fn daemon_status() -> Option<serde_json::Value> {
    let mut stream = UnixStream::connect(DAEMON_SOCKET).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    stream.write_all(b"{\"type\":\"status\"}\n").ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

pub fn spawn(health: Arc<Health>, handle: OverlayHandle) {
    std::thread::spawn(move || {
        loop {
            let _ = handle.set_stats(compose(&health));
            std::thread::sleep(REFRESH);
        }
    });
}

/// Plain-language rendering of a daemon `quarantine.reason` for the HUD. Kept
/// in sync with betamacsd's `QReason::as_str`. An unknown/future reason falls
/// back to a generic "network locked" so a newer daemon never renders blank.
fn lockdown_phrase(reason: &str) -> &'static str {
    match reason {
        "exposure" => "too many exposures",
        "focus" => "too much scrolling",
        "challenge" => "unanswered challenge",
        "earned-gate" => "earn time to unlock (allowlist only)",
        "clock-tamper" => "clock tampered",
        "capture-unhealthy" => "screen recording off",
        "heartbeat-stale" | "session/health" => "censor not reporting",
        _ => "network locked",
    }
}

fn compose(health: &Health) -> String {
    let d = daemon_status();
    let f = |k: &str| d.as_ref().and_then(|v| v.get(k)).and_then(|x| x.as_f64());
    let i = |k: &str| d.as_ref().and_then(|v| v.get(k)).and_then(|x| x.as_i64());
    let b = |k: &str| d.as_ref().and_then(|v| v.get(k)).and_then(|x| x.as_bool());
    let s_of = |k: &str| {
        d.as_ref()
            .and_then(|v| v.get(k))
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };

    let enabled = health.enabled.load(Ordering::Relaxed);
    let capture = health.capture_ok.load(Ordering::Relaxed);
    let boxes = health.boxes.load(Ordering::Relaxed);
    let exp_recent = health.exposure_recent.load(Ordering::Relaxed);
    let exp_block = health.exposure_block.load(Ordering::Relaxed);
    let cfg_epoch = health.config_epoch.load(Ordering::Relaxed);

    let balance = f("earnedBalanceMin");
    let gate = b("earnedGateActive").unwrap_or(false);
    let today = f("earnedTodayMin").unwrap_or(0.0);
    let lockout = i("exposureLockoutSecs").unwrap_or(0);
    let challenge_overdue = b("challengeOverdue").unwrap_or(false);
    let clock_tamper = b("clockTamper").unwrap_or(false);
    let assigned_tz = s_of("assignedTimezone").filter(|t| !t.is_empty());
    let heartbeat = i("heartbeatAgeSecs");
    let tasks_epoch = i("tasksEpoch").unwrap_or(0);

    // The lockdown line is driven by the daemon's authoritative `quarantine`
    // object (active/reason/secsLeft) so it ALWAYS matches what pf is actually
    // doing — previously this was re-derived here from a subset of signals and
    // read "open" during full quarantines the daemon raised for other reasons
    // (capture revoked, stale heartbeat, session/health). If the daemon is old
    // or unreachable and omits `quarantine`, fall back to the legacy subset.
    let quarantine = d.as_ref().and_then(|v| v.get("quarantine"));
    let lockdown = match quarantine {
        Some(q) => {
            let active = q.get("active").and_then(|x| x.as_bool()).unwrap_or(false);
            let reason = q.get("reason").and_then(|x| x.as_str()).unwrap_or("none");
            let secs_left = q.get("secsLeft").and_then(|x| x.as_i64()).unwrap_or(0);
            if !active {
                "open".to_string()
            } else {
                let mut line = format!("LOCKED — {}", lockdown_phrase(reason));
                if secs_left > 0 {
                    line += &format!(", {secs_left}s left");
                }
                line
            }
        }
        None => {
            if clock_tamper {
                "LOCKED — clock tampered".to_string()
            } else if lockout > 0 {
                format!("LOCKED — timed penalty, {lockout}s left")
            } else if challenge_overdue {
                "LOCKED — unanswered challenge".to_string()
            } else if gate && balance.is_some_and(|b| b <= 0.0) {
                "LOCKED — earn time (allowlist only)".to_string()
            } else {
                "open".to_string()
            }
        }
    };

    let mut s = String::new();
    s += &format!(
        "Censor: {}    Capture: {}    Boxes: {boxes}\n",
        if enabled { "on" } else { "OFF (policy)" },
        if capture { "ok" } else { "UNHEALTHY" },
    );
    s += &format!("Lockdown: {lockdown}\n");
    if exp_block > 0 {
        s += &format!("Exposure: {exp_recent} / {exp_block} in block window\n");
    } else {
        s += "Exposure: not configured\n";
    }
    match balance {
        Some(bal) => {
            s += &format!(
                "Earned time: {bal:.0} min banked · gate {}",
                if gate { "ACTIVE" } else { "inactive" },
            );
            if today > 0.0 {
                s += &format!(" · {today:.0} earned today");
            }
            s.push('\n');
        }
        None => s += "Earned time: (daemon unreachable)\n",
    }
    s += &format!("Challenge: {}\n", if challenge_overdue { "OVERDUE" } else { "none" });
    match (&assigned_tz, clock_tamper) {
        (_, true) => s += "Clock: TAMPER — changed while running\n",
        (Some(tz), false) => s += &format!("Clock: ok · tz {tz}\n"),
        (None, false) => s += "Clock: not configured\n",
    }
    s += &format!("config epoch {cfg_epoch}");
    if tasks_epoch > 0 {
        s += &format!(" · tasks epoch {tasks_epoch}");
    }
    if let Some(h) = heartbeat {
        s += &format!("    heartbeat {h}s ago");
    }
    s
}
