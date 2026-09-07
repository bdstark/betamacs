//! Native user prompts via osascript (AppleScript). The agent shows a
//! warning or asks a question without embedding a text field in the
//! CALayer overlay; it runs in the console user's Aqua session, so the
//! dialogs appear frontmost. All user text is escaped into AppleScript
//! string literals, never interpolated as code.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

/// How many content-warnings have been shown this process run, for escalation.
static EXPOSURE_WARN_COUNT: AtomicU32 = AtomicU32::new(0);

/// The minimum the content-warning holds before it can be acknowledged, and
/// how much each repeat this run adds, up to a cap.
const EXPOSURE_HOLD_MIN_SECS: u32 = 5;
const EXPOSURE_HOLD_STEP_SECS: u32 = 5;
const EXPOSURE_HOLD_MAX_SECS: u32 = 30;

/// Quote a string as an AppleScript string literal. AppleScript supports
/// `\"`, `\\`, and `\n` escapes inside double-quoted strings.
pub fn quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"),
    )
}

/// Non-blocking warning dialog (fire-and-forget on its own thread).
pub fn warn(message: &str) {
    let message = message.to_string();
    std::thread::spawn(move || {
        let script = format!(
            "display dialog {} buttons {{\"OK\"}} default button \"OK\" \
             with title \"betamacs\" with icon caution giving up after 60",
            quote(&message),
        );
        let _ = Command::new("/usr/bin/osascript").args(["-e", &script]).output();
    });
}

/// The exposure content-warning ("Are you looking at appropriate content?").
/// Unlike `warn`, this CANNOT be clicked through: it holds on screen for a
/// minimum window before the user can acknowledge it, and the window escalates
/// on repeat within a run (`MIN + STEP * priorWarnings`, capped at `MAX`).
///
/// How the hold is enforced: osascript's `display dialog` has no native
/// min-display, and `giving up after N` only auto-*closes* after N seconds — it
/// does nothing to stop an earlier click. The only way to GUARANTEE the user
/// can't dismiss early is to show a dialog with NO buttons (no dismiss
/// affordance at all) during the hold; osascript accepts an empty button list
/// and just auto-closes it when `giving up after` elapses. We show that
/// button-less dialog once per second with a live "(Ns)" countdown in the
/// title (a single osascript dialog can't update its own text, hence the
/// re-show loop), then finally show the real one-button acknowledgement dialog.
/// Tradeoff: the once-a-second re-show makes the hold dialog briefly blink each
/// second — accepted because it buys a real ticking countdown that is provably
/// non-dismissable, which a single static "please wait" dialog would not be.
///
/// Fire-and-forget on its own thread, like `warn`.
pub fn warn_exposure(message: &str) {
    let prior = EXPOSURE_WARN_COUNT.fetch_add(1, Ordering::Relaxed);
    let hold = (EXPOSURE_HOLD_MIN_SECS + prior.saturating_mul(EXPOSURE_HOLD_STEP_SECS))
        .min(EXPOSURE_HOLD_MAX_SECS);
    let message = message.to_string();
    std::thread::spawn(move || {
        for remaining in (1..=hold).rev() {
            // Button-less: no affordance to dismiss, so the loop controls the
            // full display time. `giving up after 1` closes each frame after a
            // second; the title carries the countdown.
            let title = format!("betamacs — please wait {remaining}s");
            let script = format!(
                "display dialog {} buttons {{}} with title {} with icon caution \
                 giving up after 1",
                quote(&message),
                quote(&title),
            );
            let out = Command::new("/usr/bin/osascript").args(["-e", &script]).output();
            // If osascript can't run a GUI dialog (no session), bail rather
            // than spin the loop hot for `hold` seconds.
            if out.map(|o| !o.status.success()).unwrap_or(true) {
                return;
            }
        }
        // Hold elapsed: now the dismissable acknowledgement.
        let script = format!(
            "display dialog {} buttons {{\"OK\"}} default button \"OK\" \
             with title \"betamacs\" with icon caution giving up after 60",
            quote(&message),
        );
        let _ = Command::new("/usr/bin/osascript").args(["-e", &script]).output();
    });
}

/// Blocking one-button notice (the caller waits until it is acknowledged or
/// it gives up after a minute). Use from flows that must not stack dialogs.
pub fn notice(message: &str) {
    let script = format!(
        "display dialog {} buttons {{\"OK\"}} default button \"OK\" \
         with title \"betamacs\" with icon note giving up after 60",
        quote(message),
    );
    let _ = Command::new("/usr/bin/osascript").args(["-e", &script]).output();
}

