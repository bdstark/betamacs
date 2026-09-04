//! Earned-time activity monitor (see `docs/earned-time.md`, part C).
//!
//! When the earned-time gate is enabled, this thread periodically OBSERVES
//! what the console user is doing — the frontmost app's bundle id, the
//! frontmost browser's current-tab host, and how long they have been idle —
//! and accrues "active minutes" against the policy's allowlisted `sources`.
//! It uses only what the agent can see locally (osascript + ioreg), never an
//! external API.
//!
//! For now this is observation-only: it computes and logs the earned deltas
//! but does NOT yet report them to betamacsd's balance ledger. The daemon
//! ledger and pf-based gate enforcement (parts A/B of the design) are still
//! to come; the `// TODO: report earned delta to betamacsd ledger` marker
//! below is where the agent will propose deltas for the daemon to commit.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::heartbeat::{Health, DAEMON_SOCKET};
use crate::settings::{
    ClockIntegritySettings, EarnSource, EarnedTimeSettings, Effective, FocusLimitSettings,
};

/// Is the earned-time gate active right now? Enabled, and the day/time falls
/// inside a schedule window. When clock integrity is on, the window is
/// evaluated against the ASSIGNED timezone applied to the TRUSTED epoch, so a
/// kid can't shift it by changing the OS clock or timezone; otherwise it
/// falls back to the OS local time. Empty schedule = never gated.
fn gate_active(cfg: &EarnedTimeSettings, ci: &ClockIntegritySettings) -> bool {
    if !cfg.enabled || cfg.schedule.is_empty() {
        return false;
    }
    let (tz, epoch) = if ci.enabled {
        (ci.timezone.as_deref(), crate::clock::trusted_epoch())
    } else {
        (None, None)
    };
    let mut cmd = Command::new("/bin/date");
    if let Some(tz) = tz {
        cmd.env("TZ", tz);
    }
    match epoch {
        Some(e) => {
            cmd.args(["-r", &e.to_string(), "+%u %H%M"]);
        }
        None => {
            cmd.args(["+%u %H%M"]);
        }
    }
    let Ok(out) = cmd.output() else {
        return false;
    };
    let s = String::from_utf8_lossy(&out.stdout);
    let mut it = s.split_whitespace();
    let dow: usize = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let hhmm: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(9999);
    let day = ["", "mon", "tue", "wed", "thu", "fri", "sat", "sun"]
        .get(dow)
        .copied()
        .unwrap_or("");
    let parse = |t: &str| -> Option<u32> {
        let (h, m) = t.split_once(':')?;
        Some(h.trim().parse::<u32>().ok()? * 100 + m.trim().parse::<u32>().ok()?)
    };
    cfg.schedule.iter().any(|w| {
        w.days.iter().any(|d| d.eq_ignore_ascii_case(day))
            && matches!((parse(&w.from), parse(&w.to)), (Some(f), Some(t)) if hhmm >= f && hhmm < t)
    })
}

/// Report this tick's earned seconds + resolved policy to betamacsd, which
/// owns the authoritative balance ledger and the pf gate. Sent on the
/// daemon's own socket (reachable even under an earning-mode lockout, which
/// allows loopback). Silently skipped when there is no daemon (unmanaged).
fn report_earn(secs: u32, gate_active: bool, cfg: &EarnedTimeSettings) {
    let hosts = cfg
        .sources
        .iter()
        .filter_map(|s| s.matcher.browser_host_suffix.clone())
        .map(|h| format!("{h:?}")) // debug-quotes with JSON-safe escaping
        .collect::<Vec<_>>()
        .join(",");
    let line = format!(
        "{{\"type\":\"earn\",\"secs\":{secs},\"gateActive\":{gate_active},\"spendRatio\":{},\"dailyCapMin\":{},\"maxBankMin\":{},\"allowHosts\":[{hosts}]}}\n",
        cfg.spend_ratio, cfg.daily_earn_cap_min, cfg.max_bank_min,
    );
    if let Ok(mut s) = UnixStream::connect(DAEMON_SOCKET) {
        let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
        let _ = s.write_all(line.as_bytes());
    }
}

