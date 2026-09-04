//! Configuration package model, shared as a JSON contract with the web app
//! (webapp/src/schema.ts mirrors these types; field names are camelCase on
//! the wire).
//!
//! A *package* is the document the web app pushes: a set of *named
//! configurations* (each a partial, per-module settings object), an ordered
//! *layer* stack naming which of them apply, and final explicit overrides.
//! Effective settings = module defaults <- layers in order <- overrides,
//! merged field by field. The `triggers` map merges per class, so a named
//! config can flip individual triggers without owning the whole map.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::detect::CLASSES;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    pub version: u32,
    #[serde(default)]
    pub named_configs: Vec<NamedConfig>,
    #[serde(default)]
    pub layers: Vec<String>,
    #[serde(default)]
    pub overrides: ModulePatches,
    /// Named collections of overlay text lines. The censor module's text
    /// display references one or more sets by name; their lines pool
    /// together for the per-box random pick.
    #[serde(default)]
    pub text_sets: Vec<TextSet>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSet {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub settings: ModulePatches,
}

/// Partial settings per module; `None` fields mean "not touched by this
/// layer".
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModulePatches {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<DetectionPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub censor: Option<CensorPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub challenge: Option<ChallengePatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure: Option<ExposurePatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earned_time: Option<EarnedTimePatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_limit: Option<FocusLimitPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_integrity: Option<ClockIntegrityPatch>,
}

// ---------------------------------------------------------------- detection

/// Detection engine (NudeNet) module settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectionSettings {
    /// Master switch: when false, nothing is scanned or censored. Only a
    /// signed config can flip this on a managed install, and the daemon
    /// treats "disabled by policy" as healthy (no quarantine).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// "320n" | "640m" | absolute model path.
    pub model: String,
    /// Minimum confidence for a detection to count (0..1).
    pub confidence_threshold: f32,
    /// NMS IoU threshold (0..1).
    pub iou_threshold: f32,
    /// Detections smaller than this in either dimension (captured pixels)
    /// are ignored.
    pub min_region_px: f32,
    /// Max capture rate per display (restart to apply).
    pub capture_fps: f32,
    /// Tile grid per screen (0/1 = off).
    pub tile_grid: u32,
    /// Grace period before a censor box is released (ms).
    pub hold_ms: u64,
    /// Detections scoring below `confidence_threshold + borderline_margin`
    /// are borderline: covered immediately like everything else, but the
    /// box stays provisional — dropped `debounce_window_ms` after its last
    /// sighting instead of getting the full hold — until it is sighted
    /// `debounce_count` times. 0 disables the band.
    pub borderline_margin: f32,
    /// Sightings that graduate a borderline box to a full hold (1 = off).
    pub debounce_count: u32,
    /// Lifetime of an unconfirmed borderline box after its last sighting (ms).
    pub debounce_window_ms: u64,
    /// Debug/tuning overlay: outline (don't cover) detections of
    /// trigger-enabled classes that were flagged but not blocked (low
    /// confidence or under the size floor), labeled with class,
    /// confidence, and size.
    #[serde(default)]
    pub highlight_enabled: bool,
    /// Lowest confidence worth highlighting (0..1).
    #[serde(default = "default_highlight_floor")]
    pub highlight_floor: f32,
    /// Which NudeNet classes trigger censoring.
    pub triggers: BTreeMap<String, bool>,
}

fn default_highlight_floor() -> f32 {
    0.15
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DetectionPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iou_threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_region_px: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_fps: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile_grid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub borderline_margin: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce_window_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight_floor: Option<f32>,
    /// Merged per class, not replaced wholesale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggers: Option<BTreeMap<String, bool>>,
}

