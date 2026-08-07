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

/// A cosmic-bg config operation failed. Carries the underlying error's
/// message (the concrete error type lives in a crate instance this crate
/// cannot name — see module comment). `Clone` so it can ride in iced
/// messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallpaperError(String);

impl fmt::Display for WallpaperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cosmic-bg config error: {}", self.0)
    }
}

impl std::error::Error for WallpaperError {}

fn config_err(err: impl fmt::Display) -> WallpaperError {
    WallpaperError(err.to_string())
}

/// The hardcoded download folder, `~/Pictures/BingWallpaper` — same location
/// as the reference GNOME extension's default, so an existing folder migrates.
pub fn download_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
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

/// Apply `path` as the wallpaper on all displays by writing cosmic-bg's
/// config. Order matters: the `all` entry is written first so that when
/// `same-on-all` flips to true, cosmic-bg already sees the new image (no
/// flash of the previous default). Both writes are change-only, so a
/// re-apply of the current image touches nothing.
pub fn apply(path: &Path) -> Result<(), WallpaperError> {
    let context = cosmic_bg_config::context().map_err(config_err)?;
    let entry = updated_entry(context.entry(DEFAULT_BACKGROUND).ok(), path);

    let mut config = Config::load(&context).map_err(config_err)?;
    config.set_entry(&context, entry).map_err(config_err)?;
    context.set_same_on_all(true).map_err(config_err)?;
    Ok(())
}

/// What is currently applied, per cosmic-bg's config: the `all` entry's
/// `Source::Path`. Returns `None` for a color/gradient source, an unreadable
/// config, or per-output mode (`same-on-all = false`) — in per-output mode
/// the `all` entry is not what is displayed, and `None` keeps the
/// auto-apply "don't clobber" rule conservative.
///
/// Accepted v1 limitation: this is read on demand (startup / next apply),
/// not watched — external changes are picked up then.
pub fn current_source() -> Option<PathBuf> {
    let context = cosmic_bg_config::context().ok()?;
    if !context.same_on_all() {
        return None;
    }
    match context.entry(DEFAULT_BACKGROUND).ok()?.source {
        Source::Path(path) => Some(path),
        Source::Color(_) => None,
    }
}

/// Whether `path` is a file inside our download folder — the auto-apply
/// rule's test for "the current wallpaper is one of ours".
pub fn is_ours(path: &Path) -> bool {
    is_inside(path, &download_dir())
}

/// Lexical containment: `path` is strictly inside `dir` (the dir itself does
/// not count — a slideshow source pointing at the folder is not "our image").
/// Relative paths can never match an absolute dir.
fn is_inside(path: &Path, dir: &Path) -> bool {
    path != dir && path.starts_with(dir)
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
    fn download_dir_is_under_home_pictures() {
        let dir = download_dir();
        assert!(dir.ends_with("Pictures/BingWallpaper"));
        assert!(dir.is_absolute(), "home dir should resolve in tests");
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
}
