# Managed mode: fleet-controlled, tamper-resistant betamacs

Target architecture: settings are authored centrally and published through
otactl's signed firmware pipeline; hausmeister's `BetamacsPlugin` delivers
them; betamacs verifies and applies them and accepts nothing else. The
local user cannot change settings, and tampering with the installation is
either repaired automatically or detected and reported.

## Components

```
otactl (control plane)
  └─ signed artifacts: app=betamacs (the .app zip), app=betamacs-config
     (a package.json), both ECDSA-signed manifests + epoch anti-rollback
hausmeister (per-user agent, mTLS device identity)
  └─ BetamacsPlugin: entitlement-gated install/update/config delivery,
     health reporting
betamacs.app (per-user, GUI session)              ── the censor
betamacsd (root LaunchDaemon)                     ── the watchdog
```

## Trust model

- The only settings authority is the otactl code-signing PKI. A build of
  betamacs embeds the otactl **root CA certificate** (pinned at build
  time); presence of the pin activates managed mode.
- Settings arrive as a **signed envelope**: the otactl firmware manifest
  (canonical JSON, ECDSA P-256/SHA-256, signer chained to the pinned
  root, monotonic `epoch`), plus the config artifact whose sha256 the
  manifest binds. betamacs re-verifies the envelope itself — the channel
  that delivered it (hausmeister, a socket, a file) needs no trust.
- Anti-rollback: the last accepted epoch is persisted root-owned by
  `betamacsd`; envelopes with a lower epoch are refused even if validly
  signed.
- Fail closed: in managed mode, if no valid stored config exists at
  startup, betamacs runs built-in strictest defaults (all exposure
  classes trigger, conservative thresholds) — never "off".

## Managed-mode behavior changes in betamacs

- `PUT /api/package` accepts only signed envelopes; plain bodies → 403.
- A new `PUT /api/envelope` (or extended package route) takes
  `{manifest, signature, chain, artifact}`.
- The settings webapp renders read-only (server advertises `managed:
  true` in `/api/status`); the menu bar keeps status, drops the editing
  affordance.
- Config is read from the root-owned managed path when present (see
  layout); the user-writable `~/Library` path is used only unmanaged.

## Filesystem layout (secure install)

| Path | Owner | Purpose |
|---|---|---|
| `/Applications/betamacs.app` | `root:wheel` | app bundle (pinned root cert in Resources) |
| `/Library/LaunchAgents/com.bdstark.betamacs.plist` | `root:wheel` | per-user agent, loads in every GUI session |
| `/Library/LaunchDaemons/com.bdstark.betamacsd.plist` | `root:wheel` | the watchdog daemon |
| `/Library/Application Support/betamacs/` | `root:wheel`, world-readable | managed config (`package.json` + envelope), accepted-epoch state, repair copies |
| `~/Library/Application Support/betamacs/` | user | log, api-token, unmanaged config only |

Only `betamacsd` (root) writes the managed directory. betamacs (user)
reads it and re-verifies the envelope signature at every startup, so a
hypothetical root-path compromise still can't inject unsigned config.

## betamacsd (root watchdog daemon)

Small, dependency-light second binary in this repo. Root LaunchDaemon,
`KeepAlive`; a standard user cannot stop it.

1. **Integrity repair** — periodically verify `/Applications/betamacs.app`
   (codesign + expected team/identifier), the two plists, and ownership/
   modes; repair from root-owned copies under the managed directory.
2. **Envelope custody** — listens on a unix socket
   (`/var/run/betamacsd.sock`); accepts envelopes from anyone (they are
   self-authenticating), verifies signature/chain/epoch independently,
   persists config + epoch root-owned, and notifies the user agent.
3. **Heartbeat** — betamacs reports over the socket every few seconds:
   streams up, config epoch, capture health (TCC state inferred from SCK
   errors). The daemon detects absence, staleness, or a `SIGSTOP`ped
   agent (process alive, heartbeat silent) and logs/repairs/escalates.
4. **Escalation hooks** — see enforcement layers below; consequences are
   configured policy, not hardcoded.

## Enforcement layers (documented plan; 4 is not yet built)

1. **Standard account** (deployment requirement, not code): the
   supervised user must be non-admin. Every guarantee below assumes it.
