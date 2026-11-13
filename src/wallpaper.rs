// Wallpaper application via cosmic-bg's config.
//
// cosmic-bg watches its cosmic-config keys (`same-on-all`, `all`,
// `backgrounds`, `output.*` — verified against the watch handler in
// pop-os/cosmic-bg src/main.rs at the pinned rev) and transitions live, so
// applying a wallpaper is just writing config: set `same-on-all = true` and
// write the `all` entry with `source: Path(<file>)`, preserving the user's
// other fields (scaling_mode, filter_method, …). Per-output setups
// intentionally collapse to same-on-all on first apply (documented design
// decision).
//
// Note on types: cosmic-bg-config depends on its own `cosmic-config`
// instance (unpinned git source, distinct from libcosmic's rev-pinned one),
// so its error type cannot be named from this crate. The *config* side
// (`apply` and friends) goes through cosmic-bg-config's public API and maps
// errors into a local type; the lock-screen *state* poke deliberately does
// not — it reads/writes the raw `wallpapers` key through our pinned
// `cosmic_config` instance instead (see `poke_state` for why the two-instance
// split forces that and why the bytes are identical either way).

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use cosmic_bg_config::{
    Config, DEFAULT_BACKGROUND, Entry, FilterMethod, SamplingMethod, ScalingMode, Source,
};
use serde::{Deserialize, Serialize};

/// Applying a wallpaper failed: either a cosmic-bg config operation errored
/// (the concrete error type lives in a crate instance this crate cannot
/// name — see module comment) or the source file failed the [`apply`]
/// precondition. Carries a message only; only ever logged, never matched on.
#[derive(Debug)]
pub struct WallpaperError(String);

impl fmt::Display for WallpaperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for WallpaperError {}

fn config_err(err: impl fmt::Display) -> WallpaperError {
    WallpaperError(format!("cosmic-bg config error: {err}"))
}

/// The hardcoded download folder, `~/Pictures/BingWallpaper` — same location
/// as the reference GNOME extension's default, so an existing folder migrates.
///
/// With no home directory at all the temp dir stands in, so the result stays
/// absolute: a relative download dir would put ~5 MB downloads wherever the
/// process happened to be started, and `is_ours` (which feeds the auto-apply
/// rule) rejects relative paths outright.
pub fn download_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| {
            tracing::warn!("no home directory: downloading wallpapers into the temp dir");
            std::env::temp_dir()
        })
        .join("Pictures")
        .join("BingWallpaper")
}

/// cosmic-bg's background entry, in a shape every cosmic-bg layout can read
/// and write back.
///
/// The pinned `cosmic_bg_config::Entry` is `deny_unknown_fields` and knows
/// nothing of `transition_duration`, which per-mode cosmic-bg writes into
/// every entry: reading such an entry through the pinned type fails, and an
/// apply would then rebuild it from defaults, dropping the user's scaling and
/// filter settings. This local mirror instead
///
/// - ignores fields it does not know (a future cosmic-bg stays readable; the
///   unknown fields are dropped on write, not fatal);
/// - keeps `transition_duration` only when it was already there, so an entry
///   written to a legacy cosmic-bg never grows the field that cosmic-bg's
///   `deny_unknown_fields` rejects;
/// - defaults `filter_by_theme`, which per-mode cosmic-bg made optional.
///
/// The enum/source types are cosmic-bg-config's own: they are plain serde
/// types, so the two-`cosmic-config`-instances problem (module comment) does
/// not reach them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BgEntry {
    /// The configured output (`all` for the entry this applet writes).
    pub output: String,
    /// The configured image source.
    pub source: Source,
    /// Whether images are filtered by the active theme.
    #[serde(default)]
    pub filter_by_theme: bool,
    /// Rotation frequency in seconds.
    pub rotation_frequency: u64,
    /// Fade duration in milliseconds; absent on legacy cosmic-bg. On disk
    /// it is a bare integer (see [`bare_u64`]), never RON's `Some(..)`.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "bare_u64")]
    pub transition_duration: Option<u64>,
    #[serde(default)]
    pub filter_method: FilterMethod,
    #[serde(default)]
    pub scaling_mode: ScalingMode,
    #[serde(default)]
    pub sampling_method: SamplingMethod,
}

/// (De)serialize an optional field as the **bare** `u64` cosmic-bg stores.
///
/// A derived `Option<u64>` does not work here: RON spells options
/// `Some(1200)`/`None` and (without its `implicit_some` extension, which
/// cosmic-config does not enable) rejects the bare `1200` per-mode cosmic-bg
/// writes — and would write back a `Some(1200)` that cosmic-bg's plain `u64`
/// field rejects in turn. Absence is handled by the field's `serde(default)`
/// and `skip_serializing_if`, so these only ever see a present value.
mod bare_u64 {
    use serde::{Deserialize, Deserializer, Serializer};

    // serde's `with` contract passes the field by reference.
    #[allow(clippy::ref_option, clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_u64(*value),
            // Unreachable through `skip_serializing_if`; kept total.
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        u64::deserialize(deserializer).map(Some)
    }
}

impl BgEntry {
    /// A fresh `all` entry: the pinned `Entry::new`'s defaults, with no
    /// `transition_duration` (per-mode cosmic-bg defaults a missing one).
    pub fn new_all(source: Source) -> Self {
        Self {
            output: DEFAULT_BACKGROUND.to_owned(),
            source,
            filter_by_theme: false,
            rotation_frequency: 900,
            transition_duration: None,
            filter_method: FilterMethod::default(),
            scaling_mode: ScalingMode::default(),
            sampling_method: SamplingMethod::default(),
        }
    }

    /// Back to the pinned type for the pinned `Config::set_entry` write.
    fn into_pinned(self) -> Entry {
        let mut entry = Entry::new(self.output, self.source);
        entry.filter_by_theme = self.filter_by_theme;
        entry.rotation_frequency = self.rotation_frequency;
        entry.filter_method = self.filter_method;
        entry.scaling_mode = self.scaling_mode;
        entry.sampling_method = self.sampling_method;
        entry
    }
}

impl From<Entry> for BgEntry {
    fn from(entry: Entry) -> Self {
        Self {
            output: entry.output,
            source: entry.source,
            filter_by_theme: entry.filter_by_theme,
            rotation_frequency: entry.rotation_frequency,
            transition_duration: None,
            filter_method: entry.filter_method,
            scaling_mode: entry.scaling_mode,
            sampling_method: entry.sampling_method,
        }
    }
}