/// How often we sample the foreground state. The design calls for ~20s.
const TICK: Duration = Duration::from_secs(20);

/// Frontmost application's bundle id via System Events, or None on failure
/// (no GUI session, Automation permission denied, etc.).
fn frontmost_bundle_id() -> Option<String> {
    let script = "tell application \"System Events\" to get bundle identifier \
                  of first application process whose frontmost is true";
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// The frontmost browser's current-tab URL, if `bundle_id` is a browser we
/// know how to ask. Wrapped in an AppleScript `try` so a missing window or a
/// denied Automation grant yields None instead of an error.
fn browser_url(bundle_id: &str) -> Option<String> {
    let app = match bundle_id {
        "com.apple.Safari" => "Safari",
        "com.apple.SafariTechnologyPreview" => "Safari Technology Preview",
        "com.google.Chrome" => "Google Chrome",
        "com.google.Chrome.canary" => "Google Chrome Canary",
        "com.brave.Browser" => "Brave Browser",
        "com.microsoft.edgemac" => "Microsoft Edge",
        "company.thebrowser.Browser" => "Arc",
        "com.vivaldi.Vivaldi" => "Vivaldi",
        _ => return None,
    };
    // Safari names it "current tab"; the Chromium family uses "active tab".
    let tab = if app.starts_with("Safari") {
        "URL of current tab of front window"
    } else {
        "URL of active tab of front window"
    };
    let script = format!(
        "try\n\ttell application \"{app}\" to return {tab}\non error\n\treturn \"\"\nend try"
    );
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout);
    let url = url.trim();
    (!url.is_empty()).then(|| url.to_string())
}

/// Does `host` match any suffix in `list` ("youtube.com" matches
/// "youtube.com" and "*.youtube.com")?
fn host_matches_any(host: &str, list: &[String]) -> bool {
    list.iter().any(|s| {
        let s = s.trim_start_matches('.').to_lowercase();
        !s.is_empty() && (host == s || host.ends_with(&format!(".{s}")))
    })
}