2. **Root-owned install** — as laid out above. A standard user can
   `launchctl bootout` the agent from their own session or kill it;
   KeepAlive and next-login re-load bring it back, and the heartbeat gap
   is observed.
3. **Watchdog daemon** — betamacsd as described.
4. **Detection with consequences** (documented for later):
   - **Local network quarantine (pf)** — on sustained censor-down
     (heartbeat lost, TCC revoked, integrity unrepairable), betamacsd
     loads a pf anchor blocking all traffic except loopback, DHCP/DNS,
     and the otactl origins (so management and recovery keep working).
     `pfctl` is root-only, so a standard user cannot lift it; the rules
     apply to every interface, so tethering or joining another Wi-Fi
     doesn't escape it. Cleared automatically when health returns.
   - **UniFi backstop (off-device)** — hausmeister reports betamacs
     health to otactl; network policy keys the device's internet access
     to it (nh-parentalcontrol VLAN infrastructure). Covers what local
     enforcement cannot: Safe Mode, booting other media, reinstalls —
     any state where betamacsd itself isn't running.
5. **Platform hardening** (deployment checklist): FileVault + recovery
   password, Guest account disabled. MDM supervision (managed login
   items) is the Apple-blessed extension if ever warranted.

## Screen Time interaction

Screen Time is neither an obstacle nor a defense here:

- App Limits/Downtime meter and block *foreground* app use. betamacs is
  `LSUIElement`, never frontmost, never key — its background agent and
  overlay windows are not attributed usage and are not blocked.
- Screen Time cannot protect betamacs: TCC privacy toggles (Screen
  Recording) remain per-user-editable regardless of Screen Time state.
  Worth one manual test whether macOS's Content & Privacy restrictions
  lock the Privacy pane behind the Screen Time passcode — if so it adds
  friction — but do not rely on it; the TCC hole is handled by
  detection + consequences (layer 4).
- Screen Time's web content filter coexists fine with a pf quarantine
  and with our capture pipeline. Using Screen Time *alongside* betamacs
  for app/time limits is complementary and unaffected.

## Residual risks (accepted)

With layers 1–4: revoking Screen Recording (detected → quarantined),
wiping/reinstalling macOS (device drops off the fleet — visible), or
obtaining the admin password. All remaining attacks are loud, none are
silent.

## Bootstrap (crossing the privilege boundary once)

hausmeister runs as the logged-in user by design, so it cannot create
root-owned state; something must cross the privilege boundary exactly
once, and afterwards betamacsd is the standing root foothold that every
update flows through with no further prompts. Two equivalent paths:

- **GUI (default)** — the app bundle carries its daemon plist
  (`Contents/Library/LaunchDaemons/`); `betamacs install-daemon`
  registers it via `SMAppService`, and the human approves once in
  System Settings → General → Login Items & Extensions (the OS asks for
  admin credentials there). On its first root run, betamacsd finishes
  the layout itself: chowns the bundle to `root:wheel`, installs the
  global LaunchAgent, and migrates the console session off any
  per-user agent. The hausmeister plugin drives this end to end when it
  finds no daemon: it installs the app from the signed pipeline (first
  install needs an admin session, which onboarding is anyway), runs
  `install-daemon`, and surfaces "approve in System Settings" in its
  menu/notification. A hand launch of a managed build does the same.
- **Headless** — `sudo scripts/install-managed.sh` produces the
  identical end state over SSH; it writes a /Library/LaunchDaemons
  plist instead of the SMAppService registration (betamacsd repairs
  whichever registration exists, never both).

So machine onboarding is: enroll hausmeister (existing flow) → grant
the betamacs entitlements → approve the daemon in System Settings →
grant Screen Recording. No terminal required.

## Delivery flow (steady state)

1. Operator edits the package (webapp against a staging betamacs, or
   directly), publishes `betamacs-config` vN via `otactl boot-usb
   upload` — signed, epoch-bumped.
2. BetamacsPlugin's hourly tick sees the new manifest over device mTLS,
   downloads, hands the envelope to betamacsd's socket.
3. betamacsd verifies, persists, notifies; betamacs re-verifies, applies
   live, heartbeats the new epoch; hausmeister reports it upstream.