/// Build the `all` entry to write: change only `source` (and pin `output` to
/// the `all` key we write under); every other field the user configured in
/// COSMIC Settings (scaling_mode, filter_method, rotation_frequency, …) is
/// preserved. With no existing entry, start from cosmic-bg's defaults via
/// [`BgEntry::new_all`].
pub fn updated_entry(existing: Option<BgEntry>, path: &Path) -> BgEntry {
    let source = Source::Path(path.to_path_buf());
    match existing {
        Some(mut entry) => {
            entry.output = DEFAULT_BACKGROUND.to_owned();
            entry.source = source;
            entry
        }
        None => BgEntry::new_all(source),
    }
}

/// Precondition for [`apply`]: the source must still be an existing regular
/// file when it is written into cosmic-bg's config. A catalogue image can
/// vanish externally between the last prune and an apply (user deletes the
/// folder, another tool cleans it); writing the dead path anyway would
/// persist it in cosmic-bg's config and mark it current in the applet.
/// Directories are rejected too — the applet only ever applies single
/// catalogue images, never a slideshow folder. Best-effort (TOCTOU is
/// inherent), but it closes the ordinary window.
fn check_apply_source(path: &Path) -> Result<(), WallpaperError> {
    if path.is_file() {
        Ok(())
    } else {
        Err(WallpaperError(format!(
            "wallpaper source is not an existing file: {}",
            path.display()
        )))
    }
}

/// cosmic-bg's config names. Legacy cosmic-bg reads and watches only the
/// shared config; per-mode cosmic-bg displays the `.Dark`/`.Light` config
/// matching the active theme mode and reads the shared one only to seed a
/// mode config that has no `all` key yet. All three are version 1.
const BG_SHARED: &str = cosmic_bg_config::NAME;
const BG_DARK: &str = "com.system76.CosmicBackground.Dark";
const BG_LIGHT: &str = "com.system76.CosmicBackground.Light";
const BG_VERSION: u64 = 1;

/// COSMIC's theme-mode config; its `is_dark` key selects the mode per-mode
/// cosmic-bg displays.
const THEME_MODE: &str = "com.system76.CosmicTheme.Mode";
const THEME_MODE_VERSION: u64 = 1;

/// One of per-mode cosmic-bg's two background configs. Deliberately not
/// `accent::Mode`: that one names a theme builder, this one a cosmic-bg
/// config directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgMode {
    Dark,
    Light,
}

impl BgMode {
    fn config_name(self) -> &'static str {
        match self {
            Self::Dark => BG_DARK,
            Self::Light => BG_LIGHT,
        }
    }
}

/// Whether the cosmic-bg whose config lives under `config_root` uses
/// per-mode backgrounds: true iff a `.Dark` or `.Light` config directory
/// exists. Per-mode cosmic-bg creates both at every startup (it opens them
/// before seeding), and legacy cosmic-bg never does — so the on-disk layout,
/// not any COSMIC version number, is the signal.
///
/// A pure path probe that **opens no config**: opening one creates its
/// directory, and a mode directory invented on a legacy system would make
/// this probe lie from then on (see [`open_targets`]).
pub fn per_mode_layout(config_root: &Path) -> bool {
    [BG_DARK, BG_LIGHT].into_iter().any(|name| {
        config_root
            .join("cosmic")
            .join(name)
            .join(format!("v{BG_VERSION}"))
            .is_dir()
    })
}

/// The cosmic-bg config handles an apply writes and a read consults.
// Fields are consumed by the apply/read rewiring in the following commits.
#[allow(dead_code)]
pub struct BgTargets {
    /// The shared config: all a legacy cosmic-bg reads, and what a per-mode
    /// cosmic-bg seeds a fresh mode config from.
    pub shared: cosmic::cosmic_config::Config,
    /// The per-mode configs, Dark then Light; empty on a legacy layout.
    pub modes: Vec<(BgMode, cosmic::cosmic_config::Config)>,
    /// COSMIC's theme-mode config, for picking the displayed mode. Only
    /// opened (best-effort) on a per-mode layout — nothing else reads it.
    pub theme_mode: Option<cosmic::cosmic_config::Config>,
}

/// Open the cosmic-bg handles for the layout found under `config_root`.
///
/// The mode configs are opened **only** when [`per_mode_layout`] already
/// holds. Never create them on a legacy system: when that system later
/// upgrades, per-mode cosmic-bg seeds each mode from the shared config only
/// if the mode config has no `all` key — a mode config this applet had
/// written would win over, and so discard, a wallpaper the user chose on
/// legacy after our last apply.
///
/// Raw keys through **our** pinned `cosmic_config` instance, exactly like
/// [`poke_state`] (see the module comment); `with_custom_path` keeps the
/// handles injectable for tests.
pub fn open_targets(config_root: &Path) -> Result<BgTargets, WallpaperError> {
    use cosmic::cosmic_config::Config;

    // Probe first: every open below creates its directory.
    let per_mode = per_mode_layout(config_root);
    let open = |name: &str| {
        Config::with_custom_path(name, BG_VERSION, config_root.to_path_buf()).map_err(config_err)
    };

    let shared = open(BG_SHARED)?;
    let (modes, theme_mode) = if per_mode {
        let modes = [BgMode::Dark, BgMode::Light]
            .into_iter()
            .map(|mode| open(mode.config_name()).map(|config| (mode, config)))
            .collect::<Result<Vec<_>, _>>()?;
        let theme_mode =
            Config::with_custom_path(THEME_MODE, THEME_MODE_VERSION, config_root.to_path_buf())
                .map_err(|error| tracing::warn!("cannot open the theme-mode config: {error}"))
                .ok();
        (modes, theme_mode)
    } else {
        (Vec::new(), None)
    };
    Ok(BgTargets {
        shared,
        modes,
        theme_mode,
    })
}

/// The directory cosmic-config roots user configs in — a mirror of the
/// locked cosmic-config's private `get_config_dir()`
/// (`cosmic-config/src/lib.rs`), which is what `Config::new` uses for every
/// other handle in this applet. In Flatpak that is `HOST_XDG_CONFIG_HOME`,
/// else `$HOME/.config`, and only then `dirs::config_dir()` — the last being
/// the sandbox-private `~/.var/app/<id>/config`, where cosmic-bg never
/// looks. Values are used verbatim (no emptiness filtering) to stay
/// byte-for-byte with upstream. Once a deliberate libcosmic bump makes
/// `get_config_dir` public, replace this with it.
fn config_root_from(
    flatpak: bool,
    host_xdg_config_home: Option<OsString>,
    home: Option<OsString>,
    default: Option<PathBuf>,
) -> Option<PathBuf> {
    if flatpak {
        if let Some(host) = host_xdg_config_home {
            return Some(PathBuf::from(host));
        }
        if let Some(home) = home {
            return Some(PathBuf::from(home).join(".config"));
        }
    }
    default
}

