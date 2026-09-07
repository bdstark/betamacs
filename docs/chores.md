# Chores

An **external task**: something the child does away from the screen (make
the bed, empty the dishwasher, practice piano) that a parent verifies. It
plugs into the earned-time gate (docs/earned-time.md) as a third kind of
earn source — one whose verifier is a human rather than the activity
monitor — so it reuses the root-owned ledger, the daily cap, the bank
ceiling, and the pf/DNS enforcement. Nothing new is enforced; only what
*unlocks* changes.

## Two kinds of chore, and why

The parenting literature and the commercial apps that do this (ScreenTime
Labs, Kidslox, FamiSafe, Family Link's bonus time) have converged on two
cautions that shape the design:

- **Screen time as the currency raises its status.** A per-chore price list
  turns chores into a negotiation and lets a child who does not want screen
  time that day simply skip them.
- **"First/then" beats "pay per chore".** Baseline chores are prerequisites,
  not purchases: no internet until the bed is made. Extra effort can earn a
  bonus on top of that.

So a chore is one of two kinds:

| kind | effect when verified | effect while unverified |
|---|---|---|
| `required` | nothing to credit; the gate is simply released | **holds the earned-time gate closed** on its due day, balance or not |
| `bonus` | credits `minutes` into the ledger (subject to caps) | nothing — the balance is spent as usual |

Both kinds are defined per child, claimed by the child, and verified by
the parent. Credit is **never** granted on a claim, only on verification.

## Where things live

| what | where | why |
|---|---|---|
| chore definitions + parent PIN | the per-kid **task bank** (`betamacs-tasks`) | the bank is already the per-device marker of a managed child, delivered by the `ext:betamacs-tasks` grant; chores are per kid, not fleet-wide |
| policy (caps, claim expiry, PIN lockout) | `chores` module in **betamacs-config** | fleet-wide and layerable like every other module; disabled by default |
| claims, verifications, PIN attempt counter | the daemon's **ledger** (`earned-ledger.json`, root) | tamper-resistant; the child cannot edit their own claims |
| PIN hash | root-only `chore-pin` file in the managed dir | see "The PIN" below — it must NOT sit in the world-readable `tasks.json` |

### Task bank additions

```jsonc
{
  "version": 2,
  "name": "grade-6-starter",
  "tasks": [ /* challenge tasks, unchanged */ ],
  "chorePin": "482913",             // AUTHORED plaintext; publish.sh hashes + removes it
  "chores": [
    { "id": "bed",        "name": "Make your bed",        "kind": "required",
      "repeat": "daily",  "minutes": 0 },
    { "id": "dishwasher", "name": "Empty the dishwasher", "kind": "required",
      "repeat": "daily",  "days": ["mon","tue","wed","thu","fri"], "minutes": 0 },
    { "id": "trash",      "name": "Take out the trash",   "kind": "bonus",
      "repeat": "weekly", "minutes": 30 },
    { "id": "piano",      "name": "Practice piano 20 min", "kind": "bonus",
      "repeat": "daily",  "minutes": 20 },
    { "id": "garage",     "name": "Sweep the garage",     "kind": "bonus",
      "repeat": "once",   "minutes": 60 }
  ]
}
```

- `repeat`: `daily` (due each listed day, resets at local date rollover —
  the ledger already tracks that), `weekly` (claimable once per ISO week,
  resets Monday), `once` (claimable one time ever; the ledger keeps the
  completion).
- `days`: lowercase three-letter names like `Schedule.days`; empty = every
  day. Only meaningful for `daily`.
- `minutes`: bonus credit. Ignored for `required` (authoring a non-zero
  value there is allowed but has no effect — a required chore is a gate,
  not a purchase).

### Config module

```jsonc
"chores": {
  "enabled": true,
  "bonusDailyCapMin": 60,     // chore credit per day, on top of earnedTime.dailyEarnCapMin
  "claimTtlMin": 120,         // an unverified claim expires after this long
  "requiredHoldFrom": "00:00",// required chores start holding the gate at this local time
  "verifyMaxAttempts": 5,     // wrong PINs before a lockout
  "verifyLockoutSec": 600     // how long PIN entry is refused after that
}
```

`bonusDailyCapMin` is a second, chore-specific cap; the ledger's existing
`dailyEarnCapMin` and `maxBankMin` still apply on top, so chores can never
mint more than the earned-time policy allows in total.

## The flow

1. **Claim.** The child picks a chore from the menu bar ("Chores…"). The
   agent sends `{"type": "chore-claim", "id": "bed"}` over the daemon socket. The
   daemon records `{id, period, claimedAt}` in the ledger — once per chore
   per period; a second claim in the same period is a no-op. A claim
   older than `claimTtlMin` is dropped so a stale "done" cannot be verified
   a day later.
2. **Verify.** A dialog says "Waiting for a parent — enter the chore PIN".
   The parent types it; the agent relays `{"type": "chore-verify", "id":
   "bed", "pin": "…"}`. The **daemon** checks the PIN against the root-only hash,
   applying the attempt counter and lockout, and on success moves the claim
   to `verified` and (for `bonus`) credits `minutes` into the balance
   through the same capping path as observed earn credit.
3. **Reject.** Cancel in the dialog clears the claim. No penalty — the
   design goal is a low-friction "not yet", not a punishment.
4. **Gate.** On each earned-time tick the daemon computes
   `required_outstanding = required chores due today (by `days`) that are
   not verified for today's period`, once local time is past
   `requiredHoldFrom`. If non-empty, the gate stays in earning mode even
   with a positive balance. The HUD's lockdown reason names the chores.

`required` chores affect only the earned-time gate. They never trip the
full quarantine, and on a device without a task bank (no chores) they do
not exist — so the parent's Mac stays open under a fleet-wide config, as
with every other kid-only feature.

## The PIN

The verifier has to be something the child does not have and the parent
does not need a server for. A short numeric PIN typed at the child's Mac is
the right phase-1 answer: it works offline, it needs no new infrastructure,
and it matches how verification actually happens (the parent looks at the
bed, then types).

Two details make it hold up:

- **The hash lives only in the daemon.** `tasks.json` is world-readable by
  design (the agent needs the tasks). A salted SHA-256 of a 4–6 digit PIN
  in a readable file is brute-forced in milliseconds, so `apply_tasks_envelope`
  splits `chorePinHash` out of the delivered bank into a `0600` root-owned
  `chore-pin` file and writes `tasks.json` without it. The agent never
  sees the hash; it only relays what was typed.
- **The daemon rate-limits.** `verifyMaxAttempts` wrong entries lock PIN
  entry for `verifyLockoutSec`, and the counter is in the ledger, so
  restarting the agent does not reset it. That turns an online guess of a
  6-digit PIN into hours per thousand attempts.

Rotating the PIN is republishing the bank. Shoulder-surfing is the
residual risk and is bounded by the caps: a child who learns the PIN can
mint at most `bonusDailyCapMin` a day, and only until the parent notices
and republishes.

`publish.sh tasks` handles the authoring side exactly as it does answers:
it reads the plaintext `chorePin`, emits `chorePinHash` as
`sha256$<hex salt>$<hex digest>` with a fresh random salt, and deletes
the plaintext before signing.

## Phase 2: remote approval

The in-person PIN has one real cost: if no parent is home, the child waits.
The fix is asynchronous approval — pending claims flow up in the
hausmeister heartbeat, the parent approves from the config app or a phone,
and a signed grant comes back down and is applied by the daemon like any
other envelope. That needs device-to-server claim reporting and a small
signed grant artifact, so it is deliberately after phase 1: ship the PIN
path, see how much the in-person requirement chafes, then decide.

The schema is already shaped for it — a grant is just
`{choreId, period, minutes, issuedAt}` applied to the same ledger entry the
PIN path writes.

## Wire protocol (daemon socket)

All chore state is daemon-owned; the agent only relays. One request per
connection, one JSON line back.

| request | reply |
|---|---|
| `{"type":"chores"}` | `{ok, enabled, pinSet, outstanding:[id], pending:[id], verified:[id], pinLockedSecs, chores:[{id,name,kind,repeat,minutes,due}]}` |
| `{"type":"chore-claim","id"}` | `{ok:true}` or `{ok:false, error}` (`unknown chore`, `already verified`, `chores are not enabled`) |
| `{"type":"chore-reject","id"}` | `{ok:true}` — withdraws a claim |
| `{"type":"chore-verify","id","pin"}` | `{ok:true, result:"verified", minutes}` / `{ok:false, result:"wrong-pin", attemptsLeft}` / `{ok:false, result:"locked", secs}` / `{ok:false, result:"refused", error}` |

The agent's `earn` report gained a `chores` object carrying the resolved
policy module (`enabled`, `bonusDailyCapMin`, `claimTtlMin`,
`requiredHoldFrom`, `verifyMaxAttempts`, `verifyLockoutSec`); the `status`
reply gained `chores: {enabled, pinSet, outstanding, pending, verified,
pinLockedSecs}`, and `quarantine.reason` can now be `chores`
(docs/lockdown-reasons.md).

The ledger (`earned-ledger.json`) gained a `chores` section: `claims`
(`id`, `period`, `claimed_at`), `verified` (`id`, `period`, `at`,
`minutes`), `bonus_today_min`, `pin_failures`, `pin_locked_until`. Periods
are the local date for daily chores, the ISO week for weekly, and the
literal `once`; past-period entries are pruned at date rollover (`once`
entries are kept forever).

## Status (2026-09-06)

**Built and unit-tested:**

- Schema on both sides (`settings.rs`, `webapp/src/schema.ts`),
  `publish.sh tasks` hashing/validation, examples.
- **Daemon (`betamacsd`)**: `install_bank` splits `chorePinHash` into the
  root-only `chore-pin` (0600) on every bank delivery and strips it from
  `tasks.json` (a bank without a PIN removes a stale one); ledger `chores`
  section; `chores` / `chore-claim` / `chore-reject` / `chore-verify` socket
  ops with the PIN check, attempt counter and lockout in the daemon; bonus
  credit through the same cap path as observed earn credit plus the chore
  cap; required-chore hold in the earned-time gate (earning mode, no
  balance spent while held, nothing due before `requiredHoldFrom`);
  `QReason::Chores` and the `status` summary. Eight daemon tests plus a
  socket smoke run in prefix mode.
- Agent relay: `earned.rs` sends the `chores` policy in the earn report;
  the HUD maps the `chores` lockdown reason to plain language.

**Not built yet (in order):**

1. Agent UI: "Chores…" menu item listing the bank's chores (from the
   `chores` reply), claim + PIN dialogs (`prompt.rs`) that call
   `chore-claim` / `chore-verify` / `chore-reject`, HUD lines for
   outstanding/pending chores.
2. Config app: `chores` editor tab; chore list + PIN in the tasks editor.
3. Ship: betamacs release, then a bank with chores + a PIN, then a config
   enabling `chores`.
4. Phase 2 remote approval.

## Open questions

- Should a `required` chore that is verified late (after the child has
  already been gated for hours) refund anything? Current answer: no — the
  gate is the whole point.
- Multiple parents / multiple PINs? One PIN per bank is enough for now;
  the bank is per child, so each child's PIN can differ.
- Whether `bonus` credit should count against `earnedTime.dailyEarnCapMin`
  (current: yes) or sit outside it.