impl Default for DetectionSettings {
    fn default() -> Self {
        let on_by_default = [
            "FEMALE_BREAST_EXPOSED",
            "FEMALE_GENITALIA_EXPOSED",
            "MALE_GENITALIA_EXPOSED",
            "BUTTOCKS_EXPOSED",
            "ANUS_EXPOSED",
        ];
        Self {
            enabled: true,
            model: "640m".into(),
            confidence_threshold: 0.35,
            iou_threshold: 0.45,
            min_region_px: 0.0,
            capture_fps: 4.0,
            tile_grid: 2,
            hold_ms: 1500,
            borderline_margin: 0.1,
            debounce_count: 2,
            debounce_window_ms: 3000,
            highlight_enabled: false,
            highlight_floor: default_highlight_floor(),
            triggers: CLASSES
                .iter()
                .map(|c| (c.to_string(), on_by_default.contains(c)))
                .collect(),
        }
    }
}

impl DetectionSettings {
    pub fn apply(&mut self, p: &DetectionPatch) {
        macro_rules! set {
            ($($f:ident),+) => { $( if let Some(v) = &p.$f { self.$f = v.clone(); } )+ };
        }
        set!(
            enabled, model, confidence_threshold, iou_threshold, min_region_px, capture_fps,
            tile_grid, hold_ms, borderline_margin, debounce_count, debounce_window_ms,
            highlight_enabled, highlight_floor
        );
        if let Some(triggers) = &p.triggers {
            for (class, on) in triggers {
                self.triggers.insert(class.clone(), *on);
            }
        }
    }

    /// Resolve the model name/path to (file path, input size).
    pub fn model_path(&self) -> (PathBuf, u32) {
        let path = match self.model.as_str() {
            "320n" | "640m" => PathBuf::from(format!("models/{}.onnx", self.model)),
            other => PathBuf::from(other),
        };
        let size = if path.to_string_lossy().contains("320") { 320 } else { 640 };
        (path, size)
    }
}

// ------------------------------------------------------------------- censor

