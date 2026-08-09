// Accent colour derived from the wallpaper (opt-in, off by default) — see
// docs/plans/20260808-accent-from-wallpaper.md.
//
// This module owns the whole colour domain: extraction of the dominant
// *vibrant* hue from the cached 480×270 thumbnail, the hue transplant onto
// COSMIC's own palette tones (with gamut mapping and a WCAG guard), the theme
// writer with snapshot/restore, and the pure apply/disarm decision
// (`accent_plan`) that `app.rs` executes.
//
// Extraction is a chroma-weighted hue histogram in Oklch, the shape borrowed
// from the GNOME extension's dominant-with-grey-fallback rule
// (examples/bing-wallpaper-gnome-extension is a different algorithm, but
// `GNOME-Auto-Accent-Colour`'s `saturation < 5% → slate` fallback becomes our
// mean-chroma cutoff). Weighting by chroma is what makes the result
// dominant-*vibrant* rather than dominant-pixel: a small saturated subject
// beats a large washed-out ground because grey pixels carry ~zero weight.

use cosmic::cosmic_config::{self, Config, ConfigGet, ConfigSet, CosmicConfigEntry};
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
            let ok: Oklch = unquantize([p[0], p[1], p[2]]).into_color();

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
/// production; tests root all four in a `TempDir`. `Clone` because the theme
/// writes run on the blocking pool (`app.rs`'s accent tasks) and each task
/// takes its own copy — a `Config` is just paths.
#[derive(Debug, Clone)]
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
        builder.palette = ConfigGet::get::<CosmicPalette>(cfg, "palette").unwrap_or_else(|error| {
            // An absent key is the normal state (nothing pins the palette
            // user-locally) and must stay silent; anything else — an
            // unreadable file, corrupt RON — deserves the same trace as
            // the `get_entry` errors above.
            if palette_key_present(&error) {
                tracing::warn!(
                    ?mode,
                    "invalid theme-builder palette (using default): {error}"
                );
            }
            match mode {
                Mode::Light => ThemeBuilder::light().palette,
                Mode::Dark => ThemeBuilder::dark().palette,
            }
        });
        builder
    }
}

/// Whether the failed `palette` probe found *something* on disk (as opposed
/// to the key simply not existing anywhere). `NoConfigDirectory`/`NotFound`
/// are the sandboxed/user-local absent cases; a `GetKey` wrapping io
/// `NotFound` is the system-default lookup missing the file.
fn palette_key_present(error: &cosmic_config::Error) -> bool {
    match error {
        cosmic_config::Error::NoConfigDirectory | cosmic_config::Error::NotFound => false,
        cosmic_config::Error::GetKey(_, io) => io.kind() != std::io::ErrorKind::NotFound,
        _ => true,
    }
}

/// The two builders' current accent overrides `(light, dark)`, quantised into
/// the 8-bit space the don't-clobber comparison happens in. `None` = palette
/// default (builder `accent` key unset).
pub type BuilderAccents = (Option<[u8; 3]>, Option<[u8; 3]>);

/// The two theme builders as one `(light, dark)` pair — the unit every
/// recompute reads once ([`read_builders`]) and threads through the plan
/// ([`accent_plan`]) and the write ([`write_accents`]). Each builder carries
/// both inputs a mode needs: its palette (tone band) and its current accent.
pub type Builders = (ThemeBuilder, ThemeBuilder);

/// Both builders, freshly read `(light, dark)` — one read serving both the
/// palettes (tone bands) and the current accents of a recompute, and reused
/// by [`write_accents`] so the write does not parse the same files again.
pub fn read_builders(handles: &ThemeHandles) -> Builders {
    (
        handles.read_builder(Mode::Light),
        handles.read_builder(Mode::Dark),
    )
}

/// The accent overrides of already-read builders, quantised into the 8-bit
/// don't-clobber space.
pub fn builder_accents(builders: &Builders) -> BuilderAccents {
    (
        builders.0.accent.map(quantize),
        builders.1.accent.map(quantize),
    )
}

/// Each builder's current accent override, quantised into the 8-bit space the
/// don't-clobber comparison happens in. `None` = palette default (key unset).
pub fn read_current_accents(handles: &ThemeHandles) -> BuilderAccents {
    builder_accents(&read_builders(handles))
}

