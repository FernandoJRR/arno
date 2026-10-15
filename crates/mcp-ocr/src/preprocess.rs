//! Image cleanup before Tesseract (SPEC §4.4): phone photos of bills arrive
//! small (Telegram caps them near 1280 px), unevenly lit and slightly tilted,
//! which is what garbles the digits that matter. Pure functions over
//! `GrayImage`; every parameter is a named constant tuned on real bills.

use image::{DynamicImage, GrayImage, ImageDecoder, ImageReader, Luma, imageops};
use std::io::Cursor;

/// Decompression-bomb guard: refuse anything above this many pixels.
const MAX_PIXELS: u64 = 40_000_000;
/// Upscale until the long side reaches this; Tesseract wants ~30 px glyphs.
const TARGET_LONG_SIDE: u32 = 2800;
const MAX_UPSCALE: f32 = 3.0;
/// Contrast stretch maps these percentiles to black/white.
const STRETCH_LOW_PCT: f32 = 0.01;
const STRETCH_HIGH_PCT: f32 = 0.99;
/// Sauvola window (deskew probe only) as a fraction of the width, and `k`.
const SAUVOLA_WINDOW_FRAC: f32 = 1.0 / 40.0;
const SAUVOLA_K: f64 = 0.2;
const SAUVOLA_R: f64 = 128.0;
/// Deskew search range/step in degrees, on a copy this wide.
const DESKEW_MAX_DEG: f32 = 5.0;
const DESKEW_STEP_DEG: f32 = 0.5;
const DESKEW_PROBE_WIDTH: u32 = 600;
const BORDER_PX: u32 = 20;

/// Measured on a real phone photo of a bill (SPEC §11 #19): binarizing or
/// median-filtering here erased faint digit strokes Tesseract could still
/// read in grayscale, so the pipeline stops at a clean, upright gray image.
pub fn prepare(bytes: &[u8]) -> Result<GrayImage, String> {
    let gray = decode(bytes, MAX_PIXELS)?.to_luma8();
    let mut img = upscale(&gray);
    stretch_contrast(&mut img);
    let angle = estimate_skew(&img);
    if angle.abs() >= DESKEW_STEP_DEG {
        img = rotate(&img, angle);
    }
    Ok(pad(&img))
}

fn decode(bytes: &[u8], max_pixels: u64) -> Result<DynamicImage, String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let (w, h) = decoder.dimensions();
    if u64::from(w) * u64::from(h) > max_pixels {
        return Err(format!("image too large: {w}x{h}"));
    }
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img)
}

fn upscale(img: &GrayImage) -> GrayImage {
    let long = img.width().max(img.height()).max(1);
    let factor = (TARGET_LONG_SIDE as f32 / long as f32).min(MAX_UPSCALE);
    if factor <= 1.0 {
        return img.clone();
    }
    let w = (img.width() as f32 * factor).round() as u32;
    let h = (img.height() as f32 * factor).round() as u32;
    imageops::resize(img, w, h, imageops::FilterType::CatmullRom)
}

fn stretch_contrast(img: &mut GrayImage) {
    let mut hist = [0u64; 256];
    for p in img.pixels() {
        hist[p.0[0] as usize] += 1;
    }
    let total = u64::from(img.width()) * u64::from(img.height());
    let percentile = |pct: f32| {
        let target = (total as f32 * pct) as u64;
        let mut acc = 0;
        for (v, n) in hist.iter().enumerate() {
            acc += n;
            if acc > target {
                return v as f32;
            }
        }
        255.0
    };
    let (lo, hi) = (percentile(STRETCH_LOW_PCT), percentile(STRETCH_HIGH_PCT));
    if hi - lo < 1.0 {
        return;
    }
    for p in img.pixels_mut() {
        let v = (p.0[0] as f32 - lo) * 255.0 / (hi - lo);
        p.0[0] = v.clamp(0.0, 255.0) as u8;
    }
}

/// Sauvola local threshold over integral images: each pixel is compared with
/// its own neighbourhood, so lighting gradients don't swamp the ink profile
/// the deskew probe measures.
fn sauvola(img: &GrayImage) -> GrayImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let radius = ((img.width() as f32 * SAUVOLA_WINDOW_FRAC) as usize / 2).max(1);
    let stride = w + 1;
    let mut sum = vec![0u64; stride * (h + 1)];
    let mut sq = vec![0u64; stride * (h + 1)];
    for y in 0..h {
        let (mut row, mut row_sq) = (0u64, 0u64);
        for x in 0..w {
            let v = u64::from(img.get_pixel(x as u32, y as u32).0[0]);
            row += v;
            row_sq += v * v;
            sum[(y + 1) * stride + x + 1] = sum[y * stride + x + 1] + row;
            sq[(y + 1) * stride + x + 1] = sq[y * stride + x + 1] + row_sq;
        }
    }
    let rect = |t: &[u64], x0: usize, y0: usize, x1: usize, y1: usize| {
        t[y1 * stride + x1] + t[y0 * stride + x0] - t[y0 * stride + x1] - t[y1 * stride + x0]
    };
    GrayImage::from_fn(img.width(), img.height(), |x, y| {
        let (x, y) = (x as usize, y as usize);
        let (x0, y0) = (x.saturating_sub(radius), y.saturating_sub(radius));
        let (x1, y1) = ((x + radius + 1).min(w), (y + radius + 1).min(h));
        let n = ((x1 - x0) * (y1 - y0)) as f64;
        let mean = rect(&sum, x0, y0, x1, y1) as f64 / n;
        let var = (rect(&sq, x0, y0, x1, y1) as f64 / n - mean * mean).max(0.0);
        let threshold = mean * (1.0 + SAUVOLA_K * (var.sqrt() / SAUVOLA_R - 1.0));
        let v = f64::from(img.get_pixel(x as u32, y as u32).0[0]);
        Luma([if v <= threshold { 0 } else { 255 }])
    })
}

