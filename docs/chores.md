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

## Server-side chores (the kids web app)

The in-person PIN is the offline fallback. The normal path is the kids web
app on typeserver (`/kids/`, typeserver `docs/kids.md`): a kid signs in with
a passkey on any device and taps "I did it"; a parent signs in from a phone
and taps Approve. The server is then the source of truth for definitions,
claims and approvals, and the Mac becomes a consumer of signed approvals.

**Delivery.** On every approval the server author-signs and publishes ONE
`betamacs-grants` artifact (format `betamacs-grants-json`) carrying every
kid's state:

```jsonc
{ "version": 1, "generatedAt": "…",
  "kids": { "anna": { "name": "Anna", "hosts": ["bdsmbpm101"],
                      "chores":   [ /* same shape as the bank's chores */ ],
                      "verified": [ { "id": "piano", "period": "2026-09-07", "minutes": 20, "at": 1788755843 } ] } } }
```

hausmeister fetches it like the bank (own `ext:betamacs-grants` grant) and
hands it to betamacsd as a `grants` envelope. The daemon verifies it (own
`epoch-grants` / `authored-grants` high-waters), writes it root-only as
`grants.json`, and `EarnedGate` picks **its own kid by hostname** (scutil's
LocalHostName / ComputerName / HostName, case-insensitive — a standard user
cannot change them). One artifact for all kids avoids per-kid otactl apps
or channels, which would each need their own publisher cert scope.

**Merging.** Definitions from the grants file replace the bank's for that
kid (`status.chores.source` = `server`; the bank is the fallback when the
file names no kid on this Mac, or is absent). Each `verified` entry that is
for the period this Mac is currently in (or `once`) and not already in the
ledger is recorded through the same `record_verified` path as a PIN
verification — same chore cap, same earned-time caps — and consumes any
pending local claim. Entries for other periods are ignored, so a
re-delivered artifact after midnight can never credit yesterday twice, and
a re-delivered identical artifact credits nothing (idempotent by
`(id, period)`).

**Agent.** With `chores.kidsUrl` set in the config, the menu bar's
"Chores…" opens the web app; empty, it runs the local dialogs (PIN
fallback). The HUD and menu lines are unchanged.

**Latency.** Approval → next hausmeister poll → betamacsd → gate. Minutes.

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

- Schema on both sides (`settings.rs`, `webapp/src/schema.ts`; `chores`
  module incl. `kidsUrl`), `publish.sh tasks` hashing/validation, examples.
- **Daemon**: PIN split (`install_bank`), ledger `chores` section, the four
  chore socket ops with the PIN check/lockout in the daemon, bonus credit
  through the shared cap path, required-chore hold in the earned-time gate
  (`QReason::Chores`), `status.chores`; **grants**: `apply_grants_envelope`
  (`grants` socket message, root-only `grants.json`, `grantsEpoch` in
  status), kid selection by hostname, definitions from the server with the
  bank as fallback, idempotent period-checked approval merge. 12 daemon
  tests plus a socket smoke run.
- **Agent**: chores policy relay in the earn report; "Chores…" opens
  `kidsUrl` or the local list + PIN dialogs; menu/HUD `Chores:` lines.
- **hausmeister plugin**: fetches/delivers `betamacs-grants` when the Mac
  holds `ext:betamacs-grants` (row in Check for Updates too).
- **typeserver**: `/kids/` page (kid and parent views, chore editor),
  `/api/kids/*`, restricted kid sessions, grants publishing via the config
  app's sign+upload core. See typeserver `docs/kids.md`.

**Not done (in order):**

1. Rollout: publisher cert `betamacs-grants`, `ext:betamacs-grants` per kid
   Mac, betamacs + hausmeister releases, typeserver deploy, config with
   `chores.enabled` + `kidsUrl` — see typeserver `docs/kids.md` "Rollout".
2. Live click-through on a kid Mac (and the local PIN dialogs, which have
   only been syntax-checked).
3. Config-app `chores` editor tab (today the module is edited as JSON).
4. Device → server state (balance, gate) for the kid page — optional.

## Open questions

- Should a `required` chore that is verified late (after the child has
  already been gated for hours) refund anything? Current answer: no — the
  gate is the whole point.
- Multiple parents / multiple PINs? One PIN per bank is enough for now;
  the bank is per child, so each child's PIN can differ.
- Whether `bonus` credit should count against `earnedTime.dailyEarnCapMin`
  (current: yes) or sit outside it.