/// Write the computed accents to both modes, onto the builders the caller
/// already read for this recompute (no re-read, no TOCTOU window between the
/// plan's compare and the write).
///
/// On failure, whatever (possibly) landed is **rolled back** to the builders'
/// original accents — the exact `Option<Srgb>` values read, per mode,
/// best-effort. The rollback is not optional politeness: a half-write left on
/// disk (light landed, dark failed — or a builder key landed without its
/// theme) holds *our* colour, which the next recompute's don't-clobber guard
/// cannot tell from the user intervening, so it would [`AccentAction::Disarm`]
/// — clearing the snapshot **without restoring** — and the user's pre-feature
/// accent would be unrecoverable. Callers still treat any `Err` as "log and
/// change nothing else" per the plan's failure rule; a clean rollback means
/// the guard holds and the next recompute simply retries.
pub fn write_accents(
    handles: &ThemeHandles,
    builders: Builders,
    light: [u8; 3],
    dark: [u8; 3],
) -> Result<(), cosmic_config::Error> {
    let previous = (builders.0.accent, builders.1.accent);
    let (light_builder, dark_builder) = builders;
    let rollback = |mode: Mode, accent: Option<Srgb>| {
        if let Err(error) = write_mode_accent(handles, mode, accent) {
            tracing::warn!(
                ?mode,
                "failed to roll back the accent after a write failure: {error}"
            );
        }
    };
    if let Err(error) =
        write_builder_accent(handles, Mode::Light, light_builder, Some(unquantize(light)))
    {
        // Light may be half-written (builder key without theme); dark was
        // never attempted — rolling it back too would only rewrite an
        // untouched theme and fire change notifications for nothing.
        rollback(Mode::Light, previous.0);
        return Err(error);
    }
    if let Err(error) =
        write_builder_accent(handles, Mode::Dark, dark_builder, Some(unquantize(dark)))
    {
        // Light landed fully; dark may be half-written. Both go back.
        rollback(Mode::Light, previous.0);
        rollback(Mode::Dark, previous.1);
        return Err(error);
    }
    Ok(())
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

/// One mode's write from a self-contained fresh read — the restore path,
/// which has no already-read builder on hand.
fn write_mode_accent(
    handles: &ThemeHandles,
    mode: Mode,
    accent: Option<Srgb>,
) -> Result<(), cosmic_config::Error> {
    write_builder_accent(handles, mode, handles.read_builder(mode), accent)
}

/// One mode's write onto an already-read builder, per the recipe that
/// supersedes notes §2:
///
/// 1. `set_accent` — the derive's generated **single-key setter**: only the
///    `accent` key lands on disk, nothing else gets pinned user-locally
///    (`write_entry` on the builder would materialise *every* key and cut the
///    user off from future COSMIC default changes). The setter serialises the
///    bare `Option<Srgb>` — exact-f32 RON, so our quantise-then-convert value
///    round-trips bit-exactly.
/// 2. [`write_theme`] with `build()`'s result — a **changed-keys-only
///    transaction** against the on-disk derived theme (the pattern upstream
///    cosmic-settings uses in `theme_manager.rs`'s `build_theme`; both writes
///    are required — nothing on the system rebuilds the theme from the
///    builder). The first version of this module did a full `Theme`
///    `write_entry` here (~40 fsync'd key files per mode, per write) while
///    *claiming* to match upstream; the 2026-08-08 btrfs freeze traced
///    straight to that fsync count, and the diff transaction is the fix at
///    the source.
fn write_builder_accent(
    handles: &ThemeHandles,
    mode: Mode,
    mut builder: ThemeBuilder,
    accent: Option<Srgb>,
) -> Result<(), cosmic_config::Error> {
    builder.set_accent(handles.builder_cfg(mode), accent)?;
    write_theme(handles.theme_cfg(mode), &builder.build())
}

/// Write `new` onto the derived-theme config, touching **only the keys whose
/// value actually changed** — one transaction, mirroring upstream
/// cosmic-settings (`build_theme`): read the current on-disk theme
/// (`Theme::get_entry`, accepting the partial on `Err`), compare field by
/// field, `tx.set` the differences, commit. For an accent-only change that is
/// a handful of keys instead of ~40 — fewer fsyncs (the 2026-08-08 btrfs
/// freeze) *and* a much smaller window in which other COSMIC processes can
/// read a torn theme (cosmic-config transactions are not reader-atomic; the
/// panel logged `GetKey("list_button", NotFound)` mid-write during the
/// incident).
///
/// **Virgin-dir exception**: when the theme entry has never been written
/// (probed via its `is_dark` key), fall back to a full `write_entry`.
/// Upstream diffs against `get_entry`'s fallback default — but that default
/// is `Theme::preferred_theme()`, which depends on the *environment*
/// (`XDG_CURRENT_DESKTOP`/GNOME colour scheme), so on an empty dir the diff
/// would nondeterministically skip keys that happen to match it — including
/// `is_dark`, the exact dark-leak class `read_builder`'s palette probe exists
/// to prevent. Materialising the full entry once keeps the first write
/// deterministic and self-contained; every later write diffs against it.
///
/// The diff covers **every** field of the pinned `Theme` (upstream's
/// hand-rolled list omits several); an omitted-but-changed field would leave
/// the on-disk theme permanently torn. The four `ColorRepr`-annotated fields
/// (`shade`, `accent_text`, `control_tint`, `text_tint`) are written as bare
/// palette values, like upstream: `ColorRepr` is `#[serde(untagged)]` with
/// `Rgb`/`Rgba` variants, so readers parse them fine, and the bare f32 form
/// round-trips exactly (the hex repr quantises to `u8`, which would re-flag
/// the key as changed on every subsequent diff).
///
/// The diff itself compares **serialized bytes**, not values
/// ([`would_rewrite`]): every colour inside `Component`/`Container`
/// serializes through the lossy hex `ColorRepr` quantisation, so a freshly
/// built value practically never `PartialEq`-equals its own disk round-trip
/// even when the text a write would produce is identical. Upstream diffs in
/// value space and silently rewrites every colour-bearing key byte-for-byte
/// on an unchanged mode (~19 pointless fsyncs — half the freeze's I/O);
/// "would this key's bytes change?" is the only predicate that actually
/// delivers the changed-keys-only transaction.
fn write_theme(cfg: &Config, new: &Theme) -> Result<(), cosmic_config::Error> {
    if ConfigGet::get::<bool>(cfg, "is_dark").is_err() {
        return new.write_entry(cfg);
    }
    let current = match Theme::get_entry(cfg) {
        Ok(theme) => theme,
        // Per-key degradation, like `read_builder`: unreadable keys diff as
        // their defaults, so the write repairs them.
        Err((_errors, partial)) => partial,
    };
    let tx = cfg.transaction();
    macro_rules! set_changed {
        ($key:literal, $field:ident) => {
            if would_rewrite(&current.$field, &new.$field) {
                ConfigSet::set(&tx, $key, &new.$field)?;
            }
        };
        ($key:literal, $accessor:ident($transparent:literal)) => {
            if would_rewrite(current.$accessor($transparent), new.$accessor($transparent)) {
                ConfigSet::set(&tx, $key, new.$accessor($transparent))?;
            }
        };
    }
    set_changed!("name", name);
    set_changed!("background", background(false));
    set_changed!("transparent_background", background(true));
    set_changed!("primary", primary(false));
    set_changed!("transparent_primary", primary(true));
    set_changed!("secondary", secondary(false));
    set_changed!("transparent_secondary", secondary(true));
    set_changed!("button", button);
    set_changed!("accent", accent);
    set_changed!("success", success);
    set_changed!("destructive", destructive);
    set_changed!("warning", warning);
    set_changed!("accent_button", accent_button);
    set_changed!("success_button", success_button);
    set_changed!("destructive_button", destructive_button);
    set_changed!("warning_button", warning_button);
    set_changed!("icon_button", icon_button);
    set_changed!("link_button", link_button);
    set_changed!("list_button", list_button);
    set_changed!("text_button", text_button);
    set_changed!("palette", palette);
    set_changed!("spacing", spacing);
    set_changed!("corner_radii", corner_radii);
    set_changed!("is_dark", is_dark);
    set_changed!("is_high_contrast", is_high_contrast);
    set_changed!("gaps", gaps);
    set_changed!("active_hint", active_hint);
    set_changed!("window_hint", window_hint);
    set_changed!("frosted", frosted);
    set_changed!("frosted_windows", frosted_windows);
    set_changed!("frosted_system_interface", frosted_system_interface);
    set_changed!("frosted_panel", frosted_panel);
    set_changed!("frosted_applets", frosted_applets);
    set_changed!("frosted_maximized_apps", frosted_maximized_apps);
    set_changed!("alpha_map", alpha_map);
    set_changed!("shade", shade);
    set_changed!("accent_text", accent_text);
    set_changed!("control_tint", control_tint);
    set_changed!("text_tint", text_tint);
    tx.commit()
}

/// Whether writing `new` in place of `current` would change the key file's
/// bytes — the diff predicate of [`write_theme`]. Serializes both sides
/// exactly like the transaction's `set` does (`ron::ser::to_string_pretty`,
/// default `PrettyConfig`; same `ron` 0.12.x as cosmic-config) and compares
/// the text: colour-bearing theme fields round-trip lossily in value space
/// (hex `ColorRepr` quantisation), so a `PartialEq` compare flags them as
/// changed forever even when the bytes are identical. A value that fails to
/// serialize counts as changed, so the transaction's own `set` surfaces the
/// error instead of it being swallowed here.
fn would_rewrite<T: serde::Serialize>(current: &T, new: &T) -> bool {
    match (
        ron::ser::to_string_pretty(current, ron::ser::PrettyConfig::new()),
        ron::ser::to_string_pretty(new, ron::ser::PrettyConfig::new()),
    ) {
        (Ok(current), Ok(new)) => current != new,
        _ => true,
    }
}

// ---- pure apply/disarm decision ---------------------------------------------

/// What the `AccentComputed` handler should do — the tested analogue of
/// `refresh_success_plan` / `wallpaper::classify`. The handler executes it
/// against live state:
///
/// - [`AccentAction::Write`]: write both accents ([`write_accents`]) and
///   persist `last_written`; when `snapshot_now` is set, first capture the
///   *current* builder accents as the snapshot (they are still the user's —
///   nothing of ours has landed yet).
/// - [`AccentAction::Disarm`]: the accent no longer matches what we wrote —
///   flip `accent_enabled` off through the setter and clear last-written
///   **without restoring**; a manual choice stands. `keep_snapshot` says
///   whether the snapshot survives the disarm: `false` after a *recorded*
///   write (`last_written` proves ours landed, so the mismatch is genuinely
///   the user — their pick supersedes the pre-feature record), `true` in the
///   enable→first-write gap (`last_written` still `None`), where the mismatch
///   cannot be told apart from our own **unrecorded** write — a crash or a
///   failed persist between the theme write and its `last_written` record —
///   and clearing would permanently destroy the only record of the user's
///   pre-feature accents while ours stay on disk. The kept snapshot is
///   reconciled by the next enable's deferred restore.
/// - [`AccentAction::Skip`]: change nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccentAction {
    Write {
        light: [u8; 3],
        dark: [u8; 3],
        snapshot_now: bool,
    },
    Disarm {
        keep_snapshot: bool,
    },
    Skip,
}

