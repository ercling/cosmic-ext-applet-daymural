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
// This module holds the pure decisions ([`toggle_wallpapers`],
// [`sleep_edge_to_event`]) and the zbus [`subscription`] stream that feeds
// them; the app wiring (Task 4) builds on both.

use std::any::TypeId;
use std::collections::HashSet;
use std::time::Duration;

use cosmic::iced::Subscription;
use cosmic::iced::futures::channel::mpsc;
use cosmic::iced::futures::{SinkExt, StreamExt, future, stream};
use cosmic_bg_config::Source;
use zbus::zvariant::OwnedObjectPath;

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
///
/// Delivery is **bounded-loss, not guaranteed**: cosmic-bg's own rotation
/// tick can RMW-rewrite the pre-toggle value after a rung's write lands but
/// before the locker's watcher reads the file, deduping that rung — the
/// other rung, or the next lock/resume, heals. (The two rungs also share one
/// generation and are not serialized against each other; see
/// `app.rs::arm_lock_pokes`.)
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
        let last = appended
            .last()
            .cloned()
            .expect("guarded non-empty above — a silent skip here would hide a logic bug");
        appended.push(last);
        Some(appended)
    }
}

/// Map a logind `PrepareForSleep(start)` edge to a lock event: `false`
/// (waking up) → [`LockEvent::Resumed`]; `true` (about to suspend) →
/// `None` — a state write racing suspend is useless, the locker rebuild it
/// would trigger happens while the displays are off, and the resume edge
/// follows anyway.
pub fn sleep_edge_to_event(start: bool) -> Option<LockEvent> {
    if start {
        None
    } else {
        Some(LockEvent::Resumed)
    }
}

// ---------------------------------------------------------------------------
// logind subscription stream.
//
// UNTESTED PLUMBING (same exemption class as `wallpaper::apply`): the system
// bus cannot be faked hermetically from this crate — zbus offers no injectable
// transport, and a mock bus daemon would be exactly the kind of non-hermetic
// fixture the test suite bans. Every decision the stream makes lives in the
// already-tested pure fns above (`sleep_edge_to_event`, and downstream
// `toggle_wallpapers` via `wallpaper::poke_state`); the stream itself only
// connects, resolves, and forwards.

/// Minimal hand-written slice of `org.freedesktop.login1.Manager`: session
/// resolution plus the suspend/resume signal. Deliberately not a generated
/// drop-in — the full interface is enormous and everything else is noise.
/// `gen_blocking = false`: the blocking API is never used here, and with
/// `default-features = false` it may not even be compiled in.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = false
)]
trait Manager {
    /// logind exports the acronym-cased `GetSessionByPID`; zbus's default
    /// snake→Pascal conversion capitalizes only the first letter of each
    /// `_` segment and emits `GetSessionByPid`, which logind's D-Bus
    /// policy rejects with `org.freedesktop.DBus.Error.AccessDenied` (its
    /// busconfig allowlists only the real member names — the error is not
    /// even `UnknownMethod`). Hence the explicit name override, pinned by
    /// `logind_pid_method_name_override_is_pinned` below; every *other*
    /// member in this file matches its introspected name under the default
    /// conversion (verified against `busctl introspect` 2026-08-11).
    /// Without the override, resolution silently fell through to the
    /// `$XDG_SESSION_ID` fallback — and, absent that too, retried as
    /// [`WatchEnd::Transient`]: `AccessDenied` is not a session-absence
    /// error ([`is_session_absence`]), so the misname could degrade but
    /// never permanently park the watch.
    #[zbus(name = "GetSessionByPID")]
    fn get_session_by_pid(&self, pid: u32) -> zbus::Result<OwnedObjectPath>;

    fn get_session(&self, session_id: &str) -> zbus::Result<OwnedObjectPath>;

