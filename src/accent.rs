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

use cosmic::cosmic_config::{self, Config, ConfigGet, CosmicConfigEntry};
use cosmic::cosmic_theme::palette::{
    IntoColor, IsWithinBounds, Oklch, Srgb, color_difference::Wcag21RelativeContrast,
    convert::IntoColorUnclamped,
};
use cosmic::cosmic_theme::{
    CosmicPalette, CosmicPaletteInner, DARK_THEME_BUILDER_ID, DARK_THEME_ID,
    LIGHT_THEME_BUILDER_ID, LIGHT_THEME_ID, Theme, ThemeBuilder,
};
use image::RgbImage;
use serde::{Deserialize, Serialize};

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

// ---- persisted colour types -------------------------------------------------
//
// All colours that `config.rs` persists (and that the don't-clobber rule
// compares) live in 8-bit `[u8; 3]` space: it keeps `Eq` on `AppletConfig`
// and makes every comparison exact — no float epsilon anywhere. The bridge to
// the f32 `Srgb` cosmic-theme wants is quantise-then-convert: the f32 we write
// is exactly `u8 / 255`, so a later read-back re-quantises to the same bytes.

/// One accent per mode, as last written by us — persisted so the
/// don't-clobber comparison survives applet restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccentPair {
    pub light: [u8; 3],
    pub dark: [u8; 3],
}

/// The user's accents captured at enable time, restored verbatim on disable.
/// An inner `None` means "the user had the palette default" (builder `accent`
/// key unset) — restoring must write that `None` back, not skip the mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccentSnapshot {
    pub light: Option<[u8; 3]>,
    pub dark: Option<[u8; 3]>,
}

/// Quantise a computed accent into the persisted/compared 8-bit space.
pub fn quantize(srgb: Srgb) -> [u8; 3] {
    let q = srgb.into_format::<u8>();
    [q.red, q.green, q.blue]
}

/// The exact f32 colour written for a persisted 8-bit one: each channel is
/// precisely `u8 / 255`, so `quantize(unquantize(x)) == x` by construction —
/// that identity is what makes the don't-clobber comparison exact.
pub fn unquantize(rgb: [u8; 3]) -> Srgb {
    Srgb::new(
        f32::from(rgb[0]) / 255.0,
        f32::from(rgb[1]) / 255.0,
        f32::from(rgb[2]) / 255.0,
    )
}

// ---- theme writer -----------------------------------------------------------

/// The two theme modes COSMIC keeps side by side; both are written on every
/// accent change so a light/dark flip needs no work from us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Light,
    Dark,
}

/// The four cosmic-config handles the accent writer touches (light/dark ×
/// builder/theme). Built once against the real per-user configs in
/// production; tests root all four in a `TempDir`.
#[derive(Debug)]
pub struct ThemeHandles {
    light_builder: Config,
    dark_builder: Config,
    light_theme: Config,
    dark_theme: Config,
}

impl ThemeHandles {
    /// Handles on the user's real COSMIC theme configs.
    pub fn system() -> Result<Self, cosmic_config::Error> {
        Ok(Self {
            light_builder: Config::new(LIGHT_THEME_BUILDER_ID, ThemeBuilder::VERSION)?,
            dark_builder: Config::new(DARK_THEME_BUILDER_ID, ThemeBuilder::VERSION)?,
            light_theme: Config::new(LIGHT_THEME_ID, Theme::VERSION)?,
            dark_theme: Config::new(DARK_THEME_ID, Theme::VERSION)?,
        })
    }

    /// Handles rooted in `root` via `Config::with_custom_path` — tests never
    /// touch the real user config. Note the different read path: a custom
    /// path has `system_path: None`, so absent keys yield `NoConfigDirectory`
    /// rather than a `/usr/share/cosmic` default.
    #[cfg(test)]
    pub fn sandboxed(root: &std::path::Path) -> Result<Self, cosmic_config::Error> {
        let root = root.to_path_buf();
        Ok(Self {
            light_builder: Config::with_custom_path(
                LIGHT_THEME_BUILDER_ID,
                ThemeBuilder::VERSION,
                root.clone(),
            )?,
            dark_builder: Config::with_custom_path(
                DARK_THEME_BUILDER_ID,
                ThemeBuilder::VERSION,
                root.clone(),
            )?,
            light_theme: Config::with_custom_path(LIGHT_THEME_ID, Theme::VERSION, root.clone())?,
            dark_theme: Config::with_custom_path(DARK_THEME_ID, Theme::VERSION, root)?,
        })
    }

    fn builder_cfg(&self, mode: Mode) -> &Config {
        match mode {
            Mode::Light => &self.light_builder,
            Mode::Dark => &self.dark_builder,
        }
    }

    fn theme_cfg(&self, mode: Mode) -> &Config {
        match mode {
            Mode::Light => &self.light_theme,
            Mode::Dark => &self.dark_theme,
        }
    }

