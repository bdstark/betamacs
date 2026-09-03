# betamacs

Real-time screen censor: monitors all connected displays, detects nudity
locally with the [NudeNet](https://github.com/notAI-tech/NudeNet) v3 ONNX
detector, and draws black boxes over flagged regions. Everything runs
on-device; no frame ever leaves the machine.

## Pipeline

```
capture (native ScreenCaptureKit stream per display, change-driven)
   → detect (NudeNet ONNX via ort, letterbox + YOLOv8 decode + NMS)
   → censor (pool of black always-on-top click-through windows)
```

- **capture_sck.rs** — one persistent `SCStream` per display. Frames are
  delivered only when the display's content changes, so fully static
  screens cost zero CPU. In `--censor-captures` mode the stream filter
  excludes this app's windows from *our* capture only (the detector sees
  beneath the boxes; other apps' captures see them). Captures at logical
  (point) resolution — the detector downscales anyway.
- **capture.rs** — the original xcap polling capture, still used by the
  `probe` and `demo` modes.
- **detect.rs** — ONNX Runtime session per process; letterboxes the frame
  to the model's square input, decodes the `[1, 4+18, anchors]` output,
  maps boxes back to source-pixel coordinates.
- **overlay.rs** — a pool of small plain-black borderless windows, one per
  censor region, positioned in global logical points. Always-on-top,
  click-through, never focused, visible on all Spaces (incl. fullscreen
  apps), and content-protected (`NSWindow.sharingType = .none`) so the
  detector keeps seeing the raw content underneath. No pixel drawing at
  all — the window background is the censor box.
- **pipeline.rs** — the continuous loop, event-driven off the frame
  channel. Censor-box lifetime rules (shaped by change-driven capture): a
  box is refreshed whenever a frame shows content; removed only when a
  *new frame* shows it gone and `hold_ms` has passed; if no frames arrive
  the screen hasn't changed, so boxes stay — a static image stays covered
  indefinitely. Identical region sets are deduped before touching windows,
  otherwise overlay updates would re-trigger SCK change frames forever.
- **config.rs** — thresholds, FPS, censored class list.

- **detect.rs (tiling)** — each screen is scanned as the full frame plus a
  2x2 grid of ~20%-overlapping tiles (5 inferences/screen), so thumbnails
  and quarter-screen windows survive the downscale to model input size.
  Global NMS merges duplicate boxes across tiles.

## Settings (modules / layers / packages)

Configuration is a JSON **package**: a set of **named configurations**
(each a *partial*, per-module settings object), an ordered **layer** stack
of them, and explicit overrides. Effective settings = module defaults <-
layers in order <- overrides, merged field by field (`triggers` merges per
class). `src/settings.rs` and `webapp/src/schema.ts` implement the same
contract and resolution.

Modules: **detection** (model, confidence/IoU/minimum sliders, capture
rate, tiling, hold time, grouped trigger switches) and **censor** (fill /
border colors, x/y size percentages, trigger-label + random-text overlay).
Boxes render the trigger class and a randomly picked text via NSTextField
subviews; the text pick is a stable hash of the box geometry so static
boxes don't reshuffle their text.

The settings site is TypeScript + Lit web components (`webapp/`), built
with Vite, and works identically self-hosted by the app (`/`) or hosted
externally and pushed to the local app.

```bash
cd webapp && npm install && npm run build   # app serves webapp/dist
```

API (localhost only, bearer token in `config/api-token`, printed at
startup; CORS open for the externally-hosted UI):

```
GET  /api/package    # stored package
PUT  /api/package    # validate, persist to config/package.json, apply live
GET  /api/status     # app + resolved effective settings
```

Thresholds, triggers, scales, colors, and hold time apply live; a model
change hot-swaps the detector; capture fps / tile layout need a restart.

## Setup

```bash
scripts/fetch-model.sh 320n   # or 640m
cargo run --release           # continuous censoring, 320n model
cargo run --release -- 640m   # more accurate model, slower
cargo run --release -- probe  # one-shot capture + detection with timings
cargo run --release -- demo   # 8s black box + capture-exclusion self-check
```

`--censor-captures` makes the boxes visible in screenshots / screen shares
too, flicker-free: once the first box appears, the app registers in
shareable content and every stream's filter is updated to exclude this
app's windows, so the detector keeps seeing beneath the boxes while
everyone else's captures show them. Debug: `BETAMACS_NO_PROTECT=1`
forces boxes into captures so the demo can verify they render.

Requires macOS Screen Recording permission for the terminal/app running it.

## .app + LaunchAgent (survives reboot, restarts on crash)

```bash
scripts/make-app.sh              # -> dist/betamacs.app (signed)
ditto dist/betamacs.app /Applications/betamacs.app
launchctl kickstart -k gui/$UID/com.bdstark.betamacs   # restart after reinstall
```

`~/Library/LaunchAgents/com.bdstark.betamacs.plist` runs the bundle's
binary with `RunAtLoad` + `KeepAlive`, so it starts at login and launchd
relaunches it on any exit (the panic hook's loud `exit(101)` counts on
this). First-time load: `launchctl bootstrap gui/$UID <plist>`; stop for
good with `launchctl bootout gui/$UID/com.bdstark.betamacs`. Early
launch failures (before the logger starts) land in the data dir's
`launchd.log`.

When the executable finds itself inside a bundle it pins the working
directory to `~/Library/Application Support/betamacs` (config, api-token,
`betamacs.log` — rotated at 5 MB), symlinks `models/` into the bundle's
Resources, and serves the bundled webapp; `cargo run` from the checkout is
unchanged. Signing uses the first Developer ID Application identity (or
`BETAMACS_SIGN_IDENTITY`), so the Screen Recording grant — enable
"betamacs" once under System Settings → Privacy & Security → Screen &
System Audio Recording — survives rebuilds. Login items launch once and
are not restarted on crash; use a LaunchAgent with `KeepAlive` instead if
that matters.
Note: NudeNet's GitHub release downloads are login-gated; the fetch script
works around this via the API asset endpoint (see script comments).

## Benchmarks (M-series MacBook + 2x 4K displays, release build)

Per-screen tiled detection (5 inferences), CoreML EP with CPU fallback:

| model | per screen | full 3-screen sweep |
|-------|-----------|---------------------|
| 320n  | ~110–165 ms | ~0.4 s (~2.5 fps) |
| 640m  | ~720–910 ms | ~2.3 s (~0.4 fps) |

Capture of all 3 screens: ~255 ms. CoreML currently splits the graph into
7 partitions (317/388 nodes on CoreML for 640m); forcing MLProgram format
made it fall back to CPU entirely, so the default EP config is used.
Paths to faster 640m: fewer partitions, per-monitor parallel sessions,
change-detection to skip static screens.

## NudeNet classes

18 classes; by default only the `*_EXPOSED` genital/breast/buttocks/anus
classes trigger censoring (see `Config::default`). The model also detects
faces, feet, armpits, and covered variants, which are ignored.