    /// `PrepareForSleep(true)` fires before suspend, `(false)` after resume.
    /// cosmic-greeter locks on suspend through this same signal, so a
    /// suspend lock may never emit a session `Lock` — the resume edge is the
    /// reliable trigger (see [`LockEvent::Resumed`]).
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// Minimal slice of `org.freedesktop.login1.Session`: the `Lock` signal
/// only. No default path — the session object path is resolved at runtime
/// ([`resolve_session`]) and set through the proxy builder.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    gen_blocking = false
)]
trait Session {
    /// Emitted by `loginctl lock-session` — the COSMIC lock keybinding and
    /// `cosmic-idle` both go through it on this system.
    #[zbus(signal)]
    fn lock(&self) -> zbus::Result<()>;
}

/// Backoff between reconnect attempts after a transient D-Bus failure.
const TRANSIENT_RETRY: Duration = Duration::from_secs(30);

/// How a single connect-and-listen attempt ended.
enum WatchEnd {
    /// The subscription's receiver is gone — iced tore the stream down.
    Closed,
    /// This process has no logind session (e.g. running under
    /// `user@.service` with no session scope). Permanent for the life of
    /// the process: warn **once** and park — an infinite warn loop is not
    /// acceptable, and there is no lock screen to heal without a session.
    NoSession(String),
    /// Anything else — bus unreachable, proxy failure, signal stream ended
    /// (bus restart). Retry with backoff.
    Transient(String),
}

/// Identity token for [`Subscription::run_with`] (cosmic-greeter's own
/// logind subscription uses the same `TypeId` pattern).
struct LockWatchSubscription;

/// The lock-watch subscription: yields a [`LockEvent`] per session `Lock`
/// signal and per logind resume edge. Steady state is fully signal-driven —
/// no polling, no wakeups. The stream **never finishes** (iced does not
/// restart a finished subscription): transient failures loop with a
/// [`TRANSIENT_RETRY`] backoff inside [`watch`], and a no-session
/// environment parks on a pending future after one warning.
pub fn subscription() -> Subscription<LockEvent> {
    Subscription::run_with(TypeId::of::<LockWatchSubscription>(), |_| {
        cosmic::iced::stream::channel(4, watch)
    })
}

/// Outer driver: reconnect/park policy around [`connect_and_forward`].
async fn watch(mut output: mpsc::Sender<LockEvent>) {
    // The first transient failure warns (mirroring the `NoSession` arm): a
    // permanently unreachable system bus would otherwise retry every 30 s
    // forever with only debug logs, and a user for whom the workaround
    // silently never works would have no default-visibility signal. Repeats
    // stay at debug — one warning per panel run is signal enough.
    let mut transient_warned = false;
    loop {
        match connect_and_forward(&mut output).await {
            WatchEnd::Closed => return,
            WatchEnd::NoSession(reason) => {
                tracing::warn!(
                    "no logind session for this process ({reason}); \
                     lock-screen wallpaper pokes are disabled for this run"
                );
                // Park forever — never return (a finished subscription
                // stream is never restarted by iced).
                future::pending::<()>().await;
            }
            WatchEnd::Transient(reason) => {
                if transient_warned {
                    tracing::debug!(
                        "logind lock watch failed ({reason}); \
                         retrying in {TRANSIENT_RETRY:?}"
                    );
                } else {
                    transient_warned = true;
                    tracing::warn!(
                        "logind lock watch failed ({reason}); \
                         lock-screen wallpaper pokes retry every \
                         {TRANSIENT_RETRY:?} until it recovers"
                    );
                }
                tokio::time::sleep(TRANSIENT_RETRY).await;
            }
        }
    }
}

