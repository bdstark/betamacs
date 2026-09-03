//! Activity challenges: periodically ask the console user to solve a task
//! from the signed `betamacs-tasks` bank, proving they're present and
//! attentive. An unanswered challenge sets `challenge_overdue`, which
//! betamacsd turns into a network quarantine (like tamper/uninstall) until
//! it is answered.
//!
//! The agent owns selection and answer-checking here; it reads the bank
//! the daemon persisted (`tasks.json`). Answers in a shipped bank are
//! stored hashed so the readable file is not a cheat sheet — checking
//! against a hash is a drop-in change to `check` once publishing emits
//! hashes; today it compares the authored plaintext.
//!
//! Input is a native osascript dialog (see `prompt`), so no interactive
//! text field is needed in the overlay. Missing/empty bank or no eligible
//! task means challenges are simply skipped — never a lockout from absence.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::heartbeat::Health;
use crate::prompt;
use crate::settings::{Answer, ChallengeSettings, Effective, Task, TaskBank};

/// Poll cadence for the scheduler thread (cheap; the real interval is the
/// policy's random `interval_*`).
const TICK: Duration = Duration::from_secs(10);

fn load_bank() -> Option<TaskBank> {
    for p in [
        "/Library/Application Support/betamacs/tasks.json",
        "tasks.json",
    ] {
        let path = PathBuf::from(p);
        if let Ok(data) = std::fs::read_to_string(&path)
            && let Ok(bank) = serde_json::from_str::<TaskBank>(&data)
        {
            return Some(bank);
        }
    }
    None
}

/// Cheap xorshift-ish step so we don't pull in `rand` for a scheduler.
fn bump(seed: &mut u64) -> u64 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *seed
}

fn interval(cfg: &ChallengeSettings, seed: &mut u64) -> Duration {
    let lo = cfg.interval_min_sec.min(cfg.interval_max_sec);
    let hi = cfg.interval_min_sec.max(cfg.interval_max_sec);
    let span = (hi - lo).max(1) as u64;
    Duration::from_secs(lo as u64 + (bump(seed) % span))
}

/// Weighted random pick among eligible tasks, avoiding recent ids.
fn pick<'a>(
    bank: &'a TaskBank,
    cfg: &ChallengeSettings,
    recent: &[String],
    seed: &mut u64,
) -> Option<&'a Task> {
    let eligible = |skip_recent: bool| -> Vec<&Task> {
        bank.tasks
            .iter()
            .filter(|t| {
                t.grade <= cfg.max_grade
                    && (cfg.categories.is_empty()
                        || cfg.categories.iter().any(|c| c == &t.category))
                    && (!skip_recent || !recent.contains(&t.id))
            })
            .collect()
    };
    let pool = match eligible(true) {
        p if !p.is_empty() => p,
        _ => eligible(false), // everything is recent — allow repeats
    };
    if pool.is_empty() {
        return None;
    }
    let total: f32 = pool.iter().map(|t| t.weight.max(0.0)).sum();
    if total <= 0.0 {
        return pool.first().copied();
    }
    let target = (bump(seed) % 10_000) as f32 / 10_000.0 * total;
    let mut acc = 0.0;
    for t in &pool {
        acc += t.weight.max(0.0);
        if acc >= target {
            return Some(t);
        }
    }
    pool.last().copied()
}

/// Normalize input into the canonical string that gets hashed — must match
/// the `publish.sh tasks` transform exactly (number: fixed-6-decimal then
/// trimmed; text/choice: trimmed, lowercased per `ignore_case`).
fn canonical(answer: &Answer, input: &str) -> Option<String> {
    let t = input.trim();
    match answer {
        Answer::Number { .. } => {
            let v: f64 = t.parse().ok()?;
            let mut s = format!("{v:.6}");
            while s.contains('.') && s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
            Some(s)
        }
        Answer::Text { ignore_case, .. } => Some(if *ignore_case {
            t.to_lowercase()
        } else {
            t.to_string()
        }),
        Answer::Choice { .. } => Some(t.to_lowercase()),
        Answer::Line { .. } => Some(t.to_string()),
    }
}

/// sha256(salt || 0x00 || canonical), hex — the shipped answer hash.
fn hash_one(salt: &str, canonical: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update([0u8]);
    h.update(canonical.as_bytes());
    format!("{:x}", h.finalize())
}

