# Category classifier: on-device zero-shot content categories

Status: **design only — not implemented.** This documents a proposed
second detection stage that adds configurable content *categories*
(smoking, alcohol, gambling, weapons, gore, …) beyond the body-part
nudity detection betamacs does today. Nothing here is built.

## Why

The current provider is NudeNet v3 (`src/detect.rs`), a YOLOv8-style
object detector with 18 nudity/body-part classes. Two limits:

1. **It is nudity-only.** The class list is fixed at the model's training
   taxonomy. There is no "smoking", "alcohol", "weapon", "gambling", or
   "gore" class, and no amount of threshold tuning invents one.
2. **Adding a class means retraining a detector** — collecting a labelled
   box dataset per category and training a YOLO head. That is a large,
   ongoing effort per category and not where we want to spend time.

The moving of the default to `640m` + tiling + highlights (done 2026-09-03)
addresses *recall on the classes NudeNet already has*. This doc addresses
the orthogonal problem: **more kinds of content**, added by
configuration rather than by training.

## Approach: zero-shot classification as a second stage

Keep NudeNet as the **localizer** — it draws the tight boxes the censor
overlay needs. Add a **frame/tile classifier** that answers "is this
category present?" using a vision-language model (CLIP or SigLIP), where
**categories are text prompts, not trained classes**.

A CLIP/SigLIP image encoder maps an image into the same embedding space as
a text encoder. To ask "is there smoking here?" you embed the image once,
embed a set of text prompts once (offline, cached), and compare by cosine
similarity. Adding a category is adding a prompt string — **no retraining,
no new model file**. This is the entire reason to prefer it over training
detector heads.

```
                 ┌─────────────────────────────────────────┐
 captured frame ─┤                                          │
                 │  Stage 1: NudeNet detector (unchanged)   │→ boxes → censor overlay
                 │           tight body-part boxes          │
                 │                                          │
                 │  Stage 2: SigLIP/CLIP classifier (NEW)   │→ category hits → response
                 │           per-frame or per-tile,         │   (blur tile / whole
                 │           prompt-defined categories      │    screen / log only)
                 └─────────────────────────────────────────┘
```

Stage 2 does **not** produce tight boxes. For categories like smoking,
alcohol, or gambling, a small blur box is rarely the right response
anyway — the useful responses are *blur the tile it fired on*, *blur the
whole screen*, or *log/report the sighting* without covering anything.
That maps cleanly onto the tiling the detector already does.

### Why this fits betamacs specifically

- **Same runtime.** SigLIP/CLIP image encoders export to ONNX and run
  through the existing `ort` + CoreML stack (`Detector::new` is the
  template). No new inference dependency.
- **Same tiling.** `Detector::detect_tiled` already crops an NxN
  overlapping grid. Stage 2 can classify each tile the detector already
  produced, giving coarse localization (which tile) for free.
- **Same settings channel.** Categories and thresholds live in the signed
  config package and hot-reload every pipeline cycle, exactly like
  detection triggers do today.
- **On-device, private.** No frames leave the machine — a hard
  requirement for a child's screen. This rules out the cloud APIs
  (Hive/Sightengine/Rekognition) that otherwise own the multi-category
  taxonomy.

## Model choice

Candidates, in rough preference order:

| Model | Params | Notes |
|-------|--------|-------|
| **SigLIP2 base (patch16-224)** | ~92M | Sigmoid loss → per-prompt scores are independently calibrated (better for multi-label "which of these are present"). Preferred. |
| SigLIP base | ~200M (full) / ~92M (vision) | Predecessor; fine if SigLIP2 export is fiddly. |
| OpenCLIP ViT-B/32 | ~88M vision | Softmax/contrastive; well-trodden ONNX export path; scores need per-category thresholds. |

Only the **image encoder** runs per frame. The **text encoder runs
offline** — prompts are encoded once when the category set changes and the
resulting embedding vectors are cached (shipped in the config or computed
at load). Per-frame cost is one image-encoder forward pass plus a matrix
multiply against the cached text embeddings.

SigLIP's sigmoid scoring is the reason to prefer it here: each category
gets an independent 0..1 score, so "smoking AND alcohol both present" is
natural and each category carries its own threshold. CLIP's softmax makes
scores compete across the prompt set, which is worse for multi-label
presence detection.

## Category taxonomy (initial)

Categories are config, not code. A sensible starting set, each defined by
one or more prompts (multiple prompts per category, scores max-pooled,
improves robustness):

- **smoking** — "a person smoking a cigarette", "a lit cigarette",
  "vaping", "a person exhaling smoke"
- **alcohol** — "alcoholic drinks", "beer bottles", "a cocktail",
  "a liquor advertisement"
- **drugs** — "illegal drug use", "a syringe", "drug paraphernalia"
- **gambling** — "a casino slot machine", "online poker", "roulette table"
- **weapons** — "a handgun", "a person holding a rifle", "a knife held as
  a weapon"
- **gore/violence** — "graphic violence", "blood and injury"

