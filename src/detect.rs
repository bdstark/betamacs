//! NudeNet detection via ONNX Runtime.
//!
//! The NudeNet v3 detector is a YOLOv8-style model: square letterboxed
//! input, output tensor of shape `[N, 4 + num_classes, num_anchors]` with
//! (cx, cy, w, h) in input-pixel coordinates followed by per-class scores.
//!
//! Inference runs through the CoreML execution provider (ML Program
//! format, all compute units, so eligible nodes land on the GPU/Neural
//! Engine) with CPU fallback. `BETAMACS_COREML=off|legacy` forces the CPU
//! EP or the old NeuralNetwork-format CoreML EP for troubleshooting, and
//! `BETAMACS_ORT_PROFILE=1` logs CoreML's per-op compute-plan dispatch.
//!
//! When the model was exported with a dynamic batch dimension, the full
//! frame and its tiles are packed into a single batched inference;
//! otherwise they run sequentially. `TileCache` lets callers skip tiles
//! whose pixels are unchanged since the previous frame, reusing the prior
//! detections.

use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use fast_image_resize as fir;
use fir::images::{Image as FirImage, ImageRef};
use image::RgbaImage;
use ort::ep::CoreML;
use ort::ep::coreml::{ComputeUnits, ModelFormat, SpecializationStrategy};
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

/// A crop rectangle on the source frame: (x, y, width, height) in pixels.
type CropRect = (u32, u32, u32, u32);

/// Per-monitor cache for `detect_tiled`: a content hash and the resulting
/// detections for each tile of the previous frame, so tiles whose pixels
/// haven't changed skip inference and replay their prior detections. The
/// key ties entries to the frame geometry and thresholds that produced
/// them; any mismatch invalidates the whole cache.
#[derive(Default)]
pub struct TileCache {
    key: Option<(u32, u32, u32, u32, u32)>,
    hashes: Vec<Option<u64>>,
    detections: Vec<Vec<Detection>>,
}

/// The overlapping tile layout `detect_tiled` scans (excluding the always
/// present full-frame pass). Empty for `grid` < 2.
fn tile_rects(w: u32, h: u32, grid: u32, overlap: f32) -> Vec<CropRect> {
    if grid < 2 {
        return Vec::new();
    }
    let (wf, hf) = (w as f32, h as f32);
    let (base_w, base_h) = (wf / grid as f32, hf / grid as f32);
    let (margin_x, margin_y) = (base_w * overlap / 2.0, base_h * overlap / 2.0);
    let mut rects = Vec::with_capacity((grid * grid) as usize);
    for gy in 0..grid {
        for gx in 0..grid {
            let x0 = (gx as f32 * base_w - margin_x).max(0.0);
            let y0 = (gy as f32 * base_h - margin_y).max(0.0);
            let x1 = ((gx + 1) as f32 * base_w + margin_x).min(wf);
            let y1 = ((gy + 1) as f32 * base_h + margin_y).min(hf);
            rects.push((x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32));
        }
    }
    rects
}

/// FNV-1a over every other pixel's RGB in the rect — "did this region
/// change since last frame", not a perceptual hash.
fn region_hash(frame: &RgbaImage, rect: CropRect) -> u64 {
    let raw = frame.as_raw();
    let stride = frame.width() as usize * 4;
    let (x0, y0, w, h) = rect;
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut y = y0 as usize;
    while y < (y0 + h) as usize {
        let row = &raw[y * stride..y * stride + stride];
        let mut x = x0 as usize;
        while x < (x0 + w) as usize {
            for &b in &row[x * 4..x * 4 + 3] {
                hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
            }
            x += 2;
        }
        y += 2;
    }
    hash
}

pub struct Detector {
    session: Session,
    input_name: String,
    output_name: String,
    input_size: u32,
    /// The model was exported with a dynamic batch dimension, so multiple
    /// crops can go through one `run()`.
    supports_batch: bool,
    resizer: fir::Resizer,
}

