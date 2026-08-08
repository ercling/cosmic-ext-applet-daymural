// Accent colour derived from the wallpaper (opt-in, off by default) — see
// docs/plans/20260808-accent-from-wallpaper.md.
//
// This module owns the whole colour domain: extraction of the dominant
// *vibrant* hue from the cached 480×270 thumbnail (below), and — in later
// tasks — the hue transplant onto COSMIC's own palette tones, the theme
// writer with snapshot/restore, and the pure apply/disarm decision.
//
// Extraction is a chroma-weighted hue histogram in Oklch, the shape borrowed
// from the GNOME extension's dominant-with-grey-fallback rule
// (examples/bing-wallpaper-gnome-extension is a different algorithm, but
// `GNOME-Auto-Accent-Colour`'s `saturation < 5% → slate` fallback becomes our
// mean-chroma cutoff). Weighting by chroma is what makes the result
// dominant-*vibrant* rather than dominant-pixel: a small saturated subject
// beats a large washed-out ground because grey pixels carry ~zero weight.

// TODO(Task 6): drop this once app.rs wires the extraction task in — until
// then nothing outside the tests calls into this module.
#![allow(dead_code)]

use cosmic::cosmic_theme::palette::{IntoColor, Oklch, Srgb};
use image::RgbImage;

/// Hue histogram resolution: 36 buckets of 10°.
const HUE_BUCKETS: usize = 36;
const BUCKET_WIDTH_DEG: f32 = 360.0 / HUE_BUCKETS as f32;

/// Mask pixels darker than this Oklch L: near-black pixels (night skies,
/// shadows) read as noise-hued and would otherwise vote with whatever tint
/// their sensor noise has.
const NEAR_BLACK_L: f32 = 0.1;

/// Mask pixels lighter than this Oklch L — the analogue of ColorThief's
/// near-white filter (its `color-thief.js:44-47` drops `>250,250,250`).
/// 0.975 rather than something rounder because the threshold sits between
/// two fixed points: near-white grey `(250,250,250)` is L ≈ 0.985 and must
/// be masked, while pure yellow `#FFFF00` — a plausible wallpaper dominant —
/// is L ≈ 0.968 and must survive.
const NEAR_WHITE_L: f32 = 0.975;

/// Grey cutoff, on the **mean** chroma over unmasked pixels — normalised, so
/// a 4-pixel test image and the 130k-pixel thumbnail agree by construction
/// (an absolute total would make them disagree on the same colour). Oklch
/// chroma for vibrant sRGB colours peaks around 0.3; a genuinely grey photo
/// sits well under 0.01.
const MIN_MEAN_CHROMA: f64 = 0.02;

/// Sampling budget: the 480×270 thumbnail, the module's designed input.
/// Anything larger is stride-sampled down to roughly this many pixels.
const MAX_SAMPLES: u32 = 480 * 270;

/// One histogram bucket: total chroma weight plus the weighted hue vector
/// (for the circular mean — bucket midpoints would quantise to 10°).
#[derive(Clone, Copy, Default)]
struct Bucket {
    weight: f64,
    sin: f64,
    cos: f64,
}

