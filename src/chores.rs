//! Chores (docs/chores.md) — the child's side. The daemon owns every fact
//! that matters (definitions from the bank, claims, the PIN check, credit);
//! this module only renders the list, lets the child say "I did it", and
//! relays the PIN a parent types. All dialogs are osascript (`prompt.rs`),
//! so the flow runs on its own thread and never blocks the pipeline.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::heartbeat::DAEMON_SOCKET;
use crate::prompt;
use crate::settings::Effective;

/// The resolved settings, for `chores.kidsUrl` (set once from main).
static EFFECTIVE: OnceLock<Arc<RwLock<Effective>>> = OnceLock::new();

pub fn init(shared: Arc<RwLock<Effective>>) {
    let _ = EFFECTIVE.set(shared);
}

/// The kids web app URL from policy, if configured.
fn kids_url() -> Option<String> {
    let url = EFFECTIVE
        .get()
        .and_then(|e| e.read().ok())
        .map(|e| e.chores.kids_url.trim().to_string())?;
    (!url.is_empty()).then_some(url)
}

/// One chore flow at a time (the menu item can be clicked repeatedly).
static FLOW_OPEN: AtomicBool = AtomicBool::new(false);

/// How long the PIN dialog waits for a parent before withdrawing the claim.
const PIN_WAIT_SECS: u32 = 300;

