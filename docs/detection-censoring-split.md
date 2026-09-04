# Detection / censoring split (deferred design)

**Status:** deferred (2026-09-04). Not built. The single-switch model below is
what ships today; this note captures the intended split so it isn't lost.

## The model the user wants

Two independent concepts, today collapsed into one flag:

- **Detection** — screen capture + running the model. This is what lights the
  macOS screen-recording indicator. Consumers: the censor overlay, plus the
  exposure budget and coverage-escalation metrics (which tally what detection
  flags).
- **Censoring** — the visual overlay (the boxes) drawn over detected content.

Rules:

1. **Censoring ⇒ detection.** You can't censor without detecting; enabling
   censoring must enable detection.
2. **Detection off ⇒ censoring off, capture off, icon off.** Detection is the
   master switch for the whole apparatus.
3. **Detection on ⇒ icon on** — capture runs whenever detection is on, whether
   or not the overlay is drawn.
4. **Detection on + censoring off is allowed** — "monitor mode": detect (keep
   exposure/coverage tracking) but draw no boxes.
5. **Config-app convenience:** turning censoring off turns detection off *by
   default* (so the icon goes off in the common "turn it off" case), but
   detection can be independently re-enabled for monitor mode.

## What ships today (the single-switch model)

There is one flag, `detection.enabled` (= detect **and** censor). As of
**betamacs 0.2.17** it is the true master switch: when false, the pipeline
tears the capture streams down (screen-recording indicator off) and rebuilds
them when re-enabled — see `src/pipeline.rs` (the master-switch block) and
`SckCapturer::stop_streams` in `src/capture_sck.rs`.

So rules 1–3 already hold trivially (one flag), and "turn detection off → no
censoring, no capture, no icon" works now. The only rule the single-switch
model can't express is **rule 4 (monitor mode)** — detection on without
censoring.

## Design to add the split (when wanted)

- **Runtime (`src/pipeline.rs`, `src/settings.rs`):** add `censor.enabled: bool`
  (default true). Run detection whenever `detection.enabled`; draw the overlay
  (`overlay.set_regions(...)`) only when `detection.enabled && censor.enabled`.
  With censoring off but detection on, keep sampling/detecting and keep the
  exposure/coverage trackers fed — just skip the box render and hold-list. No
  boxes are drawn, but the capture streams stay up (icon on), matching rule 3.
- **Enforcement:** at runtime, `censor.enabled` with `detection.enabled==false`
  is a no-op (no detection ⇒ no boxes). The `censor ⇒ detection` invariant is
  enforced in the config app, not relied on at runtime.
- **Config app (`webapp/src/modules/*`):** surface a censoring toggle distinct
  from detection, and wire the rule-5 relationships: enabling censoring sets
  detection on; disabling detection sets censoring off; disabling censoring sets
  detection off by default (detection re-enableable on its own for monitor
  mode). Mirror the two flags in `schema.ts`.

## Why deferred

The immediate need — "when it's off, don't keep recording the screen" — is met
by 0.2.17 (detection off ⇒ capture off) plus setting `detection.enabled=false`
on any device that shouldn't censor. Monitor mode (detect without censoring) is
a "could", not a "must", so the split waits until there's a concrete use for
detecting-without-censoring (e.g. exposure alerts on an older kid's Mac with no
visual censoring).