    /// Read `mode`'s `ThemeBuilder`, accepting the partial on `Err` (per-key
    /// degradation, like `AppletConfig::load`) — then **re-probe the `palette`
    /// key directly** and substitute the mode's own default on failure.
    ///
    /// The probe is not optional: `get_entry` starts from `Self::default()` —
    /// the **dark** palette — and silently skips `NoConfigDirectory` errors,
    /// so a light builder whose `palette` key is absent (the normal state:
    /// nothing pins it user-locally) comes back with the dark palette on the
    /// *Ok* path. Trusting it would give light mode dark-tuned tones and, on
    /// our theme write, flip the light theme dark wholesale.
    pub fn read_builder(&self, mode: Mode) -> ThemeBuilder {
        let cfg = self.builder_cfg(mode);
        let mut builder = match ThemeBuilder::get_entry(cfg) {
            Ok(builder) => builder,
            Err((errors, partial)) => {
                for error in errors.iter().filter(|error| error.is_err()) {
                    tracing::warn!(
                        ?mode,
                        "invalid theme-builder entry (using default): {error}"
                    );
                }
                partial
            }
        };
        builder.palette =
            ConfigGet::get::<CosmicPalette>(cfg, "palette").unwrap_or_else(|_| match mode {
                Mode::Light => ThemeBuilder::light().palette,
                Mode::Dark => ThemeBuilder::dark().palette,
            });
        builder
    }
}

/// Each builder's current accent override, quantised into the 8-bit space the
/// don't-clobber comparison happens in. `None` = palette default (key unset).
pub fn read_current_accents(handles: &ThemeHandles) -> (Option<[u8; 3]>, Option<[u8; 3]>) {
    (
        handles.read_builder(Mode::Light).accent.map(quantize),
        handles.read_builder(Mode::Dark).accent.map(quantize),
    )
}

/// Write the computed accents to both modes. Failure leaves whatever half
/// completed on disk consistent per mode (builder and theme are written
/// together per mode); callers treat any `Err` as "log and change nothing
/// else" per the plan's failure rule.
pub fn write_accents(
    handles: &ThemeHandles,
    light: [u8; 3],
    dark: [u8; 3],
) -> Result<(), cosmic_config::Error> {
    write_mode_accent(handles, Mode::Light, Some(unquantize(light)))?;
    write_mode_accent(handles, Mode::Dark, Some(unquantize(dark)))
}

/// Restore a snapshot verbatim — including an inner `None`, which writes the
/// "palette default" state back (builder accent unset) rather than skipping.
pub fn restore_accents(
    handles: &ThemeHandles,
    snapshot: AccentSnapshot,
) -> Result<(), cosmic_config::Error> {
    write_mode_accent(handles, Mode::Light, snapshot.light.map(unquantize))?;
    write_mode_accent(handles, Mode::Dark, snapshot.dark.map(unquantize))
}