/// The production config root; see [`config_root_from`]. Never substitute
/// `dirs::config_dir()`: a handle rooted elsewhere fails silently — the
/// layout probe says "legacy" and the write lands where cosmic-bg never
/// reads.
fn config_root() -> Option<PathBuf> {
    config_root_from(
        std::env::var_os("FLATPAK_ID").is_some(),
        std::env::var_os("HOST_XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
        dirs::config_dir(),
    )
}

/// The production [`BgTargets`], rooted at [`config_root`].
// Called by the apply/read rewiring in the following commits.
#[allow(dead_code)]
fn production_targets() -> Result<BgTargets, WallpaperError> {
    let root = config_root()
        .ok_or_else(|| WallpaperError("no config directory for cosmic-bg".to_owned()))?;
    open_targets(&root)
}

/// Apply `path` as the wallpaper on all displays by writing cosmic-bg's
/// config. Errors without touching the config if `path` no longer exists
/// (see [`check_apply_source`]). Order matters: the `all` entry is written
/// first so that when
/// `same-on-all` flips to true, cosmic-bg already sees the new image (no
/// flash of the previous default). Both writes are change-only, so a
/// re-apply of the current image touches nothing.
///
/// Test note: what stays uncovered is only the *context* plumbing —
/// `cosmic_bg_config::context()` always opens the real user config, and
/// building a `Context` rooted elsewhere requires naming the crate's own
/// `cosmic_config` instance, which this crate cannot (see module comment).
/// That much is covered by the Post-Completion manual smoke test; everything
/// decided around it (`updated_entry`, `check_apply_source`, `classify`,
/// `is_ours`, `should_auto_apply`) is pure and unit-tested below.
pub fn apply(path: &Path) -> Result<(), WallpaperError> {
    check_apply_source(path)?;
    let context = cosmic_bg_config::context().map_err(config_err)?;
    let existing = context.entry(DEFAULT_BACKGROUND).ok().map(BgEntry::from);
    let entry = updated_entry(existing, path).into_pinned();

    let mut config = Config::load(&context).map_err(config_err)?;
    config.set_entry(&context, entry).map_err(config_err)?;
    context.set_same_on_all(true).map_err(config_err)?;
    Ok(())
}

/// The single cosmic-bg *state* key the lock-screen poke rewrites.
const WALLPAPERS_KEY: &str = "wallpapers";

/// Poke cosmic-bg's *state* so a live lock screen rebuilds its wallpaper
/// cache — the cosmic-greeter#511 workaround (full rationale in
/// `lockwatch.rs`). Returns whether a write happened.
///
/// The write must be a **value change**: the locker consumes the state
/// through `cosmic_config::config_state_subscription`, and the
/// `CosmicConfigEntry` derive's `update_keys` has a value-equality guard —
/// an identical rewrite fires inotify but delivers nothing (no cache clear,
/// no rebuild). Hence [`crate::lockwatch::toggle_wallpapers`], applied to a
/// **fresh read on every poke** — a list captured at lock time would write a
/// stale value back over a wallpaper change that landed since the event.
///
/// A read error skips (`Ok(false)`) rather than writing: the alternative
/// writes a default over a real value that merely failed to read. An empty
/// list skips the same way (`toggle_wallpapers` returns `None` — nothing to
/// heal). Only a *write* failure is an error.
///
/// Caveat: the read→toggle→write is **not atomic** against the state's
/// other writers (cosmic-bg's own RMW `save_state`, the applet's apply): a
/// genuine wallpaper change landing in the milliseconds between the fresh
/// read and the `set` gets clobbered with the toggled *stale* list. Only a
/// live lock screen's image can notice, and cosmic-bg's next rotation tick
/// (≤ `rotation_frequency`, 300 s default) rewrites the current source and
/// self-heals it.
///
/// Raw keys, not `cosmic_bg_config::state::State`'s entry API: that API
/// comes from cosmic-bg-config's own `cosmic-config` instance, whose traits
/// this crate cannot name (see module comment; `Cargo.lock` carries both
/// packages — same commit today, still distinct). The RON bytes are
/// identical either way (same commit, same serializer), so raw
/// `ConfigGet::get`/`ConfigSet::set` through **our** pinned instance write
/// exactly what cosmic-bg and the greeter read — and the handle stays
/// injectable via `Config::with_custom_path` for tests.
pub fn poke_state(config: &cosmic::cosmic_config::Config) -> Result<bool, WallpaperError> {
    use cosmic::cosmic_config::{ConfigGet, ConfigSet};

    let list: Vec<(String, Source)> = match config.get(WALLPAPERS_KEY) {
        Ok(list) => list,
        Err(error) => {
            tracing::debug!("state poke skipped: cannot read `{WALLPAPERS_KEY}`: {error}");
            return Ok(false);
        }
    };
    match crate::lockwatch::toggle_wallpapers(list) {
        Some(toggled) => {
            config
                .set(WALLPAPERS_KEY, toggled)
                .map_err(|error| WallpaperError(format!("cosmic-bg state write error: {error}")))?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// The production handle for [`poke_state`]: cosmic-bg's state context,
/// built once in `init` and cloned into poke tasks. `None` (with a warning)
/// when it cannot be opened — every poke is then a no-op.
///
/// Test note: like [`apply`], this is untestable context plumbing —
/// `Config::new_state` always roots in the real user state dir
/// (`~/.local/state/cosmic/…`), so tests inject a tempdir-rooted
/// `Config::with_custom_path` handle into [`poke_state`] instead; nothing
/// here makes a decision. Side effect worth knowing: `new_state`
/// `create_dir_all`s cosmic-bg's state dir — harmless on a machine that
/// never ran cosmic-bg (the dir stays empty, and the absent `wallpapers`
/// key makes every poke skip).
pub fn poke_state_handle() -> Option<cosmic::cosmic_config::Config> {
    match cosmic::cosmic_config::Config::new_state(
        cosmic_bg_config::NAME,
        cosmic_bg_config::state::State::version(),
    ) {
        Ok(config) => Some(config),
        Err(error) => {
            tracing::warn!("cannot open cosmic-bg state for lock pokes: {error}");
            None
        }
    }
}

/// What cosmic-bg currently displays, as far as its config can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentWallpaper {
    /// Same-on-all mode showing this file.
    File(PathBuf),
    /// No catalogue file can be displayed: same-on-all with a
    /// color/gradient source, or no readable `all` entry (cosmic-bg then
    /// shows its built-in default).
    NoFile,
    /// Per-output mode (`same-on-all = false`) or an unreadable config —
    /// which file(s) are displayed cannot be determined from the `all`
    /// entry, so callers must stay conservative.
    Unknown,
}

impl CurrentWallpaper {
    /// The displayed file, if one is knowable. Every non-file state maps to
    /// `None` — that is what keeps the auto-apply "don't clobber" rule and
    /// the prune's "never delete what is displayed" rule conservative.
    pub fn into_file(self) -> Option<PathBuf> {
        match self {
            Self::File(path) => Some(path),
            Self::NoFile | Self::Unknown => None,
        }
    }
}

/// Classify cosmic-bg's config state. Split out of [`current_wallpaper`] as a
/// pure function so the three-way mapping is testable without a `Context`
/// (which can only ever be opened against the *real* user config — see
/// [`apply`]).
fn classify(same_on_all: bool, entry: Option<BgEntry>) -> CurrentWallpaper {
    if !same_on_all {
        return CurrentWallpaper::Unknown;
    }
    match entry {
        Some(entry) => match entry.source {
            Source::Path(path) => CurrentWallpaper::File(path),
            Source::Color(_) => CurrentWallpaper::NoFile,
        },
        // Same-on-all but no readable `all` entry: cosmic-bg falls back to
        // its default wallpaper — none of our files is displayed.
        None => CurrentWallpaper::NoFile,
    }
}

/// Read what cosmic-bg currently displays from its config.
///
/// Accepted v1 limitation: this is read on demand (startup / next apply /
/// prune), not watched — external changes are picked up then.
///
/// Test note: only the context read has no automated coverage (the real user
/// config is the only constructible context); the classification it feeds is
/// the pure, tested [`classify`], as are the decisions built on the result
/// ([`prune_retention`], [`should_auto_apply`]).
pub fn current_wallpaper() -> CurrentWallpaper {
    let Ok(context) = cosmic_bg_config::context() else {
        return CurrentWallpaper::Unknown;
    };
    classify(
        context.same_on_all(),
        context.entry(DEFAULT_BACKGROUND).ok().map(BgEntry::from),
    )
}

/// The applet's tracked "current" after a fresh read of the live state:
/// a [`CurrentWallpaper::File`] replaces it, [`CurrentWallpaper::NoFile`]
/// clears it (no catalogue file is *known* to be displayed — keeping a
/// stale path would present it as current and shield it from shuffle),
/// and [`CurrentWallpaper::Unknown`] keeps the previous value (some
/// output may still display it, so stay conservative).
pub fn synced_current(live: &CurrentWallpaper, previous: Option<PathBuf>) -> Option<PathBuf> {
    match live {
        CurrentWallpaper::File(path) => Some(path.clone()),
        CurrentWallpaper::NoFile => None,
        CurrentWallpaper::Unknown => previous,
    }
}

/// The retention to prune with, given what cosmic-bg says is displayed.
/// The invariant is "never delete the currently applied file"; in
/// [`CurrentWallpaper::Unknown`] (per-output mode, unreadable config) the
/// displayed files are unknowable from the `all` entry, so age-based
/// deletion is disabled entirely (`0` = keep forever — the prune then only
/// drops entries whose file already vanished). Disk cleanup resumes as
/// soon as the state is knowable again — e.g. the first apply through the
/// applet collapses per-output setups to same-on-all.
pub fn prune_retention(current: &CurrentWallpaper, configured_days: u16) -> u16 {
    match current {
        CurrentWallpaper::Unknown => 0,
        CurrentWallpaper::File(_) | CurrentWallpaper::NoFile => configured_days,
    }
}

/// Whether `path` is a file inside our download folder — the auto-apply
/// rule's test for "the current wallpaper is one of ours".
pub fn is_ours(path: &Path) -> bool {
    is_inside(path, &download_dir())
}

/// The "don't clobber" auto-apply rule: apply the freshly fetched image iff
/// (a) this is the very first successful fetch after a cold start (the
/// reason the user installed the applet), or (b) the currently applied
/// wallpaper is a file inside our download folder. If the user picked
/// another wallpaper in COSMIC Settings (or uses a color/per-output setup,
/// where the live state maps to `None`), the applet downloads but does not
/// apply until they act.
pub fn should_auto_apply(cold_start_first_fetch: bool, current_source: Option<&Path>) -> bool {
    cold_start_first_fetch || current_source.is_some_and(is_ours)
}

/// Lexical containment: `path` is strictly inside `dir` (the dir itself does
/// not count — a slideshow source pointing at the folder is not "our image").
/// Relative paths can never match an absolute dir. Paths containing `..`
/// are rejected outright: `starts_with` is purely lexical, so
/// `<dir>/../elsewhere/x.jpg` would otherwise count as inside — and since
/// "ours" feeds the auto-apply rule, that would let a foreign wallpaper
/// spelled with `..` be clobbered. Rejecting is the conservative
/// direction (fewer paths count as ours → less auto-apply).
fn is_inside(path: &Path, dir: &Path) -> bool {
    path != dir
        && path.starts_with(dir)
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic_bg_config::Entry as PinnedEntry;
    use cosmic_bg_config::{FilterMethod, SamplingMethod, ScalingMode};

    /// The `all` entry in the on-disk shape legacy cosmic-bg writes (the
    /// pinned `Entry`: no `transition_duration`).
    const LEGACY_ALL_RON: &str = r#"(
    output: "all",
    source: Path("/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg"),
    filter_by_theme: true,
    rotation_frequency: 300,
    filter_method: Lanczos,
    scaling_mode: Zoom,
    sampling_method: Alphanumeric,
)"#;

    /// The `all` entry in the on-disk shape per-mode cosmic-bg writes: the
    /// legacy fields plus a bare-integer `transition_duration`.
    const PER_MODE_ALL_RON: &str = r#"(
    output: "all",
    source: Path("/usr/share/backgrounds/cosmic/old.jpg"),
    filter_by_theme: false,
    rotation_frequency: 300,
    transition_duration: 1200,
    filter_method: Lanczos,
    scaling_mode: Fit((0.0, 0.0, 0.0)),
    sampling_method: Alphanumeric,
)"#;

    /// What per-mode cosmic-bg requires of `transition_duration`: a bare
    /// `u64`, never RON's `Some(..)` wrapper.
    #[derive(Deserialize)]
    struct PerModeOracle {
        transition_duration: u64,
    }

    #[test]
    fn updated_entry_from_none_uses_cosmic_bg_defaults() {
        let path = Path::new("/home/u/Pictures/BingWallpaper/20260807-Foo_UHD.jpg");
        let entry = updated_entry(None, path);

        assert_eq!(entry.output, "all");
        assert_eq!(entry.source, Source::Path(path.to_path_buf()));
        assert_eq!(entry, BgEntry::new_all(Source::Path(path.to_path_buf())));
        // Everything else matches the pinned Entry::new's defaults, and no
        // `transition_duration` is invented.
        let pinned = PinnedEntry::new("all".to_owned(), Source::Path(path.to_path_buf()));
        assert_eq!(entry.filter_by_theme, pinned.filter_by_theme);
        assert_eq!(entry.rotation_frequency, pinned.rotation_frequency);
        assert_eq!(entry.filter_method, pinned.filter_method);
        assert_eq!(entry.scaling_mode, pinned.scaling_mode);
        assert_eq!(entry.sampling_method, pinned.sampling_method);
        assert_eq!(entry.transition_duration, None);
    }

    #[test]
    fn updated_entry_preserves_user_fields_and_sets_source() {
        let mut existing = BgEntry::new_all(Source::Path(PathBuf::from(
            "/usr/share/backgrounds/old.jpg",
        )));
        existing.filter_by_theme = true;
        existing.rotation_frequency = 12_345;
        existing.filter_method = FilterMethod::Nearest;
        existing.scaling_mode = ScalingMode::Stretch;
        existing.sampling_method = SamplingMethod::Random;

        let path = Path::new("/home/u/Pictures/BingWallpaper/20260807-Foo_UHD.jpg");
        let entry = updated_entry(Some(existing), path);

        assert_eq!(entry.source, Source::Path(path.to_path_buf()));
        assert!(entry.filter_by_theme);
        assert_eq!(entry.rotation_frequency, 12_345);
        assert_eq!(entry.filter_method, FilterMethod::Nearest);
        assert_eq!(entry.scaling_mode, ScalingMode::Stretch);
        assert_eq!(entry.sampling_method, SamplingMethod::Random);
    }

    #[test]
    fn updated_entry_pins_output_to_all() {
        // A stray per-output entry passed in still writes under `all`.
        let mut existing = BgEntry::new_all(Source::Path(PathBuf::from("/tmp/old.jpg")));
        existing.output = "DP-1".to_owned();
        let entry = updated_entry(Some(existing), Path::new("/tmp/new.jpg"));
        assert_eq!(entry.output, "all");
    }

    #[test]
    fn updated_entry_keeps_color_source_users_scaling() {
        // Coming from a solid-color background: source swaps to the path,
        // scaling fields survive.
        let mut existing = BgEntry::new_all(Source::Color(cosmic_bg_config::Color::Single([
            0.1, 0.2, 0.3,
        ])));
        existing.scaling_mode = ScalingMode::Fit([1.0, 1.0, 1.0]);

        let path = Path::new("/home/u/Pictures/BingWallpaper/20260807-Foo_UHD.jpg");
        let entry = updated_entry(Some(existing), path);
        assert_eq!(entry.source, Source::Path(path.to_path_buf()));
        assert_eq!(entry.scaling_mode, ScalingMode::Fit([1.0, 1.0, 1.0]));
    }

    #[test]
    fn legacy_shape_round_trips_without_transition_duration() {
        let existing: BgEntry = ron::from_str(LEGACY_ALL_RON).expect("legacy entry parses");
        assert_eq!(existing.transition_duration, None);

        let path = Path::new("/home/u/Pictures/BingWallpaper/20260807-Foo_UHD.jpg");
        let written = ron::to_string(&updated_entry(Some(existing), path)).expect("serialize");

        assert!(
            !written.contains("transition_duration"),
            "a legacy entry must not grow the field legacy cosmic-bg rejects: {written}"
        );
        // The pinned Entry (`deny_unknown_fields`) is the legacy oracle.
        let pinned: PinnedEntry =
            ron::from_str(&written).expect("legacy cosmic-bg must parse what we write");
        assert_eq!(pinned.output, "all");
        assert_eq!(pinned.source, Source::Path(path.to_path_buf()));
        assert!(pinned.filter_by_theme);
        assert_eq!(pinned.rotation_frequency, 300);
    }

    #[test]
    fn new_shape_preserves_transition_duration() {
        let existing: BgEntry = ron::from_str(PER_MODE_ALL_RON).expect("per-mode entry parses");
        assert_eq!(existing.transition_duration, Some(1200));
        assert_eq!(existing.scaling_mode, ScalingMode::Fit([0.0, 0.0, 0.0]));

        let path = Path::new("/home/u/Pictures/BingWallpaper/20260807-Foo_UHD.jpg");
        let entry = updated_entry(Some(existing), path);
        assert_eq!(entry.source, Source::Path(path.to_path_buf()));
        assert_eq!(entry.transition_duration, Some(1200));
        assert_eq!(entry.scaling_mode, ScalingMode::Fit([0.0, 0.0, 0.0]));

        // Written back as the bare integer per-mode cosmic-bg deserializes.
        let written = ron::to_string(&entry).expect("serialize");
        let oracle: PerModeOracle =
            ron::from_str(&written).expect("per-mode cosmic-bg must parse what we write");
        assert_eq!(oracle.transition_duration, 1200);
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        // A future cosmic-bg adds a field we have never heard of: the entry
        // must still parse, and an apply must keep the fields we do know.
        let ron = PER_MODE_ALL_RON.replace(
            "    transition_duration: 1200,\n",
            "    transition_duration: 1200,\n    future_field: 1,\n",
        );
        assert!(ron.contains("future_field"), "fixture edit must apply");
        let existing: BgEntry = ron::from_str(&ron).expect("unknown fields must not be fatal");

        let entry = updated_entry(Some(existing), Path::new("/tmp/new.jpg"));
        assert_eq!(entry.source, Source::Path(PathBuf::from("/tmp/new.jpg")));
        assert_eq!(entry.scaling_mode, ScalingMode::Fit([0.0, 0.0, 0.0]));
        assert_eq!(entry.transition_duration, Some(1200));
    }

    #[test]
    fn missing_filter_by_theme_defaults_false() {
        // Per-mode cosmic-bg made the field optional (`serde(default)`).
        let ron = PER_MODE_ALL_RON.replace("    filter_by_theme: false,\n", "");
        assert!(!ron.contains("filter_by_theme"), "fixture edit must apply");
        let entry: BgEntry = ron::from_str(&ron).expect("entry without filter_by_theme parses");
        assert!(!entry.filter_by_theme);
    }

    #[test]
    fn apply_source_must_be_an_existing_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");

        // A missing path (e.g. a catalogue image deleted externally) is
        // refused — apply() must error before writing cosmic-bg config.
        let vanished = dir.path().join("20260807-Foo_UHD.jpg");
        let err = check_apply_source(&vanished).expect_err("missing file must be refused");
        assert!(err.to_string().contains("not an existing file"));
        assert!(err.to_string().contains("20260807-Foo_UHD.jpg"));

        // A directory is not an applyable image either.
        check_apply_source(dir.path()).expect_err("directory must be refused");

        // An existing regular file passes.
        let present = dir.path().join("20260808-Bar_UHD.jpg");
        std::fs::write(&present, b"jpg").expect("write");
        check_apply_source(&present).expect("existing file must pass");
    }

    #[test]
    fn download_dir_is_under_home_pictures() {
        // No self-skip: `download_dir` falls back to the temp dir when there
        // is no home, so both assertions hold in every environment.
        let dir = download_dir();
        assert!(dir.ends_with("Pictures/BingWallpaper"));
        assert!(dir.is_absolute());
    }

    #[test]
    fn classify_maps_the_three_cosmic_bg_states() {
        let path = PathBuf::from("/home/u/Pictures/BingWallpaper/20260807-Foo_UHD.jpg");
        let file_entry = BgEntry::new_all(Source::Path(path.clone()));
        let color_entry = BgEntry::new_all(Source::Color(cosmic_bg_config::Color::Single([
            0.1, 0.2, 0.3,
        ])));

        // Same-on-all with a path: that file is displayed.
        assert_eq!(
            classify(true, Some(file_entry.clone())),
            CurrentWallpaper::File(path.clone())
        );
        // Same-on-all with a color, or without a readable entry: nothing of
        // ours is displayed, but the state *is* known.
        assert_eq!(classify(true, Some(color_entry)), CurrentWallpaper::NoFile);
        assert_eq!(classify(true, None), CurrentWallpaper::NoFile);
        // Per-output mode: the `all` entry says nothing about what is on
        // screen, whatever it holds.
        assert_eq!(classify(false, Some(file_entry)), CurrentWallpaper::Unknown);
        assert_eq!(classify(false, None), CurrentWallpaper::Unknown);

        // `into_file` keeps only the knowable file (what the callers hand
        // to the auto-apply rule).
        assert_eq!(CurrentWallpaper::File(path.clone()).into_file(), Some(path));
        assert_eq!(CurrentWallpaper::NoFile.into_file(), None);
        assert_eq!(CurrentWallpaper::Unknown.into_file(), None);
    }

    #[test]
    fn is_ours_accepts_files_inside_the_download_dir() {
        assert!(is_ours(&download_dir().join("20260807-Foo_UHD.jpg")));
        // Nested paths count too (defensive; we never create subdirs).
        assert!(is_ours(&download_dir().join("sub/20260807-Foo_UHD.jpg")));
    }

    #[test]
    fn is_ours_rejects_outside_relative_and_the_dir_itself() {
        assert!(!is_ours(Path::new("/usr/share/backgrounds/cosmic/x.jpg")));
        assert!(!is_ours(Path::new("Pictures/BingWallpaper/x.jpg"))); // relative
        assert!(!is_ours(&download_dir())); // the folder, not a file in it
    }

    #[test]
    fn auto_apply_on_cold_start_first_fetch_regardless_of_current() {
        assert!(should_auto_apply(true, None));
        assert!(should_auto_apply(
            true,
            Some(Path::new("/usr/share/backgrounds/cosmic/x.jpg"))
        ));
    }

    #[test]
    fn auto_apply_when_current_is_ours() {
        let ours = download_dir().join("20260807-Foo_UHD.jpg");
        assert!(should_auto_apply(false, Some(&ours)));
    }

    #[test]
    fn no_auto_apply_when_current_is_not_ours() {
        // The user's own wallpaper must not be clobbered.
        assert!(!should_auto_apply(
            false,
            Some(Path::new("/usr/share/backgrounds/cosmic/x.jpg"))
        ));
        // Unknown current (color source, per-output mode, unreadable
        // config) is conservatively "not ours".
        assert!(!should_auto_apply(false, None));
        // The download dir itself (slideshow source) is not "our image".
        assert!(!should_auto_apply(false, Some(&download_dir())));
    }

    #[test]
    fn prune_retention_disables_age_deletion_when_current_is_unknowable() {
        // Per-output mode / unreadable config: the applied files are
        // unknowable, so pruning must not delete by age (0 = forever).
        assert_eq!(prune_retention(&CurrentWallpaper::Unknown, 8), 0);
        assert_eq!(prune_retention(&CurrentWallpaper::Unknown, 3), 0);
        // Known states prune with the configured retention.
        let file = CurrentWallpaper::File(PathBuf::from("/x.jpg"));
        assert_eq!(prune_retention(&file, 8), 8);
        assert_eq!(prune_retention(&CurrentWallpaper::NoFile, 3), 3);
        assert_eq!(prune_retention(&CurrentWallpaper::NoFile, 0), 0);
    }

    #[test]
    fn synced_current_clears_on_no_file_keeps_on_unknown() {
        let stale = Some(PathBuf::from("/home/u/Pictures/BingWallpaper/old.jpg"));
        // A live file always wins, stale or not.
        let live = CurrentWallpaper::File(PathBuf::from("/x.jpg"));
        assert_eq!(
            synced_current(&live, stale.clone()),
            Some(PathBuf::from("/x.jpg"))
        );
        assert_eq!(synced_current(&live, None), Some(PathBuf::from("/x.jpg")));
        // NoFile: we *know* no catalogue file is displayed — clear the
        // stale path instead of presenting it as current.
        assert_eq!(
            synced_current(&CurrentWallpaper::NoFile, stale.clone()),
            None
        );
        // Unknown: stay conservative, keep whatever we last knew.
        assert_eq!(
            synced_current(&CurrentWallpaper::Unknown, stale.clone()),
            stale
        );
        assert_eq!(synced_current(&CurrentWallpaper::Unknown, None), None);
    }

    #[test]
    fn is_inside_is_component_wise_not_string_prefix() {
        let dir = Path::new("/home/u/Pictures/BingWallpaper");
        assert!(is_inside(
            Path::new("/home/u/Pictures/BingWallpaper/a.jpg"),
            dir
        ));
        // Sibling dir sharing the string prefix must not match.
        assert!(!is_inside(
            Path::new("/home/u/Pictures/BingWallpaperOld/a.jpg"),
            dir
        ));
        assert!(!is_inside(Path::new("/home/u/Pictures"), dir));
        assert!(!is_inside(dir, dir));
    }

    #[test]
    fn is_inside_rejects_parent_dir_traversal() {
        let dir = Path::new("/home/u/Pictures/BingWallpaper");
        // Lexically "starts with" the dir but resolves outside it — a
        // foreign wallpaper spelled this way must not count as ours (it
        // would get clobbered by auto-apply).
        assert!(!is_inside(
            Path::new("/home/u/Pictures/BingWallpaper/../Documents/x.jpg"),
            dir
        ));
        // Even a `..` that resolves back inside is rejected — conservative.
        assert!(!is_inside(
            Path::new("/home/u/Pictures/BingWallpaper/sub/../a.jpg"),
            dir
        ));
    }

    mod targets {
        //! Layout detection, target handles, and the config root — all
        //! against a tempdir (`Config::with_custom_path`), never the real
        //! user config.

        use super::*;
        use crate::testutil::{bg_key_file, bg_layout};

        fn cosmic_dir_names(root: &Path) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(root.join("cosmic"))
                .expect("read cosmic dir")
                .map(|entry| entry.expect("dir entry").file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }

        #[test]
        fn mode_names_are_pinned() {
            // Literal pins of cosmic-bg's on-disk identity: a drift in the
            // consts must break here, not silently retarget the writes.
            assert_eq!(BG_SHARED, "com.system76.CosmicBackground");
            assert_eq!(
                BgMode::Dark.config_name(),
                "com.system76.CosmicBackground.Dark"
            );
            assert_eq!(
                BgMode::Light.config_name(),
                "com.system76.CosmicBackground.Light"
            );
            assert_eq!(BG_VERSION, 1);
        }

        #[test]
        fn old_layout_detects_shared_only() {
            let dir = tempfile::tempdir().expect("tempdir");
            // Nothing at all (a machine that never ran cosmic-bg) is legacy.
            assert!(!per_mode_layout(dir.path()));
            bg_layout(dir.path(), false);
            assert!(!per_mode_layout(dir.path()));
        }

        #[test]
        fn new_layout_detects_per_mode() {
            // Either mode directory alone is enough.
            for name in [
                "com.system76.CosmicBackground.Dark",
                "com.system76.CosmicBackground.Light",
            ] {
                let dir = tempfile::tempdir().expect("tempdir");
                bg_layout(dir.path(), false);
                let mode_dir = dir.path().join("cosmic").join(name);
                // The config name without its version dir is not a layout.
                std::fs::create_dir_all(&mode_dir).expect("create mode dir");
                assert!(!per_mode_layout(dir.path()), "{name} without v1");
                std::fs::create_dir_all(mode_dir.join("v1")).expect("create version dir");
                assert!(per_mode_layout(dir.path()), "{name}/v1");
            }
        }

        #[test]
        fn open_targets_old_layout_opens_no_mode_configs() {
            // The seeding-safety guard: opening a config creates its
            // directory, and a mode directory created on a legacy system
            // makes the later per-mode upgrade skip seeding from shared.
            let dir = tempfile::tempdir().expect("tempdir");
            bg_layout(dir.path(), false);

            let targets = open_targets(dir.path()).expect("open targets");

            assert!(targets.modes.is_empty());
            assert!(targets.theme_mode.is_none());
            assert_eq!(
                cosmic_dir_names(dir.path()),
                ["com.system76.CosmicBackground"],
                "a legacy probe must create nothing beside the shared config"
            );
        }

        #[test]
        fn open_targets_new_layout_opens_dark_then_light() {
            let dir = tempfile::tempdir().expect("tempdir");
            bg_layout(dir.path(), true);

            let targets = open_targets(dir.path()).expect("open targets");

            let modes: Vec<BgMode> = targets.modes.iter().map(|(mode, _)| *mode).collect();
            assert_eq!(modes, [BgMode::Dark, BgMode::Light]);
            assert!(targets.theme_mode.is_some());

            // Each handle is rooted at the directory cosmic-bg reads: a key
            // set through it lands in that config's v1 dir under the root.
            use cosmic::cosmic_config::ConfigSet as _;
            let handles = [
                ("com.system76.CosmicBackground", &targets.shared),
                ("com.system76.CosmicBackground.Dark", &targets.modes[0].1),
                ("com.system76.CosmicBackground.Light", &targets.modes[1].1),
            ];
            for (name, config) in handles {
                let key_file = bg_key_file(dir.path(), name, "same-on-all");
                assert!(!key_file.exists(), "{name}: nothing written yet");
                config.set("same-on-all", true).expect("set key");
                assert!(key_file.is_file(), "{name}: key must land under the root");
            }
        }

        fn os(value: &str) -> Option<OsString> {
            Some(OsString::from(value))
        }

        #[test]
        fn config_root_native_uses_default() {
            let default = Some(PathBuf::from("/home/u/.config"));
            assert_eq!(
                config_root_from(false, os("/host/config"), os("/home/u"), default.clone()),
                default
            );
            assert_eq!(config_root_from(false, None, None, None), None);
        }

        #[test]
        fn config_root_flatpak_prefers_host_xdg_config_home() {
            assert_eq!(
                config_root_from(
                    true,
                    os("/host/config"),
                    os("/home/u"),
                    Some(PathBuf::from("/home/u/.var/app/id/config")),
                ),
                Some(PathBuf::from("/host/config"))
            );
        }

        #[test]
        fn config_root_flatpak_falls_back_to_home_dot_config() {
            // Not the sandbox-private default: cosmic-bg never looks there.
            assert_eq!(
                config_root_from(
                    true,
                    None,
                    os("/home/u"),
                    Some(PathBuf::from("/home/u/.var/app/id/config")),
                ),
                Some(PathBuf::from("/home/u/.config"))
            );
        }

        #[test]
        fn config_root_flatpak_without_host_or_home_uses_default() {
            let default = Some(PathBuf::from("/home/u/.var/app/id/config"));
            assert_eq!(config_root_from(true, None, None, default.clone()), default);
        }
    }

    mod poke {
        //! [`poke_state`] against a tempdir-rooted `Config::with_custom_path`
        //! handle — the same injection seam `app.rs` uses in Task 4. Write
        //! assertions go by inode, via
        //! [`crate::testutil::bg_wallpapers_inode`] (the inode-vs-mtime
        //! rationale lives on that helper).

        use super::*;
        use crate::lockwatch::toggle_wallpapers;
        use crate::testutil::{
            bg_path_source as path_source, bg_state_config as state_config,
            bg_wallpapers_inode as inode, bg_wallpapers_key_file as key_file, read_only_trees,
            restore_dir_permissions,
        };
        use cosmic::cosmic_config::ConfigSet;

        fn read_wallpapers(config: &cosmic::cosmic_config::Config) -> Vec<(String, Source)> {
            use cosmic::cosmic_config::ConfigGet as _;
            config.get("wallpapers").expect("read wallpapers key")
        }

        #[test]
        fn poke_toggles_the_wallpapers_key() {
            let dir = tempfile::tempdir().expect("tempdir");
            let config = state_config(dir.path());
            let canonical = vec![("all".to_owned(), path_source("a"))];
            config.set("wallpapers", &canonical).expect("seed state");
            let seeded_inode = inode(dir.path());

            let wrote = poke_state(&config).expect("poke must succeed");
            assert!(wrote, "a canonical list must be written back toggled");
            assert_ne!(
                inode(dir.path()),
                seeded_inode,
                "a write must have landed (atomic rename = new inode)"
            );
            // The written value is exactly the toggle of the input — and it
            // parses back as Vec<(String, Source)>, i.e. what every state
            // consumer deserializes.
            assert_eq!(
                read_wallpapers(&config),
                toggle_wallpapers(canonical).expect("canonical toggles")
            );
        }

        #[test]
        fn second_poke_restores_the_canonical_value() {
            let dir = tempfile::tempdir().expect("tempdir");
            let config = state_config(dir.path());
            let canonical = vec![
                ("DP-1".to_owned(), path_source("a")),
                ("HDMI-1".to_owned(), path_source("b")),
            ];
            config.set("wallpapers", &canonical).expect("seed state");

            assert!(poke_state(&config).expect("first poke"));
            assert!(poke_state(&config).expect("second poke"));
            assert_eq!(
                read_wallpapers(&config),
                canonical,
                "toggle twice on a canonical list = identity"
            );
        }

        #[test]
        fn poke_normalizes_a_stale_two_entry_shape() {
            // The shape cosmic-bg's RMW `save_state` leaves after a genuine
            // wallpaper change lands over our resting duplicate.
            let dir = tempfile::tempdir().expect("tempdir");
            let config = state_config(dir.path());
            let stale = vec![
                ("all".to_owned(), path_source("new")),
                ("all".to_owned(), path_source("old")),
            ];
            config.set("wallpapers", &stale).expect("seed state");

            assert!(poke_state(&config).expect("poke"));
            assert_eq!(
                read_wallpapers(&config),
                vec![("all".to_owned(), path_source("new"))]
            );
        }

        #[test]
        fn poke_skips_an_empty_state() {
            let dir = tempfile::tempdir().expect("tempdir");
            let config = state_config(dir.path());
            let empty: Vec<(String, Source)> = Vec::new();
            config.set("wallpapers", &empty).expect("seed empty state");
            let seeded_inode = inode(dir.path());

            let wrote = poke_state(&config).expect("empty state must not error");
            assert!(!wrote, "nothing to heal — no write");
            assert_eq!(
                inode(dir.path()),
                seeded_inode,
                "the key file must be untouched"
            );
        }

        #[test]
        fn poke_skips_an_absent_or_unreadable_key() {
            let dir = tempfile::tempdir().expect("tempdir");
            let config = state_config(dir.path());

            // Absent key (a machine that never ran cosmic-bg): skip, and —
            // critically — never create the file (that would write a default
            // over a value that merely failed to read).
            let wrote = poke_state(&config).expect("absent key must not error");
            assert!(!wrote);
            assert!(
                !key_file(dir.path()).exists(),
                "a skip must not create the key file"
            );

            // Unreadable key (corrupt RON): same skip, file untouched.
            std::fs::write(key_file(dir.path()), b"not valid ron [").expect("write garbage");
            let garbage_inode = inode(dir.path());
            let wrote = poke_state(&config).expect("unreadable key must not error");
            assert!(!wrote);
            assert_eq!(
                inode(dir.path()),
                garbage_inode,
                "the unreadable file must be untouched"
            );
        }

        #[test]
        fn poke_writes_the_freshly_read_value() {
            // The wallpaper can change between the lock event and the poke
            // (daily auto-apply, shuffle): the poke must toggle what is on
            // disk *now*, never a value captured earlier.
            let dir = tempfile::tempdir().expect("tempdir");
            let config = state_config(dir.path());
            let original = vec![("all".to_owned(), path_source("original"))];
            config.set("wallpapers", &original).expect("seed state");
            assert!(poke_state(&config).expect("first poke"));

            // External rewrite between the two pokes (cosmic-bg applying a
            // new wallpaper).
            let changed = vec![("all".to_owned(), path_source("changed"))];
            config
                .set("wallpapers", &changed)
                .expect("external rewrite");

            assert!(poke_state(&config).expect("second poke"));
            assert_eq!(
                read_wallpapers(&config),
                toggle_wallpapers(changed).expect("canonical toggles"),
                "the second poke must toggle the freshly read value"
            );
        }

        #[test]
        fn poke_surfaces_a_failed_write_as_an_error() {
            // Only a *write* failure is an `Err` (reads skip): a read-only
            // version dir makes cosmic-config's AtomicFile temp-create fail,
            // exercising `poke_state`'s one error path hermetically.
            let dir = tempfile::tempdir().expect("tempdir");
            let config = state_config(dir.path());
            let canonical = vec![("all".to_owned(), path_source("a"))];
            config.set("wallpapers", &canonical).expect("seed state");
            let seeded_inode = inode(dir.path());

            let key_dir = key_file(dir.path())
                .parent()
                .expect("key file has a version dir")
                .to_path_buf();
            let locked = read_only_trees(&[key_dir]);
            let result = poke_state(&config);
            restore_dir_permissions(&locked);

            result.expect_err("a failed state write must surface, not be swallowed");
            assert_eq!(
                inode(dir.path()),
                seeded_inode,
                "the seeded value must be untouched"
            );
            assert_eq!(read_wallpapers(&config), canonical);
        }
    }
}