Plus one or more **negative/rejection prompts** ("a normal photo",
"a screenshot of text", "a user interface") as a calibration baseline;
zero-shot scores are only meaningful relative to a baseline, not in
absolute terms.

Note the honest caveat: **rare, small objects (a cigarette, a knife) are
the hard case for a whole-frame classifier.** Per-tile classification
helps (the cigarette is a larger fraction of a tile than of the frame),
and thresholds will need per-category tuning against real screenshots
before any category is trusted enough to *act* (as opposed to *log*).

## Integration sketch (for when we build it)

Deliberately not code — just where the seams are.

- **New module `src/classify.rs`** mirroring `detect.rs`: a `Classifier`
  wrapping an `ort::Session` for the image encoder, holding the cached
  text embeddings and the category→prompt map. Method:
  `classify(&RgbaImage) -> Vec<CategoryScore>` and a `classify_tiled`
  paralleling `detect_tiled`.
- **New settings block `ClassificationSettings`** in `src/settings.rs`
  (peer of `DetectionSettings`), carrying: `enabled`, `model`, the
  `categories` map (name → prompts + threshold + response mode), a global
  `capture_fps`/reuse-detector-tiles flag, and per-category
  `response: log | highlight | blur_tile | blur_screen`. Mirrored in
  `webapp/src/schema.ts` and a `detection.ts`-style settings page.
- **Pipeline wiring** in `src/pipeline.rs`: after Stage 1's detections,
  run Stage 2 on the same frame/tiles when `classification.enabled`.
  Category hits at/above threshold turn into either overlay regions
  (reusing `detection_to_region` for tile-sized boxes) or a
  heartbeat/report event (reusing the existing reporting path) per the
  category's response mode. The `highlight` response reuses the
  near-miss outline machinery so categories can be observed before they
  are enforced.
- **Models** land in `models/` (e.g. `siglip2-vision.onnx`) and ship in
  the bundle exactly like the NudeNet `.onnx` files; `make-app.sh` copies
  `models/*.onnx` already, so no packaging change beyond the export step.
- **Text-embedding cache**: prompts are encoded offline by an export
  script (Python, one-off) into a small `.npz`/JSON of vectors shipped
  beside the model, keyed by category. Changing prompts is a rebuild of
  that file, not a runtime cost.

### Rollout discipline

Every new category should ship in **`log`/`highlight` mode first** and
only graduate to `blur_tile`/`blur_screen` after its threshold is
validated against real screenshots — the same borderline-then-confirm
posture the detector's debounce band already takes. A category that
blurs the whole screen on a false positive is far more disruptive than a
missed body-part box.

## Performance budget

- One SigLIP2-base image-encoder pass is ~90M params — comparable to the
  `640m` NudeNet pass already running at the current capture rate. Running
  both stages roughly doubles per-frame inference cost.
- Mitigations if that is too much: run Stage 2 at a **lower cadence** than
  Stage 1 (categories are slow-moving; every Nth frame is fine), classify
  **whole-frame only** (skip tiles) except when a category needs small-
  object recall, or gate Stage 2 on scene-change so a static screen isn't
  re-classified.
- CoreML/ANE offload applies to SigLIP just as it does to NudeNet.

## Alternatives considered

- **YOLO-World (open-vocabulary detector).** Gives *boxes* for text-
  prompted classes, ONNX export works, class embeddings from CLIP baked
  in at export. The right choice **if categories need tight boxes**. Costs
  more than a classifier and, per its own docs, needs very low confidence
  thresholds for classes outside its pretraining (cigarettes, etc.), so
  recall on rare objects needs validation. Reasonable as a *later* upgrade
  from the classifier if boxes turn out to matter for a category.
- **EraX-Anti-NSFW (YOLO11).** A second *nudity* detector to ensemble with
  NudeNet for higher recall on the core explicit classes — orthogonal to
  categories. Worth revisiting for recall, not for breadth. The existing
  NMS already merges overlapping boxes from two detectors.
- **Apple SensitiveContentAnalysis (`SCSensitivityAnalyzer`).** Free,
  on-device, system-maintained. But binary (nudity yes/no), no boxes, no
  categories, and gated behind an entitlement plus the user enabling
  Sensitive Content Warning. Usable only as a cheap second opinion, not a
  category provider.
- **Cloud APIs (Hive, Sightengine, Rekognition).** These *do* own the
  multi-category taxonomy (tobacco/alcohol/drugs/gambling/weapons/gore) at
  93–98% accuracy. Rejected outright: they require streaming a child's
  screen frames to a third party. Non-starter on privacy and cost.

## Open questions

- SigLIP2 vs OpenCLIP: which exports cleanest to ONNX and runs best on
  ANE? Needs a spike.
- Whole-frame vs per-tile as the default — resolve empirically per
  category against a labelled screenshot set.
- Where the text-embedding cache lives: shipped in the config package
  (lets the fleet change prompts without an app update) vs baked beside
  the model (simpler, but prompt changes need a new app version).
- Threshold calibration methodology: we need a small labelled corpus of
  real screenshots per category before any category is trusted to act.
