// Accent colour derived from the wallpaper (opt-in, off by default) — see
// docs/plans/20260808-accent-from-wallpaper.md.
//
// This module owns the whole colour domain: extraction of the dominant
// *vibrant* hue from the cached 480×270 thumbnail, the hue transplant onto
// COSMIC's own palette tones (with gamut mapping and a WCAG guard), and — in
// later tasks — the theme writer with snapshot/restore and the pure
// apply/disarm decision.
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

use cosmic::cosmic_theme::CosmicPaletteInner;
use cosmic::cosmic_theme::palette::{
    IntoColor, IsWithinBounds, Oklch, Srgb, color_difference::Wcag21RelativeContrast,
    convert::IntoColorUnclamped,
};
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

/// WCAG contrast the transplanted accent must reach against the *better* of
/// pure white and pure black; below it the accent snaps to the palette's own
/// `accent_warm_grey`.
///
/// ⚠️ Not 4.5 (WCAG AA), although the plan first said so: for *any* colour
/// `max(contrast(x, white), contrast(x, black))` has a hard floor of ≈ 4.58 —
/// the two ratios are equal at relative luminance Y ≈ 0.179, where both are
/// `0.229 / 0.05 ≈ 4.58` — so a 4.5 guard is provably unreachable and would
/// contradict its own purpose (notes §4: *reject* mid-luminance accents,
/// which are exactly the colours near that floor). 6.0 rejects the
/// mid-luminance band Y ∈ (0.125, 0.25) while the stock palettes' tone bands
/// measure ≥ 8.3 over the full hue sweep (asserted in the tests), so real
/// palettes never hit the fallback.
const MIN_ACCENT_CONTRAST: f32 = 6.0;

/// Iterations of the chroma binary search in [`srgb_at_tone`]; on a chroma
/// range of ≤ ~0.2 this resolves the boundary to ~1e-8, far below what a
/// `u8`-quantised channel can express.
const GAMUT_SEARCH_STEPS: u32 = 24;

/// Mean Oklch (L, C) of the palette's 8 chromatic `accent_*` fields —
/// `accent_warm_grey` excluded, it is the grey *fallback*, not a tone. The
/// tone band follows the builder's own palette, so a user-customised palette
/// keeps its character. The mean matters for chroma (the stock accents spread
/// ≈ 0.07–0.16 — `accent_blue` alone would give a washed-out accent); for L
/// it is near-trivial (light accents all sit at L ≈ 0.40, dark at ≈ 0.80).
pub fn tone_band(palette: &CosmicPaletteInner) -> (f32, f32) {
    let accents = [
        palette.accent_blue,
        palette.accent_indigo,
        palette.accent_purple,
        palette.accent_pink,
        palette.accent_red,
        palette.accent_orange,
        palette.accent_yellow,
        palette.accent_green,
    ];
    let (mut l, mut c) = (0.0f32, 0.0f32);
    for accent in &accents {
        let ok: Oklch = accent.color.into_color();
        l += ok.l;
        c += ok.chroma;
    }
    let n = accents.len() as f32;
    (l / n, c / n)
}

/// The accent for `palette` at the wallpaper's dominant `hue`: transplant the
/// hue onto the palette's [`tone_band`], gamut-map by chroma reduction at
/// fixed (L, h), then verify with the WCAG guard. `None` (an effectively grey
/// wallpaper) and a guard failure both return the palette's own
/// `accent_warm_grey`.
pub fn accent_for(palette: &CosmicPaletteInner, hue: Option<f32>) -> Srgb {
    let warm_grey = palette.accent_warm_grey.color;
    let Some(hue) = hue else {
        return warm_grey;
    };

    let (l, c) = tone_band(palette);
    let accent = srgb_at_tone(l, c, hue);

    // Single check against the better of white and black — no nudge loop, the
    // stock tone bands sit far above the threshold (see MIN_ACCENT_CONTRAST).
    let contrast = accent
        .relative_contrast(Srgb::new(1.0, 1.0, 1.0))
        .max(accent.relative_contrast(Srgb::new(0.0, 0.0, 0.0)));
    if contrast >= MIN_ACCENT_CONTRAST {
        accent
    } else {
        warm_grey
    }
}