/// The dominant vibrant hue of `img`, in Oklch degrees `[0, 360)`, or `None`
/// when the image is effectively grey (mean chroma under the cutoff, or
/// every pixel masked as near-black/near-white).
///
/// Production feeds the cached 480×270 thumbnail; tests feed tiny synthetic
/// images — the normalised cutoff makes both read the same.
pub fn dominant_hue(img: &RgbImage) -> Option<f32> {
    let (width, height) = img.dimensions();
    let stride = sample_stride(width, height);

    let mut hist = [Bucket::default(); HUE_BUCKETS];
    let mut unmasked: u64 = 0;
    let mut chroma_sum: f64 = 0.0;

    for y in (0..height).step_by(stride) {
        for x in (0..width).step_by(stride) {
            let p = img.get_pixel(x, y);
            let ok: Oklch = Srgb::new(
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            )
            .into_color();

            if ok.l < NEAR_BLACK_L || ok.l > NEAR_WHITE_L {
                continue;
            }
            unmasked += 1;
            let weight = f64::from(ok.chroma);
            chroma_sum += weight;

            let hue = ok.hue.into_positive_degrees();
            let bucket = ((hue / BUCKET_WIDTH_DEG) as usize).min(HUE_BUCKETS - 1);
            let rad = f64::from(hue).to_radians();
            hist[bucket].weight += weight;
            hist[bucket].sin += weight * rad.sin();
            hist[bucket].cos += weight * rad.cos();
        }
    }

    // All-masked and effectively-grey collapse to the same answer; checking
    // `unmasked` first keeps the mean well-defined (no division by zero).
    if unmasked == 0 || chroma_sum / unmasked as f64 <= MIN_MEAN_CHROMA {
        return None;
    }

    let peak = hist
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.weight.total_cmp(&b.1.weight))
        .map(|(i, _)| i)
        .expect("HUE_BUCKETS > 0");

    // Circular weighted mean over the peak bucket ±1 neighbour (wrapping):
    // the hue vectors span at most a 30° arc, so they cannot cancel — with a
    // non-zero peak weight the summed vector is non-zero and atan2 is safe.
    let mut sin = 0.0;
    let mut cos = 0.0;
    for i in [peak + HUE_BUCKETS - 1, peak, peak + 1] {
        let bucket = &hist[i % HUE_BUCKETS];
        sin += bucket.sin;
        cos += bucket.cos;
    }
    Some(sin.atan2(cos).to_degrees().rem_euclid(360.0) as f32)
}