/// How a censor box renders its interior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CensorMode {
    /// Solid fill (the original black box).
    Box,
    /// Blurred view of the content beneath.
    Blur,
    /// Pixelated view of the content beneath.
    Mosaic,
    /// Animated analog-TV static.
    Static,
    /// Cover the detection with a fixed image.
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlurKind {
    /// True gaussian blur.
    Gaussian,
    /// Fast box blur.
    Box,
    /// Downscale-and-average (strongest smoothing per unit of intensity).
    Average,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlurSettings {
    pub kind: BlurKind,
    /// Blur strength, 1..100.
    pub intensity: f32,
}

impl Default for BlurSettings {
    fn default() -> Self {
        Self {
            kind: BlurKind::Gaussian,
            intensity: 16.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MosaicSampling {
    /// Cell = average of the pixels it covers.
    Average,
    /// Cell = gaussian-weighted sample.
    Gaussian,
    /// Cell = single point sample (harshest).
    Nearest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorMap {
    /// Keep the source colors.
    None,
    /// Map cell luminance onto the low..high color range.
    Luminance,
    /// Luminance quantized into 4 bands of the color range.
    Steps,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MosaicSettings {
    /// Square cell edge in points.
    pub cell_size_pt: f32,
    pub sampling: MosaicSampling,
    pub map: ColorMap,
    /// Color range for luminance mapping.
    pub color_low: String,
    pub color_high: String,
}

impl Default for MosaicSettings {
    fn default() -> Self {
        Self {
            cell_size_pt: 16.0,
            sampling: MosaicSampling::Average,
            map: ColorMap::None,
            color_low: "#000000".into(),
            color_high: "#ffffff".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticSettings {
    /// Fraction of grains lit, 0..100.
    pub density_pct: f32,
    /// Frame regenerations per second; 0 = frozen.
    pub speed_hz: f32,
    /// Grain edge in millimeters (approximated via display DPI).
    pub grain_mm: f32,
    /// False = classic black/white; true = use the color range.
    pub colored: bool,
    pub color_low: String,
    pub color_high: String,
}

impl Default for StaticSettings {
    fn default() -> Self {
        Self {
            density_pct: 60.0,
            speed_hz: 12.0,
            grain_mm: 1.0,
            colored: false,
            color_low: "#000000".into(),
            color_high: "#ffffff".into(),
        }
    }
}

/// How the cover image is scaled to fill a censor box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFit {
    /// Stretch to the box exactly, ignoring aspect ratio.
    Stretch,
    /// Scale to fit inside the box, preserving aspect (may letterbox — the
    /// fill color shows in the gaps).
    Contain,
    /// Scale to fill the box, preserving aspect (crops overflow). Default:
    /// guarantees full coverage with no distortion.
    Cover,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSettings {
    /// Base64 (std) of the image file bytes (PNG/JPEG/…), carried in the
    /// config so the cover image travels the signed pipeline. Takes
    /// precedence over `path`. Empty = none.
    #[serde(default)]
    pub data: String,
    /// Absolute path to an image file, for local/dev use when `data` is
    /// empty. Empty = none.
    #[serde(default)]
    pub path: String,
    /// How the image fills the box.
    #[serde(default = "default_image_fit")]
    pub fit: ImageFit,
}

fn default_image_fit() -> ImageFit {
    ImageFit::Cover
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self {
            data: String::new(),
            path: String::new(),
            fit: default_image_fit(),
        }
    }
}

/// Censor module settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CensorSettings {
    /// How box interiors render.
    pub mode: CensorMode,
    /// Overall opacity of the censor graphic, 10..100. Below 100 the
    /// content underneath shows through proportionally.
    pub opacity_pct: f32,
    /// Blur options (mode = blur).
    pub blur: BlurSettings,
    /// Mosaic options (mode = mosaic).
    pub mosaic: MosaicSettings,
    /// TV-static options (mode = static).
    pub static_noise: StaticSettings,
    /// Cover-image options (mode = image).
    #[serde(default)]
    pub image: ImageSettings,
    /// Box fill color, #rrggbb (mode = box).
    pub fill_color: String,
    /// Box border color, #rrggbb.
    pub border_color: String,
    /// Border width in points; 0 = no border.
    pub border_width: f32,
    /// Horizontal size as a percentage of the reported box (100 = as
    /// reported).
    pub x_scale_pct: f32,
    /// Vertical size percentage.
    pub y_scale_pct: f32,
    /// Overlay the trigger class name on the box.
    pub show_trigger_label: bool,
    /// Show boxes in screenshots / screen shares too.
    pub censor_in_captures: bool,
    /// Text overlay drawn on the box.
    pub text_overlay: TextOverlay,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CensorPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CensorMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity_pct: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blur: Option<BlurSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mosaic: Option<MosaicSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_noise: Option<StaticSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_width: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_scale_pct: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_scale_pct: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_trigger_label: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub censor_in_captures: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_overlay: Option<TextOverlay>,
}

impl Default for CensorSettings {
    fn default() -> Self {
        Self {
            mode: CensorMode::Image,
            opacity_pct: 100.0,
            blur: BlurSettings::default(),
            mosaic: MosaicSettings::default(),
            static_noise: StaticSettings::default(),
            image: ImageSettings::default(),
            fill_color: "#000000".into(),
            border_color: "#000000".into(),
            border_width: 0.0,
            x_scale_pct: 130.0,
            y_scale_pct: 130.0,
            show_trigger_label: false,
            censor_in_captures: false,
            text_overlay: TextOverlay::default(),
        }
    }
}

impl CensorSettings {
    pub fn apply(&mut self, p: &CensorPatch) {
        macro_rules! set {
            ($($f:ident),+) => { $( if let Some(v) = &p.$f { self.$f = v.clone(); } )+ };
        }
        set!(
            mode, opacity_pct, blur, mosaic, static_noise, image,
            fill_color, border_color, border_width, x_scale_pct, y_scale_pct,
            show_trigger_label, censor_in_captures, text_overlay
        );
    }
}

/// The text display drawn on a censor box: which named text sets supply
/// the lines, and how they render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextOverlay {
    pub enabled: bool,
    /// Names of `Package::text_sets` to draw lines from.
    pub sets: Vec<String>,
    /// Resolved lines from the referenced sets (filled by
    /// `Package::resolve`; ignored on input). One is picked per box.
    #[serde(default)]
    pub lines: Vec<String>,
    pub font_family: String,
    pub font_size_pt: f32,
    pub font_color: String,
}

impl Default for TextOverlay {
    fn default() -> Self {
        Self {
            enabled: false,
            sets: Vec::new(),
            lines: Vec::new(),
            font_family: "Helvetica".into(),
            font_size_pt: 18.0,
            font_color: "#ffffff".into(),
        }
    }
}

// ------------------------------------------------------- activity challenge
//
// Liveness/attention checks. The *policy* (cadence, difficulty band, which
// task categories are live, enforcement window) is a module patch carried
// in the signed `betamacs-config` package, so it layers/overrides like
// detection and censor. The *content* — the task bank itself — is a
// SEPARATE signed artifact (`betamacs-tasks`, see `TaskBank`) so questions
// version and swap independently of policy (per kid, or as kids age). The
// policy references tasks only abstractly, by `category` and `grade`.

/// How a challenge answer is checked. Authored in plaintext; the daemon may
/// store a salted hash instead so the answer isn't recoverable from config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Answer {
    /// Numeric answer, optional absolute tolerance (0 = exact).
    Number {
        value: f64,
        #[serde(default)]
        tolerance: f64,
    },
    /// Free text; `value` and/or any of `any_of` are accepted.
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        #[serde(default)]
        any_of: Vec<String>,
        #[serde(default = "default_true")]
        ignore_case: bool,
    },
    /// Type-this-line (anti-idle): exact match, trimmed.
    Line { value: String },
    /// Multiple choice: `options` are shown as buttons, `value` is correct.
    Choice { options: Vec<String>, value: String },
}

/// One challenge task. Self-contained and offline-checkable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    /// Selection filter / grouping, e.g. "math-word" or "type-line".
    pub category: String,
    /// Difficulty band; policy caps it with `max_grade`.
    pub grade: u8,
    /// Relative pick probability among eligible tasks.
    #[serde(default = "default_weight")]
    pub weight: f32,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub answer: Answer,
    /// Salted hashes of the acceptable answers, emitted by `publish.sh
    /// tasks` from the authored plaintext so a readable `tasks.json` is not
    /// a cheat sheet. When present the agent checks input against these
    /// (the plaintext `answer` keeps only type/presentation, e.g. choice
    /// options); when absent it falls back to the plaintext `answer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_hash: Option<Vec<String>>,
}

/// The `betamacs-tasks` artifact: a standalone, independently-versioned
/// bank delivered as its own signed envelope (own epoch / anti-rollback),
/// merged with the challenge policy at runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TaskBank {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

/// Challenge policy (lives in `betamacs-config`). Disabled by default; a
/// missing/empty task bank makes it a no-op (never a lockout from absence).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeSettings {
    #[serde(default)]
    pub enabled: bool,
    /// Random cadence bounds, measured over active-session time.
    pub interval_min_sec: u32,
    pub interval_max_sec: u32,
    /// Task categories drawn from the bank; empty = none eligible.
    #[serde(default)]
    pub categories: Vec<String>,
    /// Never pick a task above this grade band.
    pub max_grade: u8,
    /// Time to answer before the challenge counts as unprotected (feeds the
    /// daemon's quarantine, like a stale heartbeat).
    pub answer_window_sec: u32,
    /// Wrong answers allowed before a fresh task is picked.
    pub max_attempts: u32,
}

impl Default for ChallengeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_min_sec: 2700,
            interval_max_sec: 5400,
            categories: Vec::new(),
            max_grade: 6,
            answer_window_sec: 120,
            max_attempts: 3,
        }
    }
}

