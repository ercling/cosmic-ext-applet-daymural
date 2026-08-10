// Lock-screen wallpaper poke — the lock-event domain.
//
// Workaround for cosmic-greeter#511
// (https://github.com/pop-os/cosmic-greeter/issues/511, filed from this
// repo): the locker rebuilds its wallpaper cache on every *delivered*
// cosmic-bg state update (`locker.rs:1023-1027` at `epoch-1.5.0`), but
// locking after an unlock re-inserts the lock surfaces without rebuilding,
// so every lock after the first shows the bundled default background. The
// applet works around it by listening for "screen just locked / system just
// resumed" on the system D-Bus and then writing cosmic-bg's *state* with a
// semantically identical but value-different `wallpapers` list
// ([`toggle_wallpapers`]), which makes the locker reload the image bytes and
// rebuild while its lock surfaces exist.
//
// Why the write must change the value (the dedupe finding): the locker
// consumes the state through `cosmic_config::config_state_subscription`,
// whose `Waiting` arm only forwards an update `if !changed.is_empty()` after
// `update_keys` — and the `CosmicConfigEntry` derive generates `update_keys`
// with a value-equality guard (`if self.field != value { keys.push(..) }`).
// inotify fires on any rewrite, but an equal value produces no message, no
// cache clear, no rebuild. That is also why cosmic-bg's own 5-minute
// rotation churn never heals a lock: with a single-file source it rewrites
// an identical value every tick. The guard is verified identical at our
// pinned libcosmic rev and the greeter's; the mechanism-proof test below
// pins the premise against future bumps.
//
// This module holds the pure decisions only; the zbus subscription stream
// (Task 3) and the app wiring (Task 4) build on it.

use std::collections::HashSet;
use std::time::Duration;

use cosmic_bg_config::Source;

// TODO(lockwatch Task 3/4): remove these allows when the subscription stream
// and the app.rs wiring consume the items (they are test-only until then).
#[allow(dead_code)]
/// A lock-relevant event observed on the system bus.
///
/// There is deliberately **no `Unlocked` variant**: nothing on this system
/// calls logind's `UnlockSession` (the greeter unlocks the compositor, not
/// logind), so the session's `Unlock` signal likely never fires — the design
/// must not depend on it. A poke that lands after an unlock would be a
/// harmless invisible toggle anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockEvent {
    /// The session's `Lock` signal fired (`loginctl lock-session`, via the
    /// COSMIC keybinding or `cosmic-idle`).
    Locked,
    /// logind's `PrepareForSleep(false)` fired — the system just resumed.
    /// cosmic-greeter locks on suspend directly (it watches
    /// `PrepareForSleep` itself), so a suspend lock may never emit a session
    /// `Lock` signal; the resume edge is the reliable trigger.
    Resumed,
}

/// The poke retry ladder, relative to the triggering [`LockEvent`]: the
/// first poke can race the locker inserting its lock surfaces into
/// `surface_names` (`locker.rs:968` — a state update delivered *before*
/// that insert rebuilds nothing), so a second poke is the safety net on a
/// slow lock. Both pokes are full toggles — a "normalize-only" final poke
/// would write nothing when the first poke fired too early and left the
/// list canonical, get deduped, and lose the heal.
#[allow(dead_code)]
pub const POKE_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(4)];

/// The normalizing toggle: produce a `wallpapers` value that (a) compares
/// unequal to `list` under `PartialEq`, (b) renders identically, and
/// (c) every state consumer tolerates. `None` = do not write.
///
/// - Empty list → `None`: nothing to heal, and a default must never be
///   written over a value that merely failed to read.
/// - A non-canonical list (some output name appears more than once) is
///   *normalized*: only the first entry per output name survives, original
///   order preserved. Removal is itself a value change (shorter `Vec`).
/// - A canonical list gets a clone of its **last** entry appended — a `Vec`
///   of different length is unequal under any equality.
///
/// **Invariant — the first entry per output name is never altered or
/// reordered, and additions go at the end.** Every consumer reads
/// first-match-wins: the locker `break`s after the first `Path` match
/// (`common.rs:154-176`) and cosmic-bg's `save_state` mutates the first
/// match. A "prepend" variant would break rendering.
///
/// The normalization arm is what makes the toggle **self-healing** rather
/// than just reversible: cosmic-bg's `save_state` is read-modify-write
/// (`cosmic-bg/src/wallpaper.rs:72-90` — it updates the first matching
/// entry and writes everything else back verbatim), so after a genuine
/// wallpaper change lands while our resting duplicate is present, the state
/// holds a stale `[(out, new), (out, old)]` shape that nothing else would
/// ever remove — the old path would outlive retention-prune and make the
/// greeter's loader log read failures forever. Every poke normalizes first,
/// so artifacts are bounded at one extra entry per output and live only
/// until the next lock/resume. The duplicated shape *at rest* (a ladder
/// interrupted between its two pokes) is a normal, tolerated outcome;
/// parity is not guaranteed.
///
/// On the toggle's own output, toggling twice is the identity (append then
/// cleanup). That claim does not extend to arbitrary field shapes: two
/// unnamed outputs both keyed `""` (a legitimate `save_state` product)
/// normalize to one entry — a delivered change, and cosmic-bg re-pushes
/// what it needs on its next tick.
pub fn toggle_wallpapers(list: Vec<(String, Source)>) -> Option<Vec<(String, Source)>> {
    if list.is_empty() {
        return None;
    }

    let mut seen = HashSet::new();
    let normalized: Vec<(String, Source)> = list
        .iter()
        .filter(|(output, _)| seen.insert(output.as_str()))
        .cloned()
        .collect();

    if normalized.len() != list.len() {
        // Cleanup poke: removal is itself a value change.
        Some(normalized)
    } else {
        // Canonical: append a duplicate of the last entry (never the first —
        // see the first-match invariant above).
        let mut appended = list;
        let last = appended.last().cloned()?;
        appended.push(last);
        Some(appended)
    }
}