/// One attempt: connect to the system bus, resolve our session, then
/// forward merged `Lock` + `PrepareForSleep` signals until a stream ends.
async fn connect_and_forward(output: &mut mpsc::Sender<LockEvent>) -> WatchEnd {
    let conn = match zbus::Connection::system().await {
        Ok(conn) => conn,
        Err(error) => {
            return WatchEnd::Transient(format!("system bus connection failed: {error}"));
        }
    };
    let manager = match ManagerProxy::new(&conn).await {
        Ok(manager) => manager,
        Err(error) => return WatchEnd::Transient(format!("logind manager proxy failed: {error}")),
    };

    // Building the proxy makes no bus round-trip, so a resolution failure
    // proves nothing by itself — [`resolve_session`] classifies by error:
    // only logind's own "no session" method errors park permanently, and a
    // transient bus failure (NoReply/ServiceUnknown/… during a
    // systemd-logind restart) retries with the backoff.
    let session_path = match resolve_session(&manager).await {
        Ok(path) => path,
        Err(end) => return end,
    };

    let session_builder = match SessionProxy::builder(&conn).path(session_path.clone()) {
        Ok(builder) => builder,
        Err(error) => {
            return WatchEnd::Transient(format!("bad session path {session_path}: {error}"));
        }
    };
    let session = match session_builder.build().await {
        Ok(session) => session,
        Err(error) => return WatchEnd::Transient(format!("session proxy failed: {error}")),
    };

    let lock_stream = match session.receive_lock().await {
        Ok(stream) => stream,
        Err(error) => return WatchEnd::Transient(format!("subscribing to Lock failed: {error}")),
    };
    let sleep_stream = match manager.receive_prepare_for_sleep().await {
        Ok(stream) => stream,
        Err(error) => {
            return WatchEnd::Transient(format!("subscribing to PrepareForSleep failed: {error}"));
        }
    };

    tracing::debug!(session = %session_path, "logind lock watch connected");

    let mut merged = stream::select(
        lock_stream.map(|_signal| Some(LockEvent::Locked)),
        sleep_stream.map(|signal| match signal.args() {
            Ok(args) => sleep_edge_to_event(args.start),
            Err(error) => {
                tracing::debug!("undecodable PrepareForSleep payload: {error}");
                None
            }
        }),
    );
    while let Some(maybe_event) = merged.next().await {
        let Some(event) = maybe_event else { continue };
        tracing::debug!(?event, "logind lock event");
        if output.send(event).await.is_err() {
            return WatchEnd::Closed;
        }
    }

    // Both signal streams ended: the bus connection dropped (e.g. a dbus
    // broker restart). Reconnect.
    WatchEnd::Transient("signal streams ended (bus connection lost)".to_owned())
}

/// Is this error logind saying "there is no such session"? Only these
/// method errors are evidence of the permanent [`WatchEnd::NoSession`]
/// class. Every other failure — `org.freedesktop.DBus.Error.NoReply` /
/// `ServiceUnknown` / `NameHasNoOwner` during a systemd-logind restart, a
/// timeout, a dropped connection — says nothing about whether a session
/// exists and must retry as [`WatchEnd::Transient`]: parking on one of
/// those would permanently disable the workaround over a hiccup.
fn is_session_absence(error: &zbus::Error) -> bool {
    matches!(
        error,
        zbus::Error::MethodError(name, _, _)
            if matches!(
                name.as_str(),
                "org.freedesktop.login1.NoSessionForPID"
                    | "org.freedesktop.login1.NoSuchSession"
            )
    )
}