/// Partial challenge policy for layering.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChallengePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_min_sec: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_max_sec: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_grade: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_window_sec: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
}

impl ChallengeSettings {
    pub fn apply(&mut self, p: &ChallengePatch) {
        macro_rules! set {
            ($($f:ident),+) => { $( if let Some(v) = &p.$f { self.$f = v.clone(); } )+ };
        }
        set!(
            enabled, interval_min_sec, interval_max_sec, categories, max_grade,
            answer_window_sec, max_attempts
        );
    }
}

// -------------------------------------------------------- exposure budget
//
// Quantifies how much the censor is firing — frequency, on-screen area, and
// box count are all known per frame in the pipeline — and turns a sustained
// excess into an escalating response: a soft warning popup ("Are you
// looking at appropriate content?"), then, past a hard limit, a *timed*
// internet lockout via the same betamacsd quarantine. Policy only; the
// pipeline accumulates the metric and the daemon enforces the penalty.

/// What the exposure budget accumulates over its rolling window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExposureMetric {
    /// Count of distinct new detections (how OFTEN).
    Events,
    /// Seconds with at least one censor box present.
    ActiveSeconds,
    /// Integral of box count over time (how MANY).
    BoxSeconds,
    /// Integral of covered screen fraction over time (how much SPACE).
    AreaSeconds,
}