/// One mode's write, per the recipe that supersedes notes §2:
///
/// 1. `set_accent` — the derive's generated **single-key setter**: only the
///    `accent` key lands on disk, nothing else gets pinned user-locally
///    (`write_entry` on the builder would materialise *every* key and cut the
///    user off from future COSMIC default changes). The setter serialises the
///    bare `Option<Srgb>` — exact-f32 RON, so our quantise-then-convert value
///    round-trips bit-exactly.
/// 2. `build().write_entry` — the derived `Theme` *is* a full-entry write;
///    that matches upstream (cosmic-settings does the same) and both writes
///    are required: nothing on the system rebuilds the theme from the builder.
fn write_mode_accent(
    handles: &ThemeHandles,
    mode: Mode,
    accent: Option<Srgb>,
) -> Result<(), cosmic_config::Error> {
    let mut builder = handles.read_builder(mode);
    builder.set_accent(handles.builder_cfg(mode), accent)?;
    builder.build().write_entry(handles.theme_cfg(mode))
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

    // ---- theme writer + persisted colour types ----------------------------

    use cosmic::cosmic_theme::Component;

    /// The derived theme's accent, read back from the theme config's own
    /// `accent` key (a `Component`), quantised to 8-bit. Reading the raw key
    /// rather than `Theme::get_entry` keeps the test off `Theme::default()`,
    /// which probes the real user's theme-mode config.
    fn theme_accent(handles: &ThemeHandles, mode: Mode) -> [u8; 3] {
        let component: Component = ConfigGet::get(handles.theme_cfg(mode), "accent")
            .expect("derived theme must have an accent component on disk");
        let base = component.base.into_format::<u8, u8>();
        assert_eq!(base.alpha, 255, "accent must be opaque");
        [base.red, base.green, base.blue]
    }

    #[test]
    fn quantize_unquantize_is_exact_for_every_channel_value() {
        // The don't-clobber comparison is exact only if the f32 we write
        // re-quantises to the same bytes — for every possible channel value.
        for v in 0..=255u8 {
            let rgb = [v, 255 - v, v.wrapping_mul(37)];
            assert_eq!(quantize(unquantize(rgb)), rgb, "{rgb:?}");
        }
    }

    #[test]
    fn write_accents_roundtrips_exactly_through_the_ron_files() {
        let dir = tempfile::tempdir().unwrap();
        let handles = ThemeHandles::sandboxed(dir.path()).unwrap();

        // Channel values whose /255 quotients are not exactly representable
        // powers of two — the case a lossy write path would corrupt.
        let light = [7, 133, 217];
        let dark = [250, 41, 90];
        write_accents(&handles, light, dark).expect("write accents");

        // Builder accents read back to the exact same bytes…
        assert_eq!(
            read_current_accents(&handles),
            (Some(light), Some(dark)),
            "builder accent must round-trip exactly"
        );
        // …and each derived theme was rebuilt with that accent as its base.
        assert_eq!(theme_accent(&handles, Mode::Light), light);
        assert_eq!(theme_accent(&handles, Mode::Dark), dark);
        // The derived themes keep their modes (regression guard against the
        // dark-default palette leak flipping the light theme dark).
        let light_is_dark: bool =
            ConfigGet::get(handles.theme_cfg(Mode::Light), "is_dark").unwrap();
        let dark_is_dark: bool = ConfigGet::get(handles.theme_cfg(Mode::Dark), "is_dark").unwrap();
        assert!(!light_is_dark);
        assert!(dark_is_dark);
    }

    #[test]
    fn restore_accents_restores_some_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let handles = ThemeHandles::sandboxed(dir.path()).unwrap();

        // The user had explicit accents; we overwrote them; disable restores.
        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: Some([200, 100, 50]),
        };
        restore_accents(&handles, user).expect("seed the user's accents");
        write_accents(&handles, [1, 2, 3], [4, 5, 6]).expect("our overwrite");
        assert_eq!(
            read_current_accents(&handles),
            (Some([1, 2, 3]), Some([4, 5, 6]))
        );

        restore_accents(&handles, user).expect("restore");
        assert_eq!(read_current_accents(&handles), (user.light, user.dark));
        assert_eq!(theme_accent(&handles, Mode::Light), [10, 20, 30]);
        assert_eq!(theme_accent(&handles, Mode::Dark), [200, 100, 50]);
    }

    #[test]
    fn restore_accents_restores_the_palette_default_none() {
        let dir = tempfile::tempdir().unwrap();
        let handles = ThemeHandles::sandboxed(dir.path()).unwrap();

        write_accents(&handles, [1, 2, 3], [4, 5, 6]).expect("our overwrite");

        // Inner `None` = "user had the palette default": the restore must
        // write that state back (unset the override), not skip the mode.
        let snapshot = AccentSnapshot {
            light: None,
            dark: None,
        };
        restore_accents(&handles, snapshot).expect("restore to default");
        assert_eq!(read_current_accents(&handles), (None, None));

        // The rebuilt themes fall back to each palette's own default accent
        // (`accent_blue` — theme.rs's build() None branch).
        assert_eq!(
            theme_accent(&handles, Mode::Light),
            quantize(light_palette().accent_blue.color)
        );
        assert_eq!(
            theme_accent(&handles, Mode::Dark),
            quantize(dark_palette().accent_blue.color)
        );
    }

    #[test]
    fn light_builder_with_absent_palette_key_gets_the_light_palette() {
        // The dark-default-leak regression test: `get_entry` starts from
        // `Self::default()` (the DARK palette) and silently skips absent
        // keys, so without the explicit probe a light builder read would
        // come back dark on the Ok path.
        let dir = tempfile::tempdir().unwrap();
        let handles = ThemeHandles::sandboxed(dir.path()).unwrap();

        let builder = handles.read_builder(Mode::Light);
        assert!(
            matches!(builder.palette, CosmicPalette::Light(_)),
            "light builder must carry the Light palette variant"
        );
        assert_eq!(builder.palette.as_ref(), light_palette());

        // …and the built theme is a light theme through and through.
        let theme = builder.build();
        assert!(!theme.is_dark);
        assert_eq!(&theme.palette, light_palette());

        // The dark builder keeps its own mode too.
        let builder = handles.read_builder(Mode::Dark);
        assert!(matches!(builder.palette, CosmicPalette::Dark(_)));
        assert_eq!(builder.palette.as_ref(), dark_palette());
    }

    #[test]
    fn builder_write_touches_only_the_accent_key() {
        let dir = tempfile::tempdir().unwrap();
        let handles = ThemeHandles::sandboxed(dir.path()).unwrap();

        write_accents(&handles, [7, 133, 217], [250, 41, 90]).expect("write accents");

        // `set_accent` is a single-key write: pinning any other builder key
        // user-locally would cut the user off from future COSMIC default
        // changes (a full `write_entry` materialises every key).
        for id in [LIGHT_THEME_BUILDER_ID, DARK_THEME_BUILDER_ID] {
            let keys = key_files_under(&dir.path().join("cosmic").join(id));
            assert_eq!(
                keys,
                vec!["accent".to_string()],
                "{id}: builder must contain only the accent key"
            );
        }
    }

    /// Every key *file* under a config root (recursively), sorted by name —
    /// the version directories themselves don't count.
    fn key_files_under(root: &std::path::Path) -> Vec<String> {
        fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path.file_name().unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out);
        out.sort();
        out
    }
}