/// The AppleScript for `choose_from_list` (pure, unit-tested by compiling).
fn choose_script(title: &str, prompt: &str, items: &[String], ok: &str, cancel: &str) -> String {
    let list = items.iter().map(|i| quote(i)).collect::<Vec<_>>().join(", ");
    format!(
        "set r to choose from list {{{list}}} with title {} with prompt {} \
         OK button name {} cancel button name {}\n\
         if r is false then return \"__CANCEL__\"\n\
         return \"__OK__\" & item 1 of r",
        quote(title),
        quote(prompt),
        quote(ok),
        quote(cancel),
    )
}

/// A single-selection list picker. Returns the chosen item's text, or None
/// on cancel / no GUI. Blocking.
pub fn choose_from_list(
    title: &str,
    prompt: &str,
    items: &[String],
    ok: &str,
    cancel: &str,
) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let script = choose_script(title, prompt, items, ok, cancel);
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim_end_matches(['\n', '\r']);
    text.strip_prefix("__OK__").map(str::to_string)
}

/// The AppleScript for `ask_hidden` (pure, unit-tested by compiling). The
/// cancel button raises "user canceled" (non-zero exit), a give-up returns
/// the sentinel; both map to None.
fn ask_hidden_script(title: &str, prompt: &str, cancel: &str, ok: &str, timeout_sec: u32) -> String {
    format!(
        "set r to display dialog {} default answer \"\" with hidden answer \
         buttons {{{cancel}, {ok}}} default button {ok} cancel button {cancel} \
         with title {} with icon caution giving up after {timeout}\n\
         if gave up of r then return \"__GAVE_UP__\"\n\
         return \"__OK__\" & text returned of r",
        quote(prompt),
        quote(title),
        cancel = quote(cancel),
        ok = quote(ok),
        timeout = timeout_sec.max(5),
    )
}

/// Ask for a masked entry (a PIN) with a cancel button. Some(text) on `ok`,
/// None on cancel, give-up, or no GUI. Blocking.
pub fn ask_hidden(
    title: &str,
    prompt: &str,
    cancel: &str,
    ok: &str,
    timeout_sec: u32,
) -> Option<String> {
    let script = ask_hidden_script(title, prompt, cancel, ok, timeout_sec);
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim_end_matches(['\n', '\r']);
    text.strip_prefix("__OK__").map(str::to_string)
}

/// Ask for a typed answer, blocking until the user submits or the dialog
/// gives up after `timeout_sec`. The only button is Submit (no Cancel), so
/// the dialog can't be dismissed without answering — a give-up returns
/// None. Returns Some(text) on submit, None on timeout or osascript error.
pub fn ask(prompt: &str, timeout_sec: u32) -> Option<String> {
    let started = Instant::now();
    let script = format!(
        "set r to display dialog {} default answer \"\" buttons {{\"Submit\"}} \
         default button \"Submit\" with title \"betamacs\" with icon caution \
         giving up after {timeout}\n\
         if gave up of r then return \"__GAVE_UP__\"\n\
         return \"__OK__\" & text returned of r",
        quote(prompt),
        timeout = timeout_sec.max(5),
    );
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .ok()?;
    if !out.status.success() {
        // No GUI / scripting error: avoid a hot re-ask loop upstream.
        if started.elapsed().as_secs() < 1 {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim_end_matches(['\n', '\r']);
    text.strip_prefix("__OK__").map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Syntax-check a script with osacompile (compiles, never runs — no
    /// dialog appears). Skipped where osacompile is missing.
    fn compiles(script: &str) -> bool {
        if !std::path::Path::new("/usr/bin/osacompile").exists() {
            return true;
        }
        let out = std::env::temp_dir().join(format!("bm-prompt-{}.scpt", std::process::id()));
        let ok = Command::new("/usr/bin/osacompile")
            .args(["-o", out.to_str().unwrap(), "-e", script])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let _ = std::fs::remove_file(out);
        ok
    }

    #[test]
    fn quote_escapes() {
        assert_eq!(quote("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
    }

    #[test]
    fn chore_scripts_compile() {
        let items = vec!["☐ Make your \"bed\" — required today".to_string(), "✓ Trash — done".into()];
        assert!(compiles(&choose_script("betamacs — Chores", "Pick one:", &items, "I did it", "Close")));
        assert!(compiles(&ask_hidden_script(
            "betamacs — Verify a chore",
            "Wrong PIN — 2 attempt(s) left.\n\nAsk a parent to check \"bed\".",
            "Not yet",
            "Verify",
            300
        )));
    }
}