/// Resolve this process's logind session object path:
/// `Manager.GetSessionByPID(our pid)` first (verified live: the panel's PID
/// resolves through its session scope), falling back to `$XDG_SESSION_ID` →
/// `Manager.GetSession`. `Err` classifies the combined failure
/// ([`is_session_absence`] — `NoSession` only when every attempted path
/// failed with a session-shaped error) and carries the human-readable
/// evidence trail.
async fn resolve_session(manager: &ManagerProxy<'_>) -> Result<OwnedObjectPath, WatchEnd> {
    let pid_err = match manager.get_session_by_pid(std::process::id()).await {
        Ok(path) => {
            tracing::debug!(resolved_via = "GetSessionByPID", "logind session resolved");
            return Ok(path);
        }
        Err(error) => error,
    };
    match std::env::var("XDG_SESSION_ID") {
        Ok(id) => match manager.get_session(&id).await {
            Ok(path) => {
                tracing::debug!(resolved_via = "GetSession", "logind session resolved");
                Ok(path)
            }
            Err(error) => {
                let trail = format!("GetSessionByPID: {pid_err}; GetSession({id:?}): {error}");
                if is_session_absence(&pid_err) && is_session_absence(&error) {
                    Err(WatchEnd::NoSession(trail))
                } else {
                    Err(WatchEnd::Transient(trail))
                }
            }
        },
        Err(_) => {
            let trail = format!("GetSessionByPID: {pid_err}; $XDG_SESSION_ID unset");
            if is_session_absence(&pid_err) {
                Err(WatchEnd::NoSession(trail))
            } else {
                Err(WatchEnd::Transient(trail))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::bg_path_source as path_source;

    fn color_source() -> Source {
        Source::Color(cosmic_bg_config::Color::Single([0.1, 0.2, 0.3]))
    }

    #[test]
    fn canonical_list_gets_a_trailing_duplicate() {
        // Labelled loop (one scenario failing must not mask the rest): the
        // duplicate is always of the *last* entry, appended at the end, and
        // the transform never inspects the Source (hence the Color case).
        for (label, canonical) in [
            ("single output", vec![("all".to_owned(), path_source("a"))]),
            (
                "multiple outputs",
                vec![
                    ("DP-1".to_owned(), path_source("a")),
                    ("HDMI-1".to_owned(), path_source("b")),
                ],
            ),
            ("color source", vec![("all".to_owned(), color_source())]),
        ] {
            let toggled = toggle_wallpapers(canonical.clone())
                .unwrap_or_else(|| panic!("{label}: canonical list must toggle"));
            assert_ne!(
                toggled, canonical,
                "{label}: the write must be a value change"
            );
            let mut expected = canonical.clone();
            expected.push(canonical.last().cloned().expect("non-empty scenario"));
            assert_eq!(
                toggled, expected,
                "{label}: a clone of the last entry, appended at the end"
            );
        }
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
    fn only_logind_session_absence_errors_park_the_watch() {
        // The `WatchEnd::NoSession` park is permanent for the panel run, so
        // it must trigger only on logind's own "no such session" method
        // errors — a transient bus failure during a systemd-logind restart
        // fails the same calls and must stay retryable.
        fn method_error(name: &str) -> zbus::Error {
            let message =
                zbus::message::Message::method_call("/org/freedesktop/login1", "GetSession")
                    .expect("build method call")
                    .build(&())
                    .expect("build message");
            zbus::Error::MethodError(
                zbus::names::OwnedErrorName::try_from(name).expect("valid error name"),
                None,
                message,
            )
        }

        assert!(is_session_absence(&method_error(
            "org.freedesktop.login1.NoSessionForPID"
        )));
        assert!(is_session_absence(&method_error(
            "org.freedesktop.login1.NoSuchSession"
        )));
        // logind restarting / bus hiccups: same failed calls, different
        // error shapes — all transient.
        assert!(!is_session_absence(&method_error(
            "org.freedesktop.DBus.Error.NoReply"
        )));
        assert!(!is_session_absence(&method_error(
            "org.freedesktop.DBus.Error.ServiceUnknown"
        )));
        assert!(!is_session_absence(&method_error(
            "org.freedesktop.DBus.Error.NameHasNoOwner"
        )));
        assert!(!is_session_absence(&zbus::Error::Failure(
            "connection reset".to_owned()
        )));
    }

    #[test]
    fn logind_pid_method_name_override_is_pinned() {
        // zbus's default snake→Pascal conversion emits `GetSessionByPid`,
        // but logind exports the acronym-cased `GetSessionByPID` and its
        // D-Bus policy rejects the misnamed call with `AccessDenied` (see
        // the proxy doc). The generated proxy offers no reflection over its
        // member names and a live-bus assertion would not be hermetic, so
        // pin the source attribute itself — same class as the i18n tests
        // scanning `app.rs`/`view.rs` for message-id references.
        let source = include_str!("lockwatch.rs");
        assert!(
            source.contains(r#"#[zbus(name = "GetSessionByPID")]"#),
            "get_session_by_pid needs its explicit zbus name override: the \
             default conversion emits `GetSessionByPid`, which logind rejects"
        );
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
