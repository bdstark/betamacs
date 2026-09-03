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
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::heartbeat::Health;
use crate::settings::{EarnSource, Effective};

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

/// The current-tab host of `bundle_id` if it is a browser we know how to ask.
/// Wrapped in an AppleScript `try` so a missing window or a denied Automation
/// grant yields None instead of an error.
fn browser_url_host(bundle_id: &str) -> Option<String> {
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
    host_of(url.trim())
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

/// Spawn the activity monitor. Idles while the gate is disabled; while
/// enabled, samples every `TICK` and accrues active minutes per source.
pub fn spawn(shared: Arc<RwLock<Effective>>, health: Arc<Health>) {
    // The daemon ledger doesn't exist yet, so `health` is unused for now; it
    // stays in the signature so wiring the report path later needs no change
    // at the call site.
    let _ = &health;
    std::thread::spawn(move || {
        // Running per-source earned minutes for this process's lifetime. The
        // authoritative, persisted balance will live in betamacsd's ledger.
        let mut earned: HashMap<String, f64> = HashMap::new();
        let mut last = Instant::now();
        loop {
            std::thread::sleep(TICK);
            let cfg = shared.read().unwrap().earned_time.clone();
            let now = Instant::now();
            let elapsed_min = now.duration_since(last).as_secs_f64() / 60.0;
            last = now;

            if !cfg.enabled {
                continue; // disabled by policy: observe nothing
            }

            // Pause crediting while the user is idle.
            let idle = idle_seconds().unwrap_or(0.0);
            if idle > cfg.idle_timeout_sec as f64 {
                tracing::debug!(
                    "earned: idle {idle:.0}s > {}s — not crediting this tick",
                    cfg.idle_timeout_sec
                );
                continue;
            }

            let frontmost = frontmost_bundle_id();
            // Only pay the osascript cost for a browser URL when some source
            // actually keys off a host and the frontmost app is a browser.
            let need_host = cfg
                .sources
                .iter()
                .any(|s| s.matcher.browser_host_suffix.is_some());
            let browser_host = if need_host {
                frontmost.as_deref().and_then(browser_url_host)
            } else {
                None
            };

            for src in &cfg.sources {
                if source_matches(src, frontmost.as_deref(), browser_host.as_deref()) {
                    let delta = elapsed_min * src.earn_ratio as f64;
                    let total = earned.entry(src.name.clone()).or_insert(0.0);
                    *total += delta;
                    tracing::info!(
                        "earned: +{delta:.2} min on \"{}\" (session total {:.1} min)",
                        src.name,
                        *total
                    );
                    // TODO: report earned delta to betamacsd ledger (the
                    // daemon owns the persisted balance and applies the
                    // daily cap / bank ceiling / min-session rules).
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SourceMatch;

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