/// True if `input` satisfies the task — via the shipped answer hashes when
/// present (the secure path), else the plaintext answer.
fn check_task(task: &Task, input: &str) -> bool {
    if let Some(hashes) = &task.answer_hash {
        return canonical(&task.answer, input)
            .map(|c| hash_one(&task.id, &c))
            .is_some_and(|h| hashes.iter().any(|x| x.eq_ignore_ascii_case(&h)));
    }
    check(&task.answer, input)
}

/// True if `input` satisfies the task's plaintext answer.
fn check(answer: &Answer, input: &str) -> bool {
    let input = input.trim();
    match answer {
        Answer::Number { value, tolerance } => input
            .parse::<f64>()
            .map(|n| (n - value).abs() <= tolerance + f64::EPSILON)
            .unwrap_or(false),
        Answer::Text {
            value,
            any_of,
            ignore_case,
        } => {
            let norm = |s: &str| {
                if *ignore_case {
                    s.trim().to_lowercase()
                } else {
                    s.trim().to_string()
                }
            };
            let ni = norm(input);
            value.as_deref().is_some_and(|v| norm(v) == ni)
                || any_of.iter().any(|v| norm(v) == ni)
        }
        Answer::Line { value } => input == value.trim(),
        Answer::Choice { value, .. } => input.eq_ignore_ascii_case(value.trim()),
    }
}

/// The prompt text shown to the user, listing choices when relevant.
fn present(task: &Task, show_hint: bool) -> String {
    let mut p = task.prompt.clone();
    if let Answer::Choice { options, .. } = &task.answer {
        p.push_str("\n\nOptions: ");
        p.push_str(&options.join(", "));
    }
    if show_hint && let Some(h) = &task.hint {
        p.push_str("\n\nHint: ");
        p.push_str(h);
    }
    p
}

