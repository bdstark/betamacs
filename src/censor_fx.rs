//! CPU image effects for the blur and mosaic censor modes.
//!
//! The pipeline crops each censor region out of the captured frame (which
//! excludes our own overlay windows, so it is always the raw content),
//! processes it here, and ships the result to the overlay window's layer.
//! Mosaic output stays at cell resolution — the layer upscales it with
//! nearest-neighbor magnification on the GPU.

use std::sync::Arc;

use image::imageops::FilterType;
use image::RgbaImage;

use crate::settings::{parse_color, BlurKind, BlurSettings, ColorMap, MosaicSampling, MosaicSettings};

/// Processed pixels for one censor region, ready for a window layer.
#[derive(Debug, Clone)]
pub struct RegionContent {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
    /// Upscale with nearest-neighbor (mosaic/static) instead of linear.
    pub pixelated: bool,
}

pub fn blur(crop: &RgbaImage, settings: &BlurSettings) -> RegionContent {
    let intensity = settings.intensity.clamp(1.0, 100.0);
    let (w, h) = (crop.width().max(1), crop.height().max(1));
    // Above a small intensity, blur at reduced resolution: equivalent to a
    // much larger kernel, far cheaper, and it genuinely destroys structure
    // instead of just softening it.
    let downscaled = |factor: f32, sigma: f32, gaussian: bool| {
        let f = factor.max(1.0);
        let (dw, dh) = (
            ((w as f32 / f) as u32).max(1),
            ((h as f32 / f) as u32).max(1),
        );
        let small = image::imageops::resize(crop, dw, dh, FilterType::Triangle);
        let small = if sigma > 0.0 {
            if gaussian {
                image::imageops::blur(&small, sigma)
            } else {
                image::imageops::fast_blur(&small, sigma)
            }
        } else {
            small
        };
        image::imageops::resize(&small, w, h, FilterType::Triangle)
    };
    let out = match settings.kind {
        BlurKind::Gaussian if intensity <= 8.0 => image::imageops::blur(crop, intensity / 2.0),
        BlurKind::Gaussian => downscaled(intensity / 5.0, 2.5, true),
        BlurKind::Box if intensity <= 8.0 => image::imageops::fast_blur(crop, intensity / 2.0),
        BlurKind::Box => downscaled(intensity / 5.0, 2.5, false),
        // Pure downscale-average: harshest per unit of intensity.
        BlurKind::Average => downscaled(intensity / 2.0, 0.0, false),
    };
    RegionContent {
        width: out.width(),
        height: out.height(),
        rgba: Arc::new(out.into_raw()),
        pixelated: false,
    }
}

pub fn mosaic(crop: &RgbaImage, settings: &MosaicSettings, points_per_px: f32) -> RegionContent {
    let cell_px = (settings.cell_size_pt / points_per_px.max(0.01)).max(2.0);
    let cells_w = ((crop.width() as f32 / cell_px).ceil() as u32).max(1);
    let cells_h = ((crop.height() as f32 / cell_px).ceil() as u32).max(1);
    let filter = match settings.sampling {
        MosaicSampling::Average => FilterType::Triangle,
        MosaicSampling::Gaussian => FilterType::Gaussian,
        MosaicSampling::Nearest => FilterType::Nearest,
    };
    let mut small = image::imageops::resize(crop, cells_w, cells_h, filter);

    if settings.map != ColorMap::None {
        let low = parse_color(&settings.color_low).unwrap_or((0.0, 0.0, 0.0, 1.0));
        let high = parse_color(&settings.color_high).unwrap_or((1.0, 1.0, 1.0, 1.0));
        for pixel in small.pixels_mut() {
            let lum = (0.299 * pixel[0] as f32 + 0.587 * pixel[1] as f32
                + 0.114 * pixel[2] as f32)
                / 255.0;
            let t = match settings.map {
                ColorMap::Luminance => lum,
                ColorMap::Steps => (lum * 4.0).floor().min(3.0) / 3.0,
                ColorMap::None => unreachable!(),
            };
            pixel[0] = (255.0 * (low.0 as f32 + (high.0 as f32 - low.0 as f32) * t)) as u8;
            pixel[1] = (255.0 * (low.1 as f32 + (high.1 as f32 - low.1 as f32) * t)) as u8;
            pixel[2] = (255.0 * (low.2 as f32 + (high.2 as f32 - low.2 as f32) * t)) as u8;
        }
    }

    RegionContent {
        width: small.width(),
        height: small.height(),
        rgba: Arc::new(small.into_raw()),
        pixelated: true,
    }
}
