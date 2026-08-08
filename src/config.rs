// Applet settings persisted via cosmic-config under the applet's app ID.
//
// The `CosmicConfigEntry` derive stores each field as its own RON file under
// `$XDG_CONFIG_HOME/cosmic/<APP_ID>/v1/<field>` and generates per-field
// `set_<field>(&mut self, &Config, value) -> Result<bool>` setters that write
// to disk only when the value actually changed. The applet itself persists
// whole configs via `write_entry` (`Window::set_config`), so the generated
// setters are exercised only by the tests below.

use cosmic::cosmic_config::{
    self, Config, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry,
};
use serde::{Deserialize, Serialize};

use crate::accent::{AccentPair, AccentSnapshot};
use crate::app::APP_ID;

/// The retention values the applet supports (the "Keep images" dropdown's
/// choices; 0 = forever). Loaded values outside this set normalize to the
/// default — `view`'s dropdown reads this array directly so the UI can
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
    /// Derive the COSMIC accent colour from the applied wallpaper (opt-in,
    /// off by default).
    pub accent_enabled: bool,
    /// The user's accents captured at enable time, restored verbatim on
    /// disable. `None` = no snapshot taken (feature never enabled, or
    /// disarmed).
    pub accent_snapshot: Option<AccentSnapshot>,
    /// The accents we last wrote, persisted so the don't-clobber comparison
    /// survives applet restarts.
    pub accent_last_written: Option<AccentPair>,
}

impl Default for AppletConfig {
    fn default() -> Self {
        Self {
            shuffle_enabled: false,
            shuffle_interval_secs: 86_400,
            retention_days: 8,
            accent_enabled: false,
            accent_snapshot: None,
            accent_last_written: None,
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
        // The accent feature is opt-in: off by default, nothing persisted.
        assert!(!config.accent_enabled);
        assert_eq!(config.accent_snapshot, None);
        assert_eq!(config.accent_last_written, None);
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
            accent_enabled: true,
            accent_snapshot: Some(AccentSnapshot {
                light: Some([10, 20, 30]),
                dark: None,
            }),
            accent_last_written: Some(AccentPair {
                light: [40, 50, 60],
                dark: [70, 80, 90],
            }),
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
            ..Default::default()
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

    #[test]
    fn pre_accent_v1_entry_still_loads() {
        // A config written before the accent fields existed has no
        // `accent_*` key files on disk. Simulate it by writing a full entry
        // and deleting those keys — loading must yield the accent defaults
        // while keeping the stored pre-existing values.
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);

        let written = AppletConfig {
            shuffle_enabled: true,
            shuffle_interval_secs: 1_800,
            retention_days: 30,
            accent_enabled: true,
            accent_snapshot: Some(AccentSnapshot {
                light: Some([1, 2, 3]),
                dark: Some([4, 5, 6]),
            }),
            accent_last_written: Some(AccentPair {
                light: [7, 8, 9],
                dark: [10, 11, 12],
            }),
        };
        written.write_entry(&ctx).expect("write entry");
        for key in ["accent_enabled", "accent_snapshot", "accent_last_written"] {
            std::fs::remove_file(find_key_file(dir.path(), key)).unwrap();
        }

        let loaded = AppletConfig::load(&ctx);
        assert!(!loaded.accent_enabled);
        assert_eq!(loaded.accent_snapshot, None);
        assert_eq!(loaded.accent_last_written, None);
        // Pre-existing fields keep their stored values.
        assert!(loaded.shuffle_enabled);
        assert_eq!(loaded.shuffle_interval_secs, 1_800);
        assert_eq!(loaded.retention_days, 30);
    }

    #[test]
    fn accent_setters_write_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);

        let mut config = AppletConfig::default();
        config.write_entry(&ctx).expect("write entry");

        // accent_enabled: change → written, same value → no-op.
        assert!(config.set_accent_enabled(&ctx, true).expect("setter write"));
        assert!(!config.set_accent_enabled(&ctx, true).expect("setter no-op"));

        // accent_snapshot: change → written, same value → no-op.
        let snapshot = Some(AccentSnapshot {
            light: None,
            dark: Some([9, 9, 9]),
        });
        assert!(
            config
                .set_accent_snapshot(&ctx, snapshot)
                .expect("setter write")
        );
        assert!(
            !config
                .set_accent_snapshot(&ctx, snapshot)
                .expect("setter no-op")
        );

        // accent_last_written: change → written, same value → no-op.
        let pair = Some(AccentPair {
            light: [1, 2, 3],
            dark: [4, 5, 6],
        });
        assert!(
            config
                .set_accent_last_written(&ctx, pair)
                .expect("setter write")
        );
        assert!(
            !config
                .set_accent_last_written(&ctx, pair)
                .expect("setter no-op")
        );

        let reloaded = AppletConfig::load(&ctx);
        assert!(reloaded.accent_enabled);
        assert_eq!(reloaded.accent_snapshot, snapshot);
        assert_eq!(reloaded.accent_last_written, pair);
    }

    #[test]
    fn normalize_leaves_accent_fields_alone() {
        let config = AppletConfig {
            // An out-of-set retention forces normalize to actually rewrite…
            retention_days: 1,
            accent_enabled: true,
            accent_snapshot: Some(AccentSnapshot {
                light: Some([1, 2, 3]),
                dark: None,
            }),
            accent_last_written: Some(AccentPair {
                light: [4, 5, 6],
                dark: [7, 8, 9],
            }),
            ..Default::default()
        };
        let normalized = config.clone().normalize();
        // …while every accent field passes through untouched.
        assert_eq!(normalized.accent_enabled, config.accent_enabled);
        assert_eq!(normalized.accent_snapshot, config.accent_snapshot);
        assert_eq!(normalized.accent_last_written, config.accent_last_written);
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