/// Extract the lowercased host from a URL, without pulling in a URL crate.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    // Drop any userinfo and port.
    let host = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = host.split(':').next()?.trim();
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// Seconds since the last HID input event, parsed from `ioreg`'s IOHIDSystem
/// (`HIDIdleTime` is in nanoseconds). Multiple entries can appear; the
/// smallest is the most recent activity.
fn idle_seconds() -> Option<f64> {
    let out = Command::new("/usr/sbin/ioreg")
        .args(["-c", "IOHIDSystem"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut best: Option<u64> = None;
    for line in text.lines() {
        let Some(idx) = line.find("HIDIdleTime") else {
            continue;
        };
        let Some(eq) = line[idx..].find('=') else {
            continue;
        };
        let digits: String = line[idx + eq + 1..]
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(ns) = digits.parse::<u64>() {
            best = Some(best.map_or(ns, |b| b.min(ns)));
        }
    }
    best.map(|ns| ns as f64 / 1_000_000_000.0)
}

/// True when the observed foreground state matches an allowlisted source.
fn source_matches(
    src: &EarnSource,
    frontmost: Option<&str>,
    browser_host: Option<&str>,
) -> bool {
    if let (Some(want), Some(got)) = (src.matcher.bundle_id.as_deref(), frontmost)
        && want == got
    {
        return true;
    }
    if let (Some(suffix), Some(host)) = (src.matcher.browser_host_suffix.as_deref(), browser_host) {
        let suffix = suffix.trim_start_matches('.').to_lowercase();
        if host == suffix || host.ends_with(&format!(".{suffix}")) {
            return true;
        }
    }
    false
}

/// Mutable dwell state for the same-tab focus limit.
#[derive(Default)]
struct FocusState {
    /// The URL currently being timed (None = not tracking).
    url: Option<String>,
    /// Active (non-idle) seconds accrued on `url`.
    active_secs: f64,
    /// Suppress re-triggering until this passes (the lockout window).
    locked_until: Option<Instant>,
}

/// Advance the same-tab focus timer for one sample and trip a timed lockout
/// when the active dwell on one URL exceeds the limit. Dwell accrues only
/// while the user is NOT idle; passive video watching is idle and pauses it.
/// Whitelisted hosts are exempt; a non-empty blacklist restricts monitoring
/// to listed hosts.
///
/// "Active" comes from `active_scroll`: when the scroll event tap is running
/// (Accessibility granted, see `scroll.rs`), only REAL scrolling counts, so
/// reading-while-highlighting or writing an email (little scroll) is exempt
/// everywhere. Without the grant `active_scroll` is None and we fall back to
/// "not idle" (any recent input), which over-counts focused work — mitigate
/// that fallback by scoping to feeds via `blacklist_hosts`.
fn track_focus(
    fl: &FocusLimitSettings,
    health: &Health,
    idle: f64,
    active_scroll: Option<bool>,
    url: Option<&str>,
    host: Option<&str>,
    elapsed: Duration,
    st: &mut FocusState,
) {
    // Hold off while a lockout it triggered is still in effect.
    if st.locked_until.is_some_and(|t| t > Instant::now()) {
        st.url = None;
        st.active_secs = 0.0;
        return;
    }
    let host = host.unwrap_or("");
    let monitored = url.is_some()
        && !host_matches_any(host, &fl.whitelist_hosts)
        && (fl.blacklist_hosts.is_empty() || host_matches_any(host, &fl.blacklist_hosts));
    if !monitored {
        st.url = None;
        st.active_secs = 0.0;
        return;
    }
    let url = url.unwrap();
    if st.url.as_deref() != Some(url) {
        // Navigated (or first sight): restart the timer for this tab.
        st.url = Some(url.to_string());
        st.active_secs = 0.0;
        return;
    }
    // Same tab: count only ACTIVE time. Prefer real scroll activity (from
    // the event tap); when that isn't available (no Accessibility grant),
    // fall back to "not idle" (any recent input).
    let active = active_scroll.unwrap_or(idle <= fl.idle_reset_sec as f64);
    if active {
        st.active_secs += elapsed.as_secs_f64();
    }
    if st.active_secs >= fl.same_tab_limit_min.max(1) as f64 * 60.0 {
        let secs = fl.lockout_min.max(1) * 60;
        health.focus_penalty_secs.store(secs, Ordering::Relaxed);
        health.focus_over_limit.store(true, Ordering::Relaxed);
        st.locked_until = Some(Instant::now() + Duration::from_secs(secs as u64));
        st.url = None;
        st.active_secs = 0.0;
        tracing::warn!("focus: active same-tab limit reached on {host} — {secs}s lockout");
    }
}

/// Spawn the activity monitor. Idles while both earned-time and the focus
/// limit are disabled; otherwise samples every `TICK` and drives each.
pub fn spawn(shared: Arc<RwLock<Effective>>, health: Arc<Health>) {
    std::thread::spawn(move || {
        // Running per-source earned minutes for this process's lifetime (for
        // the log); the authoritative persisted balance is betamacsd's.
        let mut earned: HashMap<String, f64> = HashMap::new();
        // Fractional earned seconds not yet reported (reports are whole).
        let mut carry = 0.0_f64;
        let mut focus = FocusState::default();
        let mut last = Instant::now();
        loop {
            std::thread::sleep(TICK);
            let eff = shared.read().unwrap().clone();
            let (et, fl) = (&eff.earned_time, &eff.focus_limit);
            let now = Instant::now();
            let elapsed = now.duration_since(last);
            let elapsed_min = elapsed.as_secs_f64() / 60.0;
            last = now;

            if !et.enabled && !fl.enabled {
                report_earn(0, false, et); // clear any stale earned gate
                focus = FocusState::default();
                continue;
            }

            // One foreground sample shared by both features.
            let idle = idle_seconds().unwrap_or(0.0);
            let frontmost = frontmost_bundle_id();
            let need_url = fl.enabled
                || et.sources.iter().any(|s| s.matcher.browser_host_suffix.is_some());
            let url = if need_url {
                frontmost.as_deref().and_then(browser_url)
            } else {
                None
            };
            let host = url.as_deref().and_then(host_of);

            // Earned-time accrual.
            if et.enabled {
                let mut credited_min = 0.0_f64;
                if idle <= et.idle_timeout_sec as f64 {
                    for src in &et.sources {
                        if source_matches(src, frontmost.as_deref(), host.as_deref()) {
                            let delta = elapsed_min * src.earn_ratio as f64;
                            credited_min += delta;
                            let total = earned.entry(src.name.clone()).or_insert(0.0);
                            *total += delta;
                            tracing::info!(
                                "earned: +{delta:.2} min on \"{}\" (session total {:.1} min)",
                                src.name,
                                *total
                            );
                        }
                    }
                }
                carry += credited_min * 60.0;
                let whole = carry.floor().max(0.0) as u32;
                carry -= whole as f64;
                report_earn(whole, gate_active(et, &eff.clock_integrity), et);
            } else {
                report_earn(0, false, et);
            }

            // Same-tab focus limit. Drain the scroll counter every tick so
            // it reflects only this interval (and stays bounded when off).
            let scrolled = crate::scroll::take_scrolled();
            if fl.enabled {
                track_focus(
                    fl, &health, idle, scrolled, url.as_deref(), host.as_deref(), elapsed,
                    &mut focus,
                );
            } else {
                focus = FocusState::default();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SourceMatch;

    fn fl(same_tab_min: u32, white: &[&str], black: &[&str]) -> FocusLimitSettings {
        FocusLimitSettings {
            enabled: true,
            same_tab_limit_min: same_tab_min,
            lockout_min: 10,
            idle_reset_sec: 60,
            whitelist_hosts: white.iter().map(|s| s.to_string()).collect(),
            blacklist_hosts: black.iter().map(|s| s.to_string()).collect(),
        }
    }
    fn tripped(h: &Health) -> bool {
        h.focus_over_limit.load(Ordering::Relaxed)
    }

    #[test]
    fn focus_active_trips_after_limit() {
        let h = Health::new();
        let cfg = fl(1, &[], &[]); // 1 minute of active dwell
        let mut st = FocusState::default();
        let (u, host) = (Some("https://reddit.com/r/x"), Some("reddit.com"));
        // sample 1 sets the tab; 2 and 3 accrue 30s each -> 60s -> trip.
        for _ in 0..3 {
            track_focus(&cfg, &h, 0.0, None, u, host, Duration::from_secs(30), &mut st);
        }
        assert!(tripped(&h));
    }

    #[test]
    fn focus_scroll_gates_activity() {
        // With the event tap active (Some(...)): only scrolling accrues, so
        // a static page with input-but-no-scroll never trips.
        let h = Health::new();
        let cfg = fl(1, &[], &[]);
        let (u, host) = (Some("https://blog.example/post"), Some("blog.example"));
        let mut st = FocusState::default();
        for _ in 0..20 {
            // active input, but no scroll -> Some(false) -> no accrual.
            track_focus(&cfg, &h, 0.0, Some(false), u, host, Duration::from_secs(30), &mut st);
        }
        assert!(!tripped(&h), "reading/highlighting (no scroll) must not trip");
        // Now actually scrolling -> Some(true) -> accrues and trips.
        for _ in 0..3 {
            track_focus(&cfg, &h, 0.0, Some(true), u, host, Duration::from_secs(30), &mut st);
        }
        assert!(tripped(&h));
    }

    #[test]
    fn focus_idle_video_does_not_trip() {
        let h = Health::new();
        let cfg = fl(1, &[], &[]);
        let mut st = FocusState::default();
        let (u, host) = (Some("https://youtube.com/watch?v=x"), Some("youtube.com"));
        for _ in 0..20 {
            // idle 120s > idle_reset_sec: passive, no accrual.
            track_focus(&cfg, &h, 120.0, None, u, host, Duration::from_secs(30), &mut st);
        }
        assert!(!tripped(&h));
    }

    #[test]
    fn focus_whitelist_is_exempt() {
        let h = Health::new();
        let cfg = fl(1, &["khanacademy.org"], &[]);
        let mut st = FocusState::default();
        for _ in 0..20 {
            track_focus(
                &cfg, &h, 0.0, None,
                Some("https://khanacademy.org/math"), Some("khanacademy.org"),
                Duration::from_secs(30), &mut st,
            );
        }
        assert!(!tripped(&h));
    }

    #[test]
    fn focus_blacklist_restricts_monitoring() {
        let h = Health::new();
        let cfg = fl(1, &[], &["tiktok.com"]);
        let mut st = FocusState::default();
        // Non-blacklisted host is ignored.
        for _ in 0..20 {
            track_focus(&cfg, &h, 0.0, None, Some("https://news.example/a"), Some("news.example"),
                Duration::from_secs(30), &mut st);
        }
        assert!(!tripped(&h));
        // Blacklisted host is monitored and trips.
        for _ in 0..3 {
            track_focus(&cfg, &h, 0.0, None, Some("https://tiktok.com/@x"), Some("tiktok.com"),
                Duration::from_secs(30), &mut st);
        }
        assert!(tripped(&h));
    }

    #[test]
    fn focus_url_change_resets() {
        let h = Health::new();
        let cfg = fl(1, &[], &[]);
        let mut st = FocusState::default();
        // Almost trips on tab A (30s accrued), then navigates: timer resets.
        track_focus(&cfg, &h, 0.0, None, Some("https://a.example/1"), Some("a.example"), Duration::from_secs(30), &mut st);
        track_focus(&cfg, &h, 0.0, None, Some("https://a.example/1"), Some("a.example"), Duration::from_secs(30), &mut st);
        track_focus(&cfg, &h, 0.0, None, Some("https://b.example/2"), Some("b.example"), Duration::from_secs(30), &mut st);
        track_focus(&cfg, &h, 0.0, None, Some("https://b.example/2"), Some("b.example"), Duration::from_secs(30), &mut st);
        assert!(!tripped(&h), "navigation should reset the dwell timer");
    }

    fn src(bundle: Option<&str>, host: Option<&str>) -> EarnSource {
        EarnSource {
            name: "t".into(),
            matcher: SourceMatch {
                bundle_id: bundle.map(str::to_string),
                browser_host_suffix: host.map(str::to_string),
            },
            earn_ratio: 1.0,
        }
    }

    #[test]
    fn host_parsing() {
        assert_eq!(host_of("https://www.khanacademy.org/math").as_deref(), Some("www.khanacademy.org"));
        assert_eq!(host_of("http://EXAMPLE.com:8080/x").as_deref(), Some("example.com"));
        assert_eq!(host_of("").as_deref(), None);
    }

    #[test]
    fn bundle_match() {
        let s = src(Some("org.khanacademy.kids"), None);
        assert!(source_matches(&s, Some("org.khanacademy.kids"), None));
        assert!(!source_matches(&s, Some("com.apple.Safari"), None));
    }

    #[test]
    fn host_suffix_match() {
        let s = src(None, Some("khanacademy.org"));
        assert!(source_matches(&s, None, Some("khanacademy.org")));
        assert!(source_matches(&s, None, Some("www.khanacademy.org")));
        assert!(!source_matches(&s, None, Some("notkhanacademy.org")));
        assert!(!source_matches(&s, None, Some("example.com")));
    }
}
