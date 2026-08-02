//! NudeNet detection via ONNX Runtime.
//!
//! The NudeNet v3 detector is a YOLOv8-style model: square letterboxed
//! input, output tensor of shape `[1, 4 + num_classes, num_anchors]` with
//! (cx, cy, w, h) in input-pixel coordinates followed by per-class scores.

use std::path::Path;

use anyhow::Result;
use image::RgbaImage;
use ort::ep::CoreML;
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Tensor;

/// `ort`'s error types are not `Send + Sync`, so they can't flow through
/// `anyhow` with `?`; stringify them at the boundary instead.
trait OrtResultExt<T> {
    fn ort_ctx(self, msg: &str) -> Result<T>;
}

impl<T, E: std::fmt::Display> OrtResultExt<T> for std::result::Result<T, E> {
    fn ort_ctx(self, msg: &str) -> Result<T> {
        self.map_err(|e| anyhow::anyhow!("{msg}: {e}"))
    }
}

/// Class labels in NudeNet v3 output order.
pub const CLASSES: [&str; 18] = [
    "FEMALE_GENITALIA_COVERED",
    "FACE_FEMALE",
    "BUTTOCKS_EXPOSED",
    "FEMALE_BREAST_EXPOSED",
    "FEMALE_GENITALIA_EXPOSED",
    "MALE_BREAST_EXPOSED",
    "ANUS_EXPOSED",
    "FEET_EXPOSED",
    "BELLY_COVERED",
    "FEET_COVERED",
    "ARMPITS_COVERED",
    "ARMPITS_EXPOSED",
    "FACE_MALE",
    "BELLY_EXPOSED",
    "MALE_GENITALIA_EXPOSED",
    "ANUS_COVERED",
    "FEMALE_BREAST_COVERED",
    "BUTTOCKS_COVERED",
];

/// One detection, with the box in coordinates of the *captured image*
/// (physical pixels of the source frame).
#[derive(Debug, Clone)]
pub struct Detection {
    pub class: &'static str,
    pub confidence: f32,
    /// (x, y, width, height) in source-image pixels.
    pub bbox: (f32, f32, f32, f32),
}

pub struct Detector {
    session: Session,
    input_name: String,
    output_name: String,
    input_size: u32,
}

impl Detector {
    pub fn new(model_path: &Path, input_size: u32) -> Result<Self> {
        let session = Session::builder()
            .ort_ctx("failed to create session builder")?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .ort_ctx("failed to set optimization level")?
            .with_intra_threads(2)
            .ort_ctx("failed to set thread count")?
            // CoreML (GPU/Neural Engine) with silent fallback to CPU.
            .with_execution_providers([CoreML::default().build()])
            .ort_ctx("failed to register CoreML execution provider")?
            .commit_from_file(model_path)
            .ort_ctx(&format!("failed to load model {}", model_path.display()))?;
        let input_name = session.inputs()[0].name().to_string();
        let output_name = session.outputs()[0].name().to_string();
        Ok(Self {
            session,
            input_name,
            output_name,
            input_size,
        })
    }

    pub fn detect(
        &mut self,
        frame: &RgbaImage,
        confidence_threshold: f32,
        iou_threshold: f32,
    ) -> Result<Vec<Detection>> {
        let s = self.input_size;
        let (src_w, src_h) = (frame.width(), frame.height());

        // Letterbox: scale to fit inside s x s, pad the rest with mid-gray.
        let scale = (s as f32 / src_w as f32).min(s as f32 / src_h as f32);
        let new_w = (src_w as f32 * scale).round() as u32;
        let new_h = (src_h as f32 * scale).round() as u32;
        let resized = image::imageops::resize(
            frame,
            new_w,
            new_h,
            image::imageops::FilterType::Triangle,
        );
        let pad_x = (s - new_w) / 2;
        let pad_y = (s - new_h) / 2;

        let mut input = vec![114.0 / 255.0_f32; (3 * s * s) as usize];
        let plane = (s * s) as usize;
        for (x, y, pixel) in resized.enumerate_pixels() {
            let idx = ((y + pad_y) * s + (x + pad_x)) as usize;
            input[idx] = pixel[0] as f32 / 255.0;
            input[plane + idx] = pixel[1] as f32 / 255.0;
            input[2 * plane + idx] = pixel[2] as f32 / 255.0;
        }

        let tensor = Tensor::from_array(([1usize, 3, s as usize, s as usize], input))
            .ort_ctx("failed to build input tensor")?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .ort_ctx("inference failed")?;
        let (shape, data) = outputs[self.output_name.as_str()]
            .try_extract_tensor::<f32>()
            .ort_ctx("failed to extract output tensor")?;

        // shape = [1, 4 + num_classes, num_anchors]
        let num_attrs = shape[1] as usize;
        let num_anchors = shape[2] as usize;
        let num_classes = num_attrs - 4;
        let at = |attr: usize, anchor: usize| data[attr * num_anchors + anchor];

        let mut candidates: Vec<Detection> = Vec::new();
        for a in 0..num_anchors {
            let (mut best_class, mut best_score) = (0, 0.0_f32);
            for c in 0..num_classes {
                let score = at(4 + c, a);
                if score > best_score {
                    best_score = score;
                    best_class = c;
                }
            }
            if best_score < confidence_threshold {
                continue;
            }
            // Undo letterbox: input pixels -> source pixels.
            let cx = (at(0, a) - pad_x as f32) / scale;
            let cy = (at(1, a) - pad_y as f32) / scale;
            let w = at(2, a) / scale;
            let h = at(3, a) / scale;
            candidates.push(Detection {
                class: CLASSES.get(best_class).copied().unwrap_or("UNKNOWN"),
                confidence: best_score,
                bbox: (cx - w / 2.0, cy - h / 2.0, w, h),
            });
        }

        Ok(nms(candidates, iou_threshold))
    }

