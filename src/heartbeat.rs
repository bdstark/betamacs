//! Heartbeat client: betamacs reports its health to the root watchdog
//! daemon (betamacsd) over a unix socket, so the daemon can tell a
//! healthy-but-quiet censor from a stopped, killed, or blinded one.
//!
//! The socket may simply not exist (unmanaged installs, daemon not yet
//! installed); the client then stays quiet and keeps retrying slowly.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Socket the daemon listens on. World-connectable by design: envelopes
/// are self-authenticating and heartbeats are advisory.
pub const DAEMON_SOCKET: &str = "/var/run/betamacsd.sock";

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Shared, lock-free health snapshot updated by the pipeline.
#[derive(Default)]
pub struct Health {
    /// Number of live ScreenCaptureKit streams.
    pub streams: AtomicU32,
    /// Censor boxes currently held.
    pub boxes: AtomicU32,
    /// False once capture errors suggest Screen Recording was revoked or
    /// streams are dead.
    pub capture_ok: AtomicBool,
    /// Epoch of the applied managed config (0 = unmanaged/defaults).
    pub config_epoch: AtomicU64,
    /// False when the applied policy disables censoring — the daemon must
    /// treat that as healthy-by-policy, not as a tampered censor.
    pub enabled: AtomicBool,
    /// True while an activity challenge has gone unanswered past its
    /// window; the daemon quarantines (after grace) until it clears.
    pub challenge_overdue: AtomicBool,
    /// Edge flag: the exposure budget was just exceeded. The heartbeat
    /// consumes it (swap to false) so the daemon starts exactly one timed
    /// lockout per trip rather than refreshing it every heartbeat.
    pub exposure_over_budget: AtomicBool,
    /// Duration the daemon should hold the lockout when the edge fires.
    pub exposure_penalty_secs: AtomicU32,
}

impl Health {
    pub fn new() -> Arc<Self> {
        let h = Self::default();
        h.capture_ok.store(true, Ordering::Relaxed);
        h.enabled.store(true, Ordering::Relaxed);
        Arc::new(h)
    }
}

/// Spawn the reporting thread. Failures are silent-but-slow: a managed
/// install's daemon notices missing heartbeats on its own clock.
pub fn spawn(health: Arc<Health>) {
    std::thread::spawn(move || loop {
        // Consume the exposure edge so a single trip = a single lockout.
        let over_budget = health.exposure_over_budget.swap(false, Ordering::Relaxed);
        let line = format!(
            "{{\"type\":\"heartbeat\",\"pid\":{},\"streams\":{},\"boxes\":{},\"captureOk\":{},\"configEpoch\":{},\"enabled\":{},\"challengeOverdue\":{},\"exposureOverBudget\":{},\"exposurePenaltySec\":{}}}\n",
            std::process::id(),
            health.streams.load(Ordering::Relaxed),
            health.boxes.load(Ordering::Relaxed),
            health.capture_ok.load(Ordering::Relaxed),
            health.config_epoch.load(Ordering::Relaxed),
            health.enabled.load(Ordering::Relaxed),
            health.challenge_overdue.load(Ordering::Relaxed),
            over_budget,
            health.exposure_penalty_secs.load(Ordering::Relaxed),
        );
        match UnixStream::connect(DAEMON_SOCKET) {
            Ok(mut stream) => {
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                let _ = stream.write_all(line.as_bytes());
            }
            Err(_) => {
                // No daemon (unmanaged) — back off harder.
                std::thread::sleep(HEARTBEAT_INTERVAL * 5);
                continue;
            }
        }
        std::thread::sleep(HEARTBEAT_INTERVAL);
    });
}