/// Projection-profile deskew: the angle whose horizontal ink profile has the
/// highest variance is the one that lines text rows up.
fn estimate_skew(img: &GrayImage) -> f32 {
    let scale = DESKEW_PROBE_WIDTH as f32 / img.width().max(1) as f32;
    let probe = if scale < 1.0 {
        let h = ((img.height() as f32 * scale) as u32).max(1);
        imageops::resize(img, DESKEW_PROBE_WIDTH, h, imageops::FilterType::Triangle)
    } else {
        img.clone()
    };
    let probe = sauvola(&probe);
    let steps = (DESKEW_MAX_DEG / DESKEW_STEP_DEG) as i32;
    let mut best = (0.0f32, f64::MIN);
    for i in -steps..=steps {
        let angle = i as f32 * DESKEW_STEP_DEG;
        let score = profile_variance(&rotate(&probe, angle));
        if score > best.1 {
            best = (angle, score);
        }
    }
    best.0
}

fn profile_variance(img: &GrayImage) -> f64 {
    let rows: Vec<f64> = img
        .rows()
        .map(|r| r.filter(|p| p.0[0] < 128).count() as f64)
        .collect();
    let mean = rows.iter().sum::<f64>() / rows.len().max(1) as f64;
    rows.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rows.len().max(1) as f64
}

fn rotate(img: &GrayImage, degrees: f32) -> GrayImage {
    imageproc::geometric_transformations::rotate_about_center(
        img,
        degrees.to_radians(),
        imageproc::geometric_transformations::Interpolation::Bilinear,
        imageproc::geometric_transformations::Border::Constant(Luma([255])),
    )
}

fn pad(img: &GrayImage) -> GrayImage {
    let mut out = GrayImage::from_pixel(
        img.width() + 2 * BORDER_PX,
        img.height() + 2 * BORDER_PX,
        Luma([255]),
    );
    imageops::overlay(&mut out, img, i64::from(BORDER_PX), i64::from(BORDER_PX));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(img: &GrayImage) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    /// White page with black horizontal "text" bars.
    fn page(w: u32, h: u32) -> GrayImage {
        GrayImage::from_fn(w, h, |_, y| Luma([if (y / 10) % 4 == 1 { 0 } else { 255 }]))
    }

    #[test]
    fn sauvola_keeps_text_on_a_lighting_gradient() {
        // Background fades 250 → 120 left to right; dark marks every 20 px.
        let img = GrayImage::from_fn(400, 100, |x, y| {
            let bg = 250 - (x * 130 / 400) as u8;
            Luma([if x % 20 < 3 && (30..70).contains(&y) {
                bg / 4
            } else {
                bg
            }])
        });
        let out = sauvola(&img);
        assert_eq!(out.get_pixel(1, 50).0[0], 0, "mark on the bright side");
        assert_eq!(out.get_pixel(381, 50).0[0], 0, "mark on the dark side");
        assert_eq!(out.get_pixel(10, 50).0[0], 255, "bright background");
        assert_eq!(out.get_pixel(390, 50).0[0], 255, "dim background");
    }

    #[test]
    fn upscale_is_capped_and_never_shrinks() {
        let small = upscale(&GrayImage::new(100, 50));
        assert_eq!(small.width(), 300, "3x cap");
        let big = upscale(&GrayImage::new(4000, 3000));
        assert_eq!(big.width(), 4000, "never downscales");
        let phone = upscale(&GrayImage::new(720, 1280));
        assert_eq!(phone.height(), TARGET_LONG_SIDE);
    }

    #[test]
    fn deskew_recovers_a_known_tilt() {
        let tilted = rotate(&page(800, 600), 3.0);
        let angle = estimate_skew(&tilted);
        assert!((angle + 3.0).abs() <= DESKEW_STEP_DEG, "got {angle}");
    }

    #[test]
    fn stretch_spreads_a_washed_out_range() {
        let mut img = GrayImage::from_fn(100, 1, |x, _| Luma([100 + (x as u8) / 2]));
        stretch_contrast(&mut img);
        assert!(img.get_pixel(1, 0).0[0] < 10);
        assert!(img.get_pixel(98, 0).0[0] > 245);
    }

    #[test]
    fn prepare_outputs_upscaled_padded_image() {
        let out = prepare(&png(&page(200, 120))).unwrap();
        assert_eq!(out.width(), 600 + 2 * BORDER_PX);
        assert_eq!(out.get_pixel(0, 0).0[0], 255);
    }

    #[test]
    fn garbage_and_truncated_input_error_without_panicking() {
        assert!(prepare(b"not an image").is_err());
        let bytes = png(&page(50, 50));
        assert!(prepare(&bytes[..bytes.len() / 2]).is_err());
    }

    #[test]
    fn oversize_images_are_rejected_before_decoding() {
        let bytes = png(&page(200, 100));
        let err = decode(&bytes, 10_000).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        assert!(decode(&bytes, 20_000).is_ok());
    }
}