/// Step (in both axes) that brings `w × h` down to roughly [`MAX_SAMPLES`]
/// pixels; `1` for anything at or under the budget.
fn sample_stride(w: u32, h: u32) -> usize {
    let pixels = u64::from(w) * u64::from(h);
    if pixels <= u64::from(MAX_SAMPLES) {
        return 1;
    }
    (pixels as f64 / f64::from(MAX_SAMPLES)).sqrt().ceil() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Solid-colour image.
    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> RgbImage {
        RgbImage::from_fn(w, h, |_, _| image::Rgb(rgb))
    }

    /// The Oklch hue (positive degrees) of an sRGB colour, through the same
    /// conversion the extractor uses — tests assert against this rather than
    /// hardcoded hue numbers.
    fn hue_of(rgb: [u8; 3]) -> f32 {
        let ok: Oklch = Srgb::new(
            f32::from(rgb[0]) / 255.0,
            f32::from(rgb[1]) / 255.0,
            f32::from(rgb[2]) / 255.0,
        )
        .into_color();
        ok.hue.into_positive_degrees()
    }

    /// An in-gamut sRGB colour built from Oklch components (test-side inverse
    /// of the extractor's conversion; used to place exact hues).
    fn rgb_of_oklch(l: f32, c: f32, h: f32) -> [u8; 3] {
        let srgb: Srgb = Oklch::new(l, c, h).into_color();
        let srgb = srgb.into_format::<u8>();
        [srgb.red, srgb.green, srgb.blue]
    }

    /// Circular distance between two hues in degrees.
    fn circ_diff(a: f32, b: f32) -> f32 {
        let d = (a - b).rem_euclid(360.0);
        d.min(360.0 - d)
    }

    #[test]
    fn solid_vibrant_colours_report_their_own_hue() {
        for rgb in [
            [255, 0, 0],   // red
            [0, 160, 60],  // green
            [0, 90, 220],  // blue
            [230, 120, 0], // orange
            [255, 255, 0], // pure yellow — L ≈ 0.968, must not be masked
        ] {
            let got = dominant_hue(&solid(24, 16, rgb))
                .unwrap_or_else(|| panic!("{rgb:?} should have a dominant hue"));
            assert!(
                circ_diff(got, hue_of(rgb)) <= 5.0,
                "{rgb:?}: got {got}, want ~{}",
                hue_of(rgb)
            );
        }
    }

    #[test]
    fn vibrant_object_on_grey_ground_wins() {
        let object = [0, 90, 220];
        let img = RgbImage::from_fn(30, 30, |x, y| {
            if (10..20).contains(&x) && (10..20).contains(&y) {
                image::Rgb(object)
            } else {
                image::Rgb([128, 128, 128])
            }
        });
        let got = dominant_hue(&img).expect("vibrant object must dominate");
        assert!(
            circ_diff(got, hue_of(object)) <= 5.0,
            "got {got}, want ~{}",
            hue_of(object)
        );
    }

    #[test]
    fn dominant_vibrant_beats_dominant_pixel() {
        // 8× more pixels of a faintly red-tinted ground than of the vibrant
        // blue object: a pixel-count histogram would answer "red-ish", the
        // chroma-weighted one must answer with the object's hue.
        let object = [0, 90, 220];
        let ground = [138, 126, 126];
        let img = RgbImage::from_fn(30, 30, |x, y| {
            if (10..20).contains(&x) && (10..20).contains(&y) {
                image::Rgb(object)
            } else {
                image::Rgb(ground)
            }
        });
        let got = dominant_hue(&img).expect("the object carries the chroma");
        assert!(
            circ_diff(got, hue_of(object)) <= 5.0,
            "got {got}, want the object's ~{} not the ground's ~{}",
            hue_of(object),
            hue_of(ground)
        );
    }

    #[test]
    fn grey_and_near_grey_images_are_none() {
        // Exactly grey: zero chroma everywhere.
        assert_eq!(dominant_hue(&solid(24, 16, [100, 100, 100])), None);
        // Near-grey: a tint far below the mean-chroma cutoff.
        assert_eq!(dominant_hue(&solid(24, 16, [129, 128, 127])), None);
        // Grey gradient (mid-tones only, nothing masked): still None.
        let gradient = RgbImage::from_fn(32, 32, |x, _| {
            let v = 90 + (x * 3) as u8;
            image::Rgb([v, v, v])
        });
        assert_eq!(dominant_hue(&gradient), None);
    }

    #[test]
    fn one_by_one_images_work() {
        let vibrant = [255, 0, 0];
        let got = dominant_hue(&solid(1, 1, vibrant)).expect("one vibrant pixel is enough");
        assert!(circ_diff(got, hue_of(vibrant)) <= 5.0);
        assert_eq!(dominant_hue(&solid(1, 1, [128, 128, 128])), None);
    }

    #[test]
    fn pure_black_and_white_are_masked_without_panicking() {
        // Every pixel masked → None, and no division by zero on the mean.
        assert_eq!(dominant_hue(&solid(8, 8, [0, 0, 0])), None);
        assert_eq!(dominant_hue(&solid(8, 8, [255, 255, 255])), None);
        // A black/white checkerboard masks everything too.
        let checker = RgbImage::from_fn(8, 8, |x, y| {
            if (x + y) % 2 == 0 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            }
        });
        assert_eq!(dominant_hue(&checker), None);
    }

    #[test]
    fn hue_wraps_around_zero() {
        // Half the pixels a few degrees below 360°, half a few above 0°: the
        // circular mean must land near 0°, not near the arithmetic-mean 180°.
        let below = rgb_of_oklch(0.65, 0.1, 355.0);
        let above = rgb_of_oklch(0.65, 0.1, 5.0);
        let img = RgbImage::from_fn(20, 20, |x, _| {
            image::Rgb(if x < 10 { below } else { above })
        });
        let got = dominant_hue(&img).expect("vibrant pinks must dominate");
        assert!(
            circ_diff(got, 0.0) <= 5.0,
            "got {got}, want ~0/360 (wrap-around)"
        );
    }

    #[test]
    fn grey_cutoff_is_size_independent() {
        // The same colour must get the same verdict from a tiny test image
        // and a thumbnail-sized one — the cutoff is a mean, not a total. An
        // absolute total would flip the big image to Some by pixel count.
        let faint = [131, 128, 127];
        assert_eq!(dominant_hue(&solid(4, 4, faint)), None);
        assert_eq!(dominant_hue(&solid(480, 270, faint)), None);

        let vibrant = [0, 90, 220];
        let tiny = dominant_hue(&solid(4, 4, vibrant)).expect("tiny vibrant image");
        let thumb = dominant_hue(&solid(480, 270, vibrant)).expect("thumbnail-sized image");
        assert!(
            circ_diff(tiny, thumb) <= 1.0,
            "tiny {tiny} vs thumb {thumb}"
        );
    }

    #[test]
    fn oversized_images_are_stride_sampled_to_the_same_answer() {
        assert_eq!(sample_stride(480, 270), 1, "the thumbnail is the budget");
        assert!(sample_stride(960, 540) > 1);

        let vibrant = [230, 120, 0];
        let big = dominant_hue(&solid(960, 540, vibrant)).expect("stride-sampled image");
        assert!(circ_diff(big, hue_of(vibrant)) <= 5.0);
    }
}
