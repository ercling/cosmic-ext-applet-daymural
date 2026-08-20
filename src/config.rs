// Applet settings persisted via cosmic-config under the applet's app ID.
//
// The `CosmicConfigEntry` derive stores each field as its own RON file under
// `$XDG_CONFIG_HOME/cosmic/<APP_ID>/v1/<field>` and generates per-field
// `set_<field>(&mut self, &Config, value) -> Result<bool>` setters that write
// to disk only when the value actually changed. The applet persists whole
// configs via `write_entry` (`Window::set_config`, which merely warns on
// failure) — except for the accent state machine's critical persists
// (snapshot / last-written / the enable toggle in `app.rs`), which go through
// the generated setters precisely because those return the error: a silently
// lost accent persist is how a snapshot gets destroyed on the next startup.
// Note the setters mutate the field *before* writing, so a caller that must
// stay consistent on failure has to roll the field back itself.

use cosmic::cosmic_config::{
    self, Config, ConfigSet, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::accent::{AccentPair, AccentSnapshot};
use crate::app::APP_ID;
use crate::leader::with_coordination_lock;

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

/// Cross-process mailbox shared by the leader and the other panel instances.
///
/// Although this entry uses the applet's existing app ID and version, its
/// field names are deliberately disjoint from [`AppletConfig`]. Each helper
/// below writes exactly one key, so a process never persists a stale snapshot
/// of the rest of the mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, CosmicConfigEntry, Default)]
#[version = 1]
pub struct CoordinationConfig {
    pub refresh_request: u64,
    pub refresh_completion: PeerRefreshCompletion,
    pub apply_notice: Option<PeerApplyNotice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerApplyNotice {
    pub generation: u64,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PeerRefreshOutcome {
    #[default]
    Success,
    Network,
    Disk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PeerRefreshCompletion {
    pub request: u64,
    pub outcome: PeerRefreshOutcome,
}

impl CoordinationConfig {
    /// Use the same app ID/version directory as [`AppletConfig`]. The two
    /// entries remain independent because cosmic-config persists each field
    /// under its field name.
    pub fn context() -> Result<Config, cosmic_config::Error> {
        Config::new(APP_ID, Self::VERSION)
    }

    /// Load the mailbox without repairing or otherwise writing missing or
    /// corrupt keys. Each bad key independently falls back to its default.
    pub fn load(config: &Config) -> Self {
        match Self::get_entry(config) {
            Ok(loaded) => loaded,
            Err((errors, partial)) => {
                for error in errors.iter().filter(|error| error.is_err()) {
                    tracing::warn!("invalid coordination config entry (using default): {error}");
                }
                partial
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum CoordinationError {
    Config(cosmic_config::Error),
    Io(io::Error),
    CounterExhausted(&'static str),
}

impl fmt::Display for CoordinationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
            Self::CounterExhausted(counter) => write!(formatter, "{counter} is exhausted"),
        }
    }
}

impl std::error::Error for CoordinationError {}

impl From<cosmic_config::Error> for CoordinationError {
    fn from(error: cosmic_config::Error) -> Self {
        Self::Config(error)
    }
}

impl From<io::Error> for CoordinationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Allocate and persist the next peer refresh request while excluding other
/// mailbox writers. Intended for blocking-worker use only.
pub(crate) fn increment_refresh_request(
    config: &Config,
    state_dir: &Path,
) -> Result<u64, CoordinationError> {
    with_coordination_lock(state_dir, || {
        let current = CoordinationConfig::load(config).refresh_request;
        let next = current
            .checked_add(1)
            .ok_or(CoordinationError::CounterExhausted("refresh_request"))?;
        config.set("refresh_request", next)?;
        Ok(next)
    })
}

/// Allocate and persist an apply notice generation. Intended for
/// blocking-worker use only.
pub(crate) fn write_apply_notice(
    config: &Config,
    state_dir: &Path,
    path: PathBuf,
) -> Result<PeerApplyNotice, CoordinationError> {
    with_coordination_lock(state_dir, || {
        let current = CoordinationConfig::load(config)
            .apply_notice
            .map_or(0, |notice| notice.generation);
        let generation = current
            .checked_add(1)
            .ok_or(CoordinationError::CounterExhausted(
                "apply_notice generation",
            ))?;
        let notice = PeerApplyNotice { generation, path };
        config.set("apply_notice", Some(&notice))?;
        Ok(notice)
    })
}

/// Persist a refresh acknowledgement if it is newer than the one already on
/// disk. Returns whether the key changed. Intended for blocking-worker use
/// only.
pub(crate) fn record_refresh_completion(
    config: &Config,
    state_dir: &Path,
    completion: PeerRefreshCompletion,
) -> Result<bool, CoordinationError> {
    with_coordination_lock(state_dir, || {
        let current = CoordinationConfig::load(config).refresh_completion;
        if completion.request <= current.request {
            return Ok(false);
        }
        config.set("refresh_completion", completion)?;
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::find_key_file;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Barrier, Mutex};

    /// A cosmic-config context rooted in a TempDir — never touches the real
    /// user config.
    fn test_context(dir: &tempfile::TempDir) -> Config {
        Config::with_custom_path(APP_ID, AppletConfig::VERSION, dir.path().to_path_buf())
            .expect("create test config context")
    }

    fn files_under(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root).unwrap().to_owned(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }

        let mut files = BTreeMap::new();
        walk(root, root, &mut files);
        files
    }

    fn coordination_key_bytes(dir: &tempfile::TempDir) -> BTreeMap<&'static str, Vec<u8>> {
        ["refresh_request", "refresh_completion", "apply_notice"]
            .into_iter()
            .map(|key| (key, std::fs::read(find_key_file(dir.path(), key)).unwrap()))
            .collect()
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

    // No per-setter tests for the generated accent setters here: their
    // production call sites (the accent state machine's checked persists in
    // `app.rs`) are exercised by that module's failure-injection tests, the
    // accent fields' round-trip is covered by
    // `write_then_load_roundtrips_non_default_values` /
    // `pre_accent_v1_entry_still_loads`, and the derive's write-on-change
    // mechanics are already exercised once by
    // `generated_setter_writes_only_on_change`.

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

    #[test]
    fn coordination_defaults_and_missing_keys_load_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);
        let before = files_under(dir.path());

        assert_eq!(
            CoordinationConfig::load(&ctx),
            CoordinationConfig::default()
        );
        assert_eq!(CoordinationConfig::VERSION, AppletConfig::VERSION);
        assert_eq!(files_under(dir.path()), before);
    }

    #[test]
    fn corrupt_coordination_keys_load_defaults_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);
        for key in ["refresh_request", "refresh_completion", "apply_notice"] {
            ctx.set(key, "wrong type").unwrap();
        }
        let before = files_under(dir.path());

        assert_eq!(
            CoordinationConfig::load(&ctx),
            CoordinationConfig::default()
        );
        assert_eq!(files_under(dir.path()), before);
    }

    #[test]
    fn concurrent_refresh_requests_are_strictly_monotonic() {
        const THREADS: usize = 8;
        const REQUESTS_PER_THREAD: usize = 12;
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(THREADS));
        let allocated = Arc::new(Mutex::new(Vec::new()));

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let barrier = Arc::clone(&barrier);
                let allocated = Arc::clone(&allocated);
                let config = test_context(&dir);
                let state_dir = state.path().to_owned();
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..REQUESTS_PER_THREAD {
                        allocated
                            .lock()
                            .unwrap()
                            .push(increment_refresh_request(&config, &state_dir).unwrap());
                    }
                });
            }
        });

        let mut allocated = Arc::into_inner(allocated).unwrap().into_inner().unwrap();
        allocated.sort_unstable();
        let expected: Vec<u64> = (1..=(THREADS * REQUESTS_PER_THREAD) as u64).collect();
        assert_eq!(allocated, expected);
        assert_eq!(
            CoordinationConfig::load(&test_context(&dir)).refresh_request,
            expected.len() as u64
        );
    }

    #[test]
    fn notice_and_all_completion_outcomes_roundtrip_as_single_keys() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);
        CoordinationConfig::default().write_entry(&ctx).unwrap();
        let initial = coordination_key_bytes(&dir);

        let notice = write_apply_notice(&ctx, state.path(), PathBuf::from("wallpaper.jpg"))
            .expect("write notice");
        assert_eq!(notice.generation, 1);
        let after_notice = coordination_key_bytes(&dir);
        assert_eq!(after_notice["refresh_request"], initial["refresh_request"]);
        assert_eq!(
            after_notice["refresh_completion"],
            initial["refresh_completion"]
        );

        for (request, outcome) in [
            (1, PeerRefreshOutcome::Success),
            (2, PeerRefreshOutcome::Network),
            (3, PeerRefreshOutcome::Disk),
        ] {
            assert!(
                record_refresh_completion(
                    &ctx,
                    state.path(),
                    PeerRefreshCompletion { request, outcome },
                )
                .unwrap()
            );
            let loaded = CoordinationConfig::load(&ctx);
            assert_eq!(
                loaded.refresh_completion,
                PeerRefreshCompletion { request, outcome }
            );
            assert_eq!(loaded.apply_notice, Some(notice.clone()));
        }

        assert!(increment_refresh_request(&ctx, state.path()).is_ok());
        let loaded = CoordinationConfig::load(&ctx);
        assert_eq!(loaded.refresh_request, 1);
        assert_eq!(loaded.apply_notice, Some(notice));
        assert_eq!(loaded.refresh_completion.request, 3);
    }

    #[test]
    fn stale_completion_does_not_regress_the_mailbox() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);
        let newest = PeerRefreshCompletion {
            request: 9,
            outcome: PeerRefreshOutcome::Success,
        };
        assert!(record_refresh_completion(&ctx, state.path(), newest).unwrap());
        let bytes = std::fs::read(find_key_file(dir.path(), "refresh_completion")).unwrap();

        assert!(
            !record_refresh_completion(
                &ctx,
                state.path(),
                PeerRefreshCompletion {
                    request: 8,
                    outcome: PeerRefreshOutcome::Disk,
                },
            )
            .unwrap()
        );
        assert_eq!(CoordinationConfig::load(&ctx).refresh_completion, newest);
        assert_eq!(
            std::fs::read(find_key_file(dir.path(), "refresh_completion")).unwrap(),
            bytes
        );
    }

    #[test]
    fn exhausted_counters_fail_without_changing_persisted_values() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);
        ctx.set("refresh_request", u64::MAX).unwrap();
        ctx.set(
            "apply_notice",
            Some(PeerApplyNotice {
                generation: u64::MAX,
                path: PathBuf::from("last.jpg"),
            }),
        )
        .unwrap();
        let before = files_under(dir.path());

        assert!(matches!(
            increment_refresh_request(&ctx, state.path()),
            Err(CoordinationError::CounterExhausted("refresh_request"))
        ));
        assert!(matches!(
            write_apply_notice(&ctx, state.path(), PathBuf::from("next.jpg")),
            Err(CoordinationError::CounterExhausted(
                "apply_notice generation"
            ))
        ));
        assert_eq!(files_under(dir.path()), before);
    }

    #[test]
    fn failed_config_writes_do_not_regress_the_persisted_mailbox() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);
        let persisted = CoordinationConfig {
            refresh_request: 4,
            refresh_completion: PeerRefreshCompletion {
                request: 3,
                outcome: PeerRefreshOutcome::Network,
            },
            apply_notice: Some(PeerApplyNotice {
                generation: 2,
                path: PathBuf::from("kept.jpg"),
            }),
        };
        persisted.write_entry(&ctx).unwrap();

        let version_dir = find_key_file(dir.path(), "refresh_request")
            .parent()
            .unwrap()
            .to_owned();
        let saved_dir = version_dir.with_extension("saved");
        std::fs::rename(&version_dir, &saved_dir).unwrap();
        std::fs::write(&version_dir, b"blocks config directory recreation").unwrap();

        assert!(increment_refresh_request(&ctx, state.path()).is_err());
        assert!(write_apply_notice(&ctx, state.path(), PathBuf::from("lost.jpg")).is_err());
        assert!(
            record_refresh_completion(
                &ctx,
                state.path(),
                PeerRefreshCompletion {
                    request: 5,
                    outcome: PeerRefreshOutcome::Disk,
                },
            )
            .is_err()
        );

        std::fs::remove_file(&version_dir).unwrap();
        std::fs::rename(saved_dir, version_dir).unwrap();
        assert_eq!(CoordinationConfig::load(&ctx), persisted);
    }

    #[test]
    fn applet_full_entry_write_does_not_touch_coordination_keys() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(&dir);
        let coordination = CoordinationConfig {
            refresh_request: 7,
            refresh_completion: PeerRefreshCompletion {
                request: 6,
                outcome: PeerRefreshOutcome::Disk,
            },
            apply_notice: Some(PeerApplyNotice {
                generation: 5,
                path: PathBuf::from("peer.jpg"),
            }),
        };
        coordination.write_entry(&ctx).unwrap();
        let before = coordination_key_bytes(&dir);

        AppletConfig {
            shuffle_enabled: true,
            retention_days: 30,
            ..Default::default()
        }
        .write_entry(&ctx)
        .unwrap();

        assert_eq!(coordination_key_bytes(&dir), before);
        assert_eq!(CoordinationConfig::load(&ctx), coordination);
    }
}
