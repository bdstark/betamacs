# betamacs

Real-time screen censor: monitors all connected displays, detects nudity
locally with the [NudeNet](https://github.com/notAI-tech/NudeNet) v3 ONNX
detector, and draws black boxes over flagged regions. Everything runs
on-device; no frame ever leaves the machine.

## Pipeline

```
capture (xcap / ScreenCaptureKit, per monitor)
   → detect (NudeNet ONNX via ort, letterbox + YOLOv8 decode + NMS)
   → censor (per-monitor overlay window: transparent, always-on-top,
             click-through, excluded from capture)
```

- **capture.rs** — polls a full screenshot of every monitor per tick.
  Upgrade path: persistent ScreenCaptureKit streams for higher FPS.
- **detect.rs** — ONNX Runtime session per process; letterboxes the frame
  to the model's square input, decodes the `[1, 4+18, anchors]` output,
  maps boxes back to source-pixel coordinates.
- **overlay.rs** — a pool of small plain-black borderless windows, one per
  censor region, positioned in global logical points. Always-on-top,
  click-through, never focused, visible on all Spaces (incl. fullscreen
  apps), and content-protected (`NSWindow.sharingType = .none`) so the
  detector keeps seeing the raw content underneath. No pixel drawing at
  all — the window background is the censor box.
- **pipeline.rs** — the continuous loop: capture all monitors, detect,
  convert flagged boxes to padded global-point regions, push to the
  overlay. Regions linger `hold_ms` after last sighting (anti-flicker).
- **config.rs** — thresholds, FPS, censored class list.

- **detect.rs (tiling)** — each screen is scanned as the full frame plus a
  2x2 grid of ~20%-overlapping tiles (5 inferences/screen), so thumbnails
  and quarter-screen windows survive the downscale to model input size.
  Global NMS merges duplicate boxes across tiles.

## Setup

```bash
scripts/fetch-model.sh 320n   # or 640m
cargo run --release           # continuous censoring, 320n model
cargo run --release -- 640m   # more accurate model, slower
cargo run --release -- probe  # one-shot capture + detection with timings
cargo run --release -- demo   # 8s black box + capture-exclusion self-check
```

`--censor-captures` makes the boxes visible in screenshots / screen shares
too. Caveat: the detector then can't see beneath its own boxes, so they
blink roughly every `hold_ms`; a ScreenCaptureKit per-window exclusion
filter (planned) will fix that properly. Debug: `BETAMACS_NO_PROTECT=1`
forces boxes into captures so the demo can verify they render.

Requires macOS Screen Recording permission for the terminal/app running it.
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