/// Exposure-budget policy (lives in `betamacs-config`). Disabled by default.
/// Two thresholds over rolling windows: `warn_*` raises the acknowledgement
/// popup; `block_*` trips a `penalty_sec` network lockout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExposureSettings {
    #[serde(default)]
    pub enabled: bool,
    pub metric: ExposureMetric,
    /// Soft limit within `warn_window_sec` → warning popup.
    pub warn_threshold: f32,
    pub warn_window_sec: u32,
    /// Hard limit within `block_window_sec` → timed quarantine.
    pub block_threshold: f32,
    pub block_window_sec: u32,
    /// How long the internet stays cut once the hard limit trips.
    pub penalty_sec: u32,
    /// Minimum gap between warning popups.
    pub warn_cooldown_sec: u32,
}

impl Default for ExposureSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            metric: ExposureMetric::Events,
            warn_threshold: 20.0,
            warn_window_sec: 300,
            block_threshold: 40.0,
            block_window_sec: 600,
            penalty_sec: 900,
            warn_cooldown_sec: 120,
        }
    }
}

/// Partial exposure policy for layering.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExposurePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<ExposureMetric>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_window_sec: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_window_sec: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub penalty_sec: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_cooldown_sec: Option<u32>,
}

impl ExposureSettings {
    pub fn apply(&mut self, p: &ExposurePatch) {
        macro_rules! set {
            ($($f:ident),+) => { $( if let Some(v) = &p.$f { self.$f = v.clone(); } )+ };
        }
        set!(
            enabled, metric, warn_threshold, warn_window_sec, block_threshold,
            block_window_sec, penalty_sec, warn_cooldown_sec
        );
    }
}

fn default_weight() -> f32 {
    1.0
}

// -------------------------------------------------------------- earned time
//
// A gate (not a punishment or liveness check): during a scheduled window the
// internet is locked until the user has earned credit by active time on an
// allowlisted educational site/app. Bankable. See docs/earned-time.md; the
// daemon owns the balance ledger (part B) and the agent only observes
// activity (part C, src/earned.rs). Policy only; disabled by default.

/// A window during which the earned-time gate is active. `days` are lowercase
/// three-letter names (`mon`..`sun`); `from`/`to` are `HH:MM` local times.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct Schedule {
    pub days: Vec<String>,
    pub from: String,
    pub to: String,
}

/// What identifies an activity that earns credit. A source matches when the
/// frontmost app's bundle id equals `bundle_id`, or (for web sources) the
/// frontmost browser's current-tab host matches `browser_host_suffix`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct SourceMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_host_suffix: Option<String>,
}

/// An allowlisted activity and how fast it earns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct EarnSource {
    pub name: String,
    /// `match` on the wire (it is a keyword in Rust).
    #[serde(rename = "match")]
    pub matcher: SourceMatch,
    /// Active minutes -> earned minutes multiplier.
    pub earn_ratio: f32,
}

/// Earned-time gate settings. **Disabled by default.**
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct EarnedTimeSettings {
    /// Master switch; false means the gate and the activity monitor idle.
    #[serde(default)]
    pub enabled: bool,
    /// When the gate is active (empty = never).
    #[serde(default)]
    pub schedule: Vec<Schedule>,
    /// What earns credit, and how fast.
    #[serde(default)]
    pub sources: Vec<EarnSource>,
    /// Earned minutes -> minutes of gated internet.
    pub spend_ratio: f32,
    /// Most that can be earned in a day.
    pub daily_earn_cap_min: u32,
    /// Ceiling on the carried-over balance.
    pub max_bank_min: u32,
    /// Ignore sub-threshold blips of activity.
    pub min_session_min: u32,
    /// Pause crediting after this much idle (no input).
    pub idle_timeout_sec: u32,
}

