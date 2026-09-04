//! Trusted-clock monitor (see docs/managed-mode.md and settings.rs
//! `ClockIntegritySettings`).
//!
//! Time-of-day policy is only as trustworthy as the clock it reads, and a kid
//! quickly learns that changing the system clock (or timezone) shifts a
//! restriction window. This module closes that off two ways:
//!
//!   1. Schedule windows are evaluated against an ASSIGNED timezone applied
//!      to a TRUSTED epoch (`trusted_epoch()`), never the OS timezone/clock.
//!   2. The wall clock is watched for being CHANGED under a running instance.
//!      Between samples the wall clock must advance in step with
//!      `mach_continuous_time` (a monotonic clock that KEEPS counting across
//!      sleep); a divergence past tolerance means someone set the clock while
//!      we were running. That is tamper: `Health::clock_tamper` latches and
//!      the daemon quarantines — the same response as the censor being shut
//!      down.
//!
//! A machine that merely BOOTED with the wrong time shows no running-instance
//! jump; the network anchor notices the absolute time is off, we announce it
//! and ask the daemon (root) to resync, and DON'T punish. Our own resync
//! would itself look like a jump, so a short "expected correction" window
//! after a resync request re-baselines instead of flagging tamper.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::heartbeat::Health;
use crate::settings::{ClockIntegritySettings, Effective};

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}
unsafe extern "C" {
    // Monotonic ticks that keep advancing across sleep (unlike
    // mach_absolute_time / CLOCK_MONOTONIC), so a wall-clock change during
    // sleep is still caught after wake.
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

/// `mach_continuous_time` converted to nanoseconds.
fn continuous_nanos() -> u128 {
    // SAFETY: both are pure readers from libSystem with no side effects.
    let ticks = unsafe { mach_continuous_time() } as u128;
    let mut tb = MachTimebaseInfo { numer: 0, denom: 0 };
    unsafe { mach_timebase_info(&mut tb) };
    if tb.denom == 0 {
        return ticks; // timebase unavailable — treat ticks as ns (Apple silicon: 1:1)
    }
    ticks * tb.numer as u128 / tb.denom as u128
}

fn os_wall_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// Trusted-time anchor, published for the schedule evaluator. `trusted_epoch`
// = anchor_wall + (continuous_now - anchor_cont). Set only from a confirmed
// network time; until then `trusted_epoch()` returns None and callers fall
// back to the OS clock.
static ANCHOR_VALID: AtomicBool = AtomicBool::new(false);
static ANCHOR_WALL: AtomicI64 = AtomicI64::new(0);
static ANCHOR_CONT_NS: AtomicU64 = AtomicU64::new(0);

fn set_anchor(net_epoch: i64, cont_ns: u128) {
    ANCHOR_WALL.store(net_epoch, Ordering::Relaxed);
    ANCHOR_CONT_NS.store(cont_ns as u64, Ordering::Relaxed);
    ANCHOR_VALID.store(true, Ordering::Relaxed);
}

/// Best trusted UNIX epoch (seconds), or None if no network time has been
/// confirmed yet this run. Derived from the last anchor via the continuous
/// clock so it survives sleep and ignores OS wall-clock edits.
pub fn trusted_epoch() -> Option<i64> {
    if !ANCHOR_VALID.load(Ordering::Relaxed) {
        return None;
    }
    let base_wall = ANCHOR_WALL.load(Ordering::Relaxed);
    let base_cont = ANCHOR_CONT_NS.load(Ordering::Relaxed) as u128;
    let now = continuous_nanos();
    let elapsed = (now.saturating_sub(base_cont) / 1_000_000_000) as i64;
    Some(base_wall + elapsed)
}

/// Minimal SNTP client: one request, read the transmit timestamp. Returns a
/// UNIX epoch (seconds) or None on any failure. No crate needed.
fn sntp_query(server: &str, timeout: Duration) -> Option<i64> {
    use std::net::{ToSocketAddrs, UdpSocket};
    let addr = (server, 123u16).to_socket_addrs().ok()?.next()?;
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.set_read_timeout(Some(timeout)).ok()?;
    sock.set_write_timeout(Some(timeout)).ok()?;
    let mut pkt = [0u8; 48];
    pkt[0] = 0x1b; // LI = 0, VN = 3, Mode = 3 (client)
    sock.send_to(&pkt, addr).ok()?;
    let mut buf = [0u8; 48];
    let (n, _) = sock.recv_from(&mut buf).ok()?;
    if n < 44 {
        return None;
    }
    // Transmit timestamp seconds: bytes 40..44, big-endian, NTP epoch (1900).
    let ntp_secs = u32::from_be_bytes([buf[40], buf[41], buf[42], buf[43]]) as i64;
    const NTP_UNIX_DELTA: i64 = 2_208_988_800;
    let epoch = ntp_secs - NTP_UNIX_DELTA;
    // Sanity floor: reject a bogus/zero reply (before 2020).
    (epoch > 1_577_836_800).then_some(epoch)
}

/// Pinned-backend corroboration: read the TLS `Date` header with curl,
/// validating against the bundle's pinned otactl root. Best-effort.
fn https_date_epoch(url: &str, ca: Option<&PathBuf>, timeout_sec: u32) -> Option<i64> {
    let mut cmd = Command::new("/usr/bin/curl");
    cmd.args(["-sS", "-I", "--max-time", &timeout_sec.to_string()]);
    if let Some(ca) = ca {
        cmd.arg("--cacert").arg(ca);
    }
    cmd.arg(url);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let headers = String::from_utf8_lossy(&out.stdout);
    let line = headers
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("date:"))?;
    let value = line.splitn(2, ':').nth(1)?.trim();
    // Parse the RFC 1123 date with /bin/date (avoids a chrono dependency).
    let parsed = Command::new("/bin/date")
        .args(["-j", "-u", "-f", "%a, %d %b %Y %T GMT", value, "+%s"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&parsed.stdout).trim().parse().ok()
}

/// Confirm the absolute time over the network: NTP (majority-ish: first
/// success) corroborated by the pinned backend when configured. Returns the
/// trusted epoch, or None if nothing could be reached.
fn network_epoch(cfg: &ClockIntegritySettings, ca: Option<&PathBuf>) -> Option<i64> {
    let ntp = cfg
        .ntp_servers
        .iter()
        .find_map(|s| sntp_query(s, Duration::from_secs(5)));
    let http = cfg
        .time_url
        .as_deref()
        .and_then(|u| https_date_epoch(u, ca, 5));
    match (ntp, http) {
        (Some(n), Some(h)) => {
            if (n - h).abs() > cfg.skew_tolerance_sec as i64 {
                // The two trusted sources disagree — a spoof of one, or a bad
                // reply. Don't trust either this round.
                tracing::warn!("clock: NTP ({n}) and pinned backend ({h}) disagree; skipping anchor");
                None
            } else {
                Some(n) // prefer NTP's sub-second-agnostic value; they agree
            }
        }
        (Some(n), None) => Some(n),
        (None, Some(h)) => Some(h),
        (None, None) => None,
    }
}

/// Start the clock-integrity monitor. No-op behaviour until the config
/// enables it; safe to call once at startup.
pub fn spawn(shared: Arc<RwLock<Effective>>, health: Arc<Health>, ca: Option<PathBuf>) {
    std::thread::spawn(move || {
        // Jump-detection baseline: (continuous ns, wall secs) that must stay
        // in lockstep. Set on first tick; only re-baselined on a legitimate
        // correction we initiated.
        let mut base: Option<(u128, i64)> = None;
        let mut last_anchor_cont: Option<u128> = None;
        let mut announced_wrong = false;
        // While set (a continuous-ns deadline), a detected jump is treated as
        // our own resync landing — re-baseline, don't flag tamper.
        let mut expect_correction_until: Option<u128> = None;

        loop {
            let cfg = shared.read().unwrap().clock_integrity.clone();
            if !cfg.enabled {
                health.clock_tamper.store(false, Ordering::Relaxed);
                ANCHOR_VALID.store(false, Ordering::Relaxed);
                base = None;
                last_anchor_cont = None;
                announced_wrong = false;
                std::thread::sleep(Duration::from_secs(30));
                continue;
            }

            let cont = continuous_nanos();
            let wall = os_wall_secs();
            let tol = cfg.skew_tolerance_sec as i64;

            // --- Network anchor: confirm the ABSOLUTE time (startup + periodic).
            let due = last_anchor_cont
                .map(|a| cont.saturating_sub(a) >= cfg.anchor_interval_sec as u128 * 1_000_000_000)
                .unwrap_or(true);
            if due {
                if let Some(net) = network_epoch(&cfg, ca.as_ref()) {
                    last_anchor_cont = Some(cont);
                    set_anchor(net, cont);
                    let off = net - wall; // how wrong the OS wall clock is
                    if off.abs() > tol {
                        // Absolute time is wrong. Was it a running-instance
                        // change (jump detector already latched) or a machine
                        // that booted wrong / drifted?
                        if health.clock_tamper.load(Ordering::Relaxed) {
                            tracing::warn!("clock: OS off by {off}s and a running-instance jump was seen — tamper stands");
                        } else {
                            tracing::warn!("clock: OS off by {off}s with no running-instance jump — booted wrong / drift, announcing + resync");
                            if !announced_wrong {
                                announced_wrong = true;
                                crate::prompt::warn(&format!(
                                    "This Mac's clock is off by about {} minutes. Correcting it now.",
                                    off.abs() / 60
                                ));
                            }
                            // Ask the root daemon to resync, and expect the
                            // resulting wall-clock jump shortly (don't punish it).
                            health.clock_boot_wrong.store(true, Ordering::Relaxed);
                            expect_correction_until = Some(cont + 180 * 1_000_000_000);
                            // Baseline jump detection now (on the stable, if
                            // wrong, wall) so a later real change is still
                            // caught during the up-to-anchor-interval gap.
                            if base.is_none() {
                                base = Some((cont, wall));
                            }
                        }
                    } else {
                        announced_wrong = false;
                        if base.is_none() {
                            base = Some((cont, wall));
                        }
                    }
                } else if base.is_none() {
                    // No network yet — still baseline locally so a later jump
                    // is caught; absolute correctness waits for the network.
                    base = Some((cont, wall));
                }
            }

            // --- Local jump detection: the strong tamper signal.
            if let Some((c0, w0)) = base {
                let expected = w0 + (cont.saturating_sub(c0) / 1_000_000_000) as i64;
                let drift = wall - expected;
                if drift.abs() > tol {
                    let expecting = expect_correction_until.is_some_and(|d| cont <= d);
                    if expecting {
                        // Our own resync landed — re-baseline to the corrected
                        // clock and clear the window; not tamper.
                        tracing::info!("clock: correction of {drift}s applied — re-baselined");
                        base = Some((cont, wall));
                        expect_correction_until = None;
                        health.clock_tamper.store(false, Ordering::Relaxed);
                    } else {
                        if !health.clock_tamper.load(Ordering::Relaxed) {
                            tracing::warn!("clock changed under a running instance by {drift}s — tamper (quarantine)");
                        }
                        health.clock_tamper.store(true, Ordering::Relaxed);
                    }
                } else if health.clock_tamper.load(Ordering::Relaxed) {
                    tracing::info!("clock returned to expected — clearing tamper");
                    health.clock_tamper.store(false, Ordering::Relaxed);
                }
            }

            let interval = cfg.check_interval_sec.max(1);
            std::thread::sleep(Duration::from_secs(interval as u64));
        }
    });
}

#[cfg(test)]
mod tests {
    /// Pure jump-detection decision, mirroring the loop body, so the tamper
    /// vs. correction logic is testable without threads or the clock.
    fn is_tamper(
        base: (u128, i64),
        cont: u128,
        wall: i64,
        tol: i64,
        expect_correction: bool,
    ) -> (bool, bool) {
        // returns (tamper, rebaselined)
        let (c0, w0) = base;
        let expected = w0 + (cont.saturating_sub(c0) / 1_000_000_000) as i64;
        let drift = wall - expected;
        if drift.abs() > tol {
            if expect_correction {
                (false, true)
            } else {
                (true, false)
            }
        } else {
            (false, false)
        }
    }

    const S: u128 = 1_000_000_000;

    #[test]
    fn steady_clock_is_not_tamper() {
        // 100s of continuous time, wall advanced 100s too.
        let (t, _) = is_tamper((0, 1000), 100 * S, 1100, 300, false);
        assert!(!t);
    }

    #[test]
    fn wall_jump_under_running_instance_is_tamper() {
        // Only 10s of continuous time elapsed but wall jumped by an hour.
        let (t, rebased) = is_tamper((0, 1000), 10 * S, 1000 + 3600, 300, false);
        assert!(t);
        assert!(!rebased);
    }

    #[test]
    fn our_own_correction_rebaselines_not_tamper() {
        // Same big jump, but within the expected-correction window.
        let (t, rebased) = is_tamper((0, 1000), 10 * S, 1000 + 3600, 300, true);
        assert!(!t);
        assert!(rebased);
    }

    #[test]
    fn sleep_keeps_lockstep() {
        // Continuous time counts through sleep, so a 2h nap with the wall
        // clock advancing 2h is NOT a jump.
        let (t, _) = is_tamper((0, 1000), 7200 * S, 1000 + 7200, 300, false);
        assert!(!t);
    }

    #[test]
    fn small_drift_within_tolerance_ok() {
        let (t, _) = is_tamper((0, 1000), 100 * S, 1100 + 60, 300, false); // +60s
        assert!(!t);
    }
}