/// Run one challenge to resolution: ask until solved, or give up after
/// `max_attempts` wrong answers. Timeouts (no answer in the window) mark
/// the session overdue and keep asking. Returns true if solved.
fn run_challenge(task: &Task, cfg: &ChallengeSettings, health: &Health) -> bool {
    let mut wrong = 0u32;
    loop {
        let text = present(task, wrong > 0);
        match prompt::ask(&text, cfg.answer_window_sec) {
            Some(ans) if check_task(task, &ans) => return true,
            Some(_) => {
                wrong += 1;
                if wrong >= cfg.max_attempts.max(1) {
                    return false; // exhausted — caller re-picks, stays overdue
                }
            }
            None => {
                // Window elapsed with no answer: unprotected now, keep asking.
                health.challenge_overdue.store(true, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_tolerance() {
        let exact = Answer::Number { value: 29.0, tolerance: 0.0 };
        assert!(check(&exact, "29"));
        assert!(check(&exact, "  29 "));
        assert!(!check(&exact, "30"));
        let approx = Answer::Number { value: 3.14, tolerance: 0.02 };
        assert!(check(&approx, "3.15"));
        assert!(!check(&approx, "3.2"));
        assert!(!check(&approx, "not a number"));
    }

    #[test]
    fn text_variants_and_case() {
        let a = Answer::Text {
            value: Some("four".into()),
            any_of: vec!["4".into()],
            ignore_case: true,
        };
        assert!(check(&a, "Four"));
        assert!(check(&a, "4"));
        assert!(!check(&a, "five"));
        let cs = Answer::Text {
            value: Some("Paris".into()),
            any_of: vec![],
            ignore_case: false,
        };
        assert!(check(&cs, "Paris"));
        assert!(!check(&cs, "paris"));
    }

    #[test]
    fn line_is_exact_trimmed() {
        let a = Answer::Line {
            value: "The quick brown fox".into(),
        };
        assert!(check(&a, "The quick brown fox"));
        assert!(check(&a, "  The quick brown fox  "));
        assert!(!check(&a, "the quick brown fox"));
    }

    #[test]
    fn choice_case_insensitive() {
        let a = Answer::Choice {
            options: vec!["12".into(), "29".into()],
            value: "29".into(),
        };
        assert!(check(&a, "29"));
        assert!(!check(&a, "12"));
    }

    #[test]
    fn canonical_number_is_stable() {
        let n = Answer::Number { value: 0.0, tolerance: 0.0 };
        assert_eq!(canonical(&n, "29").as_deref(), Some("29"));
        assert_eq!(canonical(&n, "29.0").as_deref(), Some("29"));
        assert_eq!(canonical(&n, " 3.140 ").as_deref(), Some("3.14"));
        assert_eq!(canonical(&n, "not a number"), None);
    }

    #[test]
    fn hashed_answer_round_trips() {
        // A task the publish transform would produce: neutered plaintext +
        // answer_hash. The digest must match hash_one over the canonical
        // input (this exact value is cross-checked against Python).
        let task = Task {
            id: "apples".into(),
            category: "math-word".into(),
            grade: 6,
            weight: 1.0,
            prompt: "?".into(),
            hint: None,
            answer: Answer::Number { value: 0.0, tolerance: 0.0 },
            answer_hash: Some(vec![hash_one("apples", "29")]),
        };
        assert!(check_task(&task, "29"));
        assert!(check_task(&task, "29.0")); // canonicalized to the same hash
        assert!(!check_task(&task, "30"));
        // Cross-checked against Python:
        //   sha256(b"apples" + b"\x00" + b"29").hexdigest()
        assert_eq!(
            hash_one("apples", "29"),
            "4c431117eff9f58de3d930a8fdde5f7b82153e79fcda87ea956077cc5548834b",
        );
    }

    #[test]
    fn pick_respects_grade_and_category() {
        let bank = TaskBank {
            version: 1,
            name: None,
            tasks: vec![
                Task {
                    id: "a".into(),
                    category: "math-word".into(),
                    grade: 6,
                    weight: 1.0,
                    prompt: "?".into(),
                    hint: None,
                    answer: Answer::Number { value: 1.0, tolerance: 0.0 },
                    answer_hash: None,
                },
                Task {
                    id: "b".into(),
                    category: "algebra".into(),
                    grade: 9,
                    weight: 1.0,
                    prompt: "?".into(),
                    hint: None,
                    answer: Answer::Number { value: 2.0, tolerance: 0.0 },
                    answer_hash: None,
                },
            ],
        };
        let cfg = ChallengeSettings {
            categories: vec!["math-word".into()],
            max_grade: 6,
            ..Default::default()
        };
        let mut seed = 1;
        for _ in 0..20 {
            let t = pick(&bank, &cfg, &[], &mut seed).unwrap();
            assert_eq!(t.id, "a", "only the grade-6 math-word task is eligible");
        }
    }
}

pub fn spawn(shared: Arc<RwLock<Effective>>, health: Arc<Health>) {
    std::thread::spawn(move || {
        let mut recent: Vec<String> = Vec::new();
        let mut seed: u64 = std::process::id() as u64 ^ 0x9e3779b97f4a7c15;
        let mut next_at: Option<Instant> = None;
        loop {
            std::thread::sleep(TICK);
            let cfg = shared.read().unwrap().challenge.clone();
            if !cfg.enabled {
                health.challenge_overdue.store(false, Ordering::Relaxed);
                next_at = None;
                continue;
            }
            let now = Instant::now();
            let due = *next_at.get_or_insert_with(|| now + interval(&cfg, &mut seed));
            if now < due {
                continue;
            }

            let Some(bank) = load_bank() else {
                // No bank delivered → never a lockout from absence.
                next_at = Some(Instant::now() + interval(&cfg, &mut seed));
                continue;
            };
            let Some(task) = pick(&bank, &cfg, &recent, &mut seed).cloned() else {
                next_at = Some(Instant::now() + interval(&cfg, &mut seed));
                continue;
            };
            tracing::info!("challenge: posing task {} ({})", task.id, task.category);

            if run_challenge(&task, &cfg, &health) {
                health.challenge_overdue.store(false, Ordering::Relaxed);
                tracing::info!("challenge: task {} solved", task.id);
                recent.push(task.id.clone());
                if recent.len() > 8 {
                    recent.remove(0);
                }
                next_at = Some(Instant::now() + interval(&cfg, &mut seed));
            } else {
                // Unsolved: stay overdue (network cut) and re-pick shortly.
                health.challenge_overdue.store(true, Ordering::Relaxed);
                tracing::warn!("challenge: task {} unsolved — staying overdue", task.id);
                next_at = Some(Instant::now() + Duration::from_secs(30));
            }
        }
    });
}