/// `Oklch { l, c, hue }` brought into sRGB by **chroma reduction at fixed
/// (L, h)** — binary search on C down from the requested value. Naive
/// per-channel clamping is not acceptable here: at the real tone bands
/// roughly 140/360 (dark) and 168/360 (light) hues start outside sRGB, and
/// clipping shifts the hue we just went to the trouble of extracting.
fn srgb_at_tone(l: f32, chroma: f32, hue: f32) -> Srgb {
    let candidate = |c: f32| -> Srgb { Oklch::new(l, c, hue).into_color_unclamped() };

    let full = candidate(chroma);
    if full.is_within_bounds() {
        return full;
    }

    // C = 0 is the achromatic axis — inside sRGB for any L in [0, 1] (and the
    // tone-band L is a mean over in-gamut colours, so it is) — which brackets
    // the gamut boundary between `lo` and `hi`. `best` only ever holds a
    // candidate verified in-bounds.
    let (mut lo, mut hi) = (0.0f32, chroma);
    let mut best = candidate(lo);
    for _ in 0..GAMUT_SEARCH_STEPS {
        let mid = (lo + hi) * 0.5;
        let cand = candidate(mid);
        if cand.is_within_bounds() {
            best = cand;
            lo = mid;
        } else {
            hi = mid;
        }
    }
    best
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

    // ---- transplant + gamut mapping + WCAG guard --------------------------

    use cosmic::cosmic_theme::palette::Srgba;
    use cosmic::cosmic_theme::{DARK_PALETTE, LIGHT_PALETTE};

    fn dark_palette() -> &'static CosmicPaletteInner {
        (*DARK_PALETTE).as_ref()
    }

    fn light_palette() -> &'static CosmicPaletteInner {
        (*LIGHT_PALETTE).as_ref()
    }

    /// A palette whose 8 chromatic accents all sit at the given Oklch tone
    /// (hues spread around the wheel) and whose warm grey is a recognisable
    /// sentinel — for exercising `tone_band`'s mean and the guard fallback.
    fn synthetic_palette(l: f32, c: f32, warm_grey: Srgba) -> CosmicPaletteInner {
        let at = |h: f32| -> Srgba {
            let srgb: Srgb = Oklch::new(l, c, h).into_color();
            Srgba::new(srgb.red, srgb.green, srgb.blue, 1.0)
        };
        CosmicPaletteInner {
            accent_blue: at(240.0),
            accent_indigo: at(280.0),
            accent_purple: at(320.0),
            accent_pink: at(0.0),
            accent_red: at(30.0),
            accent_orange: at(60.0),
            accent_yellow: at(100.0),
            accent_green: at(150.0),
            accent_warm_grey: warm_grey,
            ..Default::default()
        }
    }

    fn max_contrast_vs_white_or_black(c: Srgb) -> f32 {
        c.relative_contrast(Srgb::new(1.0, 1.0, 1.0))
            .max(c.relative_contrast(Srgb::new(0.0, 0.0, 0.0)))
    }

    #[test]
    fn tone_band_is_the_mean_of_the_eight_chromatic_accents_only() {
        // All 8 accents share one tone; the warm grey is wildly different. If
        // tone_band averaged it in (or missed an accent), the mean would move.
        let warm_grey = Srgba::new(0.9, 0.9, 0.9, 1.0);
        let palette = synthetic_palette(0.62, 0.11, warm_grey);
        let (l, c) = tone_band(&palette);
        // u8 quantisation in the Srgba round-trip costs a little precision.
        assert!((l - 0.62).abs() < 0.02, "L {l}, want ~0.62");
        assert!((c - 0.11).abs() < 0.02, "C {c}, want ~0.11");
    }

    #[test]
    fn stock_tone_bands_match_their_documented_shape() {
        let (light_l, light_c) = tone_band(light_palette());
        let (dark_l, dark_c) = tone_band(dark_palette());
        // Light accents are dark colours (L ≈ 0.40), dark accents light ones
        // (L ≈ 0.80) — the per-mode tones the plan relies on.
        assert!(
            (0.30..=0.50).contains(&light_l),
            "light tone L {light_l}, want ~0.40"
        );
        assert!(
            (0.70..=0.90).contains(&dark_l),
            "dark tone L {dark_l}, want ~0.80"
        );
        assert!(light_l < dark_l);
        // Real chroma, not washed out (the accents spread ≈ 0.07–0.16).
        for c in [light_c, dark_c] {
            assert!((0.05..=0.20).contains(&c), "tone C {c} out of band");
        }
    }

    #[test]
    fn transplant_preserves_hue_after_gamut_mapping_across_the_full_sweep() {
        for (name, palette) in [("dark", dark_palette()), ("light", light_palette())] {
            let (l, c) = tone_band(palette);
            let warm_grey = palette.accent_warm_grey.color;
            let mut out_of_gamut = 0u32;

            for hue in 0..360 {
                let hue = hue as f32;
                let raw: Srgb = Oklch::new(l, c, hue).into_color_unclamped();
                if !raw.is_within_bounds() {
                    out_of_gamut += 1;
                }

                let accent = accent_for(palette, Some(hue));
                assert!(
                    accent.is_within_bounds(),
                    "{name} h={hue}: accent out of sRGB"
                );
                assert_ne!(
                    accent, warm_grey,
                    "{name} h={hue}: sweep must not hit the warm-grey fallback"
                );

                let ok: Oklch = accent.into_color();
                let got = ok.hue.into_positive_degrees();
                assert!(
                    circ_diff(got, hue) <= 1.5,
                    "{name} h={hue}: gamut mapping shifted hue to {got}"
                );
            }

            // The mapping path must actually run: at the real tone bands a
            // large share of hues starts outside sRGB (≈ 140/360 dark,
            // ≈ 168/360 light).
            assert!(
                out_of_gamut > 90,
                "{name}: only {out_of_gamut}/360 hues out of gamut — mapping untested"
            );
        }
    }

    #[test]
    fn stock_sweep_clears_the_guard_with_the_documented_margin() {
        // The no-nudge-loop decision rests on the stock palettes measuring
        // ≥ 8.3 against the better of white/black over all hues — re-verify
        // rather than trust the plan's number.
        for (name, palette) in [("dark", dark_palette()), ("light", light_palette())] {
            for hue in 0..360 {
                let contrast =
                    max_contrast_vs_white_or_black(accent_for(palette, Some(hue as f32)));
                assert!(
                    contrast >= 8.3,
                    "{name} h={hue}: contrast {contrast} under the documented 8.3"
                );
            }
        }
    }

    #[test]
    fn light_and_dark_palettes_give_different_tones_for_the_same_hue() {
        for hue in [10.0, 145.0, 250.0] {
            let light = accent_for(light_palette(), Some(hue));
            let dark = accent_for(dark_palette(), Some(hue));
            assert_ne!(light, dark, "h={hue}: modes must differ");
            let light_ok: Oklch = light.into_color();
            let dark_ok: Oklch = dark.into_color();
            assert!(
                light_ok.l < dark_ok.l,
                "h={hue}: light-mode accent must be the darker tone"
            );
        }
    }

    #[test]
    fn grey_hue_returns_the_palettes_own_warm_grey() {
        for palette in [dark_palette(), light_palette()] {
            assert_eq!(accent_for(palette, None), palette.accent_warm_grey.color);
        }
        // …the *palette's* warm grey, not a hardcoded one.
        let sentinel = Srgba::new(0.25, 0.5, 0.75, 1.0);
        let custom = synthetic_palette(0.62, 0.11, sentinel);
        assert_eq!(accent_for(&custom, None), sentinel.color);
    }

    #[test]
    fn mid_luminance_palette_triggers_the_warm_grey_fallback() {
        // Accents at Oklch L 0.56 sit near relative luminance Y ≈ 0.18, where
        // white and black are *both* mediocre (max contrast ≈ 4.6 — the exact
        // failure case notes §4 describes). Low chroma keeps Y pinned there
        // across hues. The guard must reject this and take the fallback — the
        // branch is unreachable on the stock palettes, so this synthetic
        // palette is what keeps it from being dead code.
        let sentinel = Srgba::new(0.2, 0.18, 0.17, 1.0);
        let palette = synthetic_palette(0.56, 0.03, sentinel);
        for hue in [0.0, 80.0, 160.0, 240.0, 320.0] {
            let accent = accent_for(&palette, Some(hue));
            assert_eq!(
                accent, sentinel.color,
                "h={hue}: mid-luminance tone must fall back to warm grey"
            );
        }
    }
}
