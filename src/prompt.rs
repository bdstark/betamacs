//! Native user prompts via osascript (AppleScript). The agent shows a
//! warning or asks a question without embedding a text field in the
//! CALayer overlay; it runs in the console user's Aqua session, so the
//! dialogs appear frontmost. All user text is escaped into AppleScript
//! string literals, never interpolated as code.

use std::process::Command;
use std::time::Instant;

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
