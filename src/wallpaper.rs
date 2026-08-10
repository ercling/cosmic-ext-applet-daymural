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
// so its error type cannot be named from this crate. Everything here goes
// through cosmic-bg-config's public API and maps errors into a local type.

use std::fmt;
use std::path::{Path, PathBuf};

use cosmic_bg_config::{Config, DEFAULT_BACKGROUND, Entry, Source};

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

/// Build the `all` entry to write: change only `source` (and pin `output` to
/// the `all` key we write under); every other field the user configured in
/// COSMIC Settings (scaling_mode, filter_method, rotation_frequency, …) is
/// preserved. With no existing entry, start from cosmic-bg's defaults via
/// `Entry::new`.
pub fn updated_entry(existing: Option<Entry>, path: &Path) -> Entry {
    let source = Source::Path(path.to_path_buf());
    match existing {
        Some(mut entry) => {
            entry.output = DEFAULT_BACKGROUND.to_owned();
            entry.source = source;
            entry
        }
        None => Entry::new(DEFAULT_BACKGROUND.to_owned(), source),
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
    let entry = updated_entry(context.entry(DEFAULT_BACKGROUND).ok(), path);

    let mut config = Config::load(&context).map_err(config_err)?;
    config.set_entry(&context, entry).map_err(config_err)?;
    context.set_same_on_all(true).map_err(config_err)?;
    Ok(())
}

/// The single cosmic-bg *state* key the lock-screen poke rewrites.
const WALLPAPERS_KEY: &str = "wallpapers";

// TODO(lockwatch Task 4): remove the allows when app.rs wires the pokes in
// (until then the fns are exercised by tests only).
#[allow(dead_code)]
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
        Err(err) => {
            tracing::debug!("state poke skipped: cannot read `{WALLPAPERS_KEY}`: {err}");
            return Ok(false);
        }
    };
    match crate::lockwatch::toggle_wallpapers(list) {
        Some(toggled) => {
            config
                .set(WALLPAPERS_KEY, toggled)
                .map_err(|err| WallpaperError(format!("cosmic-bg state write error: {err}")))?;
            Ok(true)
        }
        None => Ok(false),
    }
}

