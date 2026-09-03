# Earned-time gate

A gate on internet access that a child unlocks by doing an approved
activity — for example, on a weekday the internet is locked until they have
spent an hour actively using an allowlisted educational site or app (Khan
Academy Kids, etc.). Time is **banked**: surplus earned today can be spent
later.

This is a distinct primitive from the two attention/exposure features:

| Feature | Kind | Trigger | Framing |
|---|---|---|---|
| Activity challenge | liveness check | random cadence | neutral ("prove you're here") |
| Exposure budget | punishment | too much flagged content | corrective |
| **Earned time** | **gate / precondition** | **scheduled (e.g. weekdays)** | **positive ("earn it")** |

It reuses the same enforcement backbone (betamacsd's pf quarantine) but adds
one thing betamacs does not have yet: a **time ledger**, and one new
subsystem: a **local activity monitor** that credits earned minutes from
what it can directly observe on screen — never from an external API.

## The three parts

### A. Accounting & policy

A new config module `EarnedTime` in the signed `betamacs-config` package,
layerable like the others:

```jsonc
{
  "enabled": true,
  "schedule": [                    // when the gate is active
    { "days": ["mon","tue","wed","thu","fri"], "from": "07:00", "to": "20:00" }
  ],
  "sources": [                     // what earns credit, and how fast
    { "name": "Khan Academy Kids",
      "match": { "bundleId": "org.khanacademy.kids" },
      "earnRatio": 1.0 },          // 1 active minute = 1 earned minute
    { "name": "Khan Academy (web)",
      "match": { "browserHostSuffix": "khanacademy.org" },
      "earnRatio": 1.0 }
  ],
  "spendRatio": 1.0,               // 1 earned minute = 1 minute of gated internet
  "dailyEarnCapMin": 120,          // most that can be earned per day
  "maxBankMin": 240,               // ceiling on carried-over balance
  "minSessionMin": 5,              // ignore sub-5-minute blips
  "idleTimeoutSec": 60             // pause crediting after this much no input
}
```

### B. Enforcement (reuses the quarantine)

The authoritative **balance ledger is owned by betamacsd** (root, persisted
in the managed dir alongside the epoch files, tamper-resistant — a child
cannot edit their own minutes). Each tick:

- Outside a scheduled gate window → no gating (healthy).
- Inside a gate window with **balance > 0** → allowed; the daemon decrements
  the balance at `spendRatio` while the internet is used.
- Inside a gate window with **balance ≤ 0** → quarantine, **except** an
  "earning mode" pf ruleset that allows *only* the source allowlist (plus
  the usual loopback/DNS/DHCP/SSH/management hosts). So a child with no
  balance can still reach Khan to earn, but nothing else.

This is a small variant of the existing quarantine: the allow-list is the
educational sources instead of just management hosts, and the release
condition is "balance > 0" instead of "censor healthy."

### C. Verification — local observation only

The hard question is "did they actually do the hour?" We answer it with
what betamacs already sees, not with an external service:

- **Frontmost app**: `NSWorkspace.frontmostApplication` → bundle id.
- **Active browser URL** (for web sources): AppleScript to the frontmost
  browser (`tell application "Safari" to get URL of current tab`, and the
  Chrome/Arc equivalents) → host.
- **Idle**: `CGEventSource.secondsSinceLastEventType(...combined...)` →
  pause crediting after `idleTimeoutSec` of no keyboard/mouse input.

Credit accrues only while (frontmost app or browser host matches a source)
**and** the user is not idle. That verifies *active time on the approved
destination* — which is exactly the "interact for an hour" requirement. It
can't verify they answered questions correctly, and it deliberately doesn't
try; it can't be gamed by leaving a background tab open, which is what
matters. The agent reports earned deltas to betamacsd, which commits them to
the root-owned ledger (the agent proposes, the daemon disposes — same trust
split as the rest of managed mode).

**External APIs are an optional enhancer, never the foundation.** Khan
Academy Kids has no public API; parent-dashboard scraping is fragile and
ToS-risky. Where a service *does* expose a real parent/activity API, it can
be layered on later to cross-check or replace local minutes for that source
— but the feature must work with observation alone.

## Banking & fallback

- Surplus persists in the ledger up to `maxBankMin` (optionally with slow
  decay so it can't be hoarded indefinitely — open question).
- If the child won't do the approved activity, the policy can offer an
  **assigned challenge** (reuse the activity-challenge feature) as an
  alternate earn path, or simply leave the internet gated.

## Delivery & trust

- Policy ships in `betamacs-config` (author-signed, layerable) — no new
  artifact kind needed; the source allowlist and schedule are just config.
- The ledger is daemon-owned and root-only, like the epoch high-water.
- The activity monitor runs in the agent (it needs the GUI session for
  frontmost-app / AppleScript / idle APIs) and only *proposes* deltas; the
  daemon validates and commits, so a tampered agent can't mint minutes
  (it would instead go silent → normal quarantine).

## Status (2026-09-03)

Built: config schema (`EarnedTimeSettings`), the agent activity monitor
(`src/earned.rs` — observes frontmost app/host + idle, resolves the schedule
via `/bin/date`, reports earned seconds + policy each tick over the daemon
socket), and the **daemon side** (`betamacsd`): the root-owned ledger
(`earned-ledger.json`) with daily-cap/bank-ceiling and date-rollover reset,
balance spend while online in a window, and the two-mode quarantine (Full vs
Earning-mode, the latter allowing the earn-source hosts). Full-block reasons
take precedence; earning-mode engages when the balance is depleted in a
window and releases when it goes positive. Ledger logic is unit-tested and
earning-mode engage/spend verified in a dry-run.

Known limitations / decisions:
- **Earning-mode allowlist covers web sources (`browserHostSuffix`) only** —
  pf filters by resolved IP, so an app-only source (`bundleId`) can't be
  selectively allowed while blocking everything else. App-based earning
  relies on the app's offline content, or on earning during a non-gated
  window. Documented; revisit if an app source needs online access to earn.
- **Schedule/policy is resolved by the agent**, not the daemon (keeps the
  daemon from parsing the full config). The daemon owns the *balance* (the
  un-fakeable part) and caps agent-reported credit; a tampered agent's worst
  case is bounded by the daily cap, and a killed agent trips the normal
  full-block watchdog. Acceptable for the threat model (tamper-evidence, not
  tamper-proof). Move gate resolution into the daemon if that hardens.

## Open questions

- Bank decay policy (hoarding vs. fairness).
- Spending granularity: decrement continuously while online, or in blocks?
- Multiple children / fast-user-switching accounting (per-console-user
  ledgers keyed like the heartbeat's session check).
- Browser-URL access without prompting for Automation permission on every
  browser (one-time TCC grant per browser).