    /// Detect over the full frame plus an overlapping `grid` x `grid` of
    /// tiles, so small regions (thumbnails, quarter-screen windows) aren't
    /// lost to downscaling. Boxes are merged with a global NMS pass.
    pub fn detect_tiled(
        &mut self,
        frame: &RgbaImage,
        grid: u32,
        overlap: f32,
        confidence_threshold: f32,
        iou_threshold: f32,
    ) -> Result<Vec<Detection>> {
        let mut all = self.detect(frame, confidence_threshold, iou_threshold)?;

        if grid >= 2 {
            let (w, h) = (frame.width() as f32, frame.height() as f32);
            let (base_w, base_h) = (w / grid as f32, h / grid as f32);
            let (margin_x, margin_y) = (base_w * overlap / 2.0, base_h * overlap / 2.0);
            for gy in 0..grid {
                for gx in 0..grid {
                    let x0 = (gx as f32 * base_w - margin_x).max(0.0);
                    let y0 = (gy as f32 * base_h - margin_y).max(0.0);
                    let x1 = ((gx + 1) as f32 * base_w + margin_x).min(w);
                    let y1 = ((gy + 1) as f32 * base_h + margin_y).min(h);
                    let tile = image::imageops::crop_imm(
                        frame,
                        x0 as u32,
                        y0 as u32,
                        (x1 - x0) as u32,
                        (y1 - y0) as u32,
                    )
                    .to_image();
                    let detections =
                        self.detect(&tile, confidence_threshold, iou_threshold)?;
                    all.extend(detections.into_iter().map(|mut d| {
                        d.bbox.0 += x0;
                        d.bbox.1 += y0;
                        d
                    }));
                }
            }
        }

        Ok(nms(all, iou_threshold))
    }
}

/// Greedy per-class non-maximum suppression.
///
/// Suppresses by IoU, and also by containment (intersection over the
/// smaller box's area): the full-frame pass and a tile pass often find the
/// same region at different box sizes, whose IoU stays under the threshold
/// even though one essentially contains the other.
fn nms(mut detections: Vec<Detection>, iou_threshold: f32) -> Vec<Detection> {
    detections.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    let mut kept: Vec<Detection> = Vec::new();
    for det in detections {
        let overlaps = kept.iter().any(|k| {
            k.class == det.class
                && (iou(k.bbox, det.bbox) > iou_threshold
                    || containment(k.bbox, det.bbox) > 0.6)
        });
        if !overlaps {
            kept.push(det);
        }
    }
    kept
}

/// Intersection area over the smaller box's area (1.0 = fully contained).
fn containment(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let x1 = a.0.max(b.0);
    let y1 = a.1.max(b.1);
    let x2 = (a.0 + a.2).min(b.0 + b.2);
    let y2 = (a.1 + a.3).min(b.1 + b.3);
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let min_area = (a.2 * a.3).min(b.2 * b.3);
    if min_area <= 0.0 { 0.0 } else { inter / min_area }
}

fn iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let x1 = a.0.max(b.0);
    let y1 = a.1.max(b.1);
    let x2 = (a.0 + a.2).min(b.0 + b.2);
    let y2 = (a.1 + a.3).min(b.1 + b.3);
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let union = a.2 * a.3 + b.2 * b.3 - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}
