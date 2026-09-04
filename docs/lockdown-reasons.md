# Lockdown reasons — why the internet is blocked

betamacsd is the single authority on whether the network is fully quarantined
and *why*. It publishes that decision in two places so a locked device is never
a mystery (the bdsmbpm101 case: internet off, HUD reading "open"):

- the daemon `status` reply's `quarantine` object, and
- the always-on-top HUD's `Lockdown:` line (src/statusframe.rs), which renders
  the object in plain language.

## The `quarantine` object

The `status` reply gains (alongside the legacy fields, which are unchanged):

```json
"quarantine": { "active": true, "reason": "exposure", "secsLeft": 840 }
```

- `active` — is a FULL block in force right now (earning-mode gate included).
- `reason` — one of the stable strings below, or `"none"` when open.
- `secsLeft` — seconds remaining on a *timed* penalty; `0` for every non-timed
  reason (gates and health/tamper blocks have no countdown).

The reason is decided by the watchdog loop (`want_full` + the earned-time gate)
and cached; `secsLeft` is recomputed live in the status handler from the timed
deadline so the HUD countdown ticks smoothly even though the watchdog only runs
every ~15s. This means a health/tamper reason can lag reality by up to one
watchdog tick, but timed countdowns do not.

## The reasons

| `reason`            | HUD phrase                              | Countdown? | Kind |
|---------------------|-----------------------------------------|------------|------|
| `exposure`          | too many exposures                      | yes (`secsLeft`) | timed penalty |
| `focus`             | too much scrolling                      | yes (`secsLeft`) | timed penalty |
| `challenge`         | unanswered challenge                    | no | health block (clears when answered) |
| `earned-gate`       | earn time to unlock (allowlist only)    | no | gate (spend/earn balance) |
| `clock-tamper`      | clock tampered                          | no | tamper block (clears when clock ok) |
| `capture-unhealthy` | screen recording off                    | no | health block (clears when capture ok) |
| `heartbeat-stale`   | censor not reporting                    | no | health block (clears when heartbeats resume) |
| `session/health`    | censor not reporting                    | no | health block (agent never checked in) |
| `none`              | open                                    | — | not blocked |

Notes:
- `exposure` vs `focus`: both fold into the daemon's single
  `exposure_penalty_until` deadline. `AgentState::penalty_source` remembers which
  agent signal (`exposureOverBudget` vs `focusOverLimit`) set the standing
  deadline, so the reason is accurate rather than a generic "timed penalty".
- `earned-gate` is a legitimate no-countdown block: an active earn gate with a
  depleted balance. Only the earn-source allowlist is reachable; the block lifts
  when the child earns/has balance again.
- Health/tamper reasons are debounced by the quarantine grace window
  (`BETAMACSD_QUARANTINE_GRACE_SECS`, default 180s) before `active` flips true;
  timed penalties bypass grace and engage immediately.
- Precedence in `want_full`: a live timed penalty first (exposure/focus), then —
  after grace — `clock-tamper` > `challenge` > `capture-unhealthy` >
  `heartbeat-stale` > `session/health`. The earned-time gate is folded in last
  (only when no full block applies).

## Forcing each state on a test device

Run the daemon with quarantine in dry-run so you see the decision in the log
without actually loading pf, and a short grace so health reasons trip fast:

```sh
sudo BETAMACSD_QUARANTINE_DRYRUN=1 BETAMACSD_QUARANTINE_GRACE_SECS=10 \
  /Applications/betamacs.app/Contents/MacOS/betamacsd
```

Watch the decision and confirm the HUD line / `status`:

```sh
tail -f "/Library/Application Support/betamacs/betamacsd.log"
printf '{"type":"status"}\n' | nc -U /var/run/betamacsd.sock   # inspect quarantine{}
```

- **earned-gate** — provision the device as a kid device (a root-owned
  `tasks.json` bank must exist), have the agent report an active gate
  (`gateActive:true`) with a depleted balance (earned-ledger balance 0). With
  the gate active and balance 0, `tick` returns the earn allowlist → reason
  `earned-gate`, `secsLeft` 0. Fastest repro: deliver a task bank, let the gate
  window open, and spend the banked balance to 0.

- **capture-unhealthy** — revoke Screen Recording for betamacs in
  System Settings › Privacy & Security › Screen Recording while logged in. The
  agent's next heartbeat carries `captureOk:false`; after grace the reason is
  `capture-unhealthy` ("screen recording off"). Re-grant to clear.

- **heartbeat-stale** — suspend the agent so heartbeats stop but the process
  lives: `sudo killall -STOP betamacs` (the watchdog will SIGCONT it, so to hold
  it stale, instead stop the LaunchAgent from reporting, or block the socket).
  Simplest: `sudo launchctl kill -STOP gui/$(stat -f %u /dev/console)/com.bdstark.betamacs`
  and observe heartbeat age climb past 60s → reason `heartbeat-stale`.

- **session/health** — boot into a console session with the agent never having
  reported (e.g. remove/disable the LaunchAgent so no heartbeat is ever sent)
  while a user is logged in and policy has censoring enabled → reason
  `session/health` ("censor not reporting").

- **exposure** — drive the exposure metric over the block threshold so the
  agent sends `exposureOverBudget:true` with `exposurePenaltySec:N` (view a lot
  of flagged content within the block window). Reason `exposure`, `secsLeft`
  counting down from N.

- **focus** — trip the same-tab focus limit (long continuous scrolling on one
  tab past the configured limit) so the agent sends `focusOverLimit:true` with
  `focusPenaltySec:N`. Reason `focus` ("too much scrolling"), `secsLeft` from N.
  If both exposure and focus fire, whichever sets the longer standing deadline
  wins the reported reason.

- **challenge** — let an activity challenge go unanswered past its window so the
  agent reports `challengeOverdue:true` → reason `challenge`. Answer it to clear.

- **clock-tamper** — change the wall clock under a running instance (disable
  network time, then set the date forward/back). The agent detects the jump vs
  the monotonic clock and reports `clockTamper:true` → reason `clock-tamper`.
  (A wrong clock at *boot* is resynced, not punished — that is not a tamper.)