impl Default for EarnedTimeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule: Vec::new(),
            sources: Vec::new(),
            spend_ratio: 1.0,
            daily_earn_cap_min: 120,
            max_bank_min: 240,
            min_session_min: 5,
            idle_timeout_sec: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct EarnedTimePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Vec<Schedule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<EarnSource>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_ratio: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_earn_cap_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bank_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_session_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_sec: Option<u32>,
}

impl EarnedTimeSettings {
    pub fn apply(&mut self, p: &EarnedTimePatch) {
        macro_rules! set {
            ($($f:ident),+) => { $( if let Some(v) = &p.$f { self.$f = v.clone(); } )+ };
        }
        set!(
            enabled, schedule, sources, spend_ratio, daily_earn_cap_min,
            max_bank_min, min_session_min, idle_timeout_sec
        );
    }
}

// ------------------------------------------------------------- focus limit
//
// Auto-lockout when the user stays ACTIVELY on one browser tab too long
// (active scrolling — passive video watching registers as idle and does
// not count). "Same tab" is detected as the frontmost browser's
// current-tab URL staying unchanged while the browser is frontmost and the
// user is not idle. Kids-only for free via the same task-bank gate as
// earned-time. Policy only; disabled by default.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct FocusLimitSettings {
    #[serde(default)]
    pub enabled: bool,
    /// Active minutes on a single URL before the lockout trips.
    pub same_tab_limit_min: u32,
    /// How long the network lockout lasts.
    pub lockout_min: u32,
    /// No input for this long counts as passive (video): dwell pauses.
    pub idle_reset_sec: u32,
    /// Host suffixes that are exempt — dwell never accrues on these.
    #[serde(default)]
    pub whitelist_hosts: Vec<String>,
    /// If non-empty, ONLY these host suffixes are monitored; everything
    /// else is ignored. Empty = monitor all (minus the whitelist).
    #[serde(default)]
    pub blacklist_hosts: Vec<String>,
}

impl Default for FocusLimitSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            same_tab_limit_min: 10,
            lockout_min: 10,
            idle_reset_sec: 60,
            whitelist_hosts: Vec::new(),
            blacklist_hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct FocusLimitPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_tab_limit_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockout_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_reset_sec: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whitelist_hosts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blacklist_hosts: Option<Vec<String>>,
}

impl FocusLimitSettings {
    pub fn apply(&mut self, p: &FocusLimitPatch) {
        macro_rules! set {
            ($($f:ident),+) => { $( if let Some(v) = &p.$f { self.$f = v.clone(); } )+ };
        }
        set!(
            enabled, same_tab_limit_min, lockout_min, idle_reset_sec,
            whitelist_hosts, blacklist_hosts
        );
    }
}

// ---------------------------------------------------------------- resolution

/// Fully resolved settings for all modules.
// ------------------------------------------------------- clock integrity
//
// Time-of-day policy (earned-time windows, and future time-layers) is only
// as trustworthy as the clock it reads. A kid who can change the system
// clock or timezone could otherwise shift a schedule. So schedule windows
// are evaluated against an ASSIGNED timezone applied to a TRUSTED epoch,
// never the OS timezone; and the wall clock is watched for being CHANGED
// under a running instance — a jump relative to a sleep-inclusive monotonic
// clock (`mach_continuous_time`). A running-instance change is tamper: the
// daemon quarantines, the same as the censor being shut down. A machine that
// merely BOOTED with the wrong time (no running-instance jump) is announced
// and resynced, not punished. Disabled by default; enable per-config once
// verified on a device. See src/clock.rs.

