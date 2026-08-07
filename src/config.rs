// Applet settings persisted via cosmic-config under the applet's app ID.
//
// The `CosmicConfigEntry` derive stores each field as its own RON file under
// `$XDG_CONFIG_HOME/cosmic/<APP_ID>/v1/<field>` and generates per-field
// `set_<field>(&mut self, &Config, value) -> Result<bool>` setters that write
// to disk only when the value actually changed. The applet itself persists
// whole configs via `write_entry` (`Window::set_config`), so the generated
// setters are exercised only by the tests below (hence the module-level
// `#[allow(dead_code)]` in main.rs).

use cosmic::cosmic_config::{
    self, Config, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry,
};
use serde::{Deserialize, Serialize};

use crate::app::APP_ID;

/// The retention values the applet supports (the "Keep images" dropdown's
/// choices; 0 = forever). Loaded values outside this set normalize to the
/// default — `view::RETENTION_DAYS` is built from this array so the UI can
/// never drift from it.
pub const RETENTION_CHOICES: [u16; 4] = [3, 8, 30, 0];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, CosmicConfigEntry)]
#[version = 1]
pub struct AppletConfig {
    /// Rotate among downloaded images on a timer.
    pub shuffle_enabled: bool,
    /// Shuffle interval: 1800 | 3600 | 21600 | 86400 seconds.
    pub shuffle_interval_secs: u32,
    /// Keep images for N days; 0 = forever.
    pub retention_days: u16,
}

impl Default for AppletConfig {
    fn default() -> Self {
        Self {
            shuffle_enabled: false,
            shuffle_interval_secs: 86_400,
            retention_days: 8,
        }
    }
}

impl AppletConfig {
    /// The cosmic-config context this applet's settings live in.
    pub fn context() -> Result<Config, cosmic_config::Error> {
        Config::new(APP_ID, Self::VERSION)
    }

    /// Load settings from `config`, falling back to defaults for any key that
    /// is missing or unreadable. Never fails: a fresh install (no keys yet) and
    /// a corrupt key both degrade to defaults per-field. The result is
    /// [normalized](Self::normalize).
    pub fn load(config: &Config) -> Self {
        match Self::get_entry(config) {
            Ok(loaded) => loaded,
            Err((errors, partial)) => {
                for error in errors.iter().filter(|error| error.is_err()) {
                    tracing::warn!("invalid applet config entry (using default): {error}");
                }
                partial
            }
        }
        .normalize()
    }

    /// Snap semantically invalid loaded values to what the applet supports:
    /// a `retention_days` outside [`RETENTION_CHOICES`] falls back to the
    /// default, so the popup's dropdown display and prune/fetch behavior
    /// always agree — a hand-edited `1` must not silently delete images
    /// while the UI claims 8 days. (`shuffle_interval_secs` is deliberately
    /// *not* snapped: a custom sane interval is honored by the timer, and
    /// `schedule::shuffle_interval` defuses the dangerous values; a wrong
    /// dropdown selection there is cosmetic, not destructive.)
    pub fn normalize(mut self) -> Self {
        if !RETENTION_CHOICES.contains(&self.retention_days) {
            let fallback = Self::default().retention_days;
            tracing::warn!(
                "unsupported retention_days {} in config (using {fallback})",
                self.retention_days
            );
            self.retention_days = fallback;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cosmic-config context rooted in a TempDir — never touches the real
    /// user config.
    fn test_context(dir: &tempfile::TempDir) -> Config {
        Config::with_custom_path(APP_ID, AppletConfig::VERSION, dir.path().to_path_buf())
            .expect("create test config context")
    }

    #[test]
    fn defaults_match_plan() {
        let config = AppletConfig::default();
        assert!(!config.shuffle_enabled);
        assert_eq!(config.shuffle_interval_secs, 86_400);
        assert_eq!(config.retention_days, 8);
        assert_eq!(AppletConfig::VERSION, 1);
    }

    #[test]
    fn load_on_empty_config_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);
        assert_eq!(AppletConfig::load(&ctx), AppletConfig::default());
    }

    #[test]
    fn write_then_load_roundtrips_non_default_values() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);

        let written = AppletConfig {
            shuffle_enabled: true,
            shuffle_interval_secs: 1_800,
            retention_days: 30,
        };
        written.write_entry(&ctx).expect("write entry");

        assert_eq!(AppletConfig::load(&ctx), written);
        // And through the raw trait method too (no errors on a full config).
        let loaded = AppletConfig::get_entry(&ctx).expect("no errors on a fully written config");
        assert_eq!(loaded, written);
    }

    #[test]
    fn generated_setter_writes_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);

        let mut config = AppletConfig::default();
        config.write_entry(&ctx).expect("write entry");

        // Changed value → written, reported as changed.
        assert!(config.set_retention_days(&ctx, 3).expect("setter write"));
        // Same value again → no-op.
        assert!(!config.set_retention_days(&ctx, 3).expect("setter no-op"));

        let reloaded = AppletConfig::load(&ctx);
        assert_eq!(reloaded.retention_days, 3);
        // Untouched fields keep their values.
        assert_eq!(reloaded.shuffle_interval_secs, 86_400);
    }

    #[test]
    fn corrupt_key_falls_back_to_default_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);

        let written = AppletConfig {
            shuffle_enabled: true,
            shuffle_interval_secs: 3_600,
            retention_days: 30,
        };
        written.write_entry(&ctx).expect("write entry");

        // Clobber one key file with garbage RON.
        let key_file = find_key_file(dir.path(), "shuffle_interval_secs");
        std::fs::write(&key_file, "not a number").unwrap();

        let loaded = AppletConfig::load(&ctx);
        // Corrupt field degrades to its default…
        assert_eq!(loaded.shuffle_interval_secs, 86_400);
        // …while intact fields keep their stored values.
        assert!(loaded.shuffle_enabled);
        assert_eq!(loaded.retention_days, 30);
    }

    #[test]
    fn normalize_snaps_unsupported_retention_to_the_default() {
        // Every supported choice passes through untouched.
        for days in RETENTION_CHOICES {
            let config = AppletConfig {
                retention_days: days,
                ..Default::default()
            };
            assert_eq!(config.clone().normalize(), config, "{days}");
        }
        // Hand-edited values outside the set fall back to the default —
        // a `1` must not prune to 1 day while the dropdown shows 8.
        for days in [1u16, 2, 7, 9, 365, u16::MAX] {
            let config = AppletConfig {
                retention_days: days,
                ..Default::default()
            };
            assert_eq!(config.normalize().retention_days, 8, "{days}");
        }
    }

    #[test]
    fn load_normalizes_hand_edited_retention() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);

        // Write an unsupported value through the raw setter (bypasses the
        // applet's own UI, like a hand edit of the RON file).
        let mut config = AppletConfig::default();
        config.set_retention_days(&ctx, 1).expect("setter write");

        assert_eq!(AppletConfig::load(&ctx).retention_days, 8);
    }

    /// Locate the per-field RON file cosmic-config wrote inside the TempDir.
    fn find_key_file(root: &std::path::Path, key: &str) -> std::path::PathBuf {
        fn walk(dir: &std::path::Path, key: &str) -> Option<std::path::PathBuf> {
            for entry in std::fs::read_dir(dir).ok()? {
                let path = entry.ok()?.path();
                if path.is_dir() {
                    if let Some(found) = walk(&path, key) {
                        return Some(found);
                    }
                } else if path.file_name().is_some_and(|name| name == key) {
                    return Some(path);
                }
            }
            None
        }
        walk(root, key).expect("key file written by cosmic-config")
    }
}