/// One request, one JSON-line reply on the daemon socket. None when there
/// is no daemon (unmanaged install) or it does not answer.
fn rpc(msg: Value) -> Option<Value> {
    let mut stream = UnixStream::connect(DAEMON_SOCKET).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    stream.write_all(format!("{msg}\n").as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// The daemon's `chores` reply: definitions plus today's state.
pub fn list() -> Option<Value> {
    rpc(json!({"type": "chores"}))
}

/// A chore as shown in the picker.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    id: String,
    name: String,
    required: bool,
    minutes: u64,
    due: bool,
    verified: bool,
    pending: bool,
}

fn str_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn rows(v: &Value) -> Vec<Row> {
    let verified = str_list(v, "verified");
    let pending = str_list(v, "pending");
    v.get("chores")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let id = c.get("id")?.as_str()?.to_string();
                    Some(Row {
                        name: c
                            .get("name")
                            .and_then(|n| n.as_str())
                            .filter(|n| !n.is_empty())
                            .unwrap_or(&id)
                            .to_string(),
                        required: c.get("kind").and_then(|k| k.as_str()) == Some("required"),
                        minutes: c.get("minutes").and_then(|m| m.as_u64()).unwrap_or(0),
                        due: c.get("due").and_then(|d| d.as_bool()).unwrap_or(true),
                        verified: verified.contains(&id),
                        pending: pending.contains(&id),
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The picker line for a chore. Unique per chore because the name is in it
/// (two chores with the same name would be a bank authoring mistake).
fn label(r: &Row) -> String {
    if r.verified {
        format!("✓ {} — done", r.name)
    } else if r.pending {
        format!("⏳ {} — waiting for a parent", r.name)
    } else if r.required && r.due {
        format!("☐ {} — required today", r.name)
    } else if r.required {
        format!("☐ {} — not due today", r.name)
    } else {
        format!("☐ {} — +{} min", r.name, r.minutes)
    }
}

/// One-line summary for the HUD and the menu, from a `status`/`chores`
/// reply's `chores` object. None when the daemon predates chores.
pub fn summary(chores: Option<&Value>) -> Option<String> {
    let c = chores?;
    if !c.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false) {
        return Some("not configured".to_string());
    }
    let outstanding = str_list(c, "outstanding");
    let pending = str_list(c, "pending");
    let done = str_list(c, "verified").len();
    let locked = c.get("pinLockedSecs").and_then(|s| s.as_u64()).unwrap_or(0);
    let mut parts = Vec::new();
    if !outstanding.is_empty() {
        parts.push(format!(
            "{} required to do ({})",
            outstanding.len(),
            outstanding.join(", ")
        ));
    }
    if !pending.is_empty() {
        parts.push(format!("{} waiting for a parent", pending.len()));
    }
    if done > 0 {
        parts.push(format!("{done} done today"));
    }
    if locked > 0 {
        parts.push(format!("PIN locked {}s", locked));
    }
    Some(if parts.is_empty() {
        "nothing to do".to_string()
    } else {
        parts.join(" · ")
    })
}

/// Menu-bar entry point. With a kids web app configured (`chores.kidsUrl`)
/// open it in the browser; otherwise run the local picker/verify flow (the
/// offline PIN fallback) on a background thread.
pub fn open() {
    if let Some(url) = kids_url() {
        let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
        return;
    }
    if FLOW_OPEN.swap(true, Ordering::SeqCst) {
        return; // already showing
    }
    std::thread::spawn(|| {
        flow();
        FLOW_OPEN.store(false, Ordering::SeqCst);
    });
}

fn flow() {
    loop {
        let Some(v) = list() else {
            prompt::notice("Chores need the betamacs daemon (managed install).");
            return;
        };
        if !v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false) {
            prompt::notice("No chores are set up on this Mac yet.");
            return;
        }
        let rows = rows(&v);
        if rows.is_empty() {
            prompt::notice("The task bank has no chores in it.");
            return;
        }
        let items: Vec<String> = rows.iter().map(label).collect();
        let Some(picked) = prompt::choose_from_list(
            "betamacs — Chores",
            "Pick the chore you finished, then ask a parent to verify it.",
            &items,
            "I did it",
            "Close",
        ) else {
            return;
        };
        let Some(row) = rows.iter().find(|r| label(r) == picked) else {
            continue;
        };
        if row.verified {
            prompt::notice(&format!("\"{}\" is already verified for today.", row.name));
            continue;
        }
        match rpc(json!({"type": "chore-claim", "id": row.id})) {
            Some(r) if r.get("ok").and_then(|o| o.as_bool()) == Some(true) => {}
            Some(r) => {
                let err = r.get("error").and_then(|e| e.as_str()).unwrap_or("refused");
                prompt::notice(&format!("Can't claim \"{}\": {err}.", row.name));
                continue;
            }
            None => {
                prompt::notice("The betamacs daemon stopped answering.");
                return;
            }
        }
        verify(row);
    }
}

/// Ask a parent for the PIN until it is right, the parent says "Not yet",
/// the daemon locks PIN entry, or the dialog times out. A withdrawn attempt
/// rejects the claim so the chore goes back to "☐".
fn verify(row: &Row) {
    let mut note = String::new();
    loop {
        let msg = format!(
            "{note}Ask a parent to check \"{}\".\n\nParent: enter the chore PIN to verify it.",
            row.name
        );
        let Some(pin) =
            prompt::ask_hidden("betamacs — Verify a chore", &msg, "Not yet", "Verify", PIN_WAIT_SECS)
        else {
            let _ = rpc(json!({"type": "chore-reject", "id": row.id}));
            return;
        };
        let Some(r) = rpc(json!({"type": "chore-verify", "id": row.id, "pin": pin})) else {
            prompt::notice("The betamacs daemon stopped answering.");
            return;
        };
        match r.get("result").and_then(|x| x.as_str()).unwrap_or("") {
            "verified" => {
                let minutes = r.get("minutes").and_then(|m| m.as_f64()).unwrap_or(0.0);
                if minutes > 0.0 {
                    prompt::notice(&format!(
                        "\"{}\" verified — +{minutes:.0} minutes of screen time.",
                        row.name
                    ));
                } else {
                    prompt::notice(&format!("\"{}\" verified. Thanks!", row.name));
                }
                return;
            }
            "wrong-pin" => {
                let left = r.get("attemptsLeft").and_then(|a| a.as_u64()).unwrap_or(0);
                note = format!("Wrong PIN — {left} attempt(s) left.\n\n");
            }
            "locked" => {
                let secs = r.get("secs").and_then(|s| s.as_u64()).unwrap_or(0);
                let _ = rpc(json!({"type": "chore-reject", "id": row.id}));
                prompt::notice(&format!(
                    "Too many wrong PINs. Verification is locked for {} minutes.",
                    secs.div_ceil(60)
                ));
                return;
            }
            _ => {
                let err = r.get("error").and_then(|e| e.as_str()).unwrap_or("refused");
                let _ = rpc(json!({"type": "chore-reject", "id": row.id}));
                prompt::notice(&format!("Can't verify \"{}\": {err}.", row.name));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> Value {
        json!({
            "ok": true, "enabled": true, "pinSet": true,
            "outstanding": ["bed"], "pending": ["piano"], "verified": ["trash"],
            "pinLockedSecs": 0,
            "chores": [
                {"id":"bed","name":"Make your bed","kind":"required","repeat":"daily","minutes":0,"due":true},
                {"id":"dishes","name":"Dishes","kind":"required","repeat":"daily","minutes":0,"due":false},
                {"id":"piano","name":"Piano","kind":"bonus","repeat":"daily","minutes":20,"due":true},
                {"id":"trash","name":"Trash","kind":"bonus","repeat":"weekly","minutes":30,"due":true},
                {"id":"noname","kind":"bonus","minutes":5,"due":true}
            ]
        })
    }

    #[test]
    fn rows_and_labels() {
        let rows = rows(&reply());
        let labels: Vec<String> = rows.iter().map(label).collect();
        assert_eq!(
            labels,
            vec![
                "☐ Make your bed — required today",
                "☐ Dishes — not due today",
                "⏳ Piano — waiting for a parent",
                "✓ Trash — done",
                "☐ noname — +5 min",
            ]
        );
        // Every label maps back to exactly one row (the picker relies on it).
        for r in &rows {
            assert_eq!(rows.iter().filter(|x| label(x) == label(r)).count(), 1);
        }
    }

    #[test]
    fn summary_lines() {
        assert_eq!(
            summary(Some(&reply())).as_deref(),
            Some("1 required to do (bed) · 1 waiting for a parent · 1 done today")
        );
        assert_eq!(summary(None), None);
        assert_eq!(
            summary(Some(&json!({"enabled": false}))).as_deref(),
            Some("not configured")
        );
        assert_eq!(
            summary(Some(&json!({"enabled": true, "outstanding": [], "pending": [], "verified": []})))
                .as_deref(),
            Some("nothing to do")
        );
        assert_eq!(
            summary(Some(&json!({"enabled": true, "pinLockedSecs": 90}))).as_deref(),
            Some("PIN locked 90s")
        );
    }
}