#[allow(dead_code)]
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
        Err(err) => {
            tracing::warn!("cannot open cosmic-bg state for lock pokes: {err}");
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
fn classify(same_on_all: bool, entry: Option<Entry>) -> CurrentWallpaper {
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
        context.entry(DEFAULT_BACKGROUND).ok(),
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
    use cosmic_bg_config::{FilterMethod, SamplingMethod, ScalingMode};

    #[test]
    fn updated_entry_from_none_uses_cosmic_bg_defaults() {
        let path = Path::new("/home/u/Pictures/BingWallpaper/20260807-Foo_UHD.jpg");
        let entry = updated_entry(None, path);

        assert_eq!(entry.output, "all");
        assert_eq!(entry.source, Source::Path(path.to_path_buf()));
        // Everything else matches Entry::new's defaults.
        let defaults = Entry::new("all".to_owned(), Source::Path(path.to_path_buf()));
        assert_eq!(entry, defaults);
    }

    #[test]
    fn updated_entry_preserves_user_fields_and_sets_source() {
        let mut existing = Entry::new(
            "all".to_owned(),
            Source::Path(PathBuf::from("/usr/share/backgrounds/old.jpg")),
        );
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
        let existing = Entry::new(
            "DP-1".to_owned(),
            Source::Path(PathBuf::from("/tmp/old.jpg")),
        );
        let entry = updated_entry(Some(existing), Path::new("/tmp/new.jpg"));
        assert_eq!(entry.output, "all");
    }

    #[test]
    fn updated_entry_keeps_color_source_users_scaling() {
        // Coming from a solid-color background: source swaps to the path,
        // scaling fields survive.
        let mut existing = Entry::new(
            "all".to_owned(),
            Source::Color(cosmic_bg_config::Color::Single([0.1, 0.2, 0.3])),
        );
        existing.scaling_mode = ScalingMode::Fit([1.0, 1.0, 1.0]);

        let path = Path::new("/home/u/Pictures/BingWallpaper/20260807-Foo_UHD.jpg");
        let entry = updated_entry(Some(existing), path);
        assert_eq!(entry.source, Source::Path(path.to_path_buf()));
        assert_eq!(entry.scaling_mode, ScalingMode::Fit([1.0, 1.0, 1.0]));
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
        let file_entry = Entry::new("all".to_owned(), Source::Path(path.clone()));
        let color_entry = Entry::new(
            "all".to_owned(),
            Source::Color(cosmic_bg_config::Color::Single([0.1, 0.2, 0.3])),
        );

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

    mod poke {
        //! [`poke_state`] against a tempdir-rooted `Config::with_custom_path`
        //! handle — the same injection seam `app.rs` uses in Task 4. Writes
        //! are asserted by **inode** (`MetadataExt::ino` — cosmic-config's
        //! `set` commits an `AtomicFile`, i.e. temp+rename, so every real
        //! write is a new inode), never by mtime — the flakiness class
        //! `thumbs.rs` abandoned (timestamp ties cannot distinguish
        //! "unchanged" from "changed").

        use super::*;
        use crate::lockwatch::toggle_wallpapers;
        use cosmic::cosmic_config::{ConfigGet, ConfigSet};
        use std::os::unix::fs::MetadataExt;

        /// Mirrors cosmic-bg's real state identity (name + version) so the
        /// raw-key layout under the tempdir matches production shape; the
        /// custom path keeps it hermetic.
        fn state_config(dir: &Path) -> cosmic::cosmic_config::Config {
            cosmic::cosmic_config::Config::with_custom_path(
                cosmic_bg_config::NAME,
                cosmic_bg_config::state::State::version(),
                dir.to_path_buf(),
            )
            .expect("create tempdir-rooted state config")
        }

        /// Where `with_custom_path` puts the key file:
        /// `<dir>/cosmic/<name>/v<version>/wallpapers`.
        fn key_file(dir: &Path) -> PathBuf {
            dir.join("cosmic")
                .join(cosmic_bg_config::NAME)
                .join(format!("v{}", cosmic_bg_config::state::State::version()))
                .join("wallpapers")
        }

        fn inode(path: &Path) -> u64 {
            std::fs::metadata(path).expect("stat key file").ino()
        }

        fn read_wallpapers(config: &cosmic::cosmic_config::Config) -> Vec<(String, Source)> {
            config.get("wallpapers").expect("read wallpapers key")
        }

        fn path_source(name: &str) -> Source {
            Source::Path(PathBuf::from(format!(
                "/home/u/Pictures/BingWallpaper/{name}.jpg"
            )))
        }

        #[test]
        fn poke_toggles_the_wallpapers_key() {
            let dir = tempfile::tempdir().expect("tempdir");
            let config = state_config(dir.path());
            let canonical = vec![("all".to_owned(), path_source("a"))];
            config.set("wallpapers", &canonical).expect("seed state");
            let seeded_inode = inode(&key_file(dir.path()));

            let wrote = poke_state(&config).expect("poke must succeed");
            assert!(wrote, "a canonical list must be written back toggled");
            assert_ne!(
                inode(&key_file(dir.path())),
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
            let seeded_inode = inode(&key_file(dir.path()));

            let wrote = poke_state(&config).expect("empty state must not error");
            assert!(!wrote, "nothing to heal — no write");
            assert_eq!(
                inode(&key_file(dir.path())),
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
            let garbage_inode = inode(&key_file(dir.path()));
            let wrote = poke_state(&config).expect("unreadable key must not error");
            assert!(!wrote);
            assert_eq!(
                inode(&key_file(dir.path())),
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
}