/// Clock-integrity policy (lives in `betamacs-config`). Disabled by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockIntegritySettings {
    #[serde(default)]
    pub enabled: bool,
    /// Assigned IANA timezone (e.g. "America/Chicago") used to interpret all
    /// schedule windows. None falls back to the OS timezone (only safe when
    /// the OS timezone is itself locked down).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// How far the wall clock may diverge (from monotonic, or from network
    /// time) before it counts as changed/wrong, in seconds.
    pub skew_tolerance_sec: u32,
    /// How often the monitor samples the clock for a running-instance jump.
    pub check_interval_sec: u32,
    /// How often to re-confirm the absolute time over the network.
    pub anchor_interval_sec: u32,
    /// NTP servers queried for the absolute time (SNTP).
    #[serde(default)]
    pub ntp_servers: Vec<String>,
    /// Optional pinned-backend URL whose TLS `Date` corroborates NTP, read
    /// with the bundle's pinned otactl root. None = NTP only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_url: Option<String>,
}

impl Default for ClockIntegritySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            timezone: None,
            skew_tolerance_sec: 300,
            check_interval_sec: 15,
            anchor_interval_sec: 900,
            ntp_servers: vec!["time.apple.com".into(), "pool.ntp.org".into()],
            time_url: None,
        }
    }
}

/// Partial clock-integrity policy for layering.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClockIntegrityPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skew_tolerance_sec: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_interval_sec: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_interval_sec: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntp_servers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_url: Option<String>,
}