impl Detector {
    pub fn new(model_path: &Path, input_size: u32) -> Result<Self> {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8);
        let builder = Session::builder()
            .ort_ctx("failed to create session builder")?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .ort_ctx("failed to set optimization level")?
            .with_intra_threads(threads)
            .ort_ctx("failed to set thread count")?;
        let mut builder = match std::env::var("BETAMACS_COREML").as_deref() {
            Ok("off") => builder,
            Ok("legacy") => builder
                .with_execution_providers([CoreML::default().build()])
                .ort_ctx("failed to register CoreML execution provider")?,
            _ => {
                // Cache compiled CoreML models per ONNX file so session
                // rebuilds (display wake, wedged-session recovery) don't
                // pay recompilation.
                let cache_dir = std::env::temp_dir().join("betamacs-coreml-cache").join(
                    model_path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "model".into()),
                );
                let _ = std::fs::create_dir_all(&cache_dir);
                let mut ep = CoreML::default()
                    .with_compute_units(ComputeUnits::All)
                    .with_model_format(ModelFormat::MLProgram)
                    .with_specialization_strategy(SpecializationStrategy::FastPrediction)
                    .with_model_cache_dir(cache_dir.to_string_lossy());
                if std::env::var("BETAMACS_ORT_PROFILE").is_ok_and(|v| v == "1") {
                    ep = ep.with_profile_compute_plan(true);
                }
                builder
                    .with_execution_providers([ep.build()])
                    .ort_ctx("failed to register CoreML execution provider")?
            }
        };
        // Note on the "E5RT ... unbounded dimension" lines CoreML prints on
        // stderr during inference: the model exports its input as
        // `[batch, 3, height, width]` and has internal dynamic `Resize`
        // nodes, so the MLProgram backend keeps those specific ops on CPU
        // and runs the rest (the conv backbone) on the ANE/GPU. The logs
        // are cosmetic — results are correct and this partial-offload is
        // measurably the fastest working config. Pinning the free
        // dimensions to force everything onto CoreML makes it *fail* at
        // runtime ("output_features has no value"), because the dynamic
        // decode head genuinely can't run there. So we leave the shapes
        // dynamic and let ORT partition.
        let session = builder
            .commit_from_file(model_path)
            .ort_ctx(&format!("failed to load model {}", model_path.display()))?;
        let input_name = session.inputs()[0].name().to_string();
        let output_name = session.outputs()[0].name().to_string();
        // Batched inference needs a dynamic batch dimension in the model,
        // but it's a measured net loss here so it's off by default. A
        // dynamic outer dimension is exactly what the CoreML MLProgram
        // backend rejects ("unbounded dimension"), forcing the batched
        // program off the ANE and onto CPU; sequential single-frame passes
        // each stay on the ANE and come out faster (194ms vs 227ms per
        // grid=2 640m frame on an M-series). `BETAMACS_BATCH=on` re-enables
        // it for measurement when the model has the dynamic dimension.
        let has_dynamic_batch = session.inputs()[0]
            .dtype()
            .tensor_shape()
            .is_some_and(|shape| shape.first().copied() == Some(-1));
        let supports_batch =
            has_dynamic_batch && std::env::var("BETAMACS_BATCH").as_deref() == Ok("on");
        tracing::debug!(
            "detector loaded {} (batch inference {})",
            model_path.display(),
            if supports_batch { "on" } else { "off" },
        );
        Ok(Self {
            session,
            input_name,
            output_name,
            input_size,
            supports_batch,
            resizer: fir::Resizer::new(),
        })
    }

    /// Letterbox one crop of the frame into a `3*s*s` CHW plane slice that
    /// is pre-filled with mid-gray. Returns (scale, pad_x, pad_y) for
    /// mapping model output back to source coordinates.
    fn letterbox_into(
        &mut self,
        frame: &RgbaImage,
        crop: CropRect,
        out: &mut [f32],
    ) -> Result<(f32, f32, f32)> {
        let s = self.input_size;
        let (cx, cy, cw, ch) = crop;
        let scale = (s as f32 / cw as f32).min(s as f32 / ch as f32);
        let new_w = ((cw as f32 * scale).round() as u32).clamp(1, s);
        let new_h = ((ch as f32 * scale).round() as u32).clamp(1, s);
        let src = ImageRef::new(
            frame.width(),
            frame.height(),
            frame.as_raw(),
            fir::PixelType::U8x4,
        )
        .ort_ctx("bad source image buffer")?;
        let mut dst = FirImage::new(new_w, new_h, fir::PixelType::U8x4);
        self.resizer
            .resize(
                &src,
                &mut dst,
                &fir::ResizeOptions::new()
                    .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Bilinear))
                    // Capture alpha is opaque; skip the premultiply pass.
                    .use_alpha(false)
                    .crop(cx as f64, cy as f64, cw as f64, ch as f64),
            )
            .ort_ctx("resize failed")?;

        let pad_x = (s - new_w) / 2;
        let pad_y = (s - new_h) / 2;
        let plane = (s * s) as usize;
        let buf = dst.buffer();
        for y in 0..new_h as usize {
            let row = &buf[y * new_w as usize * 4..(y + 1) * new_w as usize * 4];
            let base = (y + pad_y as usize) * s as usize + pad_x as usize;
            for (x, px) in row.chunks_exact(4).enumerate() {
                out[base + x] = px[0] as f32 / 255.0;
                out[plane + base + x] = px[1] as f32 / 255.0;
                out[2 * plane + base + x] = px[2] as f32 / 255.0;
            }
        }
        Ok((scale, pad_x as f32, pad_y as f32))
    }

    /// Single whole-frame pass (no tiling). Currently only exercised by
    /// the bench test, kept as the obvious entry point.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn detect(
        &mut self,
        frame: &RgbaImage,
        confidence_threshold: f32,
        iou_threshold: f32,
    ) -> Result<Vec<Detection>> {
        let full = (0, 0, frame.width(), frame.height());
        Ok(self
            .detect_crops(frame, &[full], confidence_threshold, iou_threshold)?
            .pop()
            .unwrap_or_default())
    }

    /// Run detection over several crops of one frame — batched into a
    /// single inference when the model allows, sequentially otherwise.
    /// Returned boxes are in whole-frame coordinates, one Vec per crop.
    fn detect_crops(
        &mut self,
        frame: &RgbaImage,
        crops: &[CropRect],
        confidence_threshold: f32,
        iou_threshold: f32,
    ) -> Result<Vec<Vec<Detection>>> {
        if crops.is_empty() {
            return Ok(Vec::new());
        }
        if crops.len() > 1 && !self.supports_batch {
            let mut out = Vec::with_capacity(crops.len());
            for crop in crops {
                out.extend(self.detect_crops(
                    frame,
                    std::slice::from_ref(crop),
                    confidence_threshold,
                    iou_threshold,
                )?);
            }
            return Ok(out);
        }

        let s = self.input_size as usize;
        let n = crops.len();
        let started = Instant::now();
        let mut input = vec![114.0 / 255.0_f32; n * 3 * s * s];
        let mut letterboxes = Vec::with_capacity(n);
        for (i, &crop) in crops.iter().enumerate() {
            letterboxes.push(self.letterbox_into(
                frame,
                crop,
                &mut input[i * 3 * s * s..(i + 1) * 3 * s * s],
            )?);
        }
        let preprocess = started.elapsed();

        let tensor = Tensor::from_array(([n, 3, s, s], input))
            .ort_ctx("failed to build input tensor")?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .ort_ctx("inference failed")?;
        let (shape, data) = outputs[self.output_name.as_str()]
            .try_extract_tensor::<f32>()
            .ort_ctx("failed to extract output tensor")?;
        tracing::debug!(
            "detect: batch {n} preprocess {preprocess:?} inference {:?}",
            started.elapsed() - preprocess,
        );

        // shape = [n, 4 + num_classes, num_anchors]
        let num_attrs = shape[1] as usize;
        let num_anchors = shape[2] as usize;
        let num_classes = num_attrs - 4;
        let mut results = Vec::with_capacity(n);
        for (i, (&crop, &(scale, pad_x, pad_y))) in crops.iter().zip(&letterboxes).enumerate() {
            let base = i * num_attrs * num_anchors;
            let at = |attr: usize, anchor: usize| data[base + attr * num_anchors + anchor];
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
                // Undo letterbox: input pixels -> crop pixels -> frame pixels.
                let cx = crop.0 as f32 + (at(0, a) - pad_x) / scale;
                let cy = crop.1 as f32 + (at(1, a) - pad_y) / scale;
                let w = at(2, a) / scale;
                let h = at(3, a) / scale;
                candidates.push(Detection {
                    class: CLASSES.get(best_class).copied().unwrap_or("UNKNOWN"),
                    confidence: best_score,
                    bbox: (cx - w / 2.0, cy - h / 2.0, w, h),
                });
            }
            results.push(nms(candidates, iou_threshold));
        }
        Ok(results)
    }

    /// Detect over the full frame plus an overlapping `grid` x `grid` of
    /// tiles, so small regions (thumbnails, quarter-screen windows) aren't
    /// lost to downscaling. Boxes are merged with a global NMS pass.
    ///
    /// The full frame is always scanned; tiles whose pixels are unchanged
    /// since the previous call (per `cache`) reuse their prior detections
    /// instead of re-running inference.
    pub fn detect_tiled(
        &mut self,
        frame: &RgbaImage,
        grid: u32,
        overlap: f32,
        confidence_threshold: f32,
        iou_threshold: f32,
        cache: &mut TileCache,
    ) -> Result<Vec<Detection>> {
        let (w, h) = (frame.width(), frame.height());
        let rects = tile_rects(w, h, grid, overlap);
        let key = (
            w,
            h,
            grid,
            confidence_threshold.to_bits(),
            iou_threshold.to_bits(),
        );
        if cache.key != Some(key) {
            cache.key = Some(key);
            cache.hashes = vec![None; rects.len()];
            cache.detections = vec![Vec::new(); rects.len()];
        }

        // Full frame first, then only the tiles whose content changed.
        let mut crops: Vec<CropRect> = vec![(0, 0, w, h)];
        let mut fresh_tiles: Vec<usize> = Vec::new();
        for (i, &rect) in rects.iter().enumerate() {
            let hash = region_hash(frame, rect);
            if cache.hashes[i] == Some(hash) {
                continue;
            }
            cache.hashes[i] = Some(hash);
            fresh_tiles.push(i);
            crops.push(rect);
        }
        if !rects.is_empty() {
            tracing::debug!(
                "detect_tiled: {} of {} tiles changed",
                fresh_tiles.len(),
                rects.len(),
            );
        }

        let mut results =
            self.detect_crops(frame, &crops, confidence_threshold, iou_threshold)?;
        let mut all = std::mem::take(&mut results[0]);
        for (slot, &i) in fresh_tiles.iter().enumerate() {
            cache.detections[i] = std::mem::take(&mut results[slot + 1]);
        }
        for detections in &cache.detections {
            all.extend(detections.iter().cloned());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Not a correctness test: measures per-frame detect cost on this
    /// machine. Run with:
    ///   cargo test --release bench_detect -- --ignored --nocapture
    /// and optionally BETAMACS_COREML=off|legacy to compare EPs.
    #[test]
    #[ignore]
    fn bench_detect() {
        let mut img = RgbaImage::new(1728, 1117);
        let mut state = 0x12345678u32;
        for p in img.pixels_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let b = state.to_le_bytes();
            *p = image::Rgba([b[0], b[1], b[2], 255]);
        }

        for (name, path, size) in [
            ("640m", "models/640m.onnx", 640u32),
            ("320n", "models/320n.onnx", 320u32),
        ] {
            let load = Instant::now();
            let mut det = Detector::new(Path::new(path), size).unwrap();
            println!(
                "{name}: loaded in {:?}, batch inference {}",
                load.elapsed(),
                if det.supports_batch { "on" } else { "off" },
            );
            for _ in 0..3 {
                det.detect(&img, 0.2, 0.45).unwrap();
            }
            let n = 10u32;
            let t = Instant::now();
            for _ in 0..n {
                det.detect(&img, 0.2, 0.45).unwrap();
            }
            println!("{name}: single pass {:?}/frame", t.elapsed() / n);

            let mut cache = TileCache::default();
            det.detect_tiled(&img, 2, 0.2, 0.2, 0.45, &mut cache).unwrap();
            let n = 5u32;
            let t = Instant::now();
            for _ in 0..n {
                cache = TileCache::default();
                det.detect_tiled(&img, 2, 0.2, 0.2, 0.45, &mut cache).unwrap();
            }
            println!("{name}: tiled grid=2, all tiles fresh {:?}/frame", t.elapsed() / n);
            let t = Instant::now();
            for _ in 0..n {
                det.detect_tiled(&img, 2, 0.2, 0.2, 0.45, &mut cache).unwrap();
            }
            println!("{name}: tiled grid=2, unchanged {:?}/frame", t.elapsed() / n);
        }
    }
}