/// Map a logind `PrepareForSleep(start)` edge to a lock event: `false`
/// (waking up) → [`LockEvent::Resumed`]; `true` (about to suspend) →
/// `None` — a state write racing suspend is useless, the locker rebuild it
/// would trigger happens while the displays are off, and the resume edge
/// follows anyway.
#[allow(dead_code)]
pub fn sleep_edge_to_event(start: bool) -> Option<LockEvent> {
    if start {
        None
    } else {
        Some(LockEvent::Resumed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn path_source(name: &str) -> Source {
        Source::Path(PathBuf::from(format!(
            "/home/u/Pictures/BingWallpaper/{name}.jpg"
        )))
    }

    fn color_source() -> Source {
        Source::Color(cosmic_bg_config::Color::Single([0.1, 0.2, 0.3]))
    }

    #[test]
    fn canonical_list_gets_a_trailing_duplicate() {
        // Single output.
        let single = vec![("all".to_owned(), path_source("a"))];
        let toggled = toggle_wallpapers(single.clone()).expect("canonical list must toggle");
        assert_ne!(toggled, single, "the write must be a value change");
        assert_eq!(
            toggled,
            vec![
                ("all".to_owned(), path_source("a")),
                ("all".to_owned(), path_source("a")),
            ]
        );

        // Multiple outputs: the duplicate is of the *last* entry, appended
        // at the end.
        let multi = vec![
            ("DP-1".to_owned(), path_source("a")),
            ("HDMI-1".to_owned(), path_source("b")),
        ];
        let toggled = toggle_wallpapers(multi.clone()).expect("canonical list must toggle");
        assert_ne!(toggled, multi);
        assert_eq!(
            toggled,
            vec![
                ("DP-1".to_owned(), path_source("a")),
                ("HDMI-1".to_owned(), path_source("b")),
                ("HDMI-1".to_owned(), path_source("b")),
            ]
        );

        // A Color source is toggled the same way — the transform never
        // inspects the Source.
        let color = vec![("all".to_owned(), color_source())];
        let toggled = toggle_wallpapers(color.clone()).expect("color list must toggle");
        assert_ne!(toggled, color);
        assert_eq!(
            toggled,
            vec![
                ("all".to_owned(), color_source()),
                ("all".to_owned(), color_source()),
            ]
        );
    }

    #[test]
    fn toggling_twice_on_a_canonical_list_is_the_identity() {
        for canonical in [
            vec![("all".to_owned(), path_source("a"))],
            vec![
                ("DP-1".to_owned(), path_source("a")),
                ("HDMI-1".to_owned(), path_source("b")),
                ("eDP-1".to_owned(), color_source()),
            ],
        ] {
            let once = toggle_wallpapers(canonical.clone()).expect("first toggle");
            let twice = toggle_wallpapers(once).expect("second toggle");
            assert_eq!(twice, canonical);
        }
    }

    #[test]
    fn first_entry_per_output_and_order_are_preserved() {
        // Duplicates scattered across several outputs: normalization keeps
        // exactly the first entry per name, in the original relative order,
        // and never rewrites a surviving entry.
        let list = vec![
            ("DP-1".to_owned(), path_source("dp-first")),
            ("HDMI-1".to_owned(), path_source("hdmi-first")),
            ("DP-1".to_owned(), path_source("dp-stale")),
            ("eDP-1".to_owned(), path_source("edp-first")),
            ("HDMI-1".to_owned(), color_source()),
        ];
        let toggled = toggle_wallpapers(list).expect("non-canonical list must toggle");
        assert_eq!(
            toggled,
            vec![
                ("DP-1".to_owned(), path_source("dp-first")),
                ("HDMI-1".to_owned(), path_source("hdmi-first")),
                ("eDP-1".to_owned(), path_source("edp-first")),
            ]
        );
    }

    #[test]
    fn stale_post_change_shape_is_normalized_away() {
        // The shape cosmic-bg's read-modify-write `save_state` leaves after
        // a genuine wallpaper change lands over our resting duplicate:
        // first entry updated, stale second entry preserved verbatim.
        let stale = vec![
            ("all".to_owned(), path_source("new")),
            ("all".to_owned(), path_source("old")),
        ];
        let toggled = toggle_wallpapers(stale).expect("stale shape must toggle");
        assert_eq!(toggled, vec![("all".to_owned(), path_source("new"))]);
    }

    #[test]
    fn unnamed_output_duplicates_normalize_to_one() {
        // A legitimate cosmic-bg shape: `save_state` keys by
        // `output_info.name.unwrap_or_default()`, so two unnamed outputs
        // both land under "". Normalization is lossy here — accepted: the
        // removal is still a delivered change, and cosmic-bg re-pushes what
        // it needs on its next tick.
        let list = vec![
            (String::new(), path_source("a")),
            (String::new(), path_source("a")),
        ];
        let toggled = toggle_wallpapers(list.clone()).expect("must toggle");
        assert_ne!(toggled, list, "the removal is itself a value change");
        assert_eq!(toggled, vec![(String::new(), path_source("a"))]);
    }

    #[test]
    fn empty_list_is_not_poked() {
        assert_eq!(toggle_wallpapers(Vec::new()), None);
    }

    #[test]
    fn sleep_edge_maps_resume_only() {
        // Waking up pokes; going to sleep does not (the write would race
        // suspend and the rebuild would happen with displays off).
        assert_eq!(sleep_edge_to_event(false), Some(LockEvent::Resumed));
        assert_eq!(sleep_edge_to_event(true), None);
    }

    mod mechanism_proof {
        //! Pins the workaround's premise — the locker's subscription
        //! **dedupes identical rewrites and delivers value changes** —
        //! against future libcosmic bumps, by running the exact guard the
        //! locker runs: `cosmic_config::config_state_subscription`'s
        //! `Waiting` arm forwards an update only `if !changed.is_empty()`
        //! after `CosmicConfigEntry::update_keys`, whose derive compares
        //! values (`if self.field != value { keys.push(..) }`).

        use super::*;
        use cosmic::cosmic_config::{
            self, Config, ConfigSet, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry,
        };

        /// Local mirror of the single `cosmic_bg_config::state::State` key
        /// the poke writes, deriving the same `update_keys` the locker's
        /// subscription runs (`Default` is required — the generated
        /// `get_entry` calls `Self::default()`).
        #[derive(Debug, Default, Clone, PartialEq, CosmicConfigEntry)]
        #[version = 1]
        struct MirrorState {
            wallpapers: Vec<(String, Source)>,
        }

        #[test]
        fn identical_rewrites_are_deduped_and_toggles_are_delivered() {
            let dir = tempfile::tempdir().unwrap();
            let ctx = Config::with_custom_path(
                "test.CosmicBgStateMirror",
                MirrorState::VERSION,
                dir.path().to_path_buf(),
            )
            .expect("create test state context");

            let canonical = vec![
                ("all".to_owned(), path_source("current")),
                ("HDMI-1".to_owned(), color_source()),
            ];
            ctx.set("wallpapers", &canonical).expect("seed state");

            // Adopt the seeded value (the locker's state at lock time).
            let mut mirror = MirrorState::default();
            let (errors, keys) = mirror.update_keys(&ctx, &["wallpapers"]);
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(keys, vec!["wallpapers"], "initial adoption is a change");
            assert_eq!(mirror.wallpapers, canonical);

            // (a) An identical rewrite — cosmic-bg's rotation churn, or a
            // naive "just rewrite the file" poke — reports NO changed keys:
            // the subscription would forward nothing, no rebuild.
            ctx.set("wallpapers", &canonical)
                .expect("identical rewrite");
            let (errors, keys) = mirror.update_keys(&ctx, &["wallpapers"]);
            assert!(errors.is_empty(), "{errors:?}");
            assert!(
                keys.is_empty(),
                "identical rewrite must be deduped (the premise of the whole design)"
            );

            // (b) A toggled write reports `wallpapers` changed — delivered.
            let toggled = toggle_wallpapers(canonical.clone()).expect("canonical list must toggle");
            ctx.set("wallpapers", &toggled).expect("toggled write");
            let (errors, keys) = mirror.update_keys(&ctx, &["wallpapers"]);
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(keys, vec!["wallpapers"], "a toggle must be delivered");
            assert_eq!(mirror.wallpapers, toggled);

            // (c) And the second toggle round-trips to the canonical value —
            // itself a delivered change.
            let restored = toggle_wallpapers(toggled).expect("toggled list must toggle back");
            assert_eq!(restored, canonical);
            ctx.set("wallpapers", &restored).expect("restoring write");
            let (errors, keys) = mirror.update_keys(&ctx, &["wallpapers"]);
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(
                keys,
                vec!["wallpapers"],
                "the restore must be delivered too"
            );
            assert_eq!(mirror.wallpapers, canonical);
        }
    }
}
