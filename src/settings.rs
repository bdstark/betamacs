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
            model: "320n".into(),
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
            mode: CensorMode::Box,
            opacity_pct: 100.0,
            blur: BlurSettings::default(),
            mosaic: MosaicSettings::default(),
            static_noise: StaticSettings::default(),
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
            mode, opacity_pct, blur, mosaic, static_noise,
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

// ---------------------------------------------------------------- resolution

/// Fully resolved settings for both modules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Effective {
    pub detection: DetectionSettings,
    pub censor: CensorSettings,
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
            censor: None,
        };
        let censor = |p: CensorPatch| ModulePatches {
            detection: None,
            censor: Some(p),
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
