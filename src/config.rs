//! Runtime configuration for the censoring pipeline.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Path to the NudeNet ONNX detector model.
    pub model_path: PathBuf,
    /// Square input size the model expects (320 for 320n, 640 for 640m).
    pub input_size: u32,
    /// Target capture rate per monitor, in frames per second.
    pub capture_fps: f32,
    /// Minimum confidence for a detection to be acted on.
    pub confidence_threshold: f32,
    /// IoU threshold for non-maximum suppression.
    pub iou_threshold: f32,
    /// Tile grid per screen: in addition to the full frame, each screen is
    /// scanned as `tile_grid` x `tile_grid` overlapping crops so small
    /// regions survive downscaling. 0 or 1 disables tiling.
    pub tile_grid: u32,
    /// Overlap between adjacent tiles, as a fraction of tile size.
    pub tile_overlap: f32,
    /// Extra padding (fraction of box size) added around each censor box.
    pub box_padding: f32,
    /// How long a censor box lingers after the region is last detected, in ms.
    /// Prevents flicker when detection confidence hovers around the threshold.
    pub hold_ms: u64,
    /// Class labels that trigger censoring.
    pub censored_classes: Vec<&'static str>,
    /// When true, censor boxes are visible in screenshots / screen shares /
    /// recordings too (overlay windows are not content-protected).
    ///
    /// Caveat: our own detector then also sees the boxes instead of the
    /// content beneath, so a censored region is re-detected only after its
    /// box lifts — boxes blink roughly every `hold_ms`. The proper fix is
    /// direct ScreenCaptureKit capture with a per-window exclusion filter
    /// (planned); until then this mode is best-effort.
    pub censor_in_captures: bool,
}

impl Config {
    /// Select a model by short name ("320n" / "640m") or explicit path.
    pub fn with_model(mut self, model: &str) -> Self {
        self.model_path = match model {
            "320n" | "640m" => PathBuf::from(format!("models/{model}.onnx")),
            path => PathBuf::from(path),
        };
        self.input_size = if self.model_path.to_string_lossy().contains("320") {
            320
        } else {
            640
        };
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 320n meets the ~2-4 fps sweep target on this machine (see
            // README benchmarks); 640m is more accurate but ~5x slower —
            // select it with `betamacs 640m`.
            model_path: PathBuf::from("models/320n.onnx"),
            input_size: 320,
            capture_fps: 4.0,
            confidence_threshold: 0.35,
            iou_threshold: 0.45,
            tile_grid: 2,
            tile_overlap: 0.2,
            box_padding: 0.15,
            hold_ms: 1500,
            censored_classes: vec![
                "FEMALE_BREAST_EXPOSED",
                "FEMALE_GENITALIA_EXPOSED",
                "MALE_GENITALIA_EXPOSED",
                "BUTTOCKS_EXPOSED",
                "ANUS_EXPOSED",
            ],
            censor_in_captures: false,
        }
    }
}