impl ClockIntegritySettings {
    pub fn apply(&mut self, p: &ClockIntegrityPatch) {
        macro_rules! set {
            ($($f:ident),+) => { $( if let Some(v) = &p.$f { self.$f = v.clone(); } )+ };
        }
        set!(
            enabled, skew_tolerance_sec, check_interval_sec, anchor_interval_sec,
            ntp_servers
        );
        // Option-valued fields: a present patch value sets it (layers only add).
        if p.timezone.is_some() {
            self.timezone = p.timezone.clone();
        }
        if p.time_url.is_some() {
            self.time_url = p.time_url.clone();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Effective {
    pub detection: DetectionSettings,
    pub censor: CensorSettings,
    #[serde(default)]
    pub challenge: ChallengeSettings,
    #[serde(default)]
    pub exposure: ExposureSettings,
    #[serde(default)]
    pub earned_time: EarnedTimeSettings,
    #[serde(default)]
    pub focus_limit: FocusLimitSettings,
    #[serde(default)]
    pub clock_integrity: ClockIntegritySettings,
}

impl Package {
    pub fn resolve(&self) -> Effective {
        let mut effective = Effective::default();
        let layer_patches = self.layers.iter().filter_map(|name| {
            self.named_configs
                .iter()
                .find(|c| &c.name == name)
                .map(|c| &c.settings)
        });
        for patches in layer_patches.chain(std::iter::once(&self.overrides)) {
            if let Some(p) = &patches.detection {
                effective.detection.apply(p);
            }
            if let Some(p) = &patches.censor {
                effective.censor.apply(p);
            }
            if let Some(p) = &patches.challenge {
                effective.challenge.apply(p);
            }
            if let Some(p) = &patches.exposure {
                effective.exposure.apply(p);
            }
            if let Some(p) = &patches.earned_time {
                effective.earned_time.apply(p);
            }
            if let Some(p) = &patches.focus_limit {
                effective.focus_limit.apply(p);
            }
            if let Some(p) = &patches.clock_integrity {
                effective.clock_integrity.apply(p);
            }
        }
        // Pool the lines of the referenced text sets, in reference order.
        effective.censor.text_overlay.lines = effective
            .censor
            .text_overlay
            .sets
            .iter()
            .filter_map(|name| self.text_sets.iter().find(|s| &s.name == name))
            .flat_map(|s| s.lines.iter().cloned())
            .collect();
        effective
    }

    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Starter package with a few illustrative named configurations.
    pub fn starter() -> Self {
        let cfg = |name: &str, desc: &str, settings: ModulePatches| NamedConfig {
            name: name.into(),
            description: Some(desc.into()),
            settings,
        };
        let detection = |p: DetectionPatch| ModulePatches {
            detection: Some(p),
            ..Default::default()
        };
        let censor = |p: CensorPatch| ModulePatches {
            censor: Some(p),
            ..Default::default()
        };
        let all_triggers = |on: &[&str]| {
            Some(
                CLASSES
                    .iter()
                    .map(|c| (c.to_string(), on.contains(c)))
                    .collect(),
            )
        };
        Self {
            version: 1,
            named_configs: vec![
                cfg(
                    "strict",
                    "Lower threshold, all exposed and covered triggers on",
                    detection(DetectionPatch {
                        confidence_threshold: Some(0.25),
                        triggers: all_triggers(&[
                            "FEMALE_BREAST_EXPOSED",
                            "FEMALE_GENITALIA_EXPOSED",
                            "FEMALE_GENITALIA_COVERED",
                            "FEMALE_BREAST_COVERED",
                            "MALE_GENITALIA_EXPOSED",
                            "MALE_BREAST_EXPOSED",
                            "BUTTOCKS_EXPOSED",
                            "BUTTOCKS_COVERED",
                            "ANUS_EXPOSED",
                            "ANUS_COVERED",
                        ]),
                        ..Default::default()
                    }),
                ),
                cfg(
                    "explicit-only",
                    "Only fully explicit classes trigger",
                    detection(DetectionPatch {
                        triggers: all_triggers(&[
                            "FEMALE_BREAST_EXPOSED",
                            "FEMALE_GENITALIA_EXPOSED",
                            "MALE_GENITALIA_EXPOSED",
                            "BUTTOCKS_EXPOSED",
                            "ANUS_EXPOSED",
                        ]),
                        ..Default::default()
                    }),
                ),
                cfg(
                    "performance",
                    "Small model, lower rate, no tiling",
                    detection(DetectionPatch {
                        model: Some("320n".into()),
                        capture_fps: Some(2.0),
                        tile_grid: Some(1),
                        ..Default::default()
                    }),
                ),
                cfg(
                    "accuracy",
                    "Large model (slower)",
                    detection(DetectionPatch {
                        model: Some("640m".into()),
                        ..Default::default()
                    }),
                ),
                cfg(
                    "blur-soft",
                    "Gaussian blur censor",
                    censor(CensorPatch {
                        mode: Some(CensorMode::Blur),
                        blur: Some(BlurSettings {
                            kind: BlurKind::Gaussian,
                            intensity: 16.0,
                        }),
                        ..Default::default()
                    }),
                ),
                cfg(
                    "mosaic-classic",
                    "Pixelated censor, true colors",
                    censor(CensorPatch {
                        mode: Some(CensorMode::Mosaic),
                        mosaic: Some(MosaicSettings::default()),
                        ..Default::default()
                    }),
                ),
                cfg(
                    "tv-static",
                    "Animated black & white static",
                    censor(CensorPatch {
                        mode: Some(CensorMode::Static),
                        static_noise: Some(StaticSettings::default()),
                        ..Default::default()
                    }),
                ),
            ],
            layers: vec![],
            overrides: ModulePatches::default(),
            text_sets: vec![TextSet {
                name: "classic".into(),
                description: Some("Stock censor-bar phrases".into()),
                lines: vec![
                    "CENSORED".into(),
                    "NOTHING TO SEE HERE".into(),
                    "MOVE ALONG".into(),
                ],
            }],
        }
    }
}

/// Parse "#rrggbb" (or "#rrggbbaa") into rgba components 0..1.
pub fn parse_color(s: &str) -> Option<(f64, f64, f64, f64)> {
    let hex = s.strip_prefix('#')?;
    let parse = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    match hex.len() {
        6 => Some((
            parse(0)? as f64 / 255.0,
            parse(2)? as f64 / 255.0,
            parse(4)? as f64 / 255.0,
            1.0,
        )),
        8 => Some((
            parse(0)? as f64 / 255.0,
            parse(2)? as f64 / 255.0,
            parse(4)? as f64 / 255.0,
            parse(6)? as f64 / 255.0,
        )),
        _ => None,
    }
}