/// The pure decision behind every accent recompute (apply paths and the
/// startup reconciliation alike).
///
/// - `snapshot` / `last_written`: the persisted `AppletConfig` state.
/// - `builders`: the freshly read `(light, dark)` pair ([`read_builders`]).
///   Each builder carries both per-mode inputs — its accent override right
///   now (`None` = palette default) and its own palette (probed by
///   [`ThemeHandles::read_builder`]), so a user-customised palette keeps its
///   tone band. Taking the pair keeps the modes attached to their palettes;
///   two loose `&CosmicPaletteInner` arguments could be swapped without a
///   compile error, silently inverting the per-mode tones.
/// - `hue`: the wallpaper's dominant hue; `None` (an effectively grey image)
///   still writes — the palette's warm grey, per [`accent_for`].
///
/// Don't-clobber: once `last_written` exists, both builders must still hold
/// exactly those bytes (`[u8; 3]` compare — exact by the quantise-then-convert
/// construction). Any difference, including a mode reset to palette default,
/// means the user intervened → [`AccentAction::Disarm`] (clearing the
/// snapshot — their pick supersedes it). Before the first successful write
/// (`last_written` still `None`) the enable-time snapshot stands in: the only
/// accents legitimately on disk then are the snapshot's own, so a mismatch
/// there disarms too — without it the guard would be inert until a write
/// finally lands. That gap mismatch, though, is ambiguous: it is the user
/// intervening (a Settings pick between enable and the first compute) *or*
/// our own write whose `last_written` record never made it to disk (a crash
/// or failed persist in the write→record window), and the two cannot be told
/// apart. So the gap disarm keeps the snapshot (`keep_snapshot: true`) — the
/// least-lossy rule: a genuine gap pick still stands now (no restore), while
/// the pre-feature record survives for the next enable's deferred restore
/// instead of being destroyed over what may be our own leftovers.
///
/// Steady state: when the computed pair equals `last_written`, [`AccentAction::Skip`]
/// — the non-Disarm path just proved the builders hold exactly those bytes,
/// so disk is already right, and a write would rewrite both derived themes
/// key-for-key and fire change notifications into every running COSMIC app
/// for no change at all (every startup reconciliation and same-hue apply
/// lands here).
///
/// Snapshot lifecycle: the plan never takes the snapshot itself — it flags
/// `snapshot_now` when none is persisted, so the executor captures the user's
/// accents before our first write. Disable and disarm both clear the
/// snapshot, so a re-enable re-snapshots through the same flag; a snapshot
/// that *survives* into an enable (kept by a disable that could not restore)
/// is restored to disk and kept by the toggler, never re-captured — the disk
/// may still hold our own accents from the earlier run.
///
/// Single-instance assumption: two applet instances (or two machines syncing
/// one config) each write and then read the *other's* accents as an external
/// change, so the second instance disarms spuriously. Accepted — the applet
/// is a panel applet, one per session.
pub fn accent_plan(
    enabled: bool,
    snapshot: Option<AccentSnapshot>,
    last_written: Option<AccentPair>,
    builders: &Builders,
    hue: Option<f32>,
) -> AccentAction {
    if !enabled {
        return AccentAction::Skip;
    }
    let current = builder_accents(builders);
    match (last_written, snapshot) {
        (Some(last), _) if current != (Some(last.light), Some(last.dark)) => {
            return AccentAction::Disarm {
                keep_snapshot: false,
            };
        }
        (None, Some(snap)) if current != (snap.light, snap.dark) => {
            return AccentAction::Disarm {
                keep_snapshot: true,
            };
        }
        _ => {}
    }
    let light = quantize(accent_for(builders.0.palette.as_ref(), hue));
    let dark = quantize(accent_for(builders.1.palette.as_ref(), hue));
    if last_written == Some(AccentPair { light, dark }) {
        return AccentAction::Skip;
    }
    AccentAction::Write {
        light,
        dark,
        snapshot_now: snapshot.is_none(),
    }
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
        let ok: Oklch = unquantize(rgb).into_color();
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

        // The vibrant content deliberately avoids the origin: everything
        // inside the top-left thumbnail-sized window is grey, so a sampler
        // biased toward the start of the image (only the first rows, only a
        // budget-sized prefix) answers None while a stride that covers the
        // whole area answers the vibrant hue.
        let vibrant = [230, 120, 0];
        let img = RgbImage::from_fn(960, 540, |x, y| {
            if x < 480 && y < 270 {
                image::Rgb([128, 128, 128])
            } else {
                image::Rgb(vibrant)
            }
        });
        let big = dominant_hue(&img).expect("the vibrant three quadrants must dominate");
        assert!(circ_diff(big, hue_of(vibrant)) <= 5.0);
    }

    // ---- transplant + gamut mapping + WCAG guard --------------------------

    use crate::testutil::{dark_palette, light_palette};
    use cosmic::cosmic_theme::palette::Srgba;

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

    /// [`write_accents`] over a fresh read of both builders — the call shape
    /// production uses, minus the recompute the builders were read for.
    fn write(handles: &ThemeHandles, light: [u8; 3], dark: [u8; 3]) {
        write_accents(handles, read_builders(handles), light, dark).expect("write accents");
    }

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
        write(&handles, light, dark);

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
        write(&handles, [1, 2, 3], [4, 5, 6]);
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

        write(&handles, [1, 2, 3], [4, 5, 6]);

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

        write(&handles, [7, 133, 217], [250, 41, 90]);

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

    /// Every key file under a config root, `name → contents` (recursively —
    /// the version directory is skipped as a path component).
    fn key_contents_under(root: &std::path::Path) -> std::collections::BTreeMap<String, String> {
        fn walk(dir: &std::path::Path, out: &mut std::collections::BTreeMap<String, String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.insert(
                        path.file_name().unwrap().to_string_lossy().into_owned(),
                        std::fs::read_to_string(&path).unwrap(),
                    );
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(root, &mut out);
        out
    }

    /// Pin every key file under `root` to the given mtime (recursively).
    fn set_all_mtimes(root: &std::path::Path, to: std::time::SystemTime) {
        fn walk(dir: &std::path::Path, to: std::time::SystemTime) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, to);
                } else {
                    std::fs::File::options()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_modified(to)
                        .unwrap();
                }
            }
        }
        walk(root, to);
    }

    /// The key files under `root` whose mtime is newer than `than` —
    /// i.e. the files a rewrite physically touched after
    /// [`set_all_mtimes`] pinned everything to `than`.
    fn files_newer_than(
        root: &std::path::Path,
        than: std::time::SystemTime,
    ) -> std::collections::BTreeSet<String> {
        fn walk(
            dir: &std::path::Path,
            than: std::time::SystemTime,
            out: &mut std::collections::BTreeSet<String>,
        ) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, than, out);
                } else if path.metadata().unwrap().modified().unwrap() > than {
                    out.insert(path.file_name().unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = std::collections::BTreeSet::new();
        walk(root, than, &mut out);
        out
    }

    #[test]
    fn theme_rewrites_transact_only_the_changed_keys() {
        // The first write materialises the full derived theme (deterministic
        // — never a diff against the environment-dependent
        // `Theme::preferred_theme()` default); every later write must touch
        // only the keys the accent change actually moved. The full rewrite
        // was the fsync storm behind the 2026-08-08 btrfs freeze, and every
        // key rewritten is a window in which other COSMIC processes read a
        // torn theme.
        //
        // Content comparison alone cannot pin this down: a regression back
        // to a full `write_entry` rewrites the unchanged keys with
        // byte-identical contents, passing every content assertion while
        // restoring the fsync storm. So every key file's mtime is pinned to
        // a sentinel first, and the set of files *physically rewritten*
        // (mtime moved — the atomic temp-then-rename gives rewritten keys a
        // fresh timestamp) must be exactly the set whose contents changed.
        let dir = tempfile::tempdir().unwrap();
        let handles = ThemeHandles::sandboxed(dir.path()).unwrap();
        let light_dir = dir.path().join("cosmic").join(LIGHT_THEME_ID);
        let dark_dir = dir.path().join("cosmic").join(DARK_THEME_ID);

        write(&handles, [7, 133, 217], [250, 41, 90]);
        assert!(
            key_contents_under(&light_dir).contains_key("is_dark"),
            "first write materialises the full theme"
        );
        // A second write of the *same* pair settles the representation: the
        // materialising `write_entry` stored the `ColorRepr` fields (`shade`,
        // `accent_text`) in the derive's lossy hex form, and the first diff
        // rewrites those two bare-exact (upstream's form). One-time
        // migration; from here on the diff base round-trips exactly.
        write(&handles, [7, 133, 217], [250, 41, 90]);
        let light_before = key_contents_under(&light_dir);
        let dark_before = key_contents_under(&dark_dir);
        let sentinel =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        set_all_mtimes(&light_dir, sentinel);
        set_all_mtimes(&dark_dir, sentinel);

        // Change only the light accent; the dark pair is byte-identical.
        write(&handles, [200, 10, 10], [250, 41, 90]);
        let light_after = key_contents_under(&light_dir);
        assert_eq!(
            light_before.keys().collect::<Vec<_>>(),
            light_after.keys().collect::<Vec<_>>(),
            "a diff write must not create or delete keys"
        );
        let changed: std::collections::BTreeSet<String> = light_after
            .iter()
            .filter(|(key, contents)| light_before[*key] != **contents)
            .map(|(key, _)| key.clone())
            .collect();
        let rewritten = files_newer_than(&light_dir, sentinel);
        assert_eq!(
            rewritten, changed,
            "exactly the keys whose value changed may be rewritten — \
             an unchanged key rewritten byte-identically is a full-\
             `write_entry` regression the contents cannot show"
        );
        assert!(changed.contains("accent"), "changed: {changed:?}");
        for untouched in [
            "palette",
            "spacing",
            "corner_radii",
            "is_dark",
            "gaps",
            "name",
        ] {
            assert!(
                !changed.contains(untouched),
                "{untouched} must not be rewritten by an accent change (changed: {changed:?})"
            );
        }
        // An accent change legitimately touches every component/container
        // key (each carries a `focus` ring coloured by the accent —
        // cosmic-theme `derivation.rs`, `focus: accent`) plus `accent_text`.
        // That is ~20 of 39 keys — the structural floor, and still the
        // point: the stable keys above never churn, so readers see a far
        // smaller torn window and far fewer fsyncs than a full rewrite.
        assert!(
            changed.len() <= 24,
            "an accent change must not approach a full rewrite \
             ({} of {} keys changed: {changed:?})",
            changed.len(),
            light_after.len()
        );
        // The unchanged dark mode saw no writes at all: the builder setter is
        // write-on-change and the theme diff is empty — no dark key file was
        // even touched.
        assert_eq!(dark_before, key_contents_under(&dark_dir));
        assert_eq!(
            files_newer_than(&dark_dir, sentinel),
            std::collections::BTreeSet::new(),
            "the unchanged dark theme must see no file writes at all"
        );
    }

    #[test]
    fn read_builder_degrades_per_key_and_substitutes_a_corrupt_palette() {
        let dir = tempfile::tempdir().unwrap();
        let handles = ThemeHandles::sandboxed(dir.path()).unwrap();

        // Seed real key files, then corrupt the light accent key: `get_entry`
        // returns `Err(partial)` and `read_builder` must keep the partial
        // (accent degraded to its default) instead of bailing out.
        write(&handles, [7, 133, 217], [250, 41, 90]);
        let accent_key = crate::testutil::find_key_file(
            &dir.path().join("cosmic").join(LIGHT_THEME_BUILDER_ID),
            "accent",
        );
        std::fs::write(&accent_key, "not ron at all").unwrap();
        let builder = handles.read_builder(Mode::Light);
        assert_eq!(builder.accent, None, "corrupt accent degrades to default");
        assert!(
            matches!(builder.palette, CosmicPalette::Light(_)),
            "the palette substitution still applies on the Err path"
        );

        // A present-but-corrupt `palette` key: the explicit probe fails and
        // must substitute the mode's own default, not leak the dark one.
        let palette_key = accent_key.with_file_name("palette");
        std::fs::write(&palette_key, "garbage ( ron").unwrap();
        let builder = handles.read_builder(Mode::Light);
        assert!(matches!(builder.palette, CosmicPalette::Light(_)));
        assert_eq!(builder.palette.as_ref(), light_palette());
    }

    #[test]
    fn write_accents_failure_rolls_back_the_landed_mode() {
        // Light is written before dark; a dark-side failure reaches disk
        // with the light mode fully written. That half-write must be rolled
        // back — left in place it holds *our* colour, which the next
        // recompute's don't-clobber guard cannot tell from the user
        // intervening: it would Disarm and clear the snapshot without
        // restoring, destroying the user's pre-feature accent.
        let dir = tempfile::tempdir().unwrap();
        let handles = ThemeHandles::sandboxed(dir.path()).unwrap();

        let user_light = [10, 20, 30];
        let user_dark = [40, 50, 60];
        restore_accents(
            &handles,
            AccentSnapshot {
                light: Some(user_light),
                dark: Some(user_dark),
            },
        )
        .expect("seed accents");
        let dark_theme_before = theme_accent(&handles, Mode::Dark);

        let dark_dirs = crate::testutil::read_only_trees(&[
            dir.path().join("cosmic").join(DARK_THEME_BUILDER_ID),
            dir.path().join("cosmic").join(DARK_THEME_ID),
        ]);
        let result = write_accents(&handles, read_builders(&handles), [1, 2, 3], [4, 5, 6]);
        crate::testutil::restore_dir_permissions(&dark_dirs);
        result.expect_err("the dark write must fail");

        // The landed light mode was rolled back, builder and theme alike…
        assert_eq!(
            read_current_accents(&handles),
            (Some(user_light), Some(user_dark)),
            "both builders must hold the user's accents again"
        );
        assert_eq!(theme_accent(&handles, Mode::Light), user_light);
        // …and dark is exactly as it was.
        assert_eq!(theme_accent(&handles, Mode::Dark), dark_theme_before);
    }

    // ---- pure apply/disarm decision ---------------------------------------

    /// A `(light, dark)` builder pair carrying the stock palettes and the
    /// given accent overrides — the plan-test analogue of [`read_builders`].
    fn builders_with(current: BuilderAccents) -> Builders {
        let mut light = ThemeBuilder::light();
        light.accent = current.0.map(unquantize);
        let mut dark = ThemeBuilder::dark();
        dark.accent = current.1.map(unquantize);
        (light, dark)
    }

    #[test]
    fn plan_is_skip_while_disabled() {
        // Disabled means change nothing — even with (stale) persisted state
        // and a live hue on hand, the plan must never write.
        let action = accent_plan(
            false,
            Some(AccentSnapshot {
                light: None,
                dark: Some([1, 2, 3]),
            }),
            Some(AccentPair {
                light: [9, 9, 9],
                dark: [8, 8, 8],
            }),
            &builders_with((Some([1, 1, 1]), None)),
            Some(120.0),
        );
        assert_eq!(action, AccentAction::Skip);
    }

    #[test]
    fn first_write_after_enable_carries_snapshot_now() {
        // No last_written yet → nothing to clobber; no snapshot yet → the
        // executor must capture the user's accents before writing. The
        // colours are exactly the transplant's own answer for each palette.
        let hue = Some(200.0);
        let action = accent_plan(
            true,
            None,
            None,
            &builders_with((Some([10, 20, 30]), None)),
            hue,
        );
        assert_eq!(
            action,
            AccentAction::Write {
                light: quantize(accent_for(light_palette(), hue)),
                dark: quantize(accent_for(dark_palette(), hue)),
                snapshot_now: true,
            }
        );
    }

    #[test]
    fn a_changed_hue_rewrites_without_resnapshotting() {
        // Builders hold exactly what we last wrote and the wallpaper's hue
        // moved → follow it; the enable-time snapshot must not be
        // overwritten.
        let last = AccentPair {
            light: [7, 133, 217],
            dark: [250, 41, 90],
        };
        let action = accent_plan(
            true,
            Some(AccentSnapshot {
                light: None,
                dark: None,
            }),
            Some(last),
            &builders_with((Some(last.light), Some(last.dark))),
            Some(30.0),
        );
        assert!(
            matches!(
                action,
                AccentAction::Write {
                    snapshot_now: false,
                    ..
                }
            ),
            "got {action:?}"
        );
    }

    #[test]
    fn an_unchanged_computed_pair_skips_instead_of_rewriting() {
        // Steady state — same wallpaper hue, builders exactly as we left
        // them: a Write here would rewrite both derived themes key-for-key
        // and notify every running COSMIC app on every startup
        // reconciliation and same-hue apply. Disk is already right → Skip.
        let hue = Some(200.0);
        let last = AccentPair {
            light: quantize(accent_for(light_palette(), hue)),
            dark: quantize(accent_for(dark_palette(), hue)),
        };
        let action = accent_plan(
            true,
            Some(AccentSnapshot {
                light: None,
                dark: None,
            }),
            Some(last),
            &builders_with((Some(last.light), Some(last.dark))),
            hue,
        );
        assert_eq!(action, AccentAction::Skip);
    }

    #[test]
    fn a_change_in_the_enable_to_first_write_gap_disarms_keeping_the_snapshot() {
        // `last_written` is still None (the first write failed, or never
        // ran), but the builders no longer match the enable-time snapshot:
        // either the user intervened in that gap (writing would clobber
        // their pick) or our own write landed without its `last_written`
        // record (crash / failed persist) — indistinguishable. So the gap
        // disarm must keep the snapshot: clearing it over what may be our
        // own leftovers would permanently destroy the user's pre-feature
        // record; a genuine gap pick still stands (no restore happens now).
        let snapshot = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: None,
        };
        let action = accent_plan(
            true,
            Some(snapshot),
            None,
            &builders_with((Some([10, 20, 30]), Some([99, 88, 77]))),
            Some(120.0),
        );
        assert_eq!(
            action,
            AccentAction::Disarm {
                keep_snapshot: true,
            }
        );

        // While the builders still match the snapshot, the retry writes.
        let action = accent_plan(
            true,
            Some(snapshot),
            None,
            &builders_with((snapshot.light, snapshot.dark)),
            Some(120.0),
        );
        assert!(matches!(action, AccentAction::Write { .. }), "{action:?}");
    }

    #[test]
    fn any_externally_changed_builder_accent_disarms() {
        let last = AccentPair {
            light: [7, 133, 217],
            dark: [250, 41, 90],
        };
        let snapshot = Some(AccentSnapshot {
            light: None,
            dark: None,
        });
        let cases: [BuilderAccents; 4] = [
            // One channel one step off — the compare is exact, not fuzzy.
            (Some([8, 133, 217]), Some(last.dark)),
            (Some(last.light), Some([0, 0, 0])),
            // A mode reset to palette default (accent key unset) counts too.
            (None, Some(last.dark)),
            (None, None),
        ];
        for current in cases {
            assert_eq!(
                accent_plan(
                    true,
                    snapshot,
                    Some(last),
                    &builders_with(current),
                    Some(120.0),
                ),
                // A recorded write proves the mismatch is genuinely the
                // user: their pick supersedes the pre-feature record.
                AccentAction::Disarm {
                    keep_snapshot: false,
                },
                "{current:?} differs from last_written and must disarm"
            );
        }
    }

    // The end-to-end sequences (startup reconciliation after a change made
    // while stopped; re-enable re-snapshots so a later disable restores the
    // later accents) are exercised against the *real* executor —
    // `Window::update` in `app.rs`'s tests — rather than a test-local
    // re-implementation of the action semantics that could drift.

    #[test]
    fn grey_hue_still_writes_the_warm_greys() {
        // An effectively grey wallpaper is not a failure: the accent follows
        // it to each palette's own warm grey.
        let action = accent_plan(true, None, None, &builders_with((None, None)), None);
        assert_eq!(
            action,
            AccentAction::Write {
                light: quantize(light_palette().accent_warm_grey.color),
                dark: quantize(dark_palette().accent_warm_grey.color),
                snapshot_now: true,
            }
        );
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
