// The applet: a panel icon button toggling a popup, plus the refresh
// scheduling and fetch pipeline driving everything. Popup idiom patterned
// on `cosmic-applet-power` from pop-os/cosmic-applets @ ec8ffdc
// (`cosmic::surface::action::app_popup` / `destroy_popup` via `surface_task`).
//
// Scheduling model: a one-shot sleeping task carries a generation number;
// `RefreshDue` messages from a stale generation are ignored, so every
// reschedule (successful fetch, error backoff) atomically replaces the
// pending timer. The pipeline itself runs as one async task and reports
// back via `RefreshFinished`.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use cosmic::{
    Element, Task, app, cosmic_config,
    iced::{self, window},
    widget,
};

use crate::bing::ArchiveImage;
use crate::catalogue::{self, Catalogue, CatalogueRestore, ImageEntry, Provenance};
use crate::config::{
    AppletConfig, CoordinationConfig, PeerApplyNotice, PeerRefreshCompletion, PeerRefreshOutcome,
    increment_refresh_request, record_refresh_completion, write_apply_notice,
};
use crate::leader::Leadership;
// No `fl!` here: every user-visible string this applet renders lives in the
// popup (`view.rs`). The panel contributes an icon and nothing else.
use crate::{accent, bing, lockwatch, schedule, thumbs, tooltip, view, wallpaper};

/// One name everywhere: cosmic-config app ID, state dir, desktop entry.
pub const APP_ID: &str = "io.github.ercling.cosmic-applet-daymural";

/// Symbolic icon shown in the panel.
const PANEL_ICON: &str = "preferences-desktop-wallpaper-symbolic";

/// How often a surviving panel instance checks whether the leader exited.
const LEADERSHIP_RETRY_DELAY: Duration = Duration::from_secs(60);

/// Give the leader ample time for a slow UHD download before a requester
/// stops presenting the refresh as pending. A timeout never starts local
/// work; it only restores an honest popup state and requests a disk reload.
const PEER_REFRESH_ACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub fn run() -> cosmic::iced::Result {
    cosmic::applet::run::<Window>(())
}

/// The applet's state dir (`~/.local/state/<APP_ID>/`): catalogue JSON +
/// cached thumbnails. Resolved once — the view asks for it on every
/// re-render and must not repeat env/home lookups per frame.
///
/// `dirs::state_dir()` already resolves `$XDG_STATE_HOME` then
/// `~/.local/state`, so it only fails with no home at all; falling back to the
/// temp dir keeps the result *absolute* (an empty-home `unwrap_or_default()`
/// would silently scatter the catalogue and thumbnails through the process's
/// working directory).
pub fn state_dir() -> &'static Path {
    static STATE_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
        dirs::state_dir()
            .unwrap_or_else(|| {
                tracing::warn!("no home directory: keeping applet state in the temp dir");
                std::env::temp_dir()
            })
            .join(APP_ID)
    });
    &STATE_DIR
}

/// Where the catalogue JSON is persisted.
fn catalogue_path() -> PathBuf {
    state_dir().join(catalogue::CATALOGUE_FILENAME)
}

#[derive(Default)]
pub struct Window {
    pub(crate) core: cosmic::app::Core,
    /// The open popup's window id (the interval dropdown needs it as the
    /// parent surface of its menu popup).
    pub(crate) popup: Option<window::Id>,
    /// How many dropdown menu popups are believed to be mapped — the whole
    /// popup ledger (see "UI conventions → Popup stack" in `CLAUDE.md`), read
    /// through [`Window::dropdown_open`].
    ///
    /// A *count*, not a bool, because the ledger has no ids to work with: a
    /// menu's window id is minted inside the widget and is never visible here,
    /// so a create and a close can only be paired by arithmetic. The upstream
    /// pairing is exact — every popup the runtime tears down is removed from
    /// `self.popups` first (both `…/handlers/shell/xdg_popup.rs::done` and the
    /// `Action::Destroy` arm in `…/event_loop/state.rs`), so one mapped popup
    /// yields at most one `Done` — which makes "creates minus closes" the right
    /// quantity. With a single bool a `Done` for an *already gone* menu
    /// delivered after a newer create would clear it with a menu still mapped:
    /// tooltips un-paused beside a live sibling, which is the two-children
    /// state the invariant forbids. That is the mirror of the stale-*request*
    /// hazard below, and only a count answers both.
    ///
    /// Saturating in both directions, and biased toward "open": a spurious
    /// non-zero only pauses tooltips, while a spurious zero lets one arm beside
    /// a mapped menu. It is incremented optimistically on the create, which the
    /// runtime can still drop (it retries a deferred create five times at 30 ms
    /// and then gives up — reachable whenever the main thread stalls past
    /// ~150 ms with `state.destroyed` still populated, e.g. by the tooltip
    /// destroy the interlock just emitted — a `pending_popup` that a second
    /// deferred create *replaces* is dropped with no retry at all, and
    /// `get_popup` failures only log). Tooltips then stay paused for the rest
    /// of that popup session.
    ///
    /// **That phantom unit must not outlive the session that leaked it**, and
    /// this is the one bound the ledger owes the user: the count is zeroed when
    /// the session ends, but its units are re-booked as an owed close (below),
    /// and an owed close that nothing will ever pay silently swallows the *next*
    /// session's live menu close instead — one dropped create would otherwise
    /// kill hover tooltips for the rest of the process. Both bookings are
    /// therefore bounded by evidence that the ended session is fully drained:
    /// see [`ClosingPopup::menus_owed`] and [`Window::stale_menu_closes`].
    ///
    /// **Only three things lower it**, all of which are evidence that a menu is
    /// really gone: a `PopupClosed` for an unknown surface decrements
    /// ([`Window::on_popup_closed`]) *unless it is owed to an ended session*
    /// (see [`Window::stale_menu_closes`] and [`Window::closing_popups`]), and
    /// our own popup ending — its
    /// `PopupClosed` or [`Message::TogglePopup`] — resets it to zero, since
    /// every child dies with it. A `DestroyPopup` *request* deliberately does
    /// not — the two rows share one message and a widget left with a stale
    /// `is_open` (grab-loss dismissal never reaches its `ButtonPressed` arm,
    /// and `Row::update` dispatches to every child regardless of
    /// `capture_event`) emits a destroy for a popup that is already gone, in
    /// the same pass as the *other* row's create. That no-op destroy produces
    /// no `Done`, so leaving the count alone here is exactly right; a real
    /// destroy is announced back as `PopupClosed`.
    dropdowns_open: u32,
    /// How many **menu** `PopupClosed` events are still owed by a popup session
    /// a *compositor dismissal* ended — the anonymous debt of the "ours" row of
    /// [`Window::on_popup_closed`], kept as a count because those events carry
    /// no generation to compare. The self-initiated ending books its menus on
    /// the closing popup's own entry instead ([`ClosingPopup::menus_owed`]),
    /// because there the ending *is* keyed.
    ///
    /// The other generation counters in this file (`timer_generation`,
    /// `shuffle_generation`, `accent_generation`) can stamp their own message
    /// and drop a tick whose stamp is stale. `PopupClosed` cannot: its payload
    /// is a bare `window::Id` handed to us by libcosmic's `on_close_requested`,
    /// and a dropdown menu's id is minted inside the widget, so a stale menu
    /// close is indistinguishable from a live one by inspection. What *is*
    /// knowable is how many closes the ended session still has outstanding, and
    /// that is this count: each one is paid off before
    /// [`Window::dropdowns_open`] — which belongs to the *current* session — is
    /// touched. Our own popup's late close is **not** counted here; its id is
    /// known, so it is booked into [`Window::closing_popups`] instead.
    ///
    /// **Discarded when the next session's popup id is adopted**
    /// ([`Window::adopt_popup`]) — the bound that keeps an unpayable unit from
    /// outliving its session (see [`Window::dropdowns_open`]). Sound because
    /// this debt is booked only on the compositor path, and there the ending is
    /// *not* asynchronous with respect to later input: `…/handlers/shell/
    /// xdg_popup.rs::done` pushes a `Done` for every popup of the dismissed
    /// chain into `self.sctk_events` in one call, and the sctk thread drains
    /// that vec into the same `events_sender` channel that carries pointer
    /// events (`…/wayland/event_loop/mod.rs`, `for e in
    /// state.state.sctk_events.drain(..)`). So the whole chain's closes are
    /// queued ahead of the click that could open the next popup, and that click
    /// is still two message rounds away from adopting an id — anything still
    /// owed here by then is a menu that never mapped.
    ///
    /// Paying a debt can only *withhold* a decrement, so the ledger stays
    /// biased toward "open" (see [`Window::dropdowns_open`]).
    stale_menu_closes: u32,
    /// Our own popups whose destroy has been requested but whose `PopupClosed`
    /// has not been delivered yet, each carrying the menu closes its teardown
    /// still owes — the *keyed* half of the popup-session debt, and the reason
    /// [`Window::stale_menu_closes`] says "menu".
    ///
    /// [`Message::TogglePopup`] takes `self.popup` before emitting the destroy,
    /// so the `Done` that comes back four queues and a thread later can no
    /// longer match the "ours" row of [`Window::on_popup_closed`] and would be
    /// charged to the live count by elimination. It **is** an id we know,
    /// though, so it is remembered rather than merely counted — which matters
    /// because that `Done` may never arrive at all:
    ///
    /// - `self.popup` is set inside the create's *settings* closure, which
    ///   libcosmic runs (`…/src/app/cosmic.rs`, the `Action::AppPopup` arm:
    ///   `let settings = settings(&mut self.app);`) **before** it asks the sctk
    ///   thread for the surface. A create that then fails is only logged
    ///   (`…/wayland/event_loop/state.rs`, `Err(err) => log::error!("Failed to
    ///   create popup. {err:?}")`), and a create deferred behind a parent
    ///   mismatch is dropped outright after five 30 ms retries — either way
    ///   `self.popup` holds an id nothing ever mapped.
    /// - Destroying such an id is a no-op that emits nothing at all
    ///   (same file, the `Action::Destroy` arm: `None => { log::info!("No popup
    ///   to destroy"); return Ok(()); }`).
    ///
    /// An *anonymous* unit of debt booked for that popup would therefore never
    /// be paid by its own `Done`, and would instead swallow the next live
    /// menu's close — leaving `dropdowns_open` non-zero with no menu mapped and
    /// tooltips paused until the session ends. Keyed, the entry simply sits
    /// here unclaimed and consumes nothing.
    ///
    /// A `Vec` because toggling faster than the runtime drains can leave more
    /// than one in flight, and entries are removed **only** by their own id. An
    /// unclaimed entry is never evicted: it costs a `u64` and nothing else,
    /// while evicting one would hand its `Done` back to the by-elimination row
    /// — the un-pausing direction this whole ledger refuses to take.
    closing_popups: Vec<ClosingPopup>,
    /// Process-wide ownership of shared applet work. The descriptor held by
    /// this value is also what keeps the advisory lock alive.
    leadership: Leadership,
    /// A takeover owns the lock before its blocking state hydration finishes.
    /// Existing unit fixtures remain active leaders because this wrapper's
    /// default is deliberately ready.
    leader_readiness: LeaderReadiness,
    /// Generation of the one-shot retry timer used by non-leaders.
    leadership_generation: u64,
    /// Generation of the blocking takeover hydration snapshot.
    leadership_hydration_generation: u64,
    /// Config/mailbox watcher epoch used to invalidate a hydration snapshot.
    leadership_state_generation: u64,
    /// Applet settings (shuffle, retention). Defaults when the config context
    /// is unavailable.
    pub(crate) config: AppletConfig,
    /// cosmic-config context used to persist setting changes. `None` only if
    /// the config directory could not be created — the applet still runs with
    /// defaults, changes just don't persist.
    config_context: Option<cosmic_config::Config>,
    /// Independent watched entry for cross-process requests and notices.
    coordination: CoordinationConfig,
    /// Context used for raw, single-key coordination writes.
    coordination_context: Option<cosmic_config::Config>,
    /// Per-key generations for asynchronous setting writes. A follower
    /// completion may update the popup only when no newer write to that same
    /// key exists; leaders adopt before enqueueing.
    setting_write_generations: [u64; 4],
    /// One serialized queue prevents an older blocking write from landing
    /// after a newer click and regressing the on-disk key.
    setting_write_queue: VecDeque<(u64, AppletSetting)>,
    setting_write_inflight: bool,
    /// Generation guarding blocking fresh-config confirmations triggered by
    /// watcher payloads.
    config_confirmation_generation: u64,
    /// Injectable directory containing the short-lived coordination lock.
    /// Production uses [`state_dir`]; tests always provide a tempdir.
    coordination_state_dir: PathBuf,
    /// Newest peer refresh request reserved for the current or next fetch.
    peer_refresh_request: Option<u64>,
    /// Highest peer request covered by a fetch that finished in this
    /// process. This advances before the asynchronous acknowledgement write,
    /// so a late watcher/read completion cannot refetch the same request when
    /// that write is delayed or fails.
    peer_refresh_covered: u64,
    /// The one live-wallpaper read currently being used to start a peer
    /// refresh. While it is present, newer requests join the same reserved
    /// next fetch instead of spawning parallel reads.
    peer_refresh_live_read: Option<u64>,
    /// Request this non-leader popup is waiting for the leader to cover.
    requested_peer_refresh: Option<u64>,
    /// A blocking request-counter persist is in flight. Kept separate from
    /// `refresh_pending`: the popup must not claim to be checking until the
    /// write has actually landed.
    peer_refresh_write_pending: bool,
    /// One-shot acknowledgement timeout generation.
    peer_refresh_timeout_generation: u64,
    /// Read-only reload request generation. Task 8 attaches the asynchronous
    /// catalogue/live-state load to this already-guarded request point.
    non_leader_reload_generation: u64,
    /// Whether the follower reload in flight may ask the leader to repair a
    /// rebuilt catalogue. Popup opens and acknowledgement timeouts may; the
    /// reload a settled peer refresh triggers may not, or an offline leader
    /// (whose every repair fetch fails and is acknowledged as such) would be
    /// asked again on each acknowledgement, forever.
    non_leader_reload_repairs: bool,
    /// Newest peer-apply notice observed by this process. The generation is
    /// also the staleness guard for the leader's blocking cosmic-bg read.
    peer_apply_notice_generation: u64,
    /// All downloaded images (restored from disk at startup — no network).
    pub(crate) catalogue: Catalogue,
    /// Our idea of the currently applied wallpaper file. Refreshed from
    /// cosmic-bg's config at startup and around every fetch; not watched
    /// (accepted v1 limitation).
    pub(crate) current: Option<PathBuf>,
    /// A fetch pipeline is running (debounces refresh triggers).
    pub(crate) refresh_pending: bool,
    /// The startup thumbnail pass is running (see
    /// [`Window::start_thumbnail_pass_over`]). Together with
    /// `refresh_pending` this is the whole set of things that write into the
    /// thumbnail cache, which is what [`Window::may_sweep_thumbnails`] needs
    /// to know.
    thumbnail_pass_pending: bool,
    /// A refresh finished under the producer write interlock
    /// ([`Backfill::deferred`]) — its downloads have no thumbnails and the
    /// pass that owned the cache ran over a snapshot taken before they
    /// existed. Settled by one more startup-style pass, armed the moment
    /// the running pass ends ([`Window::finish_thumbnail_pass`]); without
    /// it the just-applied image would show the placeholder, and get no
    /// accent recompute, until the next refresh ~24 h out.
    thumbnails_owed: bool,
    /// Cold-start auto-apply state (see [`ColdStart`]). Spent by *any*
    /// successful apply (auto, manual navigation, shuffle tick): see
    /// [`Window::on_apply_success`].
    cold_start: ColdStart,
    /// The out-of-retention fallback the last refresh downloaded
    /// ([`RefreshBatch::fallback`]), exempt from the age prune until it is
    /// applied (the current-wallpaper protection takes over) or the next
    /// refresh replaces the selection. In-memory only: a restart between
    /// the download and a failed apply lets the startup prune delete it,
    /// and the next refresh simply re-downloads it.
    protected_fallback: Option<PathBuf>,
    /// The startup restore (or a takeover hydration) rebuilt a nonempty
    /// catalogue from the folder scan ([`Provenance::Rebuilt`]): its entries
    /// show filenames until a fetch merge refills their metadata, so the
    /// next [`Window::arm_leader_duties`] starts a repair refresh at once
    /// instead of waiting for the scheduled one, and a follower instead asks
    /// the leader for one through the mailbox
    /// ([`Window::request_follower_repair`]). Consumed by that arming.
    metadata_repair_due: bool,
    /// Generation counter for the one-shot refresh timer; `RefreshDue`
    /// messages carrying a stale generation are ignored.
    timer_generation: u64,
    /// Generation counter for the one-shot shuffle timer (same stale-tick
    /// scheme as the refresh timer).
    shuffle_generation: u64,
    /// A shuffle tick is currently scheduled.
    shuffle_armed: bool,
    /// When the last successful fetch completed (status footer).
    pub(crate) last_updated: Option<DateTime<Utc>>,
    /// The last fetch error, cleared on success (status footer).
    pub(crate) last_error: Option<RefreshError>,
    /// cosmic-theme config handles for the accent-from-wallpaper feature,
    /// built in `init` (mirroring `config_context`). `None` only if the theme
    /// configs could not be opened — the feature is then inert (the toggle
    /// refuses to enable, computes are never armed). Tests inject
    /// TempDir-rooted handles ([`accent::ThemeHandles::sandboxed`]).
    accent_handles: Option<accent::ThemeHandles>,
    /// Generation counter for the blocking-pool accent theme tasks;
    /// [`Message::AccentWriteFinished`] carrying a stale generation is
    /// ignored (same scheme as the refresh/shuffle timers).
    accent_write_generation: u64,
    /// The in-flight accent theme write/restore, if any — the **write
    /// guard**. At most one theme-writing task ever runs, and while one does:
    /// builder reads are off-limits (they would race our own write), so
    /// `AccentComputed` results are queued ([`Self::accent_recompute_queued`])
    /// and `ConfigUpdated` accent flips are *not* routed through the toggle
    /// lifecycle (our own multi-key persists echo back stale/torn during a
    /// slow write — the 2026-08-08 btrfs incident's oscillation); the
    /// completion handler reconciles against a fresh disk read instead,
    /// taken *before* its own persists and compared against the flight's
    /// spawn-time baseline ([`Self::accent_disk_enabled_at_spawn`]).
    accent_inflight: Option<AccentInflight>,
    /// An accent result arrived while a theme task was in flight and was
    /// dropped; the completion handler re-arms a fresh compute for the
    /// then-current wallpaper.
    accent_recompute_queued: bool,
    /// A toggle requested while a theme task was in flight (rendered by the
    /// popup's toggler; also pinned onto the disk config so the completion's
    /// fresh-read reconcile adopts it). Cleared by that reconcile.
    accent_flip_requested: Option<bool>,
    /// The on-disk `accent_enabled` at the moment the current accent flight
    /// *chain* started ([`Window::spawn_accent_task`]; a chained rollback
    /// keeps the original write's baseline). The completion's reconcile
    /// detects a genuine external flip by the disk flag *changing* against
    /// this baseline during the flight — comparing the completion-time disk
    /// read against memory instead would misread the enable path's
    /// deliberate persist ordering (the flag lands last, so mid-
    /// `EnableRestore` the disk legitimately still says `false`) as an
    /// external disable. `None` when no config context existed at spawn.
    accent_disk_enabled_at_spawn: Option<bool>,
    /// Generation counter for the lock-poke ladder
    /// ([`lockwatch::POKE_DELAYS`]); a [`Message::LockPokeDue`] carrying a
    /// stale generation is dropped, so a fresh [`Message::LockEvent`]
    /// atomically replaces any pending ladder (rapid re-locks: the last
    /// event's ladder wins). Same scheme as the refresh/shuffle timers.
    lock_poke_generation: u64,
    /// cosmic-bg *state* handle for the lock-screen poke
    /// ([`wallpaper::poke_state`] — the cosmic-greeter#511 workaround,
    /// rationale in `lockwatch.rs`), built once in `init` via
    /// [`wallpaper::poke_state_handle`] and cloned into each poke task.
    /// `None` when the state context cannot be opened — every poke is then a
    /// no-op. Tests inject a tempdir-rooted `Config::with_custom_path`
    /// handle (mirroring `config_context`).
    poke_config: Option<cosmic_config::Config>,
    /// Hermetic inputs used by tests that drain the real hydration/reload
    /// tasks. Production always resolves the ordinary applet paths and live
    /// cosmic-bg state inside the blocking task.
    #[cfg(test)]
    test_snapshot_inputs: Option<TestSnapshotInputs>,
    /// Sandboxed contexts used to exercise recovery from transient context
    /// creation failure without consulting the test runner's real XDG dirs.
    #[cfg(test)]
    test_hydration_contexts: Option<(cosmic_config::Config, cosmic_config::Config)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LeaderReadiness {
    #[default]
    Ready,
    Hydrating,
}

/// One entry of [`Window::closing_popups`]: a popup of ours whose destroy has
/// been requested and whose `PopupClosed` is still outstanding, plus the menu
/// closes that teardown still owes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClosingPopup {
    /// The popup's own id — the only thing that settles this entry.
    id: window::Id,
    /// Menu closes owed by the session this popup ended: one per menu counted
    /// when [`Message::TogglePopup`] took it. They are **anonymous** (a menu's
    /// id is minted inside the widget), so any unrecognised close pays one
    /// before [`Window::dropdowns_open`] is touched — but they die with this
    /// entry when `id`'s own close arrives, and that is what stops a menu
    /// create the runtime dropped from becoming permanent debt.
    ///
    /// The discard is evidence-based, not a heuristic: the `Action::Destroy`
    /// arm (`…/wayland/event_loop/state.rs`) collects the popup **and its
    /// children** into `to_destroy`, reverses, and then iterates
    /// `.into_iter().rev()`, so it `send_event`s each child's `Done` *before*
    /// the parent's, into one unbounded channel. By the time the id named here
    /// comes back, every close that teardown will ever emit has already been
    /// delivered; a unit still owed is a menu that never mapped. (Only the
    /// order within this one teardown is relied on — not any ordering against
    /// the rest of the queue, which is exactly what the debt exists for.)
    menus_owed: u32,
}

/// What the single in-flight accent theme task ([`Window::accent_inflight`])
/// is doing, with everything its completion handler — and the derived
/// [`AccentJob`] — needs. The builders in `Write` are the exact ones the plan
/// compared: they travel into the task, so there is no re-read between the
/// don't-clobber compare and the write.
#[derive(Debug, Clone)]
enum AccentInflight {
    /// `accent::write_accents` is running; on success the completion handler
    /// persists `pair` as `accent_last_written`, and when *that* persist
    /// fails it rolls the themes back to `previous` (a follow-up
    /// [`AccentInflight::Rollback`] task).
    Write {
        builders: Box<accent::Builders>,
        pair: accent::AccentPair,
        previous: accent::AccentSnapshot,
    },
    /// The disable path's snapshot restore. The toggle is already off (and
    /// `last_written` cleared) — the snapshot itself is cleared only once
    /// the restore verifiably lands; a failure keeps it (the kept-snapshot
    /// shape, retried by the next enable's deferred restore).
    DisableRestore { snapshot: accent::AccentSnapshot },
    /// An enable's deferred restore of a kept snapshot. Nothing is persisted
    /// yet; the enable's tail (clear `last_written`, flip the setting, arm a
    /// compute) runs only on success — a failure refuses the enable exactly
    /// like the old synchronous path.
    EnableRestore { snapshot: accent::AccentSnapshot },
    /// Rolling the themes back to `previous` after a write whose
    /// `last_written` persist failed. If even this fails, the in-memory
    /// record adopts `pair` so this session's guard keeps matching the disk.
    Rollback {
        pair: accent::AccentPair,
        previous: accent::AccentSnapshot,
    },
}

/// The theme write an [`AccentInflight`] record stands for — data-only, so
/// production (the blocking-pool task) and tests (running the identical job
/// synchronously before feeding [`Message::AccentWriteFinished`]) share one
/// derivation ([`accent_job`]) and one executor ([`run_accent_job`]).
enum AccentJob {
    Write {
        builders: Box<accent::Builders>,
        light: [u8; 3],
        dark: [u8; 3],
    },
    Restore(accent::AccentSnapshot),
}

/// Fresh state read after a follower acquires the leader lock. Keeping this
/// payload data-only lets tests exercise the completion decision without
/// touching the real desktop configuration.
#[derive(Debug, Clone)]
pub(crate) struct LeadershipHydration {
    config_context: Option<cosmic_config::Config>,
    coordination_context: Option<cosmic_config::Config>,
    config: AppletConfig,
    coordination: CoordinationConfig,
    catalogue: Catalogue,
    /// Whether the snapshot's catalogue was loaded or rebuilt from the folder
    /// scan: a rebuilt one wants the same metadata repair a rebuilt startup
    /// gets ([`Window::arm_leader_duties`]).
    provenance: Provenance,
    live: wallpaper::CurrentWallpaper,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct TestSnapshotInputs {
    catalogue_path: PathBuf,
    images_dir: PathBuf,
    live: wallpaper::CurrentWallpaper,
}

/// Read-only catalogue and live-wallpaper snapshot used to refresh a
/// follower's popup without letting it mutate shared state.
#[derive(Debug, Clone)]
pub(crate) struct NonLeaderReload {
    catalogue: Catalogue,
    /// A follower cannot repair a rebuilt catalogue itself; it asks the
    /// leader to ([`Window::finish_non_leader_reload`]).
    provenance: Provenance,
    live: wallpaper::CurrentWallpaper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppletSetting {
    ShuffleEnabled(bool),
    ShuffleInterval(u32),
    Retention(u16),
    AccentEnabled(bool),
}

impl AppletSetting {
    fn slot(self) -> usize {
        match self {
            Self::ShuffleEnabled(_) => 0,
            Self::ShuffleInterval(_) => 1,
            Self::Retention(_) => 2,
            Self::AccentEnabled(_) => 3,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::ShuffleEnabled(_) => "shuffle_enabled",
            Self::ShuffleInterval(_) => "shuffle_interval_secs",
            Self::Retention(_) => "retention_days",
            Self::AccentEnabled(_) => "accent_enabled",
        }
    }

    fn adopt(self, config: &mut AppletConfig) {
        match self {
            Self::ShuffleEnabled(value) => config.shuffle_enabled = value,
            Self::ShuffleInterval(value) => config.shuffle_interval_secs = value,
            Self::Retention(value) => config.retention_days = value,
            Self::AccentEnabled(value) => config.accent_enabled = value,
        }
    }

    fn persist(self, context: &cosmic_config::Config) -> Result<(), cosmic_config::Error> {
        use cosmic_config::ConfigSet as _;

        match self {
            Self::ShuffleEnabled(value) => context.set(self.key(), value),
            Self::ShuffleInterval(value) => context.set(self.key(), value),
            Self::Retention(value) => context.set(self.key(), value),
            Self::AccentEnabled(value) => context.set(self.key(), value),
        }
        .map(|_| ())
    }
}

fn read_non_leader_reload(
    catalogue_path: &Path,
    images_dir: &Path,
    live: wallpaper::CurrentWallpaper,
) -> NonLeaderReload {
    let CatalogueRestore {
        catalogue,
        provenance,
    } = Catalogue::load_or_rebuild(catalogue_path, images_dir);
    NonLeaderReload {
        catalogue,
        provenance,
        live,
    }
}

/// Watchers may deliver snapshots out of order. Mailbox counters are
/// append-only evidence, so merge every field monotonically instead of
/// replacing a newer in-memory snapshot with a late payload.
fn merge_coordination(current: &mut CoordinationConfig, incoming: CoordinationConfig) {
    current.refresh_request = current.refresh_request.max(incoming.refresh_request);
    if incoming.refresh_completion.request > current.refresh_completion.request {
        current.refresh_completion = incoming.refresh_completion;
    }
    let current_notice = current
        .apply_notice
        .as_ref()
        .map_or(0, |notice| notice.generation);
    if incoming
        .apply_notice
        .as_ref()
        .is_some_and(|notice| notice.generation > current_notice)
    {
        current.apply_notice = incoming.apply_notice;
    }
}

fn accent_job(inflight: &AccentInflight) -> AccentJob {
    match inflight {
        AccentInflight::Write { builders, pair, .. } => AccentJob::Write {
            builders: builders.clone(),
            light: pair.light,
            dark: pair.dark,
        },
        AccentInflight::DisableRestore { snapshot }
        | AccentInflight::EnableRestore { snapshot } => AccentJob::Restore(*snapshot),
        AccentInflight::Rollback { previous, .. } => AccentJob::Restore(*previous),
    }
}

/// The blocking half of an accent task: the ~hundreds of fsync'd bytes of
/// theme I/O that must never run inline in `update()` (the 2026-08-08 btrfs
/// incident: each fsync forced a transaction commit and one write cycle
/// blocked the UI thread for minutes). Failures are logged here — the
/// completion message only carries success.
fn run_accent_job(handles: &accent::ThemeHandles, job: AccentJob) -> bool {
    let result = match job {
        AccentJob::Write {
            builders,
            light,
            dark,
        } => accent::write_accents(handles, *builders, light, dark),
        AccentJob::Restore(snapshot) => accent::restore_accents(handles, snapshot),
    };
    if let Err(error) = &result {
        tracing::warn!("accent theme write failed: {error}");
    }
    result.is_ok()
}

/// The blocking half of a lock poke: the cosmic-bg state read+write
/// ([`wallpaper::poke_state`]). Not inline in `update()` — a state-dir write
/// under I/O pressure is exactly the 2026-08-08 freeze shape (cheap
/// insurance; see the accent tasks). Failures are logged here — the
/// completion message only carries whether a write happened. Shared by the
/// spawned task and the test-side settle helper so they cannot drift.
fn run_lock_poke(config: &cosmic_config::Config) -> bool {
    match wallpaper::poke_state(config) {
        Ok(wrote) => wrote,
        Err(error) => {
            tracing::warn!("lock-screen state poke failed: {error}");
            false
        }
    }
}

/// The one-shot cold-start auto-apply: a fresh install (empty catalogue at
/// startup) owes the user one applied wallpaper — that's why the applet was
/// installed — even over whatever foreign default is currently displayed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum ColdStart {
    /// Catalogue empty at startup, no apply attempted yet: the first
    /// successful fetch auto-applies unconditionally.
    Pending,
    /// The cold-start auto-apply *failed* while this wallpaper (if any)
    /// was displayed. The next scheduled fetch retries — but only while
    /// the display still shows that same wallpaper: a user actively
    /// picking a different wallpaper between failure and retry must win
    /// (the retry must not clobber it the way the unconditional
    /// `Pending` branch would).
    RetryOver(Option<PathBuf>),
    /// Spent (an apply succeeded, or the catalogue had images at
    /// startup): only the warm `is_ours` rule auto-applies from here on.
    #[default]
    Done,
}

impl ColdStart {
    /// Whether the cold-start branch may bypass the warm `is_ours` check
    /// and auto-apply over `live` (the wallpaper displayed right now).
    /// A file of ours needs no bypass — [`wallpaper::should_auto_apply`]'s
    /// warm branch covers it regardless of what this returns.
    fn applies_over(&self, live: Option<&Path>) -> bool {
        match self {
            Self::Pending => true,
            Self::RetryOver(at_failure) => match live {
                // Nothing knowable is displayed (color source,
                // per-output mode) — the same ground
                // `wallpaper::should_auto_apply` treats as safe to
                // apply over.
                None => true,
                Some(path) => Some(path) == at_failure.as_deref(),
            },
            Self::Done => false,
        }
    }
}

/// Why a refresh failed — the footer distinguishes local disk trouble from
/// Bing being unreachable.
#[derive(Debug, Clone)]
pub enum RefreshError {
    /// Transport/status/parse trouble (including an empty image list).
    Network(String),
    /// Local I/O failure persisting a download.
    Disk(String),
}

impl From<bing::FetchError> for RefreshError {
    fn from(error: bing::FetchError) -> Self {
        let text = error.to_string();
        if error.is_local() {
            Self::Disk(text)
        } else {
            Self::Network(text)
        }
    }
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(detail) | Self::Disk(detail) => write!(f, "{detail}"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(window::Id),
    /// Settings changed on disk (external edit or our own write echoed back).
    ConfigUpdated(AppletConfig),
    /// Fresh disk confirmation for a config watcher payload. The read runs on
    /// the blocking pool; the generation and role prevent late adoption.
    ConfigConfirmed {
        generation: u64,
        was_active_leader: bool,
        config: AppletConfig,
    },
    /// Cross-process coordination mailbox changed on disk.
    CoordinationUpdated(CoordinationConfig),
    /// A leader finished reading live cosmic-bg state for a peer refresh.
    /// `request` guards both role changes and a newer mailbox observation.
    PeerRefreshLiveRead {
        request: u64,
        live: wallpaper::CurrentWallpaper,
    },
    /// The one-shot leadership retry timer fired.
    LeadershipTick(u64),
    /// The blocking takeover snapshot finished. Both generations must still
    /// match before this process may become an active leader.
    LeadershipHydrated {
        generation: u64,
        state_generation: u64,
        result: Result<LeadershipHydration, String>,
    },
    /// A follower's read-only catalogue/live-state snapshot finished.
    NonLeaderReloaded {
        generation: u64,
        result: Result<NonLeaderReload, String>,
    },
    /// An applet raw single-key setting persist completed.
    AppletSettingWritten {
        generation: u64,
        setting: AppletSetting,
        result: Result<(), String>,
    },
    /// The refresh timer fired (payload: the generation it was armed with).
    RefreshDue(u64),
    /// The fetch pipeline finished (payload: the freshly fetched entries,
    /// merged into the live catalogue on the UI thread).
    RefreshFinished(Result<RefreshBatch, RefreshError>),
    /// A non-leader's blocking mailbox counter allocation completed.
    PeerRefreshRequested(Result<u64, String>),
    /// The leader's blocking acknowledgement persist completed.
    PeerRefreshCompletionWritten {
        completion: PeerRefreshCompletion,
        result: Result<bool, String>,
    },
    /// A non-leader did not observe a covering acknowledgement in time.
    PeerRefreshTimeout {
        generation: u64,
        request: u64,
    },
    /// A follower's blocking apply-notice persist completed.
    PeerApplyNoticeWritten(Result<PeerApplyNotice, String>),
    /// The leader finished validating a peer apply against live cosmic-bg
    /// state. The notice generation guards against a newer apply overtaking
    /// this blocking read.
    PeerApplyValidated {
        generation: u64,
        live: wallpaper::CurrentWallpaper,
    },
    /// The startup thumbnail pass finished. Also re-renders the popup, so
    /// previews generated while it was open appear without a reopen.
    ThumbnailsReady,
    /// Apply this downloaded file as the wallpaper (prev/next/newest
    /// buttons — browsing applies immediately).
    ApplyImage(PathBuf),
    /// The popup's refresh button (debounced while a fetch is pending).
    RefreshNow,
    /// Open a URL in the default browser (`xdg-open`): "About this image"
    /// → copyright link.
    OpenUrl(String),
    /// Open a downloaded file in the default viewer (`xdg-open`):
    /// thumbnail click → full image. The path stays a `PathBuf` end to
    /// end — no lossy string conversion.
    OpenFile(PathBuf),
    /// The shuffle timer fired (payload: the generation it was armed with).
    ShuffleDue(u64),
    /// The popup's shuffle toggler.
    SetShuffleEnabled(bool),
    /// The popup's shuffle-interval dropdown (payload: dropdown index).
    SetShuffleInterval(usize),
    /// The popup's "Keep images" retention dropdown (payload: dropdown
    /// index).
    SetRetention(usize),
    /// The accent-from-wallpaper toggler. On: snapshot the live accents,
    /// then compute for the current wallpaper. Off: restore the snapshot
    /// verbatim and clear the persisted feature state.
    SetAccentEnabled(bool),
    /// The async accent extraction finished. `source` is the staleness
    /// guard (same hazard class as the generation-counter timers): a result
    /// for a wallpaper that is no longer current is dropped. `hue: None`
    /// means an effectively grey wallpaper — still a real answer (warm
    /// grey), never a failure (failures send no message at all).
    AccentComputed {
        source: PathBuf,
        hue: Option<f32>,
    },
    /// The blocking-pool accent theme write/restore finished (payload: the
    /// generation it was spawned with — a stale completion is ignored, same
    /// scheme as the refresh/shuffle timers — and whether the write landed).
    /// What it *was* lives in [`Window::accent_inflight`], not the message.
    AccentWriteFinished {
        generation: u64,
        success: bool,
    },
    /// A lock-relevant event from the logind watch
    /// ([`lockwatch::subscription`]): arm the poke ladder — the
    /// cosmic-greeter#511 workaround (full rationale in `lockwatch.rs`).
    LockEvent(lockwatch::LockEvent),
    /// A rung of the poke ladder came due (payload: the generation it was
    /// armed with — a stale rung from a replaced ladder is dropped).
    LockPokeDue(u64),
    /// The async state poke finished (payload: whether a write happened).
    /// Log-only by design: pokes never touch other applet state.
    LockPokeFinished(bool),
    /// Surface actions from the tooltip widget ([`crate::tooltip`]). Every
    /// popup parented to `self.popup` is routed through a ledger-aware
    /// message like this one — there is deliberately no blind `Surface`
    /// forwarder left, since anything that bypassed the ledger could map a
    /// second child popup and trip the xdg-shell topmost rule. See
    /// [`Window::on_tooltip_surface`].
    TooltipSurface(cosmic::surface::Action),
    /// Surface actions from a `popup_dropdown` menu, routed here for the same
    /// reason as [`Message::TooltipSurface`] — see
    /// [`Window::on_dropdown_surface`].
    DropdownSurface(cosmic::surface::Action),
}

/// A destroy for the shared tooltip surface ([`crate::tooltip::window_id`]).
///
/// **Idempotent by construction**, which is what lets the popup ledger get away
/// with a single counter: the runtime's `Action::Destroy` arm logs
/// `"No popup to destroy"` and returns *before touching any state* when the id
/// is not mapped (`iced/winit/src/platform_specific/wayland/event_loop/state.rs`).
/// So it can be emitted whenever a tooltip *might* be mapped, and nothing has
/// to track whether one actually is — a flag for that would have to be set on
/// the widget's `Action::Task` (arming, not creation: the create the delayed
/// future resolves to goes straight to the runtime, never back through
/// `Message`) and would be wrong whenever the five tooltip widgets publish
/// arm/leave in widget-tree order rather than pointer order.
fn destroy_tooltip() -> app::Task<Message> {
    cosmic::surface::surface_task(cosmic::surface::action::destroy_popup(tooltip::window_id()))
}

impl Window {
    fn is_active_leader(&self) -> bool {
        self.leadership.is_leader() && self.leader_readiness == LeaderReadiness::Ready
    }

    fn schedule_leadership_retry(&mut self) -> app::Task<Message> {
        self.leadership_generation = self.leadership_generation.wrapping_add(1);
        let generation = self.leadership_generation;
        cosmic::task::future(async move {
            tokio::time::sleep(LEADERSHIP_RETRY_DELAY).await;
            Message::LeadershipTick(generation)
        })
    }

    /// Retry the advisory lock, or retry hydration when the lock is already
    /// ours but the contexts needed for a complete snapshot were unavailable.
    fn on_leadership_tick(&mut self, generation: u64) -> app::Task<Message> {
        if generation != self.leadership_generation || self.is_active_leader() {
            return Task::none();
        }

        if self.leadership.is_leader() {
            // Consume this one-shot before spawning the blocking read; a
            // duplicate delivery must not create concurrent hydrations.
            self.leadership_generation = self.leadership_generation.wrapping_add(1);
            return self.start_leadership_hydration();
        }
        if !self.leadership.try_acquire() {
            return self.schedule_leadership_retry();
        }

        // Owning the file lock is intentionally not sufficient to run shared
        // work: first invalidate this timer and hydrate state written by the
        // former leader and peers.
        self.leader_readiness = LeaderReadiness::Hydrating;
        self.leadership_generation = self.leadership_generation.wrapping_add(1);
        self.start_leadership_hydration()
    }

    /// Read every takeover-owned input away from the UI thread. A missing
    /// context cannot yield a complete snapshot, so remain inert and retry.
    fn start_leadership_hydration(&mut self) -> app::Task<Message> {
        if !self.leadership.is_leader() || self.leader_readiness != LeaderReadiness::Hydrating {
            return Task::none();
        }
        let config_context = self.config_context.clone();
        let coordination_context = self.coordination_context.clone();
        #[cfg(test)]
        let test_contexts = self.test_hydration_contexts.clone();

        self.leadership_hydration_generation = self.leadership_hydration_generation.wrapping_add(1);
        let generation = self.leadership_hydration_generation;
        let state_generation = self.leadership_state_generation;
        #[cfg(test)]
        let (snapshot_catalogue_path, snapshot_images_dir, test_live) =
            if let Some(inputs) = &self.test_snapshot_inputs {
                (
                    inputs.catalogue_path.clone(),
                    inputs.images_dir.clone(),
                    Some(inputs.live.clone()),
                )
            } else {
                (catalogue_path(), wallpaper::download_dir(), None)
            };
        #[cfg(not(test))]
        let (snapshot_catalogue_path, snapshot_images_dir) =
            (catalogue_path(), wallpaper::download_dir());
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                let (test_config, test_coordination) = test_contexts
                    .map(|(config, coordination)| (Some(config), Some(coordination)))
                    .unwrap_or((None, None));
                let config_context = config_context
                    .or({
                        #[cfg(test)]
                        {
                            test_config
                        }
                        #[cfg(not(test))]
                        {
                            None
                        }
                    })
                    .map(Ok)
                    .unwrap_or_else(AppletConfig::context)
                    .map_err(|error| format!("cannot open applet config: {error}"))?;
                let coordination_context = coordination_context
                    .or({
                        #[cfg(test)]
                        {
                            test_coordination
                        }
                        #[cfg(not(test))]
                        {
                            None
                        }
                    })
                    .map(Ok)
                    .unwrap_or_else(CoordinationConfig::context)
                    .map_err(|error| format!("cannot open coordination config: {error}"))?;
                let restore =
                    Catalogue::load_or_rebuild(&snapshot_catalogue_path, &snapshot_images_dir);
                Ok::<_, String>(LeadershipHydration {
                    config: AppletConfig::load(&config_context),
                    coordination: CoordinationConfig::load(&coordination_context),
                    config_context: Some(config_context),
                    coordination_context: Some(coordination_context),
                    catalogue: restore.catalogue,
                    provenance: restore.provenance,
                    live: {
                        #[cfg(test)]
                        if let Some(live) = test_live {
                            live
                        } else {
                            wallpaper::current_wallpaper()
                        }
                        #[cfg(not(test))]
                        wallpaper::current_wallpaper()
                    },
                })
            })
            .await
            .map_err(|error| format!("leadership hydration task failed: {error}"))
            .and_then(|result| result);
            Message::LeadershipHydrated {
                generation,
                state_generation,
                result,
            }
        })
    }

    /// Adopt an injected takeover snapshot when it is still current. This is
    /// the sole readiness transition, and deliberately performs no reads.
    fn finish_leadership_hydration(
        &mut self,
        generation: u64,
        state_generation: u64,
        result: Result<LeadershipHydration, String>,
    ) -> app::Task<Message> {
        if !self.leadership.is_leader()
            || self.leader_readiness != LeaderReadiness::Hydrating
            || generation != self.leadership_hydration_generation
        {
            return Task::none();
        }
        let hydration = match result {
            Ok(hydration) => hydration,
            Err(error) => {
                tracing::warn!("cannot hydrate takeover state: {error}");
                return self.schedule_leadership_retry();
            }
        };
        if state_generation != self.leadership_state_generation {
            tracing::debug!("takeover state changed during hydration; reading it again");
            return self.start_leadership_hydration();
        }

        if let Some(context) = hydration.config_context {
            self.config_context = Some(context);
        }
        if let Some(context) = hydration.coordination_context {
            self.coordination_context = Some(context);
        }
        self.config = hydration.config;
        self.coordination = hydration.coordination;
        self.catalogue = hydration.catalogue;
        // The same repair decision a rebuilt startup gets: the follower
        // period may have consumed its own request, but the snapshot just
        // read is what this leader will serve from.
        self.metadata_repair_due = hydration.provenance == Provenance::Rebuilt;
        self.peer_apply_notice_generation = self
            .coordination
            .apply_notice
            .as_ref()
            .map_or(0, |notice| notice.generation);
        self.sync_current(&hydration.live);
        // A takeover must not inherit follower request state. In particular,
        // stale `refresh_pending` would reject every leader timer forever.
        self.peer_refresh_request = None;
        self.peer_refresh_covered = self.coordination.refresh_completion.request;
        self.peer_refresh_live_read = None;
        self.requested_peer_refresh = None;
        self.peer_refresh_write_pending = false;
        self.refresh_pending = false;
        self.setting_write_queue.clear();
        self.peer_refresh_timeout_generation = self.peer_refresh_timeout_generation.wrapping_add(1);
        self.non_leader_reload_generation = self.non_leader_reload_generation.wrapping_add(1);
        // Only an explicitly empty live wallpaper is safe for a takeover to
        // retain first-run auto-apply semantics. Any displayed/unknown state
        // conservatively spends the bypass so an external choice is safe.
        self.cold_start = if self.catalogue.images.is_empty()
            && matches!(&hydration.live, wallpaper::CurrentWallpaper::NoFile)
        {
            ColdStart::Pending
        } else {
            ColdStart::Done
        };
        self.leader_readiness = LeaderReadiness::Ready;
        self.arm_leader_duties(hydration.live)
    }

    /// Arm ordinary leader startup work. The startup thumbnail pass is the
    /// guaranteed preview producer and is armed unconditionally; a refresh
    /// is started *in addition* — never instead — when a peer request is
    /// outstanding or the restore rebuilt a nonempty catalogue whose
    /// metadata wants repairing (one refresh covers both). Offline, that
    /// refresh dies at the list fetch and the pass is the only thing that
    /// ever fills the cache; and because the pass is armed first, the
    /// refresh sees `thumbnail_pass_pending` and writes no thumbnails of
    /// its own (the producer write interlock, [`Backfill::deferred`]).
    fn arm_leader_duties(&mut self, live: wallpaper::CurrentWallpaper) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }

        let delay = schedule::next_refresh(
            self.catalogue
                .newest()
                .map(|entry| entry.fullstartdate.as_str()),
            Utc::now(),
        );
        let timer = self.schedule_refresh(delay);
        let shuffle = self.sync_shuffle(false);
        let outstanding = (self.coordination.refresh_request
            > self.coordination.refresh_completion.request)
            .then_some(self.coordination.refresh_request);
        let repair =
            std::mem::take(&mut self.metadata_repair_due) && !self.catalogue.images.is_empty();
        let pass = self.start_thumbnail_pass_over(live.clone());
        let refresh = if outstanding.is_some() || repair {
            if repair {
                tracing::info!(
                    "catalogue was rebuilt from the folder; refreshing to repair metadata"
                );
            }
            // Coalesced: the one refresh settles the peer request and
            // repairs the rebuilt entries alike.
            self.peer_refresh_request = outstanding;
            self.start_refresh_over(live)
        } else {
            Task::none()
        };
        let accent = self.accent_compute_for_current();
        Task::batch([timer, shuffle, pass, refresh, accent])
    }

    fn arm_initial_duties(&mut self, live: wallpaper::CurrentWallpaper) -> app::Task<Message> {
        if self.is_active_leader() {
            self.arm_leader_duties(live)
        } else {
            let retry = self.schedule_leadership_retry();
            let repair = if std::mem::take(&mut self.metadata_repair_due) {
                self.request_follower_repair()
            } else {
                Task::none()
            };
            Task::batch([retry, repair])
        }
    }

    /// A follower found a rebuilt, nonempty catalogue: it must not fetch
    /// itself, so it asks the leader for one refresh through the mailbox —
    /// the same request the popup button makes, with the same duplicate
    /// suppression ([`Window::refresh_now`]: nothing while the counter
    /// persist, the refresh, or the acknowledgement is pending) and the same
    /// timeout retry ([`Window::timeout_peer_refresh`] reloads, and a still
    /// rebuilt reload asks again).
    fn request_follower_repair(&mut self) -> app::Task<Message> {
        // Both callers are the non-leader branch already; only emptiness
        // is decided here.
        if self.catalogue.images.is_empty() {
            return Task::none();
        }
        if self.refresh_pending || self.peer_refresh_write_pending {
            tracing::debug!("rebuilt catalogue repair joins the pending peer refresh");
            return Task::none();
        }
        tracing::info!(
            "catalogue was rebuilt from the folder; asking the leader to repair metadata"
        );
        self.refresh_now()
    }

    /// Whether a dropdown menu is believed to be mapped — the ledger's one
    /// question ([`Window::dropdowns_open`], whose doc carries the rules).
    pub(crate) fn dropdown_open(&self) -> bool {
        self.dropdowns_open > 0
    }

    /// A popup surface closed: ours, the tooltip's, or a dropdown menu's.
    ///
    /// `PopupClosed` fires for popups we destroy ourselves as well as for
    /// compositor dismissals — the runtime's `Action::Destroy` arm sends a
    /// `PopupEvent::Done` for *every* popup it tears down, byte for byte the
    /// same emission the compositor path makes
    /// (`iced/winit/src/platform_specific/wayland/event_loop/state.rs`, the
    /// `for popup in to_destroy.into_iter().rev()` loop), translated against
    /// the winit-side `surface_ids` map rather than the state's own. It is
    /// still the *only* signal for a dropdown dismissed by grab loss, which
    /// publishes no `DestroyPopup` of its own.
    fn on_popup_closed(&mut self, id: window::Id) -> app::Task<Message> {
        if id == tooltip::window_id() {
            // Nothing to record: the ledger tracks menus only.
            tracing::debug!(
                "popup closed: the tooltip (dropdowns open: {})",
                self.dropdowns_open
            );
            return Task::none();
        }
        if self.popup == Some(id) {
            // Our popup takes every child with it, so the count goes to zero
            // whatever it held — but each of those children still owes a
            // `Done` (the compositor `done`s the whole grab chain), and those
            // belong to the session that just ended, not to the next one.
            // Anonymously, there being no id to key them to, and only until the
            // next session's popup is adopted — by then the whole chain's
            // closes have been delivered, so a unit still standing is a menu
            // that never mapped ([`Window::stale_menu_closes`]).
            self.popup = None;
            self.stale_menu_closes = self.stale_menu_closes.saturating_add(self.dropdowns_open);
            self.dropdowns_open = 0;
            tracing::debug!(
                "popup closed: our own popup (dropdowns open: 0, menu closes owed: {})",
                self.stale_menu_closes
            );
        } else if let Some(index) = self.closing_popups.iter().position(|open| open.id == id) {
            // A popup of ours that `TogglePopup` already took, announcing its
            // destroy back to us long after the fact (see the field doc for the
            // delivery trace). Its id was booked, so it is settled by name and
            // charged to nothing — never to the live count, and never to the
            // anonymous menu debt, which a popup that was never mapped would
            // leave owed forever.
            //
            // Its own session's menus are settled with it: upstream emits every
            // child's `Done` ahead of the parent's within one teardown
            // ([`ClosingPopup::menus_owed`]), so anything still owed here is a
            // menu that never mapped — a create the runtime dropped. Left
            // standing it would swallow the *next* live menu close and strand
            // `dropdowns_open` at one with nothing mapped, session after
            // session.
            let settled = self.closing_popups.remove(index);
            tracing::debug!(
                "popup closed: a popup of ours already taken, discarding {} unpayable menu close(s) (dropdowns open: {}, popups closing: {})",
                settled.menus_owed,
                self.dropdowns_open,
                self.closing_popups.len()
            );
        } else if self.stale_menu_closes > 0 {
            // A menu close owed by a session a compositor dismissal ended — see
            // the field doc for why it can land here after the session was
            // reset. It says nothing about the menus of the *current* session,
            // so pay the debt and leave the live count alone.
            self.stale_menu_closes -= 1;
            tracing::debug!(
                "popup closed: a menu owed by a dismissed session (dropdowns open: {}, menu closes owed: {})",
                self.dropdowns_open,
                self.stale_menu_closes
            );
        } else if let Some(owing) = self
            .closing_popups
            .iter_mut()
            .find(|closing| closing.menus_owed > 0)
        {
            // A menu close owed by a session `TogglePopup` ended, whose popup's
            // own `Done` has not come back yet — same rule as the row above,
            // only keyed to the popup that owes it.
            owing.menus_owed -= 1;
            tracing::debug!(
                "popup closed: a menu owed by a popup still closing (dropdowns open: {}, that popup still owes: {})",
                self.dropdowns_open,
                owing.menus_owed
            );
        } else {
            // By elimination: a dropdown menu of the current session. Its
            // window id is minted inside the widget (`window::Id::unique()`
            // into private state) and is never visible to us at creation, so
            // there is nothing to match on. This is a *decrement*, not a
            // clear: a `Done` that arrives after a newer create still belongs
            // to the create it pairs with, and clearing would leave the count
            // at zero with that newer menu mapped. The saturating floor keeps
            // any remaining misclassification harmless — an extra close can
            // never push the ledger below "nothing open".
            self.dropdowns_open = self.dropdowns_open.saturating_sub(1);
            tracing::debug!(
                "popup closed: a dropdown menu (dropdowns open: {})",
                self.dropdowns_open
            );
        }
        // Our popup dying does *not* take a mapped tooltip with it on the
        // compositor path (`…/handlers/shell/xdg_popup.rs::done` collects the
        // dismissed popup's *ancestors* and no children), so the orphan is
        // cleaned up here; on the self-initiated path it is already gone and
        // this is the documented no-op.
        destroy_tooltip()
    }

    /// Adopt a freshly minted popup id as `self.popup`, booking any id it
    /// **displaces** into [`Window::closing_popups`]. The create's *settings*
    /// closure ([`Message::TogglePopup`]) is the only place `self.popup` is
    /// ever set, and it goes through here.
    ///
    /// Normally it displaces nothing: `TogglePopup` only reaches its create
    /// branch with `self.popup == None`. But that branch is decided in
    /// `update()` and the closure runs a whole message round later, so the two
    /// are not one step. Verified against the pinned rev:
    ///
    /// - libcosmic's `update` helper drains **every** queued message before
    ///   running any of the resulting actions (`iced/winit/src/lib.rs`, `for
    ///   message in messages.drain(..)` collecting into `actions`, run only
    ///   after the loop);
    /// - the create *is* one of those actions — `surface_task` is
    ///   `crate::task::message(..)`, i.e. an `Action::Output` that `run_action`
    ///   pushes back onto `messages`, and only the `Action::AppPopup` arm of
    ///   the next round (`…/src/app/cosmic.rs`: `let settings =
    ///   settings(&mut self.app);`) runs this closure.
    ///
    /// So two panel clicks landing in one drain (an input burst, or one stalled
    /// frame — the same window that makes late `Done`s possible) both see
    /// `self.popup == None`, both take the create branch, and the second
    /// closure lands on the id the first one just set.
    ///
    /// The displaced popup is not stale bookkeeping — upstream really destroys
    /// it. A create whose parent is not the topmost popup takes the
    /// `parent_mismatch` path and destroys everything above it
    /// (`…/wayland/event_loop/state.rs`), and that destroy sends a
    /// `PopupEvent::Done` for every popup it tears down. Left unbooked, that
    /// `Done` names an id `self.popup` no longer holds and falls through to the
    /// by-elimination row of [`Window::on_popup_closed`], decrementing a
    /// *newer* session's live menu count — zero with a menu still mapped, i.e.
    /// a tooltip armed beside a live sibling, which is the two-children state
    /// this whole ledger exists to forbid.
    ///
    /// `dropdowns_open` is deliberately **not** reset here, unlike the
    /// `TogglePopup` path: nothing is subtracted, so any menu the displaced
    /// popup takes with it is still counted and is paid off by its own `Done`
    /// through the by-elimination row. The arithmetic stays exact with no
    /// anonymous debt, and the count never dips below the number of mapped
    /// menus — resetting it would be the un-pausing direction.
    fn adopt_popup(&mut self, new_id: window::Id) {
        // A new session begins, so the *anonymous* debt of a session a
        // compositor dismissal ended is settled by definition: that path queues
        // the whole chain's closes in one `done()` call, ahead of the very
        // click that led here ([`Window::stale_menu_closes`]). Anything left is
        // a menu that never mapped — a create the runtime dropped — and would
        // otherwise swallow this session's first live menu close and pause its
        // tooltips for good.
        if self.stale_menu_closes > 0 {
            tracing::debug!(
                "new popup session: discarding {} unpayable menu close(s) from a dismissed session",
                self.stale_menu_closes
            );
            self.stale_menu_closes = 0;
        }
        if let Some(displaced) = self.popup.replace(new_id) {
            // By id, like the `TogglePopup` path and for the same reason: this
            // popup may never have mapped at all (see
            // [`Window::closing_popups`]), and an anonymous unit for it would
            // swallow a live menu's close instead of sitting unclaimed. It
            // cannot already be booked — the only other writer of `self.popup`
            // clears it in the same step that books it, and ids are unique.
            // Nothing is subtracted from the live count here, so this entry
            // owes no menu closes: whatever the displaced popup takes with it
            // is still counted and pairs with its own `Done`.
            self.closing_popups.push(ClosingPopup {
                id: displaced,
                menus_owed: 0,
            });
            tracing::debug!(
                "popup create displaced an earlier popup, booked by name (dropdowns open: {}, popups closing: {})",
                self.dropdowns_open,
                self.closing_popups.len()
            );
        }
    }

    /// A surface action published by the tooltip widget, run through the popup
    /// ledger on its way to the runtime.
    ///
    /// While a menu is mapped **every** tooltip action is dropped: a create
    /// would map a second child of `self.popup` (two siblings on one xdg-shell
    /// stack), and a destroy would target a popup that is no longer topmost —
    /// the fatal one. Nothing is lost by dropping the destroy, because the
    /// menu's own close re-emits it unconditionally (see [`destroy_tooltip`]
    /// and [`Window::on_dropdown_surface`]); this is the deferral, collapsed
    /// into the idempotence of the destroy itself.
    ///
    /// Otherwise the action is forwarded **untouched**. Never clone it: the
    /// runtime recovers a create's settings with `Arc::try_unwrap`, so a
    /// surviving clone makes it log `"Invalid settings for popup"` and create
    /// nothing — silently, since logging is off by default.
    fn on_tooltip_surface(&self, action: cosmic::surface::Action) -> app::Task<Message> {
        if self.dropdown_open() {
            tracing::debug!("tooltip surface action dropped behind an open dropdown: {action:?}");
            return Task::none();
        }
        tracing::debug!("tooltip surface action forwarded: {action:?}");
        cosmic::surface::surface_task(action)
    }

    /// A surface action published by a `popup_dropdown` menu, run through the
    /// popup ledger on its way to the runtime.
    ///
    /// As in [`Window::on_tooltip_surface`] the action is matched **by
    /// reference** and then forwarded untouched — never clone it, or the
    /// runtime's `Arc::try_unwrap` of the settings fails and it silently
    /// creates nothing.
    fn on_dropdown_surface(&mut self, action: cosmic::surface::Action) -> app::Task<Message> {
        // Sequenced around the forwarded action, never batched: the whole
        // point is the order in which the compositor sees the destroys.
        let mut before = Task::none();
        let mut after = Task::none();
        match &action {
            cosmic::surface::Action::Popup(..) | cosmic::surface::Action::AppPopup(..) => {
                // The interlock. Both the tooltip and this menu are children
                // of `self.popup`, i.e. siblings on one xdg-shell stack, and
                // only the topmost may be destroyed. The instant the create
                // arrives is the *last* moment the tooltip is still topmost —
                // once the menu is mapped nothing can close the tooltip until
                // the menu goes away, and we can never close the menu
                // ourselves (its window id is minted inside the widget). So
                // the destroy is chained ahead of the create, unconditionally:
                // if no tooltip is mapped it is a no-op, and if one is mapped
                // *above* an already-open menu it is the topmost, so the
                // destroy is legal either way.
                self.dropdowns_open = self.dropdowns_open.saturating_add(1);
                tracing::debug!("dropdown opened (dropdowns open: {})", self.dropdowns_open);
                before = destroy_tooltip();
            }
            cosmic::surface::Action::DestroyPopup(_) => {
                // The count is deliberately *not* lowered here — see the
                // field's doc. Both rows publish through this one message, and
                // a row whose widget kept a stale `is_open` (grab loss
                // dismisses the menu without ever reaching its `ButtonPressed`
                // arm) emits a destroy for an already-dead popup in the same
                // pass as the other row's create; decrementing here would end
                // that pass at zero with a menu mapped, which is the state the
                // invariant forbids. A destroy that really tears a menu down
                // comes back as `PopupClosed`, and that is what decrements.
                tracing::debug!(
                    "dropdown destroy forwarded (dropdowns open: {})",
                    self.dropdowns_open
                );
                // Ordered after the menu's own destroy: only then is a tooltip
                // topmost again. Unconditional, so it also collects a tooltip
                // whose destroy was dropped while the menu was up.
                after = destroy_tooltip();
            }
            other => tracing::debug!("dropdown surface action forwarded as-is: {other:?}"),
        }
        before
            .chain(cosmic::surface::surface_task(action))
            .chain(after)
    }

    /// Treat a watcher payload as evidence and confirm the complete current
    /// entry from disk away from the UI thread. With no context there can be
    /// no self-write echo, so the payload itself remains usable.
    fn confirm_config_update(&mut self, payload: AppletConfig) -> app::Task<Message> {
        self.config_confirmation_generation = self.config_confirmation_generation.wrapping_add(1);
        let generation = self.config_confirmation_generation;
        let was_active_leader = self.is_active_leader();
        let Some(context) = self.config_context.clone() else {
            return self.finish_config_confirmation(
                generation,
                was_active_leader,
                payload.normalize(),
            );
        };
        cosmic::task::future(async move {
            let config = tokio::task::spawn_blocking(move || AppletConfig::load(&context))
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!("config confirmation task failed: {error}");
                    payload.normalize()
                });
            Message::ConfigConfirmed {
                generation,
                was_active_leader,
                config,
            }
        })
    }

    fn finish_config_confirmation(
        &mut self,
        generation: u64,
        was_active_leader: bool,
        mut config: AppletConfig,
    ) -> app::Task<Message> {
        if generation != self.config_confirmation_generation
            || was_active_leader != self.is_active_leader()
        {
            return Task::none();
        }

        config = config.normalize();
        let shuffle_changed = config.shuffle_enabled != self.config.shuffle_enabled
            || config.shuffle_interval_secs != self.config.shuffle_interval_secs;
        let retention_reduced =
            schedule::retention_reduced(self.config.retention_days, config.retention_days);
        if !self.is_active_leader() {
            config.accent_snapshot = self.config.accent_snapshot;
            config.accent_last_written = self.config.accent_last_written;
            self.config = config;
            return Task::none();
        }

        let accent_flip = if self.accent_inflight.is_some() {
            None
        } else {
            (config.accent_enabled != self.config.accent_enabled).then_some(config.accent_enabled)
        };
        config.accent_enabled = self.config.accent_enabled;
        config.accent_snapshot = self.config.accent_snapshot;
        config.accent_last_written = self.config.accent_last_written;
        self.config = config;
        let mut tasks = Vec::new();
        if retention_reduced {
            tasks.push(self.prune_immediately());
        }
        if shuffle_changed {
            tasks.push(self.sync_shuffle(true));
        }
        if let Some(enabled) = accent_flip {
            tasks.push(self.set_accent_enabled(enabled));
        }
        Task::batch(tasks)
    }

    /// Persist only fields this transition actually changed. In particular,
    /// never serialize the full in-memory entry: a follower may have written
    /// an unrelated key after our last watcher snapshot.
    fn set_config(&mut self, config: AppletConfig) {
        use cosmic_config::ConfigSet as _;

        if !self.is_active_leader() {
            tracing::debug!("dropping leader-owned applet-config transition from a non-leader");
            return;
        }
        if self.config == config {
            return;
        }
        let previous = std::mem::replace(&mut self.config, config.clone());
        let Some(context) = &self.config_context else {
            return;
        };
        macro_rules! persist_changed {
            ($field:ident) => {
                if previous.$field != config.$field
                    && let Err(error) = context.set(stringify!($field), &config.$field)
                {
                    tracing::warn!(
                        "failed to persist applet config key {}: {error}",
                        stringify!($field)
                    );
                }
            };
        }
        persist_changed!(shuffle_enabled);
        persist_changed!(shuffle_interval_secs);
        persist_changed!(retention_days);
        persist_changed!(accent_enabled);
        persist_changed!(accent_snapshot);
        persist_changed!(accent_last_written);
    }

    /// Persist one user setting as a raw key without serializing unrelated
    /// fields. Leaders adopt immediately so their duties follow the control;
    /// followers adopt only after the write lands so their UI reflects a
    /// request the leader can observe. A memory-only fixture retains the
    /// established in-memory behavior, except follower accent enablement.
    fn set_applet_setting(&mut self, setting: AppletSetting) -> app::Task<Message> {
        let active_leader = self.is_active_leader();
        let slot = setting.slot();
        let Some(context) = self.config_context.clone() else {
            if !matches!(setting, AppletSetting::AccentEnabled(true)) {
                setting.adopt(&mut self.config);
            } else {
                tracing::warn!(
                    "cannot enable accent-from-wallpaper from a non-leader: applet config is not persistable"
                );
            }
            return Task::none();
        };

        // Leaders update their controls/timers immediately, retaining the
        // established memory-on-write-failure behavior. Followers wait for a
        // successful persist so their UI never claims a request the leader
        // cannot observe.
        if active_leader {
            setting.adopt(&mut self.config);
        }

        self.setting_write_generations[slot] = self.setting_write_generations[slot].wrapping_add(1);
        let generation = self.setting_write_generations[slot];
        self.setting_write_queue.push_back((generation, setting));
        self.start_next_setting_write(context)
    }

    fn start_next_setting_write(&mut self, context: cosmic_config::Config) -> app::Task<Message> {
        if self.setting_write_inflight {
            return Task::none();
        }
        let Some((generation, setting)) = self.setting_write_queue.pop_front() else {
            return Task::none();
        };
        self.setting_write_inflight = true;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || setting.persist(&context))
                .await
                .map_err(|error| format!("applet setting task failed: {error}"))
                .and_then(|result| result.map_err(|error| error.to_string()));
            Message::AppletSettingWritten {
                generation,
                setting,
                result,
            }
        })
    }

    fn finish_applet_setting_write(
        &mut self,
        generation: u64,
        setting: AppletSetting,
        result: Result<(), String>,
    ) -> app::Task<Message> {
        self.setting_write_inflight = false;
        if self.is_active_leader() {
            if let Err(error) = result {
                tracing::warn!(
                    "failed to persist applet setting {}: {error}",
                    setting.key()
                );
            }
        } else if generation == self.setting_write_generations[setting.slot()] {
            match result {
                Ok(()) => setting.adopt(&mut self.config),
                Err(error) => tracing::warn!(
                    "failed to persist applet setting {}: {error}",
                    setting.key()
                ),
            }
        }
        self.config_context
            .clone()
            .map_or_else(Task::none, |context| self.start_next_setting_write(context))
    }

    /// Arm the one-shot refresh timer for `delay` from now, invalidating any
    /// previously armed timer via the generation counter.
    ///
    /// Accepted v1 limitation (same as the GNOME reference): the sleep is
    /// monotonic and does not advance during system suspend, so a refresh
    /// due while suspended fires late after resume instead of immediately.
    fn schedule_refresh(&mut self, delay: Duration) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }
        self.timer_generation += 1;
        let generation = self.timer_generation;
        tracing::info!("next refresh in {}s", delay.as_secs());
        cosmic::task::future(async move {
            tokio::time::sleep(delay).await;
            Message::RefreshDue(generation)
        })
    }

    /// Arm (or re-arm) the one-shot shuffle timer for one full (sanitized)
    /// interval, invalidating any pending tick.
    fn arm_shuffle(&mut self) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }
        self.shuffle_generation += 1;
        self.shuffle_armed = true;
        let generation = self.shuffle_generation;
        let delay = schedule::shuffle_interval(self.config.shuffle_interval_secs);
        tracing::info!("next shuffle in {}s", delay.as_secs());
        cosmic::task::future(async move {
            tokio::time::sleep(delay).await;
            Message::ShuffleDue(generation)
        })
    }

    /// Invalidate any pending shuffle tick.
    fn disarm_shuffle(&mut self) {
        if !self.is_active_leader() {
            return;
        }
        self.shuffle_generation += 1;
        self.shuffle_armed = false;
    }

    /// Arm the lock-poke ladder: one one-shot sleeping task per
    /// [`lockwatch::POKE_DELAYS`] rung, all carrying a freshly bumped
    /// generation — which atomically invalidates any pending ladder (the
    /// [`Self::schedule_refresh`] shape). Both rungs are full toggles; see
    /// `POKE_DELAYS` for why the second exists.
    ///
    /// The rungs share one generation deliberately (the ladder cancels as a
    /// unit), which also means they are **not serialized against each
    /// other**: under an extreme I/O stall rung 1's `spawn_blocking` write
    /// can still be in flight when rung 2 fires, and the interleaved
    /// fresh-reads can double-append or normalize an uncommitted shape.
    /// Every such outcome stays inside the duplicated-rest-shape class
    /// [`lockwatch::toggle_wallpapers`] tolerates by design and is cleaned
    /// by the next poke's normalization.
    fn arm_lock_pokes(&mut self) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }
        self.lock_poke_generation += 1;
        let generation = self.lock_poke_generation;
        Task::batch(lockwatch::POKE_DELAYS.map(|delay| {
            cosmic::task::future(async move {
                tokio::time::sleep(delay).await;
                Message::LockPokeDue(generation)
            })
        }))
    }

    /// The [`Message::LockPokeDue`] decision, shared with the test-side
    /// settle helper so the two cannot drift: the handle to poke with, or
    /// `None` when the rung is stale (a newer ladder replaced it) or no
    /// state handle exists.
    fn due_lock_poke(&self, generation: u64) -> Option<cosmic_config::Config> {
        if !self.is_active_leader() || generation != self.lock_poke_generation {
            return None;
        }
        self.poke_config.clone()
    }

    /// Bring the shuffle timer in line with the current state: it runs only
    /// while shuffle is enabled and at least two images exist. Pass
    /// `reset_countdown` to force a re-arm even when a tick is already
    /// pending (manual navigation, settings changes).
    fn sync_shuffle(&mut self, reset_countdown: bool) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }
        let should_run = self.config.shuffle_enabled && self.catalogue.images.len() >= 2;
        if !should_run {
            self.disarm_shuffle();
            Task::none()
        } else if !self.shuffle_armed || reset_countdown {
            self.arm_shuffle()
        } else {
            Task::none()
        }
    }

    /// Prune the catalogue against the current retention setting right now
    /// (retention was reduced — no point keeping over-limit files on disk
    /// until the next fetch). The currently applied file is always
    /// protected; the shuffle timer follows the (possibly shrunken)
    /// catalogue.
    fn prune_immediately(&mut self) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }
        self.prune_and_persist();
        // Pruning may leave fewer than two images — disarm shuffle if so.
        self.sync_shuffle(false)
    }

    /// The one prune sequence both the immediate path and the post-fetch
    /// path run: refresh our idea of what is applied from cosmic-bg's live
    /// config (so the right file is protected even after external
    /// wallpaper changes), prune against the *current* retention, sweep the
    /// thumbnail cache down to what the catalogue still holds
    /// ([`thumbs::reconcile`] — a sweep, not a removal list, because the
    /// backfill may be generating on a blocking pool right now), and
    /// persist the catalogue. Returns the live wallpaper source that was
    /// read (`None` for every non-file state).
    ///
    /// When what is displayed is *unknowable* (per-output mode, unreadable
    /// config), `wallpaper::prune_retention` disables age-based deletion
    /// entirely — protecting only the possibly stale `self.current` could
    /// delete a Bing image some output actually displays.
    fn prune_and_persist(&mut self) -> Option<PathBuf> {
        if !self.is_active_leader() {
            return None;
        }
        self.prune_over(
            wallpaper::current_wallpaper(),
            &wallpaper::download_dir(),
            state_dir(),
            &catalogue_path(),
        )
    }

    /// Remove the images Bing explicitly marked `wp: false`, entry and file
    /// ([`Catalogue::remove_ineligible`]), gated on the same evidence the
    /// prune uses: while the displayed file is unknowable
    /// ([`wallpaper::CurrentWallpaper::Unknown`] — per-output mode or an
    /// unreadable cosmic-bg config) nothing is deleted this refresh; the
    /// live wallpaper's own entry is exempt either way. Absent `wp` never
    /// reaches here: it blocks downloads only.
    ///
    /// Runs over an already [`Window::sync_current`]ed state: the caller
    /// (`finish_refresh_over`) syncs once for this pass and the prune that
    /// follows it, and both exempt the same [`protected_paths`].
    fn remove_ineligible_over(
        &mut self,
        ineligible: &[String],
        live: &wallpaper::CurrentWallpaper,
        download_dir: &Path,
    ) {
        if ineligible.is_empty() || !self.is_active_leader() {
            return;
        }
        if matches!(live, wallpaper::CurrentWallpaper::Unknown) {
            tracing::warn!(
                "skipping removal of {} ineligible image(s): the displayed wallpaper is unknowable",
                ineligible.len()
            );
            return;
        }
        let protected = protected_paths(&self.current, &self.protected_fallback);
        self.catalogue
            .remove_ineligible(ineligible, download_dir, &protected);
    }

    /// Refresh our idea of what is applied from an already-read cosmic-bg
    /// state ([`wallpaper::synced_current`]), so the file the deleting
    /// passes protect is the one actually on screen — `self.current` only
    /// records what the applet itself applied and goes stale the moment the
    /// user picks a wallpaper in Settings.
    fn sync_current(&mut self, live: &wallpaper::CurrentWallpaper) {
        self.current = wallpaper::synced_current(live, self.current.take());
    }

    /// [`Window::prune_and_persist`] against an already-read cosmic-bg state
    /// and explicit roots (injected so tests never touch the real folder,
    /// state dir or catalogue — same idiom as
    /// [`Window::start_refresh_over`]).
    fn prune_over(
        &mut self,
        live: wallpaper::CurrentWallpaper,
        download_dir: &Path,
        state_dir: &Path,
        catalogue_path: &Path,
    ) -> Option<PathBuf> {
        if !self.is_active_leader() {
            return live.into_file();
        }
        self.sync_current(&live);
        self.prune_synced(&live, download_dir, state_dir, catalogue_path);
        live.into_file()
    }

    /// [`Window::prune_over`]'s deleting half, for a caller that has already
    /// [`Window::sync_current`]ed against `live` (the refresh end, which
    /// syncs once for the eligibility reconciliation and this prune alike).
    fn prune_synced(
        &mut self,
        live: &wallpaper::CurrentWallpaper,
        download_dir: &Path,
        state_dir: &Path,
        catalogue_path: &Path,
    ) {
        let protected = protected_paths(&self.current, &self.protected_fallback);
        self.catalogue.prune_protecting(
            download_dir,
            wallpaper::prune_retention(live, self.config.retention_days),
            &protected,
            Utc::now(),
        );
        self.sweep_thumbnails(state_dir);
        if let Err(error) = self.catalogue.save(catalogue_path) {
            // Non-fatal: the catalogue is rebuildable from the folder scan.
            tracing::warn!("failed to persist catalogue after prune: {error}");
        }
    }

    /// Whether the thumbnail cache may be swept right now.
    ///
    /// [`thumbs::reconcile`] deletes every cache file the *live* catalogue
    /// does not name, and a thumbnail pass writes files before the UI thread
    /// knows about them: the fetch pipeline's downloads only join the
    /// catalogue when `RefreshFinished` merges them, and the startup pass
    /// runs on a blocking pool while the UI thread may prune. A sweep landing
    /// inside either window deletes what that pass just produced, and nothing
    /// regenerates it until the next successful refresh — up to ~24 h of the
    /// placeholder on the image that was just applied. Skipping costs
    /// nothing: every pass ends in a sweep of its own
    /// ([`Window::finish_refresh`], [`Window::finish_thumbnail_pass`]), and
    /// startup sweeps unconditionally whatever was missed.
    fn may_sweep_thumbnails(&self) -> bool {
        !self.refresh_pending && !self.thumbnail_pass_pending
    }

    /// Sweep the thumbnail cache down to what the catalogue still holds,
    /// unless a pass is writing into it right now
    /// ([`Window::may_sweep_thumbnails`]).
    fn sweep_thumbnails(&self, state_dir: &Path) {
        if self.may_sweep_thumbnails() {
            thumbs::reconcile(
                self.catalogue.images.iter().map(|e| e.filename.as_path()),
                state_dir,
            );
        }
    }

    /// Kick off the fetch pipeline unless one is already running. The
    /// pipeline only fetches and downloads; merge/prune/save happen back
    /// on the UI thread in `RefreshFinished` against the live state.
    fn start_refresh(&mut self) -> app::Task<Message> {
        if !self.is_active_leader() || self.refresh_pending {
            return Task::none();
        }
        // The backfill skips what the prune after this refresh will delete,
        // so it must see the same live cosmic-bg state that prune reads —
        // `prune_retention` turns age deletion *off* whenever the displayed
        // file is unknowable (per-output mode is a permanent such state, not
        // a transient one), and a backfill still honouring the configured
        // cutoff there would leave every older entry on the placeholder for
        // good while the catalogue grows without bound.
        self.start_refresh_over(wallpaper::current_wallpaper())
    }

    /// Handle the popup refresh button according to this process's role.
    fn refresh_now(&mut self) -> app::Task<Message> {
        if self.is_active_leader() {
            return self.start_refresh();
        }
        if self.refresh_pending || self.peer_refresh_write_pending {
            return Task::none();
        }
        let Some(config) = self.coordination_context.clone() else {
            tracing::warn!("cannot request a peer refresh without coordination config");
            return Task::none();
        };
        let state = self.coordination_state_dir.clone();
        self.peer_refresh_write_pending = true;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                increment_refresh_request(&config, &state).map_err(|error| error.to_string())
            })
            .await
            .unwrap_or_else(|error| Err(format!("peer refresh request task failed: {error}")));
            Message::PeerRefreshRequested(result)
        })
    }

    fn finish_peer_refresh_request(&mut self, result: Result<u64, String>) -> app::Task<Message> {
        self.peer_refresh_write_pending = false;
        if self.is_active_leader() {
            return Task::none();
        }
        let request = match result {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!("failed to request peer refresh: {error}");
                return Task::none();
            }
        };
        self.coordination.refresh_request = self.coordination.refresh_request.max(request);
        self.requested_peer_refresh = Some(request);
        self.refresh_pending = true;
        self.peer_refresh_timeout_generation = self.peer_refresh_timeout_generation.wrapping_add(1);
        if self.coordination.refresh_completion.request >= request {
            return self.settle_peer_refresh(self.coordination.refresh_completion);
        }
        let generation = self.peer_refresh_timeout_generation;
        cosmic::task::future(async move {
            tokio::time::sleep(PEER_REFRESH_ACK_TIMEOUT).await;
            Message::PeerRefreshTimeout {
                generation,
                request,
            }
        })
    }

    /// Reload a follower's catalogue and live wallpaper off the UI thread.
    /// The generation is invalidated by every newer reload, successful local
    /// apply, and takeover, so an old disk snapshot cannot move navigation
    /// state backwards.
    ///
    /// `may_repair` says whether a rebuilt reload may turn into a leader
    /// repair request (see `non_leader_reload_repairs`).
    fn request_non_leader_reload(&mut self, may_repair: bool) -> app::Task<Message> {
        if self.leadership.is_leader() {
            return Task::none();
        }
        self.non_leader_reload_generation = self.non_leader_reload_generation.wrapping_add(1);
        self.non_leader_reload_repairs = may_repair;
        let generation = self.non_leader_reload_generation;
        #[cfg(test)]
        let (catalogue_path, images_dir, test_live) =
            if let Some(inputs) = &self.test_snapshot_inputs {
                (
                    inputs.catalogue_path.clone(),
                    inputs.images_dir.clone(),
                    Some(inputs.live.clone()),
                )
            } else {
                (catalogue_path(), wallpaper::download_dir(), None)
            };
        #[cfg(not(test))]
        let (catalogue_path, images_dir) = (catalogue_path(), wallpaper::download_dir());
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let live = {
                    #[cfg(test)]
                    if let Some(live) = test_live {
                        live
                    } else {
                        wallpaper::current_wallpaper()
                    }
                    #[cfg(not(test))]
                    wallpaper::current_wallpaper()
                };
                read_non_leader_reload(&catalogue_path, &images_dir, live)
            })
            .await
            .map_err(|error| format!("non-leader reload task failed: {error}"));
            Message::NonLeaderReloaded { generation, result }
        })
    }

    /// Adopt an injected follower snapshot only while it is still current.
    /// This completion performs no filesystem or cosmic-config reads.
    fn finish_non_leader_reload(
        &mut self,
        generation: u64,
        result: Result<NonLeaderReload, String>,
    ) -> app::Task<Message> {
        if self.leadership.is_leader() || generation != self.non_leader_reload_generation {
            return Task::none();
        }
        match result {
            Ok(reload) => {
                self.catalogue = reload.catalogue;
                self.sync_current(&reload.live);
                if self.non_leader_reload_repairs && reload.provenance == Provenance::Rebuilt {
                    return self.request_follower_repair();
                }
                Task::none()
            }
            Err(error) => {
                tracing::warn!("failed to reload non-leader state: {error}");
                Task::none()
            }
        }
    }

    fn settle_peer_refresh(&mut self, completion: PeerRefreshCompletion) -> app::Task<Message> {
        let Some(request) = self.requested_peer_refresh else {
            return Task::none();
        };
        if self.is_active_leader() || completion.request < request {
            return Task::none();
        }

        self.requested_peer_refresh = None;
        self.refresh_pending = false;
        self.peer_refresh_timeout_generation = self.peer_refresh_timeout_generation.wrapping_add(1);
        match completion.outcome {
            PeerRefreshOutcome::Success => {
                self.last_updated = Some(Utc::now());
                self.last_error = None;
            }
            PeerRefreshOutcome::Network => {
                self.last_error = Some(RefreshError::Network(String::new()));
            }
            PeerRefreshOutcome::Disk => {
                self.last_error = Some(RefreshError::Disk(String::new()));
            }
        }
        // Settled, whatever the outcome: a rebuilt reload here must not ask
        // again, or an offline leader would be polled on every acknowledgement.
        self.request_non_leader_reload(false)
    }

    fn timeout_peer_refresh(&mut self, generation: u64, request: u64) -> app::Task<Message> {
        if self.is_active_leader()
            || generation != self.peer_refresh_timeout_generation
            || self.requested_peer_refresh != Some(request)
        {
            return Task::none();
        }
        self.requested_peer_refresh = None;
        self.refresh_pending = false;
        self.peer_refresh_timeout_generation = self.peer_refresh_timeout_generation.wrapping_add(1);
        // The established retry point: no leader covered the request, so a
        // still rebuilt catalogue may ask once more.
        self.request_non_leader_reload(true)
    }

    /// Persist evidence of a successful follower apply without trusting the
    /// follower's path as the leader's view of cosmic-bg state.
    fn write_peer_apply_notice(&self, path: PathBuf) -> app::Task<Message> {
        let Some(config) = self.coordination_context.clone() else {
            tracing::warn!(
                "cannot notify the leader about an applied wallpaper without coordination config"
            );
            return Task::none();
        };
        let state = self.coordination_state_dir.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                write_apply_notice(&config, &state, path).map_err(|error| error.to_string())
            })
            .await
            .unwrap_or_else(|error| Err(format!("peer apply notice task failed: {error}")));
            Message::PeerApplyNoticeWritten(result)
        })
    }

    /// Shared manual-apply success tail. Followers update only their local
    /// navigation view and mailbox; the leader alone spends its cold-start
    /// state, owns shuffle timing, and enters the accent lifecycle.
    fn finish_manual_apply(&mut self, path: PathBuf) -> app::Task<Message> {
        if self.is_active_leader() {
            let accent = self.on_apply_success(path);
            let shuffle = self.sync_shuffle(true);
            return Task::batch([accent, shuffle]);
        }

        self.current = Some(path.clone());
        self.non_leader_reload_generation = self.non_leader_reload_generation.wrapping_add(1);
        self.write_peer_apply_notice(path)
    }

    /// Observe a newer peer apply and validate it with a fresh blocking read
    /// of cosmic-bg. Notice paths are evidence only: another apply may have
    /// won before the watcher event reached this process.
    fn consume_peer_apply_notice(&mut self, notice: Option<PeerApplyNotice>) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }
        let Some(notice) = notice else {
            return Task::none();
        };
        if notice.generation <= self.peer_apply_notice_generation {
            return Task::none();
        }
        self.peer_apply_notice_generation = notice.generation;
        let generation = notice.generation;
        tracing::debug!(
            generation,
            path = %notice.path.display(),
            "validating a peer wallpaper apply"
        );
        cosmic::task::future(async move {
            let live = tokio::task::spawn_blocking(wallpaper::current_wallpaper)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!("peer apply validation task failed: {error}");
                    wallpaper::CurrentWallpaper::Unknown
                });
            Message::PeerApplyValidated { generation, live }
        })
    }

    fn finish_peer_apply_validation(
        &mut self,
        generation: u64,
        live: wallpaper::CurrentWallpaper,
    ) -> app::Task<Message> {
        if !self.is_active_leader() || generation != self.peer_apply_notice_generation {
            return Task::none();
        }
        match live {
            wallpaper::CurrentWallpaper::File(path) => self.on_apply_success(path),
            wallpaper::CurrentWallpaper::NoFile | wallpaper::CurrentWallpaper::Unknown => {
                tracing::debug!(generation, "peer apply no longer resolves to one live file");
                Task::none()
            }
        }
    }

    /// Reserve every currently outstanding mailbox request for the one fetch
    /// in flight, or asynchronously obtain the live state needed to start it.
    fn consume_peer_refresh_request(&mut self) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }
        let request = self.coordination.refresh_request;
        if request
            <= self
                .coordination
                .refresh_completion
                .request
                .max(self.peer_refresh_covered)
        {
            return Task::none();
        }

        // Reserve synchronously. Any refresh already in flight—or started by
        // another message before the live read returns—will now acknowledge
        // this request when it finishes.
        self.peer_refresh_request =
            Some(self.peer_refresh_request.unwrap_or_default().max(request));
        if self.refresh_pending || self.peer_refresh_live_read.is_some() {
            return Task::none();
        }
        self.peer_refresh_live_read = Some(request);
        #[cfg(test)]
        let test_live = self
            .test_snapshot_inputs
            .as_ref()
            .map(|inputs| inputs.live.clone());
        cosmic::task::future(async move {
            let live = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if let Some(live) = test_live {
                    return live;
                }
                wallpaper::current_wallpaper()
            })
            .await
            .unwrap_or_else(|error| {
                tracing::warn!("peer refresh live-state task failed: {error}");
                wallpaper::CurrentWallpaper::Unknown
            });
            Message::PeerRefreshLiveRead { request, live }
        })
    }

    fn finish_peer_refresh_live_read(
        &mut self,
        request: u64,
        live: wallpaper::CurrentWallpaper,
    ) -> app::Task<Message> {
        if self.peer_refresh_live_read != Some(request) {
            return Task::none();
        }
        self.peer_refresh_live_read = None;
        if !self.is_active_leader() || self.refresh_pending {
            return Task::none();
        }
        let Some(reserved) = self.peer_refresh_request else {
            return Task::none();
        };
        if reserved
            <= self
                .coordination
                .refresh_completion
                .request
                .max(self.peer_refresh_covered)
        {
            self.peer_refresh_request = None;
            return Task::none();
        }
        self.start_refresh_over(live)
    }

    /// [`Window::consume_peer_refresh_request`] with injected live wallpaper
    /// state for hermetic decision tests.
    #[cfg(test)]
    fn consume_peer_refresh_request_over(
        &mut self,
        live: wallpaper::CurrentWallpaper,
    ) -> app::Task<Message> {
        if !self.is_active_leader()
            || self.coordination.refresh_request
                <= self
                    .coordination
                    .refresh_completion
                    .request
                    .max(self.peer_refresh_covered)
        {
            return Task::none();
        }
        self.peer_refresh_request = Some(
            self.peer_refresh_request
                .unwrap_or_default()
                .max(self.coordination.refresh_request),
        );
        if self.refresh_pending {
            Task::none()
        } else {
            self.start_refresh_over(live)
        }
    }

    fn record_peer_refresh_completion(
        &self,
        request: u64,
        outcome: PeerRefreshOutcome,
    ) -> app::Task<Message> {
        let Some(config) = self.coordination_context.clone() else {
            tracing::warn!(
                request,
                "cannot persist peer refresh completion without config"
            );
            return Task::none();
        };
        let state = self.coordination_state_dir.clone();
        let completion = PeerRefreshCompletion { request, outcome };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                record_refresh_completion(&config, &state, completion)
                    .map_err(|error| error.to_string())
            })
            .await
            .unwrap_or_else(|error| Err(format!("peer refresh completion task failed: {error}")));
            Message::PeerRefreshCompletionWritten { completion, result }
        })
    }

    /// [`Window::start_refresh`] against an already-read cosmic-bg state
    /// (injected so tests can stage a wallpaper the applet has not seen
    /// itself apply). The caller has established that no fetch is in flight.
    fn start_refresh_over(&mut self, live: wallpaper::CurrentWallpaper) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }
        self.refresh_pending = true;
        let catalogue = self.catalogue.clone();
        let retention_days = self.config.retention_days;
        // The applied file is passed along so the backfill protects its
        // thumbnail exactly as the prune protects its file — which means it
        // must be the file the *prune* will protect. `self.current` only
        // records what the applet itself applied, so it goes stale the
        // moment the user picks a wallpaper in Settings; the prune reads the
        // live state (see `prune_and_persist`), and reading it here without
        // doing the same would skip the thumbnail of the one image the prune
        // keeps no matter its age.
        self.sync_current(&live);
        let downloads = Downloads {
            horizon: schedule::download_horizon(retention_days),
            fallback: self.fallback_permitted(&live),
        };
        let backfill = Backfill {
            // Producer write interlock: while the startup pass owns the
            // cache the refresh writes no thumbnails.
            deferred: self.thumbnail_pass_pending,
            ..Backfill::new(&live, retention_days, self.current.clone())
        };
        cosmic::task::future(async move {
            Message::RefreshFinished(run_refresh(catalogue, downloads, backfill).await)
        })
    }

    /// Whether a refresh started over `live` may download the one
    /// out-of-retention fallback ([`Downloads::fallback`]).
    ///
    /// The fallback exists only to give the auto-apply something to apply;
    /// when that is suppressed (the user's own wallpaper is up) it would be
    /// pruned and re-fetched daily, so it is not requested. The permission
    /// is therefore the completion's own rule ([`refresh_success_plan`],
    /// `wallpaper::should_auto_apply` over `live.into_file()`) evaluated
    /// at the start — and deliberately **not** over `self.current`: under
    /// [`wallpaper::CurrentWallpaper::Unknown`] (per-output mode, unreadable
    /// config) [`Window::sync_current`] keeps the stale path of our own
    /// last apply, which `is_ours` would pass, while the completion maps
    /// the same state to `None` and applies nothing warm. Computed from
    /// `self.current` the start downloaded a fallback the end never
    /// applied — the exact churn the suppression exists to prevent.
    fn fallback_permitted(&self, live: &wallpaper::CurrentWallpaper) -> bool {
        let live_file = match live {
            wallpaper::CurrentWallpaper::File(path) => Some(path.as_path()),
            wallpaper::CurrentWallpaper::NoFile | wallpaper::CurrentWallpaper::Unknown => None,
        };
        wallpaper::should_auto_apply(self.cold_start.applies_over(live_file), live_file)
    }

    /// Kick off the startup thumbnail pass against an already-read cosmic-bg
    /// state: generate the previews the restored catalogue is still missing,
    /// with no network involved.
    ///
    /// The popup renders previews from the cache only, and until this pass
    /// existed the cache was filled *exclusively* by the fetch pipeline. A
    /// non-empty catalogue at startup (a folder migrated from the reference
    /// GNOME extension, a warm start whose cache was swept, a state dir
    /// cleared by hand) is not a cold start, so the first refresh is due off
    /// the newest `fullstartdate` — up to ~24 h away, and never at all on a
    /// machine that is offline. Every entry would show the placeholder until
    /// then. The files are all on disk already; nothing about generating
    /// their thumbnails needs Bing.
    ///
    /// Same policy object as the pipeline's own backfill ([`Backfill::new`]),
    /// so the two cannot disagree about what is worth decoding.
    fn start_thumbnail_pass_over(
        &mut self,
        live: wallpaper::CurrentWallpaper,
    ) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }
        self.thumbnail_pass_pending = true;
        // This pass snapshots the whole catalogue, so any debt booked so
        // far is covered by it; a refresh deferred *behind* it books anew.
        self.thumbnails_owed = false;
        let catalogue = self.catalogue.clone();
        // As in `start_refresh_over`: the protected file must be the one the
        // *prune* protects, i.e. the live state, not our own last apply.
        self.sync_current(&live);
        let backfill = Backfill {
            // And the same second exemption: a downloaded fallback is out of
            // retention by construction and not applied yet when a deferred
            // refresh arms this pass (`finish_refresh_over` settles the debt
            // before its apply arm), so without it the pass would skip
            // exactly the image the refresh is about to apply.
            protected_fallback: self.protected_fallback.clone(),
            ..Backfill::new(&live, self.config.retention_days, self.current.clone())
        };
        let download_dir = wallpaper::download_dir();
        cosmic::task::future(async move {
            run_thumbnail_pass(catalogue, backfill, &download_dir, state_dir()).await;
            Message::ThumbnailsReady
        })
    }

    /// The startup thumbnail pass finished: nothing writes into the cache
    /// any more, so collect whatever a prune skipped while it ran (see
    /// [`Window::may_sweep_thumbnails`]). If a refresh landed meanwhile
    /// under the write interlock (`thumbnails_owed`), its downloads still
    /// have no previews: one more pass is armed over the now-merged
    /// catalogue — cached slots are free skips, so it costs only the
    /// decodes the deferred refresh withheld — and it ends in its own
    /// sweep and `ThumbnailsReady` accent recompute like the first.
    fn finish_thumbnail_pass(
        &mut self,
        state_dir: &Path,
        live: wallpaper::CurrentWallpaper,
    ) -> app::Task<Message> {
        self.thumbnail_pass_pending = false;
        if !self.is_active_leader() {
            return Task::none();
        }
        self.sweep_thumbnails(state_dir);
        self.settle_owed_thumbnails(live)
    }

    /// Arm the pass a deferred refresh owes, if one is owed and no pass is
    /// running; a no-op otherwise. Called from both ends of the overlap —
    /// the refresh finishing after the pass, and the pass finishing after
    /// the refresh — so whichever producer ends last pays the debt.
    fn settle_owed_thumbnails(&mut self, live: wallpaper::CurrentWallpaper) -> app::Task<Message> {
        if !self.thumbnails_owed || self.thumbnail_pass_pending || !self.is_active_leader() {
            return Task::none();
        }
        self.thumbnails_owed = false;
        tracing::debug!("running the thumbnail pass a deferred refresh owes");
        self.start_thumbnail_pass_over(live)
    }

    /// React to the fetch pipeline finishing: merge the fetched entries
    /// into the live catalogue, prune + persist, auto-apply per the plan,
    /// and reschedule the next refresh.
    fn finish_refresh(&mut self, result: Result<RefreshBatch, RefreshError>) -> app::Task<Message> {
        self.finish_refresh_over(
            result,
            wallpaper::current_wallpaper(),
            &wallpaper::download_dir(),
            state_dir(),
            &catalogue_path(),
        )
    }

    /// [`Window::finish_refresh`] against an already-read cosmic-bg state
    /// and explicit roots (injected so tests never touch the real folder,
    /// state dir or catalogue — same idiom as [`Window::prune_over`]).
    fn finish_refresh_over(
        &mut self,
        result: Result<RefreshBatch, RefreshError>,
        live: wallpaper::CurrentWallpaper,
        download_dir: &Path,
        state_dir: &Path,
        catalogue_path: &Path,
    ) -> app::Task<Message> {
        self.refresh_pending = false;
        if !self.is_active_leader() {
            tracing::debug!("dropping refresh completion after leadership changed");
            return Task::none();
        }
        let peer_request = self.peer_refresh_request.take();
        if let Some(request) = peer_request {
            self.peer_refresh_covered = self.peer_refresh_covered.max(request);
        }
        let peer_outcome = match &result {
            Ok(_) => PeerRefreshOutcome::Success,
            Err(RefreshError::Network(_)) => PeerRefreshOutcome::Network,
            Err(RefreshError::Disk(_)) => PeerRefreshOutcome::Disk,
        };
        let batch = match result {
            Ok(batch) => batch,
            Err(error) => {
                tracing::warn!("refresh failed: {error}");
                self.last_error = Some(error);
                let retry = self.schedule_refresh(schedule::ERROR_RETRY_DELAY);
                let acknowledgement = peer_request.map_or_else(Task::none, |request| {
                    self.record_peer_refresh_completion(request, peer_outcome)
                });
                return Task::batch([retry, acknowledgement]);
            }
        };

        // Merge into the *live* catalogue: a wholesale replacement from
        // the pipeline's snapshot would resurrect entries a concurrent
        // prune removed. The prune likewise runs here on the UI thread,
        // against what is applied *right now* and the *current* retention
        // — the pipeline's start-of-fetch snapshot may be stale on both
        // counts, and the currently applied file must never be deleted.
        let delivered = !batch.fetched.is_empty();
        self.catalogue.merge(batch.fetched, download_dir);
        // A fallback is out of retention by construction: protect it from
        // the prune below until it is applied (then the current-wallpaper
        // protection takes over) or the next refresh reselects. Replacing
        // the previous selection unconditionally is what bounds the
        // protection to one refresh cycle.
        self.protected_fallback = batch.fallback;
        // Eligibility reconciliation, *before* the prune so its thumbnail
        // sweep collects what this removes. It reuses the prune's evidence:
        // in `CurrentWallpaper::Unknown` the displayed file is unknowable,
        // `self.current` protects nothing, and nothing is deleted — the
        // URL bases come back with the next response, so it simply retries.
        // A valid response with zero eligible images lands here too and is
        // a successful no-op: history and the current wallpaper stay, the
        // error clears, cold start stays armed (nothing applied), the peer
        // request is acknowledged as a success below.
        // One sync of the applied file for both deleting passes below.
        self.sync_current(&live);
        self.remove_ineligible_over(&batch.ineligible, &live, download_dir);
        // Thumbnails withheld under the write interlock are owed to the
        // merged entries; paid by the pass's end, or right now if the pass
        // already ended while this refresh was in flight.
        self.thumbnails_owed |= batch.thumbnails_deferred;
        let owed_pass = self.settle_owed_thumbnails(live.clone());
        self.prune_synced(&live, download_dir, state_dir, catalogue_path);
        let live = live.into_file();

        self.last_updated = Some(Utc::now());
        self.last_error = None;

        // The live catalogue answers "is there anything to apply?"; the
        // *response* answers "when is the next refresh due?". The apply
        // target is the downloaded fallback when there is one — it was
        // fetched for exactly that, and the catalogue's newest may be an
        // ineligible image kept only because it is on screen — else the
        // newest entry. Cloned because the apply arm mutates `self`.
        let target = apply_target(&self.catalogue, self.protected_fallback.as_deref()).cloned();
        let plan = refresh_success_plan(
            self.cold_start.applies_over(live.as_deref()),
            live.as_deref(),
            target.is_some(),
            delivered,
            &batch.anchor,
            Utc::now(),
        );
        let mut apply_failed = false;
        let mut accent = Task::none();
        if plan.auto_apply
            && let Some(target) = &target
        {
            let path = target.filename.clone();
            match wallpaper::apply(&path) {
                Ok(()) => {
                    // Applied: the current-wallpaper protection covers it.
                    self.protected_fallback = None;
                    accent = self.on_apply_success(path);
                }
                Err(error) => {
                    tracing::warn!("failed to apply wallpaper: {error}");
                    apply_failed = true;
                    // Remember what was displayed when the cold-start
                    // apply failed: the retry on the next scheduled
                    // fetch only fires while the display is unchanged —
                    // a wallpaper the user picks in the meantime wins.
                    if self.cold_start != ColdStart::Done {
                        self.cold_start = ColdStart::RetryOver(live.clone());
                    }
                }
            }
        }
        // The cold-start flag is spent only once an apply actually landed
        // (the Ok arm above already spent it via `on_apply_success`; this
        // keeps the pure-plan contract explicit) — otherwise first use
        // would silently end up with images downloaded but no wallpaper
        // set, and nothing would ever retry. Keeping the flag retries the
        // apply on the *next scheduled* fetch (no tighter loop: refresh
        // scheduling is unchanged by an apply failure) — unless a manual
        // or shuffle apply succeeds first, which spends it too. A retry
        // *suppressed* because the user picked another wallpaper lands
        // here with `auto_apply` false and spends the flag: the user's
        // choice wins permanently, the warm rule governs from then on.
        // A "success" that left nothing to apply (an all-ineligible day over
        // an empty catalogue) keeps the flag armed for the fetch that finally
        // delivers a target.
        if target.is_some() && !apply_failed {
            self.cold_start = ColdStart::Done;
        }

        let refresh_timer = self.schedule_refresh(plan.delay);
        // A grown catalogue may unlock a waiting shuffle (≥2 images); a
        // pending tick keeps its countdown.
        let shuffle = self.sync_shuffle(false);
        let acknowledgement = peer_request.map_or_else(Task::none, |request| {
            self.record_peer_refresh_completion(request, peer_outcome)
        });
        Task::batch([refresh_timer, shuffle, accent, acknowledgement, owed_pass])
    }

    /// Shared state transition for every path that successfully applied a
    /// wallpaper (post-fetch auto-apply, manual navigation, shuffle tick):
    /// remember the file as current and spend the cold-start state. Its only
    /// purpose is guaranteeing that a fresh install ends up with *some*
    /// wallpaper applied once; any successful apply through the applet
    /// fulfills that. Leaving it armed after e.g. a failed cold-start
    /// auto-apply followed by a successful manual apply would let a later
    /// refresh's cold-start branch ([`wallpaper::should_auto_apply`]) clobber
    /// a wallpaper the user picked in COSMIC Settings in the meantime.
    ///
    /// Returns the accent recompute task for the freshly applied wallpaper
    /// (`Task::none()` unless the accent feature is on) — callers batch it
    /// into whatever they were returning anyway.
    fn on_apply_success(&mut self, path: PathBuf) -> app::Task<Message> {
        self.current = Some(path.clone());
        self.cold_start = ColdStart::Done;
        self.start_accent_compute(path)
    }

    /// Remove a vanished image after an apply failure, but only while this
    /// process owns the authoritative catalogue.
    fn on_apply_failure(&mut self, path: &Path) -> app::Task<Message> {
        if self.is_active_leader() && !path.is_file() {
            self.prune_immediately()
        } else {
            Task::none()
        }
    }

    /// Arm the async accent extraction for `source` (a freshly applied
    /// wallpaper, or the startup-restored current). Gated on the setting and
    /// on usable theme handles; finishes in [`Message::AccentComputed`],
    /// whose handler re-checks everything against live state.
    fn start_accent_compute(&self, source: PathBuf) -> app::Task<Message> {
        if !self.is_active_leader() || !self.config.accent_enabled || self.accent_handles.is_none()
        {
            return Task::none();
        }
        cosmic::task::future(async move {
            match extract_accent_hue(&source, state_dir()).await {
                Some(hue) => cosmic::Action::App(Message::AccentComputed { source, hue }),
                // Failure paths (no thumbnail, decode error) change nothing:
                // they were logged in the task and produce no message.
                None => cosmic::Action::None,
            }
        })
    }

    /// The `AccentComputed` handler: run on the UI thread against *live*
    /// state, because the async task's snapshot goes stale during the decode
    /// — the wallpaper may have changed (the `source` guard) and so may the
    /// builder accents (read fresh here, right before the plan).
    fn finish_accent_compute(&mut self, source: PathBuf, hue: Option<f32>) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }
        if self.current.as_deref() != Some(source.as_path()) {
            tracing::debug!("dropping stale accent result for {}", source.display());
            return Task::none();
        }
        if self.accent_inflight.is_some() {
            // Write guard: the builders on disk are mid-mutation by our own
            // task — a plan against them would compare torn state. Drop the
            // result and re-arm a fresh compute once the task completes.
            tracing::debug!("deferring accent result: a theme write is in flight");
            self.accent_recompute_queued = true;
            return Task::none();
        }
        let Some(handles) = &self.accent_handles else {
            // Near-unreachable (the compute is only armed with handles on
            // hand), but the module's failure contract is log-and-change-
            // nothing — never a silent return.
            tracing::debug!("dropping accent result: theme configs unavailable");
            return Task::none();
        };
        let builders = accent::read_builders(handles);
        let action = accent::accent_plan(
            self.config.accent_enabled,
            self.config.accent_snapshot,
            self.config.accent_last_written,
            &builders,
            hue,
        );
        self.execute_accent_action(action, builders)
    }

    /// Execute a pure [`accent::AccentAction`] against the theme configs and
    /// the persisted feature state, writing onto the same `builders` the plan
    /// just compared. Every failure path logs and leaves the user's accent
    /// exactly as it was.
    ///
    /// A `Write` splits across the async gap (the theme write itself must
    /// never run inline in `update()` — the 2026-08-08 btrfs incident): the
    /// snapshot persist stays here, *before* anything is spawned; the theme
    /// write runs as an [`AccentInflight::Write`] task; `accent_last_written`
    /// is persisted by the completion handler ([`Window::finish_accent_write`])
    /// only after the write verifiably landed.
    fn execute_accent_action(
        &mut self,
        action: accent::AccentAction,
        builders: accent::Builders,
    ) -> app::Task<Message> {
        match action {
            accent::AccentAction::Write {
                light,
                dark,
                snapshot_now,
            } => {
                // Both dependencies checked before anything is persisted: a
                // missing theme handle or a memory-only config must not leave
                // a half-done state (a snapshot stored without its write, or
                // theme colours without their on-disk record).
                let (Some(_), Some(context)) = (&self.accent_handles, &self.config_context) else {
                    // Near-unreachable (enabling refuses without both), but
                    // the failure contract is log-and-change-nothing.
                    tracing::debug!(
                        "not writing accent colours: theme configs or applet config unavailable"
                    );
                    return Task::none();
                };
                let context = context.clone();
                let current = accent::builder_accents(&builders);
                if snapshot_now {
                    // The current accents are still the user's — nothing of
                    // ours has landed yet — so capture them *before* writing,
                    // with a *checked* persist (`set_config` only warns): the
                    // snapshot must be safely on disk before the write it
                    // exists to undo, or a restart would find our colours in
                    // the themes with no record behind them. No snapshot on
                    // disk → no write (and no task spawned).
                    let snapshot = accent::AccentSnapshot {
                        light: current.0,
                        dark: current.1,
                    };
                    if let Err(error) = self.config.set_accent_snapshot(&context, Some(snapshot)) {
                        // The derive's setter mutates the field before
                        // writing — undo that too, so the next plan still
                        // says `snapshot_now`.
                        self.config.accent_snapshot = None;
                        tracing::warn!(
                            "not writing accent colours: \
                             cannot persist the accent snapshot: {error}"
                        );
                        return Task::none();
                    }
                }
                // The snapshot is safely on disk; the write leaves the UI
                // thread with the exact builders the plan compared (no
                // re-read TOCTOU) and reports back to
                // `finish_accent_write`.
                self.spawn_accent_task(AccentInflight::Write {
                    builders: Box::new(builders),
                    pair: accent::AccentPair { light, dark },
                    previous: accent::AccentSnapshot {
                        light: current.0,
                        dark: current.1,
                    },
                })
            }
            accent::AccentAction::Disarm { keep_snapshot } => {
                // The accent no longer matches what we wrote: a manual choice
                // stands — no restore. Flipping the setting through
                // `set_config` persists it, so the popup's toggler row
                // follows by itself. In the enable→first-write gap the
                // mismatch may equally be our own unrecorded write (crash or
                // failed persist between the theme write and its record), so
                // the plan says to keep the snapshot — the only record of
                // the user's pre-feature accents — for the next enable's
                // deferred restore.
                tracing::info!("accent changed externally: disabling accent-from-wallpaper");
                let mut config = self.config.clone();
                config.accent_enabled = false;
                if !keep_snapshot {
                    config.accent_snapshot = None;
                }
                config.accent_last_written = None;
                self.set_config(config);
                Task::none()
            }
            accent::AccentAction::Skip => Task::none(),
        }
    }

    /// Launch the blocking-pool task for an accent theme write/restore and
    /// hold the write guard: `inflight` records what is running (and
    /// everything its completion needs), [`accent_job`] derives the actual
    /// theme I/O from it, and the completion arrives as
    /// [`Message::AccentWriteFinished`] carrying this generation.
    fn spawn_accent_task(&mut self, inflight: AccentInflight) -> app::Task<Message> {
        let Some(handles) = self.accent_handles.clone() else {
            // Callers check; never hold the guard for a task that cannot run.
            tracing::debug!("not spawning an accent task: theme configs unavailable");
            return Task::none();
        };
        // Record the flight's disk-flag baseline — a rollback chained onto a
        // write continues that write's flight, so it keeps the original
        // baseline (re-reading here would adopt an external flip that landed
        // *during* the write as the baseline and never route it).
        if !matches!(inflight, AccentInflight::Rollback { .. }) {
            self.accent_disk_enabled_at_spawn = self
                .config_context
                .as_ref()
                .map(|context| AppletConfig::load(context).accent_enabled);
        }
        self.accent_write_generation += 1;
        let generation = self.accent_write_generation;
        let job = accent_job(&inflight);
        self.accent_inflight = Some(inflight);
        cosmic::task::future(async move {
            let success =
                match tokio::task::spawn_blocking(move || run_accent_job(&handles, job)).await {
                    Ok(success) => success,
                    Err(error) => {
                        tracing::warn!("accent theme task failed: {error}");
                        false
                    }
                };
            Message::AccentWriteFinished {
                generation,
                success,
            }
        })
    }

    /// The [`Message::AccentWriteFinished`] handler: retire the write guard,
    /// finish what the task started (against *live* state — the reason none
    /// of this runs inside the task), then reconcile anything that was
    /// deferred while the task flew: an `accent_enabled` flip (against a
    /// **fresh** read of the on-disk config, never the possibly-stale echo
    /// payloads suppressed meanwhile) and a queued recompute.
    fn finish_accent_task(&mut self, generation: u64, success: bool) -> app::Task<Message> {
        if !self.is_active_leader() {
            return Task::none();
        }
        if generation != self.accent_write_generation {
            // Same contract as the timers: a superseded task's completion
            // must not touch the guard state of the current one.
            tracing::debug!("ignoring a stale accent task completion");
            return Task::none();
        }
        let Some(inflight) = self.accent_inflight.take() else {
            tracing::debug!("ignoring an accent task completion with nothing in flight");
            return Task::none();
        };
        // The fresh disk read happens *before* the completion handlers run:
        // their own persists write the very key an external flip landed on
        // mid-flight (`arm_accent_enable` pins the flag `true`,
        // `finish_disable_restore`'s changed-field `set_config` rewrites it
        // `false` from memory) — reading after them would read our own write back
        // and silently clobber a genuine external flip instead of routing
        // it.
        let disk_enabled_now = self
            .config_context
            .as_ref()
            .map(|context| AppletConfig::load(context).accent_enabled);
        let follow_up = match inflight {
            AccentInflight::Write { pair, previous, .. } => {
                self.finish_accent_write(pair, previous, success)
            }
            AccentInflight::DisableRestore { .. } => {
                self.finish_disable_restore(success);
                Task::none()
            }
            AccentInflight::EnableRestore { .. } => self.finish_enable_restore(success),
            AccentInflight::Rollback { pair, .. } => {
                self.finish_rollback(pair, success);
                Task::none()
            }
        };
        if self.accent_inflight.is_some() {
            // The completion chained another theme task (the rollback): the
            // guard stays held — reconcile and the queued recompute wait for
            // *that* task's completion (which re-reads the disk; the flight's
            // baseline survives the chain).
            return follow_up;
        }
        let disk_enabled_at_spawn = self.accent_disk_enabled_at_spawn.take();
        let reconcile = self.reconcile_accent_after_task(disk_enabled_at_spawn, disk_enabled_now);
        // The reconcile may itself have spawned a task (a routed disable's
        // restore); the recompute then keeps waiting — and arming it is
        // free when the reconcile just disabled the feature
        // (`start_accent_compute` gates on the setting).
        let recompute = if self.accent_inflight.is_none() && self.accent_recompute_queued {
            self.accent_recompute_queued = false;
            self.accent_compute_for_current()
        } else {
            Task::none()
        };
        Task::batch([follow_up, reconcile, recompute])
    }

    /// Completion of an [`AccentInflight::Write`]: persist the don't-clobber
    /// record — **only now**, after the theme write verifiably landed. The
    /// failure semantics are exactly the old synchronous executor's, split
    /// across the gap:
    ///
    /// - write failed → the task already rolled the themes back to the
    ///   accents the plan compared, so the guard still holds and the next
    ///   recompute retries; `accent_last_written` stays as it was (`None`
    ///   before a first successful write, when the snapshot stands in for it
    ///   in `accent_plan`'s guard).
    /// - write landed but the record's persist failed → roll the themes back
    ///   (a follow-up [`AccentInflight::Rollback`] task — the restore is as
    ///   heavy as the write and must not run inline either); the guard then
    ///   still holds against the *old* record and the next recompute retries.
    ///   Until that rollback lands the themes briefly hold unrecorded
    ///   colours — the same exposure as the documented crash window between
    ///   write and record, which the gap disarm (snapshot kept) covers.
    fn finish_accent_write(
        &mut self,
        pair: accent::AccentPair,
        previous: accent::AccentSnapshot,
        success: bool,
    ) -> app::Task<Message> {
        if !success {
            tracing::warn!("failed to write accent colours");
            return Task::none();
        }
        let Some(context) = self.config_context.clone() else {
            // The context existed when the write was planned; losing it
            // mid-flight is test-only in practice, but the invariant stands:
            // a landed write without its record must be rolled back.
            tracing::warn!("cannot persist the written accents; rolling the theme write back");
            return self.spawn_accent_task(AccentInflight::Rollback { pair, previous });
        };
        let recorded = self.config.accent_last_written;
        if let Err(error) = self.config.set_accent_last_written(&context, Some(pair)) {
            self.config.accent_last_written = recorded; // setter mutates first
            tracing::warn!(
                "cannot persist the written accents; rolling the theme write back: {error}"
            );
            return self.spawn_accent_task(AccentInflight::Rollback { pair, previous });
        }
        Task::none()
    }

    /// Completion of an [`AccentInflight::Rollback`]. Nothing to do on
    /// success (the in-memory record was already restored when the rollback
    /// was spawned); on failure — config and themes failing together — the
    /// themes are left holding `pair`, so the in-memory record adopts it:
    /// this session's guard still holds (and disable still restores).
    ///
    /// The *on-disk* record, though, still holds the pre-write value while
    /// the themes hold `pair` — and because memory now equals the themes,
    /// every later recompute is a steady-state Skip that never re-persists
    /// it. A restart inside that window would find themes ≠ record and hit
    /// `Disarm { keep_snapshot: false }`, destroying the snapshot without a
    /// restore (this is *not* the gap shape — the record is `Some`). So the
    /// disk record is repaired here, best-effort: persist `Some(pair)` (the
    /// value matching the themes — a restart then Skips); if even that
    /// fails, degrade it to `None` (the enable→first-write gap shape, whose
    /// disarm keeps the snapshot). Only when *both* writes fail does the
    /// destructive shape survive — the config is then wholly unwritable and
    /// nothing writable is left to repair it with.
    fn finish_rollback(&mut self, pair: accent::AccentPair, success: bool) {
        use cosmic_config::ConfigSet as _;

        if success {
            return;
        }
        tracing::error!("accent rollback failed too");
        self.config.accent_last_written = Some(pair);
        let Some(context) = &self.config_context else {
            return;
        };
        if let Err(error) = context.set("accent_last_written", Some(pair)) {
            tracing::warn!("cannot persist the accents the failed rollback left behind: {error}");
            if let Err(error) = context.set("accent_last_written", None::<accent::AccentPair>) {
                tracing::warn!("cannot clear the stale on-disk accent record either: {error}");
            }
        }
    }

    /// Completion of an [`AccentInflight::DisableRestore`]: the toggle went
    /// off (and `last_written` was cleared) when the disable was requested;
    /// the snapshot is cleared only now that the restore verifiably landed.
    /// A failed restore keeps it — the accents on disk may still be (partly)
    /// ours, and the snapshot is the only record of the user's pre-feature
    /// accents; the next enable's deferred restore is what retries it.
    fn finish_disable_restore(&mut self, success: bool) {
        if success {
            let mut config = self.config.clone();
            config.accent_snapshot = None;
            self.set_config(config);
        } else {
            tracing::warn!("failed to restore accent snapshot (snapshot kept)");
        }
    }

    /// Completion of an [`AccentInflight::EnableRestore`] (the deferred
    /// restore of a kept snapshot). Only a *successful* restore arms the
    /// feature — the old synchronous refusal semantics across the gap: a
    /// failure leaves everything off with the snapshot kept, and pins the
    /// flag off on disk (an external enable arrives already persisted).
    /// A toggle-off requested while the restore flew abandons the enable the
    /// same way — with the pleasant side effect that the restore just put
    /// the user's accents back.
    fn finish_enable_restore(&mut self, success: bool) -> app::Task<Message> {
        if !success {
            tracing::warn!("cannot enable accent-from-wallpaper: deferred snapshot restore failed");
            self.persist_accent_disabled();
            return Task::none();
        }
        if self.accent_flip_requested == Some(false) {
            tracing::info!("accent enable cancelled by a toggle during its restore");
            self.persist_accent_disabled();
            return Task::none();
        }
        self.arm_accent_enable()
    }

    /// The post-task reconcile for everything deferred while the write guard
    /// was held, routed through the same toggle lifecycle the not-in-flight
    /// path uses. A no-op in the common case (our own persists left disk and
    /// memory agreeing). Precedence for the desired flag:
    ///
    /// 1. A toggle the *user* requested mid-flight
    ///    (`accent_flip_requested`) — the newest action whose ordering is
    ///    known. It was pinned onto disk too; the in-memory record remains
    ///    authoritative because watcher delivery can still be late or torn.
    /// 2. Otherwise a genuine external flip, evidenced by the on-disk flag
    ///    having *changed* during the flight: `at_completion` (read before
    ///    the completion handlers' own persists — see
    ///    [`Window::finish_accent_task`]) differing from `at_spawn` (the
    ///    flight's baseline). Never the echo payloads suppressed during the
    ///    flight, which may have been stale or torn (the incident's
    ///    oscillation) — and never a bare completion-time disk-vs-memory
    ///    compare, which would misread the enable path's flag-lands-last
    ///    persist ordering as an external disable.
    /// 3. Otherwise memory stands — no flip.
    fn reconcile_accent_after_task(
        &mut self,
        at_spawn: Option<bool>,
        at_completion: Option<bool>,
    ) -> app::Task<Message> {
        let requested = self.accent_flip_requested.take();
        let desired = match (requested, at_spawn, at_completion) {
            (Some(requested), _, _) => requested,
            (None, Some(spawn), Some(completion)) if spawn != completion => completion,
            _ => self.config.accent_enabled,
        };
        if desired != self.config.accent_enabled {
            tracing::info!("reconciling an accent toggle deferred during a theme write");
            return self.set_accent_enabled(desired);
        }
        Task::none()
    }

    /// The accent toggler — the flip/echo dispatcher. Enable is
    /// [`Window::try_enable_accent`]. Disable: flip the setting off and clear
    /// `last_written` *now* (the toggler must not wait on theme I/O), then
    /// restore the snapshot verbatim (including the `None` = palette-default
    /// state) on the blocking pool; the snapshot itself is cleared only once
    /// the restore lands ([`Window::finish_disable_restore`]) — a restore
    /// that fails keeps it (still off), and the next enable's deferred
    /// restore is the retry. A crash mid-restore leaves the persisted
    /// kept-snapshot shape, which that same deferred restore reconciles.
    ///
    /// While an accent theme task is in flight, a flip is not executed (it
    /// would race the running write): the request is recorded for the
    /// toggler to render, pinned onto the disk config, and routed through
    /// this same dispatcher by the completion's fresh-disk-read reconcile.
    fn set_accent_enabled(&mut self, enabled: bool) -> app::Task<Message> {
        if !self.is_active_leader() {
            return self.set_non_leader_accent_enabled(enabled);
        }
        if self.accent_inflight.is_some() {
            if enabled == self.accent_toggler_state() {
                return Task::none();
            }
            self.accent_flip_requested = Some(enabled);
            self.persist_accent_flag(enabled);
            return Task::none();
        }
        if enabled == self.config.accent_enabled {
            // The toggler only fires on a flip; an echo must not re-snapshot
            // (it would capture *our* accents as the user's).
            return Task::none();
        }
        if enabled {
            return self.try_enable_accent();
        }
        match (&self.accent_handles, self.config.accent_snapshot) {
            (Some(_), Some(snapshot)) => {
                let mut config = self.config.clone();
                config.accent_enabled = false;
                config.accent_last_written = None;
                self.set_config(config);
                self.spawn_accent_task(AccentInflight::DisableRestore { snapshot })
            }
            (None, Some(_)) => {
                // Theme handles gone (config enabled on disk while the theme
                // configs failed to open this run): the snapshot cannot be
                // restored now — keep it instead of silently discarding the
                // only record of the user's pre-feature accents.
                tracing::warn!(
                    "cannot restore accent snapshot: theme configs unavailable (snapshot kept)"
                );
                self.disable_keeping_snapshot()
            }
            // Nothing was ever snapshotted; nothing to restore.
            (_, None) => {
                let mut config = self.config.clone();
                config.accent_enabled = false;
                config.accent_snapshot = None;
                config.accent_last_written = None;
                self.set_config(config);
                Task::none()
            }
        }
    }

    /// Proxy the follower's accent toggle through the one raw flag the
    /// leader watches. No snapshot, builder read/write, restore, or compute
    /// is permitted here. Unlike ordinary follower settings, a missing
    /// config context is not allowed to create a memory-only enabled state:
    /// no leader could observe or safely own that lifecycle.
    fn set_non_leader_accent_enabled(&mut self, enabled: bool) -> app::Task<Message> {
        if enabled == self.config.accent_enabled {
            return Task::none();
        }
        self.set_applet_setting(AppletSetting::AccentEnabled(enabled))
    }

    /// What the popup's accent toggler renders: the *requested* state while
    /// an accent theme task is in flight (a flip deferred behind the write
    /// guard, or an enable whose deferred restore is still running), the
    /// setting itself otherwise — so the row answers a click immediately
    /// even though the lifecycle behind it is asynchronous.
    pub(crate) fn accent_toggler_state(&self) -> bool {
        if let Some(requested) = self.accent_flip_requested {
            return requested;
        }
        if matches!(
            self.accent_inflight,
            Some(AccentInflight::EnableRestore { .. })
        ) {
            return true;
        }
        self.config.accent_enabled
    }

    /// The enable arm of [`Window::set_accent_enabled`]. Snapshot the *live*
    /// accents (they are still the user's — disable and disarm both clear the
    /// snapshot, so a normal re-enable re-snapshots), **unless** a snapshot
    /// survived from a disable that could not restore — that one is the only
    /// record of the user's pre-feature accents while the disk may still hold
    /// *ours*, so it is restored first (the deferred restore, now an
    /// [`AccentInflight::EnableRestore`] task: theme writes never run inline)
    /// and kept, never re-captured. The enable's tail —
    /// [`Window::arm_accent_enable`] — runs inline on the normal path and
    /// from the restore's completion on the deferred one, and only after a
    /// *successful* restore: a failure refuses the enable, exactly like the
    /// old synchronous path.
    ///
    /// Every persist here is checked (the derive's setters, not the
    /// warn-and-continue [`Window::set_config`]): the snapshot must be on
    /// disk before the feature arms, or a crash would leave our colours with
    /// no record behind them. Every refusal to enable also pins
    /// `accent_enabled = false` back onto the disk config
    /// ([`Window::persist_accent_disabled`]) — an *external* enable arrives
    /// already persisted, and leaving it there would re-arm the feature at
    /// the next startup over state this toggler never built.
    fn try_enable_accent(&mut self) -> app::Task<Message> {
        let Some(handles) = &self.accent_handles else {
            // Without theme handles nothing could ever write or restore —
            // enabling would be a lie, so the toggle stays off.
            tracing::warn!("cannot enable accent-from-wallpaper: theme configs unavailable");
            self.persist_accent_disabled();
            return Task::none();
        };
        let Some(context) = &self.config_context else {
            // Memory-only feature state cannot survive a restart: the
            // theme would stay modified while the snapshot needed to
            // undo it dies with the process. Refuse, like above. (There
            // is no disk config to pin the refusal onto either.)
            tracing::warn!("cannot enable accent-from-wallpaper: applet config is not persistable");
            return Task::none();
        };
        match self.config.accent_snapshot {
            // A snapshot surviving a disabled period is the kept record
            // of a disable that could not restore: the on-disk accents
            // may still be *ours* from that earlier run, so re-capturing
            // them would clobber the only record of the user's
            // pre-feature accents — and a later disable would "restore"
            // our own colours. Honour the deferred restore instead; its
            // completion arms the feature, or refuses when it fails: an
            // unreconciled disk/snapshot mismatch would read as user
            // intervention to the next recompute's guard.
            Some(snapshot) => self.spawn_accent_task(AccentInflight::EnableRestore { snapshot }),
            // The normal enable: the live accents are the user's —
            // capture them so disable can put them back. Checked persist,
            // and the snapshot goes first: if a later step fails, a
            // persisted snapshot next to `accent_enabled = false` is the
            // benign kept-snapshot shape (its values are the live
            // accents, so the eventual deferred restore is a no-op).
            None => {
                let context = context.clone();
                let (light, dark) = accent::read_current_accents(handles);
                let snapshot = accent::AccentSnapshot { light, dark };
                if let Err(error) = self.config.set_accent_snapshot(&context, Some(snapshot)) {
                    // The derive's setter mutates the field before
                    // writing — undo that, or the un-persisted snapshot
                    // would sidestep every safeguard built on it.
                    self.config.accent_snapshot = None;
                    tracing::warn!(
                        "cannot enable accent-from-wallpaper: \
                         cannot persist the accent snapshot: {error}"
                    );
                    self.persist_accent_disabled();
                    return Task::none();
                }
                self.arm_accent_enable()
            }
        }
    }

    /// The enable's tail, shared by the normal (inline) path and the
    /// deferred-restore completion: clear the stale `last_written`, flip the
    /// setting on, arm a compute. Runs only once the snapshot situation is
    /// settled — persisted on the normal path, restored on the deferred one.
    fn arm_accent_enable(&mut self) -> app::Task<Message> {
        let Some(context) = self.config_context.clone() else {
            // Checked before anything was persisted or spawned; refusing
            // here (test-only in practice) keeps the sequencing honest.
            tracing::warn!("cannot enable accent-from-wallpaper: applet config is not persistable");
            return Task::none();
        };
        // Nothing of ours is on disk yet as far as this enablement is
        // concerned; a stale pair would trip the don't-clobber compare.
        let recorded = self.config.accent_last_written;
        if let Err(error) = self.config.set_accent_last_written(&context, None) {
            self.config.accent_last_written = recorded; // setter mutates first
            tracing::warn!(
                "cannot enable accent-from-wallpaper: \
                 cannot clear the stale last-written pair: {error}"
            );
            self.persist_accent_disabled();
            return Task::none();
        }
        // The toggle lands last: a failure prefix of this sequence never
        // leaves `accent_enabled = true` on disk over unpersisted state.
        if let Err(error) = self.config.set_accent_enabled(&context, true) {
            self.config.accent_enabled = false; // setter mutates first
            tracing::warn!("cannot enable accent-from-wallpaper: {error}");
            self.persist_accent_disabled();
            return Task::none();
        }
        // `self.current` can be stale against an external Settings change
        // (the documented v1 limitation — cosmic-bg is re-read around
        // fetches/prunes, not watched); the next apply recomputes from
        // the then-current wallpaper.
        self.accent_compute_for_current()
    }

    /// The shared tail of every disable that could not restore the snapshot:
    /// off, `last_written` cleared, snapshot **kept** — it is the only record
    /// of the user's pre-feature accents, and the next enable's deferred
    /// restore is the retry mechanism for it.
    fn disable_keeping_snapshot(&mut self) -> app::Task<Message> {
        let mut config = self.config.clone();
        config.accent_enabled = false;
        config.accent_last_written = None;
        self.set_config(config);
        Task::none()
    }

    /// Pin `accent_enabled = false` onto the *disk* config regardless of the
    /// in-memory value — which is already `false` on every enable-refusal
    /// path, so the write-on-change setters would not write anything. Needed
    /// because an external enable (`ConfigUpdated`) lands on disk *before*
    /// the handler runs: refusing without overwriting it leaves
    /// disk-enabled/memory-disabled, and the next startup would arm the
    /// feature over state the toggler never built. Best-effort — when even
    /// this write fails there is nothing left to do but log.
    fn persist_accent_disabled(&self) {
        self.persist_accent_flag(false);
    }

    /// Raw single-key persist of `accent_enabled`, bypassing the in-memory
    /// struct: the enable-refusal pin ([`Window::persist_accent_disabled`])
    /// and the record of a toggle deferred behind the write guard (the
    /// completion's reconcile re-reads it from disk). Best-effort.
    fn persist_accent_flag(&self, enabled: bool) {
        use cosmic_config::ConfigSet as _;

        if let Some(context) = &self.config_context
            && let Err(error) = context.set("accent_enabled", enabled)
        {
            tracing::warn!("failed to persist the requested accent toggle: {error}");
        }
    }

    /// The accent recompute for the tracked current wallpaper, or nothing
    /// when none is tracked. (Gating on the setting and the handles happens
    /// inside [`Window::start_accent_compute`].)
    fn accent_compute_for_current(&self) -> app::Task<Message> {
        match self.current.clone() {
            Some(current) => self.start_accent_compute(current),
            None => Task::none(),
        }
    }
}

/// The async half of the accent recompute: decode the *cached* thumbnail of
/// `source` and extract its dominant hue, off the UI thread (the decode goes
/// to the blocking pool for the same reason [`ensure_thumbnail_logged`]'s
/// does).
///
/// Outer `None` = nothing to act on (no cached thumbnail, or the decode
/// failed): the caller sends no message and the accent stays exactly as it
/// was — the plan's failure rule. `Some(None)` is a real answer: an
/// effectively grey wallpaper. Never `ensure_thumbnail` here — it
/// short-circuits only a `Cached` slot, so a `Failed` one would re-decode the
/// full ~5 MB UHD file on every apply (the unbounded-retry bug the
/// `decode_failed` guard exists to prevent).
async fn extract_accent_hue(source: &Path, state_dir: &Path) -> Option<Option<f32>> {
    if !thumbs::is_cached(source, state_dir) {
        if thumbs::decode_failed(source, state_dir) {
            tracing::info!(
                "no accent from {}: its thumbnail failed to decode",
                source.display()
            );
        } else {
            tracing::info!(
                "no accent from {}: no cached thumbnail yet",
                source.display()
            );
        }
        return None;
    }
    let thumb = thumbs::thumbnail_path(source, state_dir)?;
    match tokio::task::spawn_blocking(move || {
        image::open(&thumb).map(|img| accent::dominant_hue(&img.into_rgb8()))
    })
    .await
    {
        Ok(Ok(hue)) => Some(hue),
        Ok(Err(error)) => {
            tracing::warn!(
                "failed to decode the thumbnail of {}: {error}",
                source.display()
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                "accent extraction task for {} failed: {error}",
                source.display()
            );
            None
        }
    }
}

/// Restore the catalogue at startup and drop entries whose file vanished
/// while the applet wasn't running (folder cleaned out by the user, moved
/// drive, …). A catalogue that is valid JSON but points at nothing must
/// not count as "images exist" — the cold start would stay unarmed
/// and the popup/actions would trust dead paths until a much later
/// refresh. Pruning with retention `0` deletes nothing: it only drops
/// vanished entries — and scrubs tampered entries pointing outside
/// `images_dir` (their files stay untouched).
///
/// The thumbnail cache is then swept against the surviving entries
/// ([`thumbs::reconcile`]) — unconditionally, because the leftovers this
/// collects are exactly the ones no removal list can name: artefacts a
/// killed process or a prune-racing backfill wrote for entries the
/// catalogue no longer holds, and everything a rebuild from the folder
/// scan silently dropped.
fn restore_catalogue(path: &Path, images_dir: &Path, state_dir: &Path) -> CatalogueRestore {
    let CatalogueRestore {
        mut catalogue,
        provenance,
    } = Catalogue::load_or_rebuild(path, images_dir);
    let removed = catalogue.prune(images_dir, 0, None, Utc::now());
    if !removed.is_empty() {
        tracing::info!(
            "dropped {} catalogue entr{} whose file vanished",
            removed.len(),
            if removed.len() == 1 { "y" } else { "ies" }
        );
        if let Err(error) = catalogue.save(path) {
            // Non-fatal: the catalogue is rebuildable from the folder scan.
            tracing::warn!("failed to persist catalogue after startup sweep: {error}");
        }
    }
    thumbs::reconcile(
        catalogue.images.iter().map(|e| e.filename.as_path()),
        state_dir,
    );
    CatalogueRestore {
        catalogue,
        provenance,
    }
}

/// Restore startup state according to process ownership. A follower may scan
/// the image folder to rebuild its in-memory view, but it must not persist,
/// prune, or reconcile shared files.
fn restore_catalogue_for_role(
    path: &Path,
    images_dir: &Path,
    state_dir: &Path,
    active_leader: bool,
) -> CatalogueRestore {
    if active_leader {
        restore_catalogue(path, images_dir, state_dir)
    } else {
        Catalogue::load_or_rebuild(path, images_dir)
    }
}

/// `link` if it is an ordinary web URL, otherwise `None`.
///
/// The "About this image" link comes from Bing's JSON and survives in the
/// user-editable `catalogue.json`, and it is handed to `xdg-open` — which
/// takes flags (a value starting with `-`) and happily launches the default
/// handler for a `file:` URL or a bare local path. Only `http(s)://` reaches
/// the browser.
fn web_url(link: &str) -> Option<&str> {
    (link.starts_with("https://") || link.starts_with("http://")).then_some(link)
}

/// Open `target` with the default handler (`xdg-open`), detached; a
/// thread reaps the child so no zombie lingers per click.
fn open_detached(target: OsString) {
    match std::process::Command::new("xdg-open").arg(&target).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => tracing::warn!("xdg-open {} failed: {error}", target.display()),
    }
}

/// The network half of the refresh pipeline, run off the UI thread: fetch
/// the image list, download what's missing, cache thumbnails. Merging,
/// pruning, and persisting happen back on the UI thread (`RefreshFinished`)
/// against the *live* catalogue and wallpaper — a long fetch must never act
/// on a stale snapshot (it could delete the currently applied file or
/// resurrect concurrently pruned entries). Any HTTP/parse failure aborts
/// (→ 1 h retry); files downloaded before the failure stay on disk and are
/// skipped next time.
async fn run_refresh(
    catalogue: Catalogue,
    downloads: Downloads,
    backfill: Backfill,
) -> Result<RefreshBatch, RefreshError> {
    let client = bing::http_client()?;
    fetch_and_download(
        &client,
        bing::BING_BASE_URL,
        &catalogue,
        downloads,
        &wallpaper::download_dir(),
        state_dir(),
        &backfill,
    )
    .await
    .map_err(RefreshError::from)
}

/// Which of the archive's eight positions a refresh downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Downloads {
    /// Newest archive positions considered (`schedule::download_horizon`).
    horizon: u8,
    /// Whether an empty horizon may fall back to the newest eligible image
    /// beyond it — `wallpaper::should_auto_apply` at the refresh's start.
    fallback: bool,
}

/// The eligible entries one refresh downloads ([`select_downloads`]): the
/// two halves are mutually exclusive — `fallback` is only ever `Some` while
/// `in_window` is empty.
#[derive(Debug)]
struct Selection<'a> {
    in_window: Vec<&'a ArchiveImage>,
    fallback: Option<&'a ArchiveImage>,
}

/// Pick what one refresh downloads: everything eligible inside the horizon,
/// or — when the horizon holds nothing eligible and a fallback is permitted
/// — just the newest eligible image beyond it. Never both, and never any
/// other out-of-retention image: the fallback exists only so the applet has
/// *something* to apply, not to widen the history.
fn select_downloads(eligible: &[ArchiveImage], downloads: Downloads) -> Selection<'_> {
    let horizon = usize::from(downloads.horizon);
    let in_window: Vec<_> = eligible
        .iter()
        .filter(|image| image.position < horizon)
        .collect();
    let fallback = (in_window.is_empty() && downloads.fallback)
        .then(|| {
            // Response order is newest first, so the first beyond the
            // horizon is the newest eligible one.
            eligible.iter().find(|image| image.position >= horizon)
        })
        .flatten();
    Selection {
        in_window,
        fallback,
    }
}

/// Fetch the full archive window from `base_url`, download the missing
/// images `downloads` selects into `download_dir` (thumbnails cached under
/// `state_dir`), then backfill thumbnails for older catalogue entries per
/// `backfill`. All roots, the endpoint, the selection and the backfill
/// policy are injected so tests can run the whole pipeline against tempdirs
/// and a loopback mock server.
async fn fetch_and_download(
    client: &reqwest::Client,
    base_url: &str,
    catalogue: &Catalogue,
    downloads: Downloads,
    download_dir: &Path,
    state_dir: &Path,
    backfill: &Backfill,
) -> Result<RefreshBatch, bing::FetchError> {
    // A crash mid-download leaves an orphaned `.part` behind; sweep first.
    bing::sweep_part_files(download_dir);

    let bing::FetchedArchive { archive, anchor } = bing::fetch_image_list(client, base_url).await?;
    if archive.eligible.is_empty() {
        // Distinguishable in logs without a new i18n string: an
        // all-restricted day (explicit `false`) versus a Bing payload change
        // that dropped the field (absent).
        tracing::warn!(
            "Bing returned no downloadable image: {} marked wp=false, {} without wp",
            archive.ineligible.len(),
            archive.absent_wp
        );
    }

    // Only `wp: true` entries inside the horizon (or the one fallback) reach
    // this loop — an image GET is never issued for an explicitly
    // ineligible, `wp`-less, or merely out-of-retention entry.
    let selection = select_downloads(&archive.eligible, downloads);
    if let Some(fallback) = selection.fallback {
        tracing::info!(
            "no eligible image within the newest {} position(s); falling back to position {}",
            downloads.horizon,
            fallback.position
        );
    }
    let mut fetched = Vec::with_capacity(selection.in_window.len() + 1);
    for ArchiveImage { image, .. } in selection.in_window.iter().chain(&selection.fallback) {
        // A rebuilt entry may already hold this image at a different
        // resolution suffix — that file stays authoritative (no
        // re-download); the merge refills its metadata.
        //
        // Unless it never was an image: the usable-file lookup skips the
        // download just as permanently as `bing::download_image`'s own
        // existence check does, so a catalogued file failing the same
        // magic-byte test the download applies to a fresh body would
        // otherwise stay the entry's wallpaper for good. It is unlinked
        // only *after* the replacement lands, so a failed download leaves
        // the user's folder exactly as it was; the merge heals the entry
        // either way — `Catalogue::merge` treats a non-JPEG claim as dead,
        // so a failed unlink (logged) cannot pin the entry to the bad
        // file and orphan the fresh download.
        let path = match usable_existing_file(catalogue, &image.urlbase, download_dir) {
            Ok(usable) => usable,
            Err(existing) => {
                let fresh = bing::download_image(client, base_url, image, download_dir).await?;
                if let Some(corrupt) = existing.filter(|path| *path != fresh)
                    && let Err(error) = std::fs::remove_file(&corrupt)
                {
                    tracing::warn!("failed to remove {}: {error}", corrupt.display());
                }
                fresh
            }
        };
        tracing::debug!("{} resolved to {}", image.urlbase, path.display());
        if !backfill.deferred {
            ensure_thumbnail_logged(&path, state_dir).await;
        }
        fetched.push(ImageEntry::from_bing(image, path));
    }
    // The fallback, when selected, is the only entry fetched
    // (`select_downloads` offers it solely for an empty window).
    let fallback_path = selection
        .fallback
        .map(|_| fetched.last().expect("the fallback was just fetched"))
        .map(|e| e.filename.clone());

    // Backfill thumbnails for catalogue entries outside this fetch window —
    // rebuilt or older entries would otherwise show the placeholder forever.
    // Files the fetch loop above just handled are skipped.
    let handled: HashSet<&Path> = fetched.iter().map(|e| e.filename.as_path()).collect();
    backfill_thumbnails(catalogue, &handled, download_dir, state_dir, backfill).await;
    // The debt is owed only for entries the download loop would have
    // thumbnailed: an all-ineligible or suppressed refresh that fetched
    // nothing (hydrated entries were already in the catalogue the running
    // pass snapshotted) has nothing to owe, and booking it anyway would
    // buy an extra pass, sweep and accent recompute for no preview.
    let thumbnails_deferred = backfill.deferred && !handled.is_empty();

    // Metadata repair: every eligible entry *beyond* the selection whose
    // image is already on disk is hydrated from the response too — no GET,
    // just the title/credit/link and real time the merge refills a rebuilt
    // entry with. Nothing out of retention is downloaded for this; an image
    // Bing no longer lists keeps its honest filename fallback.
    let hydrated: Vec<ImageEntry> = archive
        .eligible
        .iter()
        .filter(|ArchiveImage { image, .. }| !fetched.iter().any(|e| e.urlbase == image.urlbase))
        .filter_map(|ArchiveImage { image, .. }| {
            usable_existing_file(catalogue, &image.urlbase, download_dir)
                .ok()
                .map(|existing| ImageEntry::from_bing(image, existing))
        })
        .collect();
    fetched.extend(hydrated);

    Ok(RefreshBatch {
        fetched,
        ineligible: archive.ineligible,
        anchor,
        fallback: fallback_path,
        thumbnails_deferred,
    })
}

/// The paths the two file-deleting passes exempt — the applied wallpaper
/// ([`Window::current`], freshly synced) and the downloaded fallback awaiting
/// its apply ([`Window::protected_fallback`]). One list for
/// [`Catalogue::remove_ineligible`] and [`Catalogue::prune_protecting`]
/// alike; a free function over the two fields so the caller can keep a
/// mutable borrow of its catalogue.
fn protected_paths<'a>(
    current: &'a Option<PathBuf>,
    fallback: &'a Option<PathBuf>,
) -> Vec<&'a Path> {
    current
        .iter()
        .chain(fallback.iter())
        .map(PathBuf::as_path)
        .collect()
}

/// The file the catalogue already holds for `urlbase` inside `download_dir`
/// ([`Catalogue::existing_file`]), provided it passes the same magic-byte
/// test a fresh download body must ([`bing::is_jpeg_file`]) — the one
/// predicate for "no GET needed", shared by the download loop and the
/// metadata repair so the two cannot disagree about what counts as usable.
/// `Err` carries what the catalogue claims instead — `Some` for a file that
/// exists but failed the test, which the download loop unlinks once its
/// replacement has landed.
fn usable_existing_file(
    catalogue: &Catalogue,
    urlbase: &str,
    download_dir: &Path,
) -> Result<PathBuf, Option<PathBuf>> {
    match catalogue.existing_file(urlbase, download_dir) {
        Some(existing) if bing::is_jpeg_file(&existing) => Ok(existing),
        existing => Err(existing),
    }
}

/// What one successful fetch hands back to the UI thread
/// ([`Message::RefreshFinished`]): the entries it hydrated or downloaded,
/// the URL bases Bing explicitly marked `wp: false` (to be removed, entry
/// and file, by [`Window::finish_refresh`] — never here, off the live
/// state), and the response's scheduling anchor — the newest structurally
/// valid `fullstartdate` regardless of eligibility, so an all-ineligible day
/// still schedules the normal daily delay instead of falling into
/// `next_refresh`'s out-of-range reset off a stale catalogue entry — and,
/// when the horizon held nothing eligible, the one out-of-retention
/// fallback it downloaded ([`select_downloads`]), which `finish_refresh`
/// protects from its own prune and applies in place of the newest entry.
/// `thumbnails_deferred` records that the batch was produced under the
/// producer write interlock ([`Backfill::deferred`]) *and* handled at least
/// one download, so it owes those downloads a thumbnail pass once the
/// startup pass has ended — a deferred refresh that fetched nothing owes
/// nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshBatch {
    pub fetched: Vec<ImageEntry>,
    pub ineligible: Vec<String>,
    pub anchor: String,
    pub fallback: Option<PathBuf>,
    pub thumbnails_deferred: bool,
}

/// Give catalogue entries that still lack a usable preview one, newest first,
/// within `backfill`'s budget and policy; `handled` names paths the caller
/// already thumbnailed itself.
///
/// Deliberately free of anything network-shaped: this is the whole of the
/// applet's thumbnail production, run both as the tail of a refresh
/// ([`fetch_and_download`]) and on its own at startup
/// ([`run_thumbnail_pass`]), so previews never depend on Bing being
/// reachable — only on the image files already sitting in `download_dir`.
async fn backfill_thumbnails(
    catalogue: &Catalogue,
    handled: &HashSet<&Path>,
    download_dir: &Path,
    state_dir: &Path,
    backfill: &Backfill,
) {
    if backfill.deferred {
        tracing::debug!("thumbnail backfill deferred to the running startup pass");
        return;
    }
    let mut spent = 0;
    for entry in catalogue.images.iter().rev() {
        if spent >= backfill.budget {
            break;
        }
        if handled.contains(entry.filename.as_path())
            || !backfill.should_decode(entry, download_dir, state_dir)
        {
            continue;
        }
        // Everything past this point is a real decode, so it costs budget
        // whether or not it succeeds — a *decode* failure is remembered by
        // [`thumbs::ensure_thumbnail`] and skipped for free from the next
        // refresh on. That is what keeps the two bounds true at once: work
        // per refresh is capped, and no entry is ever paid for twice, so the
        // budget always moves down the catalogue instead of being pinned to
        // an undecodable newest end.
        spent += 1;
        ensure_thumbnail_logged(&entry.filename, state_dir).await;
    }
}

/// The startup thumbnail pass, run off the UI thread: [`backfill_thumbnails`]
/// over the whole restored catalogue, nothing pre-handled and no network at
/// any point. See [`Window::start_thumbnail_pass_over`] for why it exists.
/// Roots are injected exactly as the pipeline's are, so tests never touch the
/// real folder or state dir.
async fn run_thumbnail_pass(
    catalogue: Catalogue,
    backfill: Backfill,
    download_dir: &Path,
    state_dir: &Path,
) {
    backfill_thumbnails(
        &catalogue,
        &HashSet::new(),
        download_dir,
        state_dir,
        &backfill,
    )
    .await;
}

/// Policy for the out-of-window thumbnail backfill in [`fetch_and_download`]:
/// how much decoding one refresh may do, and which entries are worth it.
/// Owned, so it is resolved on the UI thread against live state
/// (`start_refresh_over` / `start_thumbnail_pass_over`) and travels into
/// the producer task whole.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Backfill {
    /// Decode attempts this refresh may spend (see
    /// [`MAX_THUMBNAIL_BACKFILL`]).
    budget: usize,
    /// The *effective* retention in days the prune after this refresh will
    /// apply (`0` = keep forever) — [`wallpaper::prune_retention`] of the
    /// configured value, never the configured value itself.
    retention_days: u16,
    /// The currently applied wallpaper, if known — protected from the
    /// retention skip just as the prune protects it from deletion.
    current: Option<PathBuf>,
    /// A downloaded fallback awaiting its apply (`Window::protected_fallback`)
    /// — out of retention by construction, and exempt from the retention
    /// skip exactly as `Window::prune_over` exempts it from deletion. Only
    /// the startup/owed pass sets it: the pipeline's own backfill has no
    /// fallback yet (the per-download thumbnail write covers it there).
    protected_fallback: Option<PathBuf>,
    /// Reference time for the retention cutoff (injected for tests).
    now: DateTime<Utc>,
    /// Producer write interlock: the startup thumbnail pass was still
    /// running when this refresh started, so the refresh writes **no**
    /// thumbnails — neither per download nor in the tail backfill. Both
    /// producers write `<thumb>.part`/`<thumb>.meta` at fixed names
    /// (`fsutil::temp_sibling`), so two of them over the same rebuilt
    /// catalogue would race on the same files; the pass ends in its own
    /// sweep and `ThumbnailsReady` recompute, and the next refresh backfills
    /// whatever this one downloaded.
    deferred: bool,
}

impl Backfill {
    /// The retention is derived here, from the same live cosmic-bg state the
    /// prune will consult, so the two predicates cannot be handed different
    /// numbers by a caller.
    fn new(
        live: &wallpaper::CurrentWallpaper,
        configured_days: u16,
        current: Option<PathBuf>,
    ) -> Self {
        Self {
            budget: MAX_THUMBNAIL_BACKFILL,
            retention_days: wallpaper::prune_retention(live, configured_days),
            current,
            protected_fallback: None,
            now: Utc::now(),
            deferred: false,
        }
    }

    /// Whether `entry` will still exist after the prune that follows this
    /// refresh ([`Catalogue::prune`], run by `finish_refresh`) — the same age
    /// test, protected file included.
    ///
    /// The backfill runs *before* that prune, on the pipeline's snapshot, so
    /// without this check the default 8-day retention would spend the whole
    /// budget reading ~5 MB apiece for thumbnails the same refresh unlinks
    /// minutes later — worst on the very first refresh over a folder migrated
    /// from the GNOME extension, which is exactly the case the budget is
    /// sized for.
    ///
    /// There is no divergence from the prune to reason about: `retention_days`
    /// *is* what the prune will use ([`Backfill::new`]). In particular a
    /// per-output cosmic-bg setup — a steady state, not a transient one —
    /// disables age deletion for both, so nothing is skipped as doomed that
    /// the refresh then keeps.
    fn worth_decoding(&self, entry: &ImageEntry) -> bool {
        let path = entry.filename.as_path();
        entry.within_retention(self.retention_days, self.now)
            || self.current.as_deref() == Some(path)
            || self.protected_fallback.as_deref() == Some(path)
    }

    /// Whether `entry` earns a real `image::open` from this refresh's budget:
    /// it must be a file we own inside `download_dir`, still be there after
    /// the imminent prune, and have no usable slot in the `state_dir` cache
    /// yet.
    ///
    /// The three free skips come in the order that costs least — what the
    /// prune is about to delete anyway, what is already cached, and what
    /// already failed to decode (a `stat` each, and none of them work).
    /// Weakening any of them re-opens a starvation or unbounded-retry bug.
    fn should_decode(&self, entry: &ImageEntry, download_dir: &Path, state_dir: &Path) -> bool {
        // Same containment gate the prune and the download lookup apply: a
        // hand-edited `catalogue.json` must not make us decode (and cache a
        // copy of) an arbitrary readable image.
        if entry.filename.parent() != Some(download_dir)
            || !entry.names_own_file()
            || !entry.filename.is_file()
        {
            return false;
        }
        self.worth_decoding(entry)
            && !thumbs::is_cached(&entry.filename, state_dir)
            && !thumbs::decode_failed(&entry.filename, state_dir)
    }
}

/// How many out-of-window thumbnails one refresh may decode (see the
/// backfill loop in [`fetch_and_download`]).
///
/// Sized for the migrated-folder case: the decode runs on the blocking pool
/// (see [`ensure_thumbnail_logged`]) rather than on the applet's single async
/// worker, so the cap no longer has to keep that thread responsive — it only
/// bounds the one-time burst so a manual refresh cannot turn into a long CPU
/// hog. 256 covers roughly eight months of daily images in the *first*
/// refresh, so the folder the reference GNOME extension leaves behind is done
/// in one pass (retention "forever"; under a finite retention the pass stops
/// at the cutoff long before the budget runs out); even a years-deep library
/// converges in a handful of refreshes instead of the months a single-digit
/// budget would need against the ~24 h refresh cadence.
const MAX_THUMBNAIL_BACKFILL: usize = 256;

/// Generate the thumbnail for `path`; a failure is logged, never fatal — a
/// corrupt file must not abort the refresh.
///
/// Whether the failure is *remembered* is decided inside
/// [`thumbs::ensure_thumbnail`], which alone can tell a source that will not
/// decode (remember it, or the backfill pays for the same doomed
/// `image::open` on every later refresh) from a state-dir write failure (say
/// nothing — an ENOSPC blip must not condemn a decodable wallpaper). All this
/// end sees is "something went wrong", which is not a verdict about the file.
///
/// The decode goes to tokio's blocking pool: libcosmic's
/// `SingleThreadExecutor` is a *one-worker* runtime shared with reqwest I/O
/// and both one-shot timers, and a full UHD JPEG is a multi-hundred-
/// millisecond CPU burst that would otherwise stall all of it.
async fn ensure_thumbnail_logged(path: &Path, state_dir: &Path) {
    let (image, state) = (path.to_path_buf(), state_dir.to_path_buf());
    match tokio::task::spawn_blocking(move || thumbs::ensure_thumbnail(&image, &state)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::warn!(
                "thumbnail generation failed for {}: {error}",
                path.display()
            );
        }
        Err(error) => {
            tracing::warn!("thumbnail task for {} failed: {error}", path.display());
        }
    }
}

/// What a successful refresh applies: the downloaded fallback's entry when
/// the batch named one and it survived the merge/prune, else the
/// catalogue's newest entry. A fallback path with no entry (it cannot
/// survive the merge without one) degrades to the newest rather than
/// applying nothing.
fn apply_target<'a>(catalogue: &'a Catalogue, fallback: Option<&Path>) -> Option<&'a ImageEntry> {
    fallback
        .and_then(|path| catalogue.entry_for(path))
        .or_else(|| catalogue.newest())
}

/// Pure decisions after a successful fetch (tested): whether to auto-apply
/// the newest image, and when the next refresh is due.
struct RefreshSuccessPlan {
    auto_apply: bool,
    delay: Duration,
}

/// `has_images` comes from the *live catalogue* after the merge (is there
/// anything to apply?); `anchor` comes from the *response* (the newest
/// structurally valid `fullstartdate`, eligible or not — when is Bing's next
/// image due?). The two were once one `newest_fullstartdate` read off the
/// catalogue, which scheduled an all-ineligible response on a warm
/// catalogue off a stale entry and hit `next_refresh`'s ~6-minute
/// out-of-range reset every time.
///
/// `delivered` is whether the *response* handed anything over
/// (`RefreshBatch::fetched` non-empty — downloaded, hydrated, or the
/// fallback). The warm rule applies the newest image only when it did: a
/// valid response with zero eligible images is a no-op that keeps the
/// current wallpaper, even though the live wallpaper is ours — otherwise
/// an all-restricted day would pull a user who navigated to an older image
/// back to the newest one. Cold start is exempt: a restored catalogue is
/// still worth applying on the fetch that fails to deliver.
fn refresh_success_plan(
    cold_start_pending: bool,
    live_current: Option<&Path>,
    has_images: bool,
    delivered: bool,
    anchor: &str,
    now: DateTime<Utc>,
) -> RefreshSuccessPlan {
    RefreshSuccessPlan {
        auto_apply: has_images
            && (cold_start_pending || delivered)
            && wallpaper::should_auto_apply(cold_start_pending, live_current),
        delay: schedule::next_refresh(Some(anchor), now),
    }
}

impl cosmic::Application for Window {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &cosmic::app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::app::Core {
        &mut self.core
    }

    fn init(core: cosmic::app::Core, _flags: ()) -> (Self, app::Task<Self::Message>) {
        // Election precedes catalogue restoration: a loser must never run the
        // destructive startup prune/cache sweep before it knows its role.
        let leadership = Leadership::acquire(state_dir());
        let active_leader = leadership.is_leader();

        // Missing/invalid config must never crash the applet: a failed context
        // or unreadable keys both degrade to defaults.
        let config_context = match AppletConfig::context() {
            Ok(context) => Some(context),
            Err(error) => {
                tracing::warn!("cannot open applet config (using defaults): {error}");
                None
            }
        };
        let config = config_context
            .as_ref()
            .map(AppletConfig::load)
            .unwrap_or_default();
        let coordination_context = match CoordinationConfig::context() {
            Ok(context) => Some(context),
            Err(error) => {
                tracing::warn!("cannot open coordination config (using defaults): {error}");
                None
            }
        };
        let coordination = coordination_context
            .as_ref()
            .map(CoordinationConfig::load)
            .unwrap_or_default();
        let peer_apply_notice_generation = coordination
            .apply_notice
            .as_ref()
            .map_or(0, |notice| notice.generation);
        let peer_refresh_covered = coordination.refresh_completion.request;

        // Same degradation for the theme configs: without them the accent
        // feature is inert, everything else keeps working.
        let accent_handles = match accent::ThemeHandles::system() {
            Ok(handles) => Some(handles),
            Err(error) => {
                tracing::warn!("cannot open theme configs (accent feature inactive): {error}");
                None
            }
        };

        // Restore instantly from disk — no network involved. A corrupt or
        // missing catalogue rebuilds from the download folder scan; entries
        // whose file vanished while we weren't running are dropped so a
        // hollow catalogue still counts as a cold start.
        let CatalogueRestore {
            catalogue,
            provenance,
        } = restore_catalogue_for_role(
            &catalogue_path(),
            &wallpaper::download_dir(),
            state_dir(),
            active_leader,
        );
        // Read once and handed to the thumbnail pass below: it must protect
        // the applied file's preview exactly as the prune protects its file.
        let live = wallpaper::current_wallpaper();
        let current = live.clone().into_file();
        let cold_start = if catalogue.images.is_empty() {
            ColdStart::Pending
        } else {
            ColdStart::Done
        };

        let mut window = Self {
            core,
            popup: None,
            dropdowns_open: 0,
            stale_menu_closes: 0,
            closing_popups: Vec::new(),
            leadership,
            leader_readiness: LeaderReadiness::Ready,
            leadership_generation: 0,
            leadership_hydration_generation: 0,
            leadership_state_generation: 0,
            config,
            config_context,
            coordination,
            coordination_context,
            setting_write_generations: [0; 4],
            setting_write_queue: VecDeque::new(),
            setting_write_inflight: false,
            config_confirmation_generation: 0,
            coordination_state_dir: state_dir().to_path_buf(),
            peer_refresh_request: None,
            peer_refresh_covered,
            peer_refresh_live_read: None,
            requested_peer_refresh: None,
            peer_refresh_write_pending: false,
            peer_refresh_timeout_generation: 0,
            non_leader_reload_generation: 0,
            non_leader_reload_repairs: false,
            peer_apply_notice_generation,
            catalogue,
            current,
            refresh_pending: false,
            thumbnail_pass_pending: false,
            thumbnails_owed: false,
            cold_start,
            protected_fallback: None,
            // A rebuilt pack shows filenames until a fetch merge refills its
            // metadata; the leader repairs that immediately rather than at
            // the next scheduled refresh (`arm_leader_duties`).
            metadata_repair_due: provenance == Provenance::Rebuilt,
            timer_generation: 0,
            shuffle_generation: 0,
            shuffle_armed: false,
            last_updated: None,
            last_error: None,
            accent_handles,
            accent_write_generation: 0,
            accent_inflight: None,
            accent_recompute_queued: false,
            accent_flip_requested: None,
            accent_disk_enabled_at_spawn: None,
            lock_poke_generation: 0,
            poke_config: wallpaper::poke_state_handle(),
            #[cfg(test)]
            test_snapshot_inputs: None,
            #[cfg(test)]
            test_hydration_contexts: None,
        };
        let startup = window.arm_initial_duties(live);
        (window, startup)
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::TogglePopup => {
                if let Some(popup_id) = self.popup.take() {
                    // The runtime *does* announce this destroy back to us
                    // (`PopupEvent::Done` → [`Message::PopupClosed`], see
                    // [`Window::on_popup_closed`]), but only asynchronously —
                    // across a thread and four queues — and by then the id no
                    // longer matches `self.popup`. So the ledger is reset here
                    // rather than left to a row that can no longer fire: a
                    // stale count would pause every tooltip for the rest of
                    // the session. Zero, not a decrement — every child dies
                    // with our popup.
                    //
                    // Those late closes are then owed to *this* session, not
                    // the next one: without the debt they would decrement a
                    // reopened session's count with its menu still up. One per
                    // menu that was mapped, anonymously — a menu's id is minted
                    // inside the widget — plus our own popup, which the
                    // `Action::Destroy` arm also emits a `Done` for and whose id
                    // we *do* know: a create that never mapped leaves a destroy
                    // that emits nothing, and an anonymous unit for it would
                    // swallow a live menu's close instead of going unclaimed.
                    //
                    // Both go on one entry, which is what bounds the anonymous
                    // half: this popup's own `Done` comes back *after* every
                    // child close that teardown will emit, so whatever is still
                    // owed then never existed ([`ClosingPopup::menus_owed`]).
                    self.closing_popups.push(ClosingPopup {
                        id: popup_id,
                        menus_owed: self.dropdowns_open,
                    });
                    self.dropdowns_open = 0;
                    return cosmic::surface::surface_task(cosmic::surface::action::destroy_popup(
                        popup_id,
                    ));
                }
                let popup = cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                    |_: &Window| Default::default(),
                    |window: &mut Window| {
                        let new_id = window::Id::unique();
                        // Never a bare `replace`: a second toggle drained in
                        // the same round reaches this closure with `self.popup`
                        // already set, and the id it overwrites is a popup
                        // upstream is about to destroy — see
                        // [`Window::adopt_popup`].
                        window.adopt_popup(new_id);

                        window.core.applet.get_popup_settings(
                            window.core.main_window_id().unwrap(),
                            new_id,
                            None,
                            None,
                            None,
                        )
                    },
                    None,
                ));
                // The popup action is emitted immediately. A follower also
                // refreshes its read-only view in parallel unless a peer
                // refresh is already settling; that path reloads once its
                // completion or timeout arrives instead.
                if !self.leadership.is_leader()
                    && !self.peer_refresh_write_pending
                    && self.requested_peer_refresh.is_none()
                    && !self.refresh_pending
                {
                    return Task::batch([popup, self.request_non_leader_reload(true)]);
                }
                return popup;
            }
            Message::PopupClosed(id) => return self.on_popup_closed(id),
            Message::ConfigUpdated(config) => {
                self.leadership_state_generation = self.leadership_state_generation.wrapping_add(1);
                if self.leadership.is_leader()
                    && self.leader_readiness == LeaderReadiness::Hydrating
                {
                    // The blocking takeover read will be discarded and
                    // repeated. Do not adopt a possibly older watcher payload
                    // (or synchronously confirm it from disk) in between.
                    return Task::none();
                }
                return self.confirm_config_update(config);
            }
            Message::ConfigConfirmed {
                generation,
                was_active_leader,
                config,
            } => {
                return self.finish_config_confirmation(generation, was_active_leader, config);
            }
            Message::CoordinationUpdated(coordination) => {
                self.leadership_state_generation = self.leadership_state_generation.wrapping_add(1);
                if self.leadership.is_leader()
                    && self.leader_readiness == LeaderReadiness::Hydrating
                {
                    return Task::none();
                }
                merge_coordination(&mut self.coordination, coordination);
                let completion = self.coordination.refresh_completion;
                let apply_notice = self.coordination.apply_notice.clone();
                if self.is_active_leader() {
                    let refresh = self.consume_peer_refresh_request();
                    let apply = self.consume_peer_apply_notice(apply_notice);
                    return Task::batch([refresh, apply]);
                }
                return self.settle_peer_refresh(completion);
            }
            Message::PeerRefreshLiveRead { request, live } => {
                return self.finish_peer_refresh_live_read(request, live);
            }
            Message::LeadershipTick(generation) => return self.on_leadership_tick(generation),
            Message::LeadershipHydrated {
                generation,
                state_generation,
                result,
            } => {
                return self.finish_leadership_hydration(generation, state_generation, result);
            }
            Message::NonLeaderReloaded { generation, result } => {
                return self.finish_non_leader_reload(generation, result);
            }
            Message::AppletSettingWritten {
                generation,
                setting,
                result,
            } => return self.finish_applet_setting_write(generation, setting, result),
            Message::RefreshDue(generation) => {
                // Stale timers (replaced by a newer reschedule) are ignored.
                if self.is_active_leader() && generation == self.timer_generation {
                    return self.start_refresh();
                }
            }
            Message::RefreshNow => return self.refresh_now(),
            Message::PeerRefreshRequested(result) => {
                return self.finish_peer_refresh_request(result);
            }
            Message::PeerRefreshCompletionWritten { completion, result } => match result {
                Ok(true) => {
                    if completion.request > self.coordination.refresh_completion.request {
                        self.coordination.refresh_completion = completion;
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        request = completion.request,
                        "failed to persist peer refresh completion: {error}"
                    );
                }
            },
            Message::PeerRefreshTimeout {
                generation,
                request,
            } => return self.timeout_peer_refresh(generation, request),
            Message::PeerApplyNoticeWritten(result) => match result {
                Ok(notice) => {
                    let current = self
                        .coordination
                        .apply_notice
                        .as_ref()
                        .map_or(0, |current| current.generation);
                    if notice.generation > current {
                        self.coordination.apply_notice = Some(notice);
                    }
                }
                Err(error) => tracing::warn!("failed to persist peer apply notice: {error}"),
            },
            Message::PeerApplyValidated { generation, live } => {
                return self.finish_peer_apply_validation(generation, live);
            }
            Message::ApplyImage(path) => {
                // Browsing is setting: prev/next/newest apply immediately.
                // Manual navigation also resets the shuffle countdown —
                // and spends the cold-start flag (any successful apply
                // fulfills its purpose; see `on_apply_success`).
                match wallpaper::apply(&path) {
                    Ok(()) => return self.finish_manual_apply(path),
                    Err(error) => {
                        tracing::warn!("failed to apply {}: {error}", path.display());
                        // If the file vanished externally (the common way
                        // apply refuses), prune right away: the routine
                        // prune drops entries whose file is gone, so the
                        // dead image leaves the popup instead of failing
                        // on every further click until the next fetch.
                        return self.on_apply_failure(&path);
                    }
                }
            }
            Message::OpenUrl(url) => match web_url(&url) {
                Some(url) => open_detached(url.into()),
                None => tracing::warn!("refusing to open non-web link {url:?}"),
            },
            // Same reasoning as `web_url`: the path comes from the
            // user-editable catalogue and `xdg-open` reads a leading `-` as a
            // flag. Every path the applet stores is absolute, which rejects
            // both that and a relative path resolved against the process CWD.
            Message::OpenFile(path) if path.is_absolute() => {
                open_detached(path.into_os_string());
            }
            Message::OpenFile(path) => {
                tracing::warn!("refusing to open non-absolute path {}", path.display());
            }
            Message::ShuffleDue(generation) => {
                // Stale ticks (replaced by a newer re-arm) are ignored.
                if !self.is_active_leader() || generation != self.shuffle_generation {
                    return Task::none();
                }
                self.shuffle_armed = false;
                if !self.config.shuffle_enabled {
                    return Task::none();
                }
                let mut accent = Task::none();
                if let Some(pick) = self.catalogue.random_other(self.current.as_deref()) {
                    let path = pick.filename.clone();
                    match wallpaper::apply(&path) {
                        Ok(()) => accent = self.on_apply_success(path),
                        Err(error) => {
                            tracing::warn!("shuffle failed to apply {}: {error}", path.display());
                            // A vanished file gets dropped from the
                            // catalogue right away (see the ApplyImage
                            // arm); no immediate re-pick — the next tick
                            // draws from the cleaned catalogue, and the
                            // trailing sync_shuffle disarms if it shrank
                            // below two images.
                            if !path.is_file() {
                                self.prune_and_persist();
                            }
                        }
                    }
                }
                // A tick starts a fresh cycle: re-arming waits one full
                // interval again.
                let shuffle = self.sync_shuffle(false);
                return Task::batch([accent, shuffle]);
            }
            Message::SetShuffleEnabled(enabled) => {
                let active_leader = self.is_active_leader();
                let persist = self.set_applet_setting(AppletSetting::ShuffleEnabled(enabled));
                // Enabling starts a fresh full-interval countdown.
                return if active_leader {
                    Task::batch([persist, self.sync_shuffle(true)])
                } else {
                    persist
                };
            }
            Message::SetShuffleInterval(index) => {
                let active_leader = self.is_active_leader();
                let interval = view::shuffle_interval_secs(index);
                let persist = self.set_applet_setting(AppletSetting::ShuffleInterval(interval));
                // Picking an interval restarts the countdown at that length.
                return if active_leader {
                    Task::batch([persist, self.sync_shuffle(true)])
                } else {
                    persist
                };
            }
            Message::SetRetention(index) => {
                let active_leader = self.is_active_leader();
                let new_days = view::retention_days(index);
                let reduced = schedule::retention_reduced(self.config.retention_days, new_days);
                let persist = self.set_applet_setting(AppletSetting::Retention(new_days));
                if active_leader && reduced {
                    // Reduced retention prunes immediately (the routine
                    // post-fetch prune would otherwise leave over-limit
                    // files around for up to a day).
                    return Task::batch([persist, self.prune_immediately()]);
                }
                return persist;
            }
            Message::SetAccentEnabled(enabled) => return self.set_accent_enabled(enabled),
            Message::AccentComputed { source, hue } => {
                if !self.is_active_leader() {
                    return Task::none();
                }
                return self.finish_accent_compute(source, hue);
            }
            Message::AccentWriteFinished {
                generation,
                success,
            } => {
                if !self.is_active_leader() {
                    return Task::none();
                }
                return self.finish_accent_task(generation, success);
            }
            Message::LockEvent(event) => {
                if !self.is_active_leader() {
                    return Task::none();
                }
                tracing::debug!(?event, "arming lock-screen poke ladder");
                return self.arm_lock_pokes();
            }
            Message::LockPokeDue(generation) => {
                let Some(config) = self.due_lock_poke(generation) else {
                    return Task::none();
                };
                return cosmic::task::future(async move {
                    let wrote =
                        match tokio::task::spawn_blocking(move || run_lock_poke(&config)).await {
                            Ok(wrote) => wrote,
                            Err(error) => {
                                tracing::warn!("lock poke task failed: {error}");
                                false
                            }
                        };
                    Message::LockPokeFinished(wrote)
                });
            }
            Message::LockPokeFinished(wrote) => {
                // Log-only by design: pokes never touch other applet state
                // (and never the popup ledger).
                tracing::debug!(wrote, "lock-screen state poke finished");
            }
            Message::TooltipSurface(action) => return self.on_tooltip_surface(action),
            Message::DropdownSurface(action) => return self.on_dropdown_surface(action),
            Message::RefreshFinished(result) => return self.finish_refresh(result),
            // Returning to the message loop re-renders the popup, so a
            // preview generated while it was open shows up by itself.
            Message::ThumbnailsReady => {
                let owed_pass =
                    self.finish_thumbnail_pass(state_dir(), wallpaper::current_wallpaper());
                if !self.is_active_leader() {
                    return Task::none();
                }
                // The startup (or enable-time) accent compute may have found
                // a cold cache and dropped its answer; the pass that just
                // ended is what writes those thumbnails, so this is the
                // moment a retry can succeed. Free when disabled or already
                // answered (a cached decode plus the steady-state Skip).
                return Task::batch([owed_pass, self.accent_compute_for_current()]);
            }
        }
        Task::none()
    }

    fn subscription(&self) -> iced::Subscription<Self::Message> {
        iced::Subscription::batch([
            // Keep `self.config` in sync with on-disk changes (our own setter
            // writes echo back through here too, which is harmless).
            self.core
                .watch_config::<AppletConfig>(APP_ID)
                .map(|update| Message::ConfigUpdated(update.config)),
            self.core
                .watch_config::<CoordinationConfig>(APP_ID)
                .map(|update| Message::CoordinationUpdated(update.config)),
            // logind lock/resume events → the poke ladder (the
            // cosmic-greeter#511 workaround; rationale in `lockwatch.rs`).
            lockwatch::subscription().map(Message::LockEvent),
        ])
    }

    /// The panel button — bare, with **no** hover tooltip naming the applet.
    ///
    /// libcosmic's applet example wraps this button in `applet_tooltip`, but
    /// the panel's own status applets (audio, battery, network, notifications,
    /// time) do not label themselves on hover, and the first-party applets that
    /// *do* use a panel tooltip put dynamic content in it rather than their own
    /// name — window titles in cosmic-app-list and cosmic-applet-minimize, the
    /// button label in cosmic-panel-button. A tooltip here made this applet the
    /// only icon in the tray to announce itself. The tooltips inside the popup
    /// stay: they name icon-only controls, which is what tooltips are for.
    fn view(&self) -> Element<'_, Self::Message> {
        self.core
            .applet
            .icon_button(PANEL_ICON)
            .on_press_down(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, id: window::Id) -> Element<'_, Self::Message> {
        if matches!(self.popup, Some(popup_id) if popup_id == id) {
            view::popup_view(self)
        } else {
            widget::text("").into()
        }
    }

    fn style(&self) -> Option<iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use cosmic::cosmic_config::CosmicConfigEntry as _;
    use std::collections::BTreeMap;

    fn file_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
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

    #[test]
    fn default_window_is_a_ready_leader() {
        let mut window = Window::default();
        assert!(window.leadership.is_leader());
        assert_eq!(window.leader_readiness, LeaderReadiness::Ready);
        assert!(window.is_active_leader());
        window.leader_readiness = LeaderReadiness::Hydrating;
        assert!(
            !window.is_active_leader(),
            "owning the lock is inert until hydration is complete"
        );
    }

    #[test]
    fn startup_arms_only_duties_owned_by_the_instance() {
        let mut leader = Window::default();
        let leader_task = leader.arm_initial_duties(wallpaper::CurrentWallpaper::NoFile);
        assert_eq!(leader.timer_generation, 1, "ordinary refresh timer armed");
        assert_eq!(leader.shuffle_generation, 1, "shuffle state synchronized");
        assert!(leader.thumbnail_pass_pending, "startup producer armed");
        assert!(!leader.refresh_pending);
        assert_eq!(leader.leadership_generation, 0);
        assert_eq!(leader_task.units(), 2, "refresh timer plus thumbnail pass");

        let mut follower = Window {
            leadership: Leadership::forced(false),
            ..Window::default()
        };
        let follower_task = follower.arm_initial_duties(wallpaper::CurrentWallpaper::NoFile);
        assert_eq!(follower.leadership_generation, 1);
        assert_eq!(follower.timer_generation, 0);
        assert_eq!(follower.shuffle_generation, 0);
        assert!(!follower.shuffle_armed);
        assert!(!follower.thumbnail_pass_pending);
        assert!(!follower.refresh_pending);
        assert_eq!(follower_task.units(), 1, "takeover retry only");
    }

    #[test]
    fn initial_leader_consumes_outstanding_peer_refresh_beside_the_thumbnail_pass() {
        let mut window = Window {
            coordination: CoordinationConfig {
                refresh_request: 7,
                refresh_completion: crate::config::PeerRefreshCompletion {
                    request: 5,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Window::default()
        };

        let task = window.arm_initial_duties(wallpaper::CurrentWallpaper::NoFile);

        assert_eq!(window.peer_refresh_request, Some(7));
        assert!(window.refresh_pending, "peer request starts the fetch now");
        // The pre-existing hole: offline, that fetch dies at the list
        // request, so the pass must be armed *as well* — it is the only
        // guaranteed thumbnail producer. Armed first, so the refresh defers
        // its own thumbnail writes to it.
        assert!(
            window.thumbnail_pass_pending,
            "the startup pass is never replaced by a refresh"
        );
        assert_eq!(
            window.timer_generation, 1,
            "ordinary timer also remains armed"
        );
        assert_eq!(
            task.units(),
            3,
            "timer, thumbnail pass and immediate refresh"
        );
    }

    #[test]
    fn a_rebuilt_restore_starts_a_repair_refresh_beside_the_pass() {
        let mut window = Window {
            metadata_repair_due: true,
            ..window_with_images(1)
        };

        let task = window.arm_initial_duties(wallpaper::CurrentWallpaper::NoFile);

        assert!(window.refresh_pending, "repair refresh starts immediately");
        assert!(window.thumbnail_pass_pending, "pass armed regardless");
        assert_eq!(window.peer_refresh_request, None, "no peer to settle");
        assert!(!window.metadata_repair_due, "consumed by the arming");
        assert_eq!(task.units(), 3, "timer, thumbnail pass and repair refresh");

        // An empty rebuild (an empty folder) has nothing to repair: cold
        // start semantics stand and no refresh is started out of turn.
        let mut empty = Window {
            metadata_repair_due: true,
            ..Window::default()
        };
        let task = empty.arm_initial_duties(wallpaper::CurrentWallpaper::NoFile);
        assert!(!empty.refresh_pending);
        assert!(empty.thumbnail_pass_pending);
        assert_eq!(task.units(), 2, "timer plus thumbnail pass");

        // A loaded catalogue needs no repair either.
        let mut loaded = window_with_images(1);
        let task = loaded.arm_initial_duties(wallpaper::CurrentWallpaper::NoFile);
        assert!(!loaded.refresh_pending);
        assert_eq!(task.units(), 2);
    }

    #[test]
    fn a_repair_refresh_coalesces_with_an_outstanding_peer_request() {
        // One refresh in flight, settling the peer request and repairing
        // the rebuilt entries alike.
        let mut window = Window {
            metadata_repair_due: true,
            coordination: CoordinationConfig {
                refresh_request: 3,
                ..Default::default()
            },
            ..window_with_images(2)
        };

        let task = window.arm_initial_duties(wallpaper::CurrentWallpaper::NoFile);

        assert!(window.refresh_pending);
        assert_eq!(window.peer_refresh_request, Some(3));
        assert!(window.thumbnail_pass_pending);
        assert_eq!(task.units(), 3, "exactly one refresh beside the pass");
    }

    #[test]
    fn a_takeover_over_a_rebuilt_catalogue_starts_the_repair_refresh() {
        // Provenance is carried through the takeover snapshot: the follower
        // period may have already asked for (and been refused) a repair,
        // but the catalogue this leader serves from is the one just read.
        let rebuilt = LeadershipHydration {
            catalogue: Catalogue {
                images: vec![entry_in_memory("20260820", "Rebuilt_ROW1")],
            },
            provenance: Provenance::Rebuilt,
            ..hydration(
                AppletConfig::default(),
                CoordinationConfig::default(),
                wallpaper::CurrentWallpaper::NoFile,
            )
        };
        let mut window = Window {
            leadership: Leadership::forced(true),
            leader_readiness: LeaderReadiness::Hydrating,
            ..Window::default()
        };
        let duties = window.finish_leadership_hydration(0, 0, Ok(rebuilt.clone()));
        assert!(window.is_active_leader());
        assert!(window.refresh_pending, "repair refresh starts at readiness");
        assert!(
            window.thumbnail_pass_pending,
            "beside the pass, never instead"
        );
        assert!(!window.metadata_repair_due, "consumed by the arming");
        assert_eq!(window.peer_refresh_request, None);
        assert_eq!(
            duties.units(),
            3,
            "timer, thumbnail pass and repair refresh"
        );

        // The same snapshot loaded from JSON needs no repair.
        let loaded = LeadershipHydration {
            provenance: Provenance::Loaded,
            ..rebuilt.clone()
        };
        let mut window = Window {
            leadership: Leadership::forced(true),
            leader_readiness: LeaderReadiness::Hydrating,
            ..Window::default()
        };
        let duties = window.finish_leadership_hydration(0, 0, Ok(loaded));
        assert!(window.is_active_leader());
        assert!(!window.refresh_pending);
        assert_eq!(duties.units(), 2, "timer plus thumbnail pass");

        // A follower's own repair request that was still waiting for a
        // leader when it took over: the mailbox still shows it outstanding,
        // and the one repair refresh covers it too.
        let mut window = Window {
            leadership: Leadership::forced(true),
            leader_readiness: LeaderReadiness::Hydrating,
            requested_peer_refresh: Some(2),
            refresh_pending: true,
            ..Window::default()
        };
        let hydration = LeadershipHydration {
            coordination: CoordinationConfig {
                refresh_request: 2,
                ..Default::default()
            },
            ..rebuilt
        };
        let duties = window.finish_leadership_hydration(0, 0, Ok(hydration));
        assert!(window.refresh_pending);
        assert_eq!(window.requested_peer_refresh, None, "follower state shed");
        assert_eq!(
            window.peer_refresh_request,
            Some(2),
            "covered by the repair"
        );
        assert_eq!(duties.units(), 3, "exactly one refresh");
    }

    #[tokio::test]
    async fn a_real_takeover_snapshot_reports_a_missing_catalogue_as_rebuilt() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        let dir = tempfile::tempdir().unwrap();
        let (config_context, coordination_context) = takeover_contexts(&dir.path().join("config"));
        AppletConfig::default()
            .write_entry(&config_context)
            .unwrap();
        CoordinationConfig::default()
            .write_entry(&coordination_context)
            .unwrap();
        let images_dir = dir.path().join("images");
        std::fs::create_dir_all(&images_dir).unwrap();
        let entry = entry_on_disk(&images_dir, "20260820", "Pack_ROW1");
        // No catalogue.json at all: the snapshot rebuilds from the folder.
        let mut window = Window {
            leadership: Leadership::forced(true),
            leader_readiness: LeaderReadiness::Hydrating,
            config_context: Some(config_context),
            coordination_context: Some(coordination_context),
            test_snapshot_inputs: Some(TestSnapshotInputs {
                catalogue_path: dir.path().join("state/catalogue.json"),
                images_dir,
                live: wallpaper::CurrentWallpaper::NoFile,
            }),
            ..Window::default()
        };

        let hydration_task = window.start_leadership_hydration();
        let mut messages = app_messages(hydration_task).await;
        assert_eq!(messages.len(), 1);
        let Message::LeadershipHydrated { result, .. } = &messages[0] else {
            panic!("expected a hydration completion");
        };
        assert_eq!(
            result.as_ref().unwrap().provenance,
            Provenance::Rebuilt,
            "provenance travels with the snapshot"
        );
        drop(window.update(messages.pop().unwrap()));
        assert!(window.is_active_leader());
        assert_eq!(window.catalogue.images.len(), 1);
        assert_eq!(window.catalogue.images[0].filename, entry.filename);
        assert!(window.refresh_pending, "rebuilt takeover repairs at once");
        assert!(window.thumbnail_pass_pending);
    }

    fn takeover_contexts(root: &Path) -> (cosmic_config::Config, cosmic_config::Config) {
        let config = cosmic_config::Config::with_custom_path(
            APP_ID,
            AppletConfig::VERSION,
            root.to_path_buf(),
        )
        .unwrap();
        (config.clone(), config)
    }

    fn hydration(
        config: AppletConfig,
        coordination: CoordinationConfig,
        live: wallpaper::CurrentWallpaper,
    ) -> LeadershipHydration {
        LeadershipHydration {
            config_context: None,
            coordination_context: None,
            config,
            coordination,
            catalogue: Catalogue::default(),
            provenance: Provenance::Loaded,
            live,
        }
    }

    #[tokio::test]
    async fn real_lock_loser_takes_over_once_and_stays_inert_until_hydrated() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        let dir = tempfile::tempdir().unwrap();
        let winner = Leadership::acquire(dir.path());
        let loser = Leadership::acquire(dir.path());
        assert!(winner.is_leader());
        assert!(!loser.is_leader());
        let (config_context, coordination_context) = takeover_contexts(&dir.path().join("config"));
        let disk_config = AppletConfig {
            retention_days: 30,
            ..Default::default()
        };
        disk_config.write_entry(&config_context).unwrap();
        let disk_coordination = CoordinationConfig {
            refresh_request: 4,
            refresh_completion: PeerRefreshCompletion {
                request: 4,
                outcome: PeerRefreshOutcome::Success,
            },
            ..Default::default()
        };
        disk_coordination
            .write_entry(&coordination_context)
            .unwrap();
        let images_dir = dir.path().join("images");
        std::fs::create_dir_all(&images_dir).unwrap();
        let entry = entry_on_disk(&images_dir, "20260820", "Takeover_ROW1");
        let catalogue_path = dir.path().join("state/catalogue.json");
        std::fs::create_dir_all(catalogue_path.parent().unwrap()).unwrap();
        let mut disk_catalogue = Catalogue::default();
        disk_catalogue.images.push(entry.clone());
        disk_catalogue.save(&catalogue_path).unwrap();
        let live_path = entry.filename.clone();
        let mut window = Window {
            leadership: loser,
            leadership_generation: 1,
            config_context: Some(config_context),
            coordination_context: Some(coordination_context),
            cold_start: ColdStart::Pending,
            test_snapshot_inputs: Some(TestSnapshotInputs {
                catalogue_path,
                images_dir,
                live: wallpaper::CurrentWallpaper::File(live_path.clone()),
            }),
            ..Window::default()
        };

        let blocked = window.update(Message::LeadershipTick(1));
        assert!(!window.leadership.is_leader());
        assert_eq!(window.leadership_generation, 2, "lockout rearms once");
        assert_eq!(blocked.units(), 1);
        assert_eq!(
            window.update(Message::LeadershipTick(1)).units(),
            0,
            "stale retry is dropped"
        );

        drop(winner);
        let hydration_task = window.update(Message::LeadershipTick(2));
        assert!(window.leadership.is_leader());
        assert_eq!(window.leader_readiness, LeaderReadiness::Hydrating);
        assert!(!window.is_active_leader());
        assert_eq!(hydration_task.units(), 1, "one blocking snapshot is armed");
        assert!(
            !window.leadership.try_acquire(),
            "acquisition edge fires once"
        );
        assert_eq!(
            window.update(Message::LeadershipTick(2)).units(),
            0,
            "the consumed takeover tick cannot arm another snapshot"
        );

        let mut outputs = app_messages(hydration_task).await;
        assert_eq!(outputs.len(), 1);
        drop(window.update(outputs.pop().unwrap()));
        assert!(window.is_active_leader());
        assert_eq!(window.config, disk_config);
        assert_eq!(window.coordination, disk_coordination);
        assert_eq!(window.catalogue, disk_catalogue);
        assert_eq!(window.current, Some(live_path));
        assert_eq!(window.cold_start, ColdStart::Done);
        assert_eq!(window.timer_generation, 1);
        assert!(
            !window.refresh_pending,
            "a catalogue loaded from JSON asks for no repair refresh"
        );
    }

    #[tokio::test]
    async fn takeover_reopens_missing_contexts_and_recovers_on_retry() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        let dir = tempfile::tempdir().unwrap();
        let (config_context, coordination_context) =
            takeover_contexts(&dir.path().join("recovered-config"));
        let disk_config = AppletConfig {
            retention_days: 30,
            ..Default::default()
        };
        disk_config.write_entry(&config_context).unwrap();
        let disk_coordination = CoordinationConfig {
            refresh_request: 6,
            refresh_completion: PeerRefreshCompletion {
                request: 6,
                outcome: PeerRefreshOutcome::Success,
            },
            ..Default::default()
        };
        disk_coordination
            .write_entry(&coordination_context)
            .unwrap();
        let images_dir = dir.path().join("images");
        std::fs::create_dir_all(&images_dir).unwrap();
        let catalogue_path = dir.path().join("catalogue.json");

        let mut window = Window {
            leadership: Leadership::forced(true),
            leader_readiness: LeaderReadiness::Hydrating,
            config_context: None,
            coordination_context: None,
            test_hydration_contexts: Some((config_context, coordination_context)),
            test_snapshot_inputs: Some(TestSnapshotInputs {
                catalogue_path,
                images_dir,
                live: wallpaper::CurrentWallpaper::NoFile,
            }),
            ..Window::default()
        };

        let retry = window.finish_leadership_hydration(
            0,
            0,
            Err("transient context creation failure".into()),
        );
        assert_eq!(retry.units(), 1);
        assert!(!window.is_active_leader());
        let hydration_task = window.update(Message::LeadershipTick(1));
        let mut messages = app_messages(hydration_task).await;
        assert_eq!(messages.len(), 1);
        let duties = window.update(messages.pop().unwrap());
        assert!(window.is_active_leader());
        assert!(window.config_context.is_some());
        assert!(window.coordination_context.is_some());
        assert_eq!(window.config.retention_days, 30);
        assert_eq!(window.coordination.refresh_request, 6);
        assert!(duties.units() >= 2, "recovered leader arms its duties");
    }

    #[test]
    fn takeover_adopts_full_disk_state_and_recovers_outstanding_request_once() {
        use cosmic::Application as _;

        let snapshot = accent::AccentSnapshot {
            light: Some([1, 2, 3]),
            dark: None,
        };
        let written = accent::AccentPair {
            light: [4, 5, 6],
            dark: [7, 8, 9],
        };
        let config = AppletConfig {
            shuffle_enabled: true,
            shuffle_interval_secs: 1_800,
            retention_days: 30,
            accent_enabled: true,
            accent_snapshot: Some(snapshot),
            accent_last_written: Some(written),
        };
        let coordination = CoordinationConfig {
            refresh_request: 9,
            refresh_completion: PeerRefreshCompletion {
                request: 7,
                outcome: PeerRefreshOutcome::Success,
            },
            apply_notice: Some(PeerApplyNotice {
                generation: 11,
                path: PathBuf::from("/mailbox-evidence.jpg"),
            }),
        };
        let live_path = PathBuf::from("/currently-applied.jpg");
        let mut window = Window {
            leadership: Leadership::forced(true),
            leader_readiness: LeaderReadiness::Hydrating,
            leadership_hydration_generation: 3,
            leadership_state_generation: 5,
            config: AppletConfig::default(),
            coordination: CoordinationConfig::default(),
            current: Some(PathBuf::from("/stale.jpg")),
            requested_peer_refresh: Some(4),
            peer_refresh_write_pending: true,
            refresh_pending: true,
            peer_refresh_timeout_generation: 6,
            cold_start: ColdStart::Pending,
            ..Window::default()
        };

        let mut hydrated = hydration(
            config.clone(),
            coordination.clone(),
            wallpaper::CurrentWallpaper::File(live_path.clone()),
        );
        hydrated
            .catalogue
            .images
            .push(entry_in_memory("20260820", "Hydrated_ROW1"));

        let duties = window.finish_leadership_hydration(3, 5, Ok(hydrated));

        assert!(window.is_active_leader());
        assert_eq!(window.config, config, "the complete accent trio is adopted");
        assert_eq!(window.coordination, coordination);
        assert_eq!(window.current, Some(live_path));
        assert_eq!(window.catalogue.images.len(), 1);
        assert_eq!(window.requested_peer_refresh, None);
        assert!(!window.peer_refresh_write_pending);
        assert_eq!(window.cold_start, ColdStart::Done);
        assert!(window.peer_refresh_timeout_generation > 6);
        assert_eq!(window.peer_apply_notice_generation, 11);
        assert_eq!(window.peer_refresh_request, Some(9));
        assert!(
            window.refresh_pending,
            "the outstanding request is serviced"
        );
        assert!(
            window.thumbnail_pass_pending,
            "the pass is armed beside the refresh"
        );
        assert_eq!(window.timer_generation, 1, "refresh duty armed once");
        assert_eq!(window.shuffle_generation, 1, "shuffle duty considered once");
        assert_eq!(duties.units(), 3, "timer, thumbnail pass and peer refresh");

        let duplicate = window.finish_leadership_hydration(
            3,
            5,
            Ok(hydration(
                AppletConfig::default(),
                CoordinationConfig::default(),
                wallpaper::CurrentWallpaper::NoFile,
            )),
        );
        assert_eq!(duplicate.units(), 0);
        assert_eq!(window.timer_generation, 1);
        assert_eq!(window.peer_refresh_request, Some(9));
        assert_eq!(
            window.update(Message::LeadershipTick(0)).units(),
            0,
            "an active leader drops takeover ticks"
        );
    }

    #[test]
    fn takeover_watcher_events_dirty_snapshot_and_force_one_fresh_read() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let (config_context, coordination_context) = takeover_contexts(&dir.path().join("config"));
        let original_config = AppletConfig::default();
        let original_coordination = CoordinationConfig::default();
        let mut window = Window {
            leadership: Leadership::forced(true),
            leader_readiness: LeaderReadiness::Hydrating,
            leadership_hydration_generation: 4,
            leadership_state_generation: 8,
            config: original_config.clone(),
            coordination: original_coordination.clone(),
            config_context: Some(config_context),
            coordination_context: Some(coordination_context),
            ..Window::default()
        };

        let mut changed_config = original_config.clone();
        changed_config.accent_enabled = true;
        let changed_coordination = CoordinationConfig {
            refresh_request: 12,
            ..Default::default()
        };
        assert_eq!(
            window
                .update(Message::ConfigUpdated(changed_config))
                .units(),
            0
        );
        assert_eq!(
            window
                .update(Message::CoordinationUpdated(changed_coordination))
                .units(),
            0
        );
        assert_eq!(window.leadership_state_generation, 10);
        assert_eq!(
            window.config, original_config,
            "watcher payload is not adopted"
        );
        assert_eq!(window.coordination, original_coordination);

        let reread = window.finish_leadership_hydration(
            4,
            8,
            Ok(hydration(
                AppletConfig::default(),
                CoordinationConfig::default(),
                wallpaper::CurrentWallpaper::NoFile,
            )),
        );
        assert_eq!(reread.units(), 1);
        assert_eq!(window.leader_readiness, LeaderReadiness::Hydrating);
        assert_eq!(window.leadership_hydration_generation, 5);
        assert_eq!(window.timer_generation, 0, "dirty state cannot arm duties");
    }

    #[test]
    fn stale_failed_and_contextless_takeovers_cannot_arm_from_stale_state() {
        let mut stale = Window {
            leadership: Leadership::forced(true),
            leader_readiness: LeaderReadiness::Hydrating,
            leadership_hydration_generation: 7,
            leadership_state_generation: 2,
            ..Window::default()
        };
        assert_eq!(
            stale
                .finish_leadership_hydration(
                    6,
                    2,
                    Ok(hydration(
                        AppletConfig::default(),
                        CoordinationConfig::default(),
                        wallpaper::CurrentWallpaper::NoFile,
                    )),
                )
                .units(),
            0
        );
        assert_eq!(stale.timer_generation, 0);
        assert_eq!(
            stale
                .finish_leadership_hydration(7, 2, Err("read failed".into()))
                .units(),
            1,
            "a failed blocking read retries later"
        );
        assert_eq!(stale.timer_generation, 0);
        assert_eq!(stale.leader_readiness, LeaderReadiness::Hydrating);

        let mut contextless = Window {
            leadership: Leadership::forced(true),
            leader_readiness: LeaderReadiness::Hydrating,
            leadership_generation: 3,
            config_context: None,
            coordination_context: None,
            ..Window::default()
        };
        assert_eq!(contextless.on_leadership_tick(3).units(), 1);
        assert_eq!(contextless.leadership_generation, 4);
        assert_eq!(contextless.timer_generation, 0);
        assert!(!contextless.is_active_leader());
        assert_eq!(
            contextless.on_leadership_tick(3).units(),
            0,
            "a stale retry cannot bypass missing contexts"
        );
    }

    #[test]
    fn app_id_is_reverse_dns() {
        assert_eq!(APP_ID, "io.github.ercling.cosmic-applet-daymural");
        assert_eq!(<Window as cosmic::Application>::APP_ID, APP_ID);
        assert!(APP_ID.split('.').count() >= 3);
    }

    #[test]
    fn cargo_identity_is_daymural() {
        const PROJECT_URL: &str = "https://github.com/ercling/cosmic-applet-daymural";

        assert_eq!(env!("CARGO_PKG_NAME"), "daymural");
        assert_eq!(env!("CARGO_PKG_REPOSITORY"), PROJECT_URL);
        assert_eq!(env!("CARGO_PKG_HOMEPAGE"), PROJECT_URL);
        assert_eq!(
            env!("CARGO_PKG_DESCRIPTION"),
            "Daymural: daily Microsoft Bing wallpaper applet for the COSMIC desktop"
        );
    }

    #[test]
    fn panel_icon_is_symbolic() {
        assert!(PANEL_ICON.ends_with("-symbolic"));
    }

    #[test]
    fn state_paths_live_under_the_app_id() {
        // No self-skip: `state_dir` falls back to the temp dir when there is
        // no home, so it is absolute in every environment.
        let state = state_dir();
        assert!(state.ends_with(APP_ID));
        assert!(state.is_absolute());
        assert_eq!(state_dir().join("catalogue.json"), catalogue_path());
    }

    #[test]
    fn refresh_error_classifies_local_versus_network() {
        let disk: RefreshError = bing::FetchError::from(std::io::Error::other("disk full")).into();
        assert!(matches!(disk, RefreshError::Disk(_)));
        let net: RefreshError = bing::FetchError::EmptyList.into();
        assert!(matches!(net, RefreshError::Network(_)));
    }

    #[test]
    fn refresh_success_plan_with_images_applies_and_reschedules() {
        let now = Utc::now();
        // Cold start: auto-apply regardless of the live wallpaper, spend
        // the cold-start flag, reschedule off the newest fullstartdate.
        let plan = refresh_success_plan(
            true,
            Some(std::path::Path::new("/usr/share/backgrounds/x.jpg")),
            true,
            true,
            "202608070700",
            now,
        );
        assert!(plan.auto_apply);
        assert_eq!(
            plan.delay,
            schedule::next_refresh(Some("202608070700"), now)
        );

        // Warm, foreign wallpaper: never clobber.
        let plan = refresh_success_plan(
            false,
            Some(std::path::Path::new("/usr/share/backgrounds/x.jpg")),
            true,
            true,
            "202608070700",
            now,
        );
        assert!(!plan.auto_apply);
    }

    #[test]
    fn an_undelivered_refresh_never_moves_a_warm_wallpaper_of_ours() {
        // All eight images restricted: nothing fetched, and the live
        // wallpaper is one of ours but not the newest (the user navigated
        // back). The no-op must not pull them to `newest()` — re-applying
        // is reserved for a response that delivered something (the daily
        // rule) and for cold start, where a restored catalogue is the only
        // wallpaper on offer.
        let now = Utc::now();
        let ours = wallpaper::download_dir().join("20260806-Old_ROW1_UHD.jpg");
        let ours = ours.as_path();
        assert!(
            wallpaper::is_ours(ours),
            "premise: the live wallpaper is ours"
        );

        let plan = refresh_success_plan(false, Some(ours), true, false, "202608070700", now);
        assert!(!plan.auto_apply, "warm + ours + nothing delivered: no-op");
        assert_eq!(
            plan.delay,
            schedule::next_refresh(Some("202608070700"), now)
        );

        let plan = refresh_success_plan(false, Some(ours), true, true, "202608070700", now);
        assert!(plan.auto_apply, "a delivered response applies as before");

        let plan = refresh_success_plan(true, Some(ours), true, false, "202608070700", now);
        assert!(
            plan.auto_apply,
            "cold start still applies the restored catalogue"
        );
    }

    #[test]
    fn refresh_success_plan_without_images_backs_off() {
        // A "successful" fetch that still leaves no images: no 5 s
        // cold-start delay (that would tight-loop against Bing — the
        // response's anchor schedules instead) and no auto-apply. `finish_refresh` also keeps the cold-start flag armed
        // for the fetch that finally delivers (it only spends the flag on a
        // successful apply — see `any_successful_apply_spends_the_cold_start_flag`).
        let now = Utc::now();
        let plan = refresh_success_plan(true, None, false, false, "202608070700", now);
        assert!(!plan.auto_apply);
        assert_eq!(
            plan.delay,
            schedule::next_refresh(Some("202608070700"), now),
            "the response anchor still schedules the next refresh"
        );
    }

    #[test]
    fn any_successful_apply_spends_the_cold_start_flag() {
        // The cold start stays armed after the first fetch's auto-apply
        // *failed*; the user then navigates (or a
        // shuffle tick fires) and an apply succeeds. That apply fulfills
        // the cold-start purpose — the flag must be spent, or the next
        // refresh's unconditional cold-start branch would clobber a
        // wallpaper the user picked in COSMIC Settings in between.
        let mut window = Window {
            cold_start: ColdStart::Pending,
            ..Window::default()
        };

        drop(window.on_apply_success(PathBuf::from("/imgs/20260807-Foo_ROW1_UHD.jpg")));

        assert_eq!(
            window.current.as_deref(),
            Some(Path::new("/imgs/20260807-Foo_ROW1_UHD.jpg"))
        );
        assert_eq!(window.cold_start, ColdStart::Done);

        // With the flag spent, a later refresh over a foreign (user-picked)
        // wallpaper no longer auto-applies.
        let user_choice = Path::new("/usr/share/backgrounds/user-choice.jpg");
        let plan = refresh_success_plan(
            window.cold_start.applies_over(Some(user_choice)),
            Some(user_choice),
            true,
            true,
            "202608070700",
            Utc::now(),
        );
        assert!(!plan.auto_apply);
    }

    #[test]
    fn a_refresh_syncs_the_applied_wallpaper_before_handing_it_to_the_backfill() {
        // `self.current` only ever records the applet's *own* applies, so it
        // goes stale the moment the user picks another Bing image in COSMIC
        // Settings. The prune that follows this refresh reads the live state
        // and protects that file from age deletion; the backfill is handed
        // the same file so it protects its thumbnail — from the stale field
        // it would instead skip the one out-of-window entry guaranteed to
        // survive, leaving it on the placeholder until some later refresh.
        let live = PathBuf::from("/imgs/20250101-Picked_ROW0_UHD.jpg");
        let mut window = Window {
            current: Some(PathBuf::from("/imgs/20260807-Ours_ROW1_UHD.jpg")),
            ..Window::default()
        };

        drop(window.start_refresh_over(wallpaper::CurrentWallpaper::File(live.clone())));

        assert_eq!(window.current.as_deref(), Some(live.as_path()));

        // And the other direction: cosmic-bg displaying no file at all
        // clears the stale path rather than presenting it as applied.
        window.refresh_pending = false;
        drop(window.start_refresh_over(wallpaper::CurrentWallpaper::NoFile));
        assert_eq!(window.current, None);
    }

    #[test]
    fn cold_start_retry_never_clobbers_a_wallpaper_picked_after_the_failure() {
        // The cold-start auto-apply failed while the system default was
        // displayed; before the retry the user picks a
        // different (foreign) wallpaper in COSMIC Settings. The retry must
        // not fire — the user's choice wins.
        let default_bg = Path::new("/usr/share/backgrounds/cosmic/default.jpg");
        let user_choice = Path::new("/usr/share/backgrounds/user-choice.jpg");
        let retry = ColdStart::RetryOver(Some(default_bg.to_path_buf()));

        // Display unchanged since the failure: the retry still fires (a
        // fresh install with a transient failure must not end up
        // wallpaper-less).
        assert!(retry.applies_over(Some(default_bg)));
        // The user picked something else meanwhile: the retry yields.
        assert!(!retry.applies_over(Some(user_choice)));
        // Nothing knowable displayed (color source / per-output mode):
        // safe ground, same as the unconditional Pending branch.
        assert!(retry.applies_over(None));

        // Failure happened over an unknowable display: a foreign file
        // picked afterwards still wins.
        let retry_over_none = ColdStart::RetryOver(None);
        assert!(retry_over_none.applies_over(None));
        assert!(!retry_over_none.applies_over(Some(user_choice)));

        // The plain states bracket the retry: Pending applies over
        // anything, Done over nothing.
        assert!(ColdStart::Pending.applies_over(Some(user_choice)));
        assert!(!ColdStart::Done.applies_over(None));

        // End to end through the plan: a suppressed retry over the user's
        // pick neither auto-applies nor blocks the flag from being spent.
        let plan = refresh_success_plan(
            retry.applies_over(Some(user_choice)),
            Some(user_choice),
            true,
            true,
            "202608070700",
            Utc::now(),
        );
        assert!(!plan.auto_apply);
    }

    /// A catalogue entry whose file really exists under `dir`.
    fn entry_on_disk(dir: &Path, startdate: &str, name: &str) -> ImageEntry {
        let filename = dir.join(format!("{startdate}-{name}_UHD.jpg"));
        std::fs::write(&filename, b"jpeg bytes").unwrap();
        ImageEntry {
            urlbase: format!("/th?id=OHR.{name}"),
            startdate: startdate.to_owned(),
            fullstartdate: format!("{startdate}0700"),
            title: format!("Title {name}"),
            copyright: "© Someone".to_owned(),
            copyrightlink: "https://example.com".to_owned(),
            filename,
        }
    }

    #[test]
    fn non_leader_restore_is_read_only_for_missing_corrupt_and_stale_catalogues() {
        for case in ["missing", "corrupt", "stale"] {
            let dir = tempfile::tempdir().unwrap();
            let images = dir.path().join("images");
            let state = dir.path().join("state");
            std::fs::create_dir_all(&images).unwrap();
            std::fs::create_dir_all(&state).unwrap();
            let image = entry_on_disk(&images, "20260807", "Kept_ROW1");
            let catalogue_path = state.join(catalogue::CATALOGUE_FILENAME);

            match case {
                "missing" => {}
                "corrupt" => std::fs::write(&catalogue_path, b"not json").unwrap(),
                "stale" => {
                    let stale = entry_on_disk(&images, "20260101", "Gone_ROW2");
                    Catalogue {
                        images: vec![stale.clone(), image.clone()],
                    }
                    .save(&catalogue_path)
                    .unwrap();
                    std::fs::remove_file(stale.filename).unwrap();
                }
                _ => unreachable!(),
            }

            // Cache debris makes an accidental reconciliation visible in all
            // three cases, including missing/corrupt catalogue rebuilds.
            let cache = state.join("thumbs");
            std::fs::create_dir_all(&cache).unwrap();
            std::fs::write(cache.join("orphan.jpg"), b"keep").unwrap();
            let before = file_snapshot(dir.path());

            let restored =
                restore_catalogue_for_role(&catalogue_path, &images, &state, false).catalogue;

            assert!(
                restored
                    .images
                    .iter()
                    .any(|entry| entry.filename == image.filename),
                "{case}: read-only restoration still produces a useful view"
            );
            assert_eq!(
                file_snapshot(dir.path()),
                before,
                "{case}: follower restoration must not save, prune, or reconcile"
            );
        }
    }

    #[test]
    fn restore_catalogue_drops_entries_whose_files_vanished() {
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&images).unwrap();
        let kept = entry_on_disk(&images, "20260807", "Kept_ROW1");
        let gone = entry_on_disk(&images, "20260101", "Gone_ROW2");
        let cat_path = state.join(catalogue::CATALOGUE_FILENAME);
        Catalogue {
            images: vec![gone.clone(), kept.clone()],
        }
        .save(&cat_path)
        .unwrap();
        // A cached thumbnail for the file about to vanish must go too.
        let orphan_thumb = thumbs::thumbnail_path(&gone.filename, &state).unwrap();
        std::fs::create_dir_all(orphan_thumb.parent().unwrap()).unwrap();
        std::fs::write(&orphan_thumb, b"thumb").unwrap();
        std::fs::remove_file(&gone.filename).unwrap();

        let restored = restore_catalogue(&cat_path, &images, &state).catalogue;

        // Old-but-vanished entry dropped without deleting anything else —
        // no age-based pruning happens at startup (retention 0).
        assert_eq!(restored.images, vec![kept.clone()]);
        assert!(kept.filename.is_file());
        assert!(!orphan_thumb.exists());
        // The sweep is persisted, so a restart doesn't resurrect the entry.
        assert_eq!(Catalogue::load(&cat_path).unwrap(), restored);
    }

    #[test]
    fn restore_catalogue_sweeps_cache_leftovers_no_prune_can_name() {
        // The prune only ever walks the entries it still holds, so anything
        // written for a path the catalogue lost — a backfill that finished
        // after the prune that dropped its entry, or every entry a rebuild
        // from the folder scan silently forgot — is orphaned for good unless
        // startup sweeps the directory itself.
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&images).unwrap();
        let kept = entry_on_disk(&images, "20260807", "Kept_ROW1");
        let cat_path = state.join(catalogue::CATALOGUE_FILENAME);
        Catalogue {
            images: vec![kept.clone()],
        }
        .save(&cat_path)
        .unwrap();

        let live_thumb = thumbs::thumbnail_path(&kept.filename, &state).unwrap();
        let thumbs_dir = live_thumb.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&thumbs_dir).unwrap();
        std::fs::write(&live_thumb, b"thumb").unwrap();
        std::fs::write(
            thumbs_dir.join("20260807-Kept_ROW1_UHD.jpg.meta"),
            b"ok 1 2",
        )
        .unwrap();
        for orphan in [
            "20250101-Forgotten_ROW9_UHD.jpg",
            "20250101-Forgotten_ROW9_UHD.jpg.meta",
            "20260807-Kept_ROW1_UHD.jpg.part",
        ] {
            std::fs::write(thumbs_dir.join(orphan), b"leftover").unwrap();
        }

        let restored = restore_catalogue(&cat_path, &images, &state).catalogue;

        assert_eq!(restored.images, vec![kept]);
        let survivors: Vec<_> = std::fs::read_dir(&thumbs_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            survivors.len(),
            2,
            "only the live slot and its sidecar survive: {survivors:?}"
        );
        assert!(live_thumb.is_file());
    }

    #[test]
    fn restore_catalogue_of_valid_json_pointing_at_nothing_is_empty() {
        // The cold-start case of a valid catalogue JSON whose files are
        // all gone. Startup must see an *empty* catalogue (so the cold
        // start arms and no UI action trusts dead paths).
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&images).unwrap();
        let gone = entry_on_disk(&images, "20260807", "Gone_ROW1");
        let cat_path = state.join(catalogue::CATALOGUE_FILENAME);
        Catalogue {
            images: vec![gone.clone()],
        }
        .save(&cat_path)
        .unwrap();
        std::fs::remove_file(&gone.filename).unwrap();

        let restored = restore_catalogue(&cat_path, &images, &state).catalogue;

        assert!(restored.images.is_empty());
    }

    #[test]
    fn restore_catalogue_survives_a_download_folder_that_is_not_there() {
        // The folder can be missing for reasons that have nothing to do with
        // its contents: an unmounted or slow-mounting drive, a user rename, a
        // symlink target not yet present. Every entry then looks vanished —
        // and the startup sweep *persists* what it concludes, so treating
        // that as evidence destroys the whole history permanently (a
        // valid-but-empty catalogue loads fine and never rescans).
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&images).unwrap();
        let older = entry_on_disk(&images, "20260806", "One_ROW1");
        let newer = entry_on_disk(&images, "20260807", "Two_ROW2");
        let cat_path = state.join(catalogue::CATALOGUE_FILENAME);
        let stored = Catalogue {
            images: vec![older, newer.clone()],
        };
        stored.save(&cat_path).unwrap();
        let thumb = thumbs::thumbnail_path(&newer.filename, &state).unwrap();
        std::fs::create_dir_all(thumb.parent().unwrap()).unwrap();
        std::fs::write(&thumb, b"thumb").unwrap();

        std::fs::rename(&images, dir.path().join("moved")).unwrap();
        let restored = restore_catalogue(&cat_path, &images, &state).catalogue;

        assert_eq!(restored, stored, "an absent folder is not a hollow one");
        assert_eq!(
            Catalogue::load(&cat_path).unwrap(),
            stored,
            "nothing empty may be persisted over the history"
        );
        assert!(thumb.is_file(), "and no preview is thrown away either");

        // Recovery for a catalogue an older build already emptied this way:
        // with the folder back, an empty catalogue rescans it.
        std::fs::rename(dir.path().join("moved"), &images).unwrap();
        Catalogue::default().save(&cat_path).unwrap();

        let restored = restore_catalogue(&cat_path, &images, &state).catalogue;

        assert_eq!(restored.images.len(), 2, "the folder is rescanned");
    }

    #[test]
    fn restore_catalogue_scrubs_tampered_entries_without_deleting_files() {
        // A hand-edited catalogue.json pointing at a user file must never
        // get that file deleted — startup drops the entry and persists
        // the scrub, leaving the file alone.
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&images).unwrap();
        let victim = dir.path().join("important.pdf");
        std::fs::write(&victim, b"precious").unwrap();
        let kept = entry_on_disk(&images, "20260807", "Kept_ROW1");
        let mut hostile = kept.clone();
        hostile.urlbase = "/th?id=OHR.Evil_ROW2".to_owned();
        hostile.fullstartdate = "202001010700".to_owned();
        hostile.filename = victim.clone();
        let cat_path = state.join(catalogue::CATALOGUE_FILENAME);
        Catalogue {
            images: vec![hostile, kept.clone()],
        }
        .save(&cat_path)
        .unwrap();

        let restored = restore_catalogue(&cat_path, &images, &state).catalogue;

        assert!(victim.is_file(), "tampered entry must not delete the file");
        assert_eq!(restored.images, vec![kept]);
        assert_eq!(Catalogue::load(&cat_path).unwrap(), restored);
    }

    /// Download everything eligible within the newest `horizon` positions,
    /// falling back beyond it when nothing there is eligible.
    fn within(horizon: u8) -> Downloads {
        Downloads {
            horizon,
            fallback: true,
        }
    }

    /// One-image HPImageArchive response for the pipeline tests below.
    const LIST_JSON: &str = r#"{"images":[{"urlbase":"/th?id=OHR.Foo_ROW1","startdate":"20260807","fullstartdate":"202608070700","copyright":"Foo place (© Bar)","copyrightlink":"https://example.com/foo","wp":true}]}"#;

    #[tokio::test]
    async fn pipeline_downloads_missing_images_and_thumbnails() {
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        // A stale .part from a crashed download must be swept.
        std::fs::create_dir_all(&download_dir).unwrap();
        let stale_part = download_dir.join("20260101-Old_ROW0_UHD.jpg.part");
        std::fs::write(&stale_part, b"torn").unwrap();

        let jpeg = crate::testutil::tiny_jpeg(64, 36);
        let expected = jpeg.clone();
        let base = crate::testutil::spawn_mock(move |path| {
            if path.starts_with("/HPImageArchive.aspx") {
                (200, LIST_JSON.as_bytes().to_vec())
            } else if path.starts_with("/th?id=OHR.") {
                (200, jpeg.clone())
            } else {
                (404, Vec::new())
            }
        });

        let client = bing::http_client().unwrap();
        let fetched = fetch_and_download(
            &client,
            &base,
            &Catalogue::default(),
            within(1),
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .unwrap()
        .fetched;

        assert_eq!(fetched.len(), 1);
        let path = &fetched[0].filename;
        assert_eq!(path, &download_dir.join("20260807-Foo_ROW1_UHD.jpg"));
        assert_eq!(std::fs::read(path).unwrap(), expected);
        assert_eq!(fetched[0].title, "Foo place");
        assert!(thumbs::thumbnail_path(path, &state).unwrap().is_file());
        assert!(!stale_part.exists(), "orphaned .part must be swept");
    }

    #[tokio::test]
    async fn pipeline_never_downloads_when_the_catalogue_holds_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&download_dir).unwrap();
        // The image already on disk at a *different* resolution suffix
        // (folder written by the reference GNOME extension) — only a
        // catalogue lookup finds it; a bare UHD-path existence check would
        // re-download.
        let existing = download_dir.join("20260807-Foo_ROW1_1920x1080.jpg");
        std::fs::write(&existing, crate::testutil::tiny_jpeg(64, 36)).unwrap();
        let catalogue = Catalogue::rebuild_from_folder(&download_dir);

        // Any download attempt gets a 500 and would fail the pipeline.
        let base = crate::testutil::spawn_mock(|path| {
            if path.starts_with("/HPImageArchive.aspx") {
                (200, LIST_JSON.as_bytes().to_vec())
            } else {
                (500, Vec::new())
            }
        });

        let client = bing::http_client().unwrap();
        let fetched = fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .expect("existing file must be reused, not re-downloaded")
        .fetched;

        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].filename, existing);
        assert!(!download_dir.join("20260807-Foo_ROW1_UHD.jpg").exists());
        // The thumbnail backfill covered the pre-existing entry too.
        assert!(thumbs::thumbnail_path(&existing, &state).unwrap().is_file());
    }

    #[tokio::test]
    async fn pipeline_replaces_a_catalogued_file_that_is_not_an_image() {
        // The lookup above skips the download permanently, so what it hands
        // back *is* the wallpaper from now on. A folder migrated from the
        // reference GNOME extension (no magic-byte check on its downloads)
        // can hold a saved error page under a perfectly valid Bing name:
        // reusing it would leave the entry pointing at a file that never
        // renders, with every later refresh skipping the download again.
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&download_dir).unwrap();
        let corrupt = download_dir.join("20260807-Foo_ROW1_1920x1080.jpg");
        std::fs::write(&corrupt, b"<html>login here</html>").unwrap();
        let catalogue = Catalogue::rebuild_from_folder(&download_dir);
        assert_eq!(catalogue.images.len(), 1, "the bad file is catalogued");

        let base = spawn_list_and_jpeg_mock();
        let client = bing::http_client().unwrap();
        let fetched = fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .unwrap()
        .fetched;

        let fresh = download_dir.join("20260807-Foo_ROW1_UHD.jpg");
        assert_eq!(fetched[0].filename, fresh);
        assert!(bing::is_jpeg_file(&fresh));
        assert!(thumbs::thumbnail_path(&fresh, &state).unwrap().is_file());
        // …and the merge adopts the replacement, which it only does for an
        // entry whose file claim is dead — hence the unlink.
        assert!(!corrupt.exists(), "the dead file must not survive");
        let mut healed = catalogue.clone();
        healed.merge(fetched, &download_dir);
        assert_eq!(healed.images[0].filename, fresh);
    }

    #[tokio::test]
    async fn pipeline_backfills_thumbnails_outside_the_fetch_window() {
        // First open after a migration: the folder holds far more images
        // than one fetch window covers (the reference GNOME extension kept
        // months of them). Entries older than the window are never touched
        // by the download loop, so only the backfill pass gives them a
        // preview — without it the popup shows the placeholder forever for
        // everything but the last few days.
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&download_dir).unwrap();
        let old = download_dir.join("20250101-Old_ROW0_UHD.jpg");
        // Deliberately *not* the bytes the mock serves: if the backfill were
        // a re-download in disguise, the file's content would change.
        let old_bytes = crate::testutil::tiny_jpeg(48, 27);
        std::fs::write(&old, &old_bytes).unwrap();
        // A foreign file in the folder is not catalogued and gets no thumb.
        let foreign = download_dir.join("holiday.jpg");
        std::fs::write(&foreign, crate::testutil::tiny_jpeg(32, 18)).unwrap();
        let catalogue = Catalogue::rebuild_from_folder(&download_dir);
        assert_eq!(catalogue.images.len(), 1, "only the wallpaper is tracked");

        let jpeg = crate::testutil::tiny_jpeg(64, 36);
        let base = crate::testutil::spawn_mock(move |path| {
            if path.starts_with("/HPImageArchive.aspx") {
                (200, LIST_JSON.as_bytes().to_vec())
            } else if path.starts_with("/th?id=OHR.") {
                (200, jpeg.clone())
            } else {
                (404, Vec::new())
            }
        });

        let client = bing::http_client().unwrap();
        let fetched = fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .unwrap()
        .fetched;

        // The fetch window holds only the new image…
        assert_eq!(fetched.len(), 1);
        assert!(
            thumbs::thumbnail_path(&fetched[0].filename, &state)
                .unwrap()
                .is_file()
        );
        // …yet the older catalogue entry got a thumbnail all the same —
        // generated from the file already on disk, not re-fetched.
        assert!(
            thumbs::thumbnail_path(&old, &state).unwrap().is_file(),
            "out-of-window entry must be backfilled"
        );
        assert_eq!(
            std::fs::read(&old).unwrap(),
            old_bytes,
            "the backfill must not re-download the image it thumbnails"
        );
        assert!(!thumbs::thumbnail_path(&foreign, &state).unwrap().exists());
    }

    #[tokio::test]
    async fn thumbnails_are_generated_at_startup_before_any_fetch() {
        // The first open after `just install` over a folder migrated from the
        // reference GNOME extension. The catalogue is non-empty, so this is
        // *not* a cold start: the first refresh is due off the newest
        // `fullstartdate`, up to ~24 h away — and on an offline machine it
        // never lands at all. Previews therefore cannot wait for a fetch;
        // they come from the files already on disk, which is what the startup
        // pass ([`run_thumbnail_pass`]) runs. No mock server here on purpose:
        // nothing in this path may touch the network.
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&download_dir).unwrap();
        // The reference extension's own resolution suffix, i.e. a file no
        // fetch of ours ever wrote.
        let migrated = download_dir.join("20260807-Foo_ROW1_1920x1080.jpg");
        std::fs::write(&migrated, crate::testutil::tiny_jpeg(64, 36)).unwrap();
        let older = download_dir.join("20250101-Old_ROW0_UHD.jpg");
        std::fs::write(&older, crate::testutil::tiny_jpeg(48, 27)).unwrap();
        // A foreign file in the folder is not catalogued and gets no preview.
        let foreign = download_dir.join("holiday.jpg");
        std::fs::write(&foreign, crate::testutil::tiny_jpeg(32, 18)).unwrap();
        let catalogue = Catalogue::rebuild_from_folder(&download_dir);
        assert_eq!(catalogue.images.len(), 2, "only wallpapers are tracked");

        // Exactly what `Window::start_thumbnail_pass_over` runs at startup,
        // with the roots injected: retention "forever", nothing applied.
        run_thumbnail_pass(
            catalogue,
            Backfill::new(&wallpaper::CurrentWallpaper::NoFile, 0, None),
            &download_dir,
            &state,
        )
        .await;

        for image in [&migrated, &older] {
            assert!(
                thumbs::thumbnail_path(image, &state).unwrap().is_file(),
                "{} must have a preview before the first fetch",
                image.display()
            );
        }
        assert!(!thumbs::thumbnail_path(&foreign, &state).unwrap().exists());
        assert_eq!(thumb_count(&state), 2);
    }

    /// A catalogue of `count` decodable images inside `download_dir`,
    /// oldest first (the order `Catalogue` keeps).
    fn catalogue_of_decodable_images(download_dir: &Path, count: usize) -> Catalogue {
        let mut catalogue = Catalogue::default();
        for i in 0..count {
            let entry = entry_on_disk(download_dir, &format!("2025{:04}", 101 + i), "Old_ROW0");
            // `entry_on_disk` writes placeholder bytes; a decodable image is
            // needed for the thumbnail to actually be produced.
            std::fs::write(&entry.filename, crate::testutil::tiny_jpeg(64, 36)).unwrap();
            catalogue.images.push(entry);
        }
        catalogue
    }

    /// Mock serving the one-image list plus a decodable JPEG for anything else.
    fn spawn_list_and_jpeg_mock() -> String {
        crate::testutil::spawn_mock(move |path| {
            if path.starts_with("/HPImageArchive.aspx") {
                (200, LIST_JSON.as_bytes().to_vec())
            } else {
                (200, crate::testutil::tiny_jpeg(64, 36))
            }
        })
    }

    /// Cached thumbnails only — the thumbs dir also holds each slot's
    /// `.meta` sidecar (including those of files that failed to decode).
    fn thumb_count(state: &Path) -> usize {
        std::fs::read_dir(state.join("thumbs"))
            .unwrap()
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                !name.ends_with(".meta") && !name.ends_with(".part")
            })
            .count()
    }

    /// Backfill policy for the pipeline tests, with `budget` decode attempts:
    /// retention "forever" (nothing is skipped as doomed) and no applied file.
    /// A lowered budget lets the cap and the progression across refreshes be
    /// exercised with a handful of files instead of `MAX_THUMBNAIL_BACKFILL`
    /// of them.
    fn capped(budget: usize) -> Backfill {
        Backfill {
            budget,
            retention_days: 0,
            current: None,
            protected_fallback: None,
            now: Utc::now(),
            deferred: false,
        }
    }

    /// [`capped`] at the production budget, which no test staging a handful
    /// of files can reach — i.e. "backfill everything".
    fn test_backfill() -> Backfill {
        capped(MAX_THUMBNAIL_BACKFILL)
    }

    #[test]
    fn backfill_retention_is_the_one_the_prune_will_use() {
        // The backfill skips what the following prune deletes, so the two
        // must never be handed different numbers — `Backfill::new` derives
        // its cutoff from the live cosmic-bg state exactly as the prune does.
        // A per-output setup (`Unknown`) is the case that used to diverge
        // *permanently*: the prune keeps everything, so the backfill must
        // thumbnail everything too.
        let dir = tempfile::tempdir().unwrap();
        let old = entry_on_disk(dir.path(), "20200101", "Ancient_ROW0");
        for (live, wanted) in [
            (wallpaper::CurrentWallpaper::Unknown, true),
            (wallpaper::CurrentWallpaper::NoFile, false),
            (
                wallpaper::CurrentWallpaper::File(old.filename.clone()),
                false,
            ),
        ] {
            let backfill = Backfill::new(&live, 8, None);
            assert_eq!(
                backfill.retention_days,
                wallpaper::prune_retention(&live, 8),
                "{live:?}"
            );
            assert_eq!(backfill.worth_decoding(&old), wanted, "{live:?}");
        }
    }

    #[tokio::test]
    async fn pipeline_backfill_is_capped_and_advances_on_later_refreshes() {
        // Each backfilled thumbnail fully decodes a UHD JPEG, so the pass is
        // capped per refresh, newest first — but the cap must buy *progress*:
        // each refresh has to spend it on entries that still lack a thumbnail,
        // or a folder migrated from the GNOME extension would never get past
        // the first batch. The budget is injected here so the test needs a
        // handful of files rather than `MAX_THUMBNAIL_BACKFILL` of them.
        const CAP: usize = 4;
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&download_dir).unwrap();

        let catalogue = catalogue_of_decodable_images(&download_dir, 2 * CAP + 2);
        let base = spawn_list_and_jpeg_mock();
        let client = bing::http_client().unwrap();

        fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &capped(CAP),
        )
        .await
        .unwrap();
        // The downloaded image plus exactly the capped number of backfills.
        assert_eq!(thumb_count(&state), CAP + 1);

        // Second refresh over the same catalogue: the newest entries are
        // cached now, so the budget reaches the next batch down.
        fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &capped(CAP),
        )
        .await
        .unwrap();
        assert_eq!(
            thumb_count(&state),
            2 * CAP + 1,
            "a later refresh must not burn its budget on already-cached entries"
        );

        // Third: only the two stragglers are left.
        fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &capped(CAP),
        )
        .await
        .unwrap();
        assert_eq!(
            thumb_count(&state),
            catalogue.images.len() + 1,
            "every catalogue entry is reached eventually"
        );
    }

    #[tokio::test]
    async fn pipeline_backfill_pays_for_a_failed_decode_once_and_then_moves_on() {
        // A permanently undecodable file (truncated download, or a non-JPEG
        // saved under a wallpaper name in a folder migrated from the GNOME
        // extension, which had no magic-byte check) never becomes `is_cached`.
        // Both bounds have to hold at once:
        //   * the attempt costs budget — `image::open` is real work, so a
        //     catalogue full of corrupt files must not turn one refresh into
        //     an unbounded scan;
        //   * it costs it exactly once — the failure is remembered, so the
        //     next refresh skips it for free and the budget moves down to the
        //     healthy entries instead of being pinned to the corrupt newest
        //     end forever.
        const CAP: usize = 4;
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&download_dir).unwrap();

        // Oldest first, the order `Catalogue` keeps: one healthy entry, then a
        // full cap's worth of corrupt ones sitting at the newest end.
        let mut catalogue = catalogue_of_decodable_images(&download_dir, 1);
        let healthy = catalogue.images[0].filename.clone();
        let corrupt: Vec<PathBuf> = (0..CAP)
            .map(|i| {
                let entry =
                    entry_on_disk(&download_dir, &format!("2026{:04}", 101 + i), "Bad_ROW0");
                std::fs::write(&entry.filename, b"not actually a jpeg").unwrap();
                let path = entry.filename.clone();
                catalogue.images.push(entry);
                path
            })
            .collect();

        let base = spawn_list_and_jpeg_mock();
        let client = bing::http_client().unwrap();
        fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &capped(CAP),
        )
        .await
        .expect("undecodable files must not fail the pipeline");

        // Refresh 1 spent the whole budget on the four doomed decodes…
        assert_eq!(thumb_count(&state), 1, "only the fetched image thumbnailed");
        for path in &corrupt {
            assert!(
                thumbs::decode_failed(path, &state),
                "a failed decode must be remembered, not retried forever"
            );
        }

        // …and refresh 2 gets them for free, so the healthy entry is reached.
        fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &capped(CAP),
        )
        .await
        .unwrap();
        assert!(
            thumbs::thumbnail_path(&healthy, &state).unwrap().is_file(),
            "a healthy older entry must not be starved by undecodable newer ones"
        );
    }

    #[tokio::test]
    async fn pipeline_backfill_retries_a_repaired_file() {
        // The negative cache is keyed on the file's identity, so replacing a
        // corrupt download with a good one must not leave it on the
        // placeholder until the state dir is cleared by hand.
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&download_dir).unwrap();

        let mut catalogue = Catalogue::default();
        let entry = entry_on_disk(&download_dir, "20250101", "Old_ROW0");
        let path = entry.filename.clone();
        std::fs::write(&path, b"not actually a jpeg").unwrap();
        catalogue.images.push(entry);

        let base = spawn_list_and_jpeg_mock();
        let client = bing::http_client().unwrap();
        fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .unwrap();
        assert!(thumbs::decode_failed(&path, &state));

        std::fs::write(&path, crate::testutil::tiny_jpeg(64, 36)).unwrap();
        assert!(
            !thumbs::decode_failed(&path, &state),
            "a rewritten file invalidates the recorded verdict"
        );
        fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .unwrap();
        assert!(thumbs::thumbnail_path(&path, &state).unwrap().is_file());
    }

    #[tokio::test]
    async fn pipeline_backfill_skips_what_the_prune_is_about_to_delete() {
        // The backfill runs inside the pipeline, i.e. *before* `finish_refresh`
        // prunes. Decoding an entry the same refresh then deletes costs ~5 MB
        // of I/O for a thumbnail unlinked minutes later — at the default 8-day
        // retention over a migrated folder, that is the whole budget wasted.
        // The applied wallpaper is exempt, exactly as it is in the prune.
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&download_dir).unwrap();

        let now = Utc::now();
        let day = |ago: i64| {
            (now - chrono::Duration::days(ago))
                .format("%Y%m%d")
                .to_string()
        };
        let mut catalogue = Catalogue::default();
        let mut on_disk = |startdate: String, name: &str| {
            let entry = entry_on_disk(&download_dir, &startdate, name);
            std::fs::write(&entry.filename, crate::testutil::tiny_jpeg(64, 36)).unwrap();
            let path = entry.filename.clone();
            catalogue.images.push(entry);
            path
        };
        let doomed = on_disk(day(40), "Doomed_ROW0");
        let applied = on_disk(day(30), "Applied_ROW1");
        let kept = on_disk(day(2), "Kept_ROW2");

        let base = spawn_list_and_jpeg_mock();
        let client = bing::http_client().unwrap();
        let backfill = Backfill {
            budget: MAX_THUMBNAIL_BACKFILL,
            retention_days: 8,
            current: Some(applied.clone()),
            protected_fallback: None,
            now,
            deferred: false,
        };
        fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &backfill,
        )
        .await
        .unwrap();

        assert!(thumbs::thumbnail_path(&kept, &state).unwrap().is_file());
        assert!(
            thumbs::thumbnail_path(&applied, &state).unwrap().is_file(),
            "the applied wallpaper survives the prune, so it needs its preview"
        );
        assert!(
            !thumbs::thumbnail_path(&doomed, &state).unwrap().exists(),
            "no decode for an entry this very refresh deletes"
        );
    }

    #[tokio::test]
    async fn pipeline_backfill_skips_tampered_entries() {
        // The same containment gate the prune applies: a hand-edited
        // `catalogue.json` must not make us decode (and cache a copy of) an
        // arbitrary readable image. Both halves of the gate get their own
        // victim, and both sit at the *newest* end so the cap cannot hide
        // them.
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&download_dir).unwrap();

        let mut catalogue = catalogue_of_decodable_images(&download_dir, 2);
        assert!(catalogue.images.len() < MAX_THUMBNAIL_BACKFILL);

        // (a) Outside the download dir — perfectly named for its own urlbase,
        //     so only the `parent()` check stops it.
        let outside = dir.path().join("private");
        std::fs::create_dir_all(&outside).unwrap();
        // A date no catalogue entry uses, so its cache slot is its own.
        let outside_victim = outside.join("20240101-Old_ROW0_UHD.jpg");
        std::fs::write(&outside_victim, crate::testutil::tiny_jpeg(32, 18)).unwrap();
        let mut hostile = catalogue.images[0].clone();
        hostile.filename = outside_victim.clone();
        catalogue.images.push(hostile);

        // (b) Inside the download dir but naming a *different* image's file,
        //     so only `names_own_file` stops it.
        let renamed_victim = download_dir.join("20250101-Other_ROW9_UHD.jpg");
        std::fs::write(&renamed_victim, crate::testutil::tiny_jpeg(32, 18)).unwrap();
        let mut hostile = catalogue.images[0].clone();
        hostile.filename = renamed_victim.clone();
        catalogue.images.push(hostile);

        let base = spawn_list_and_jpeg_mock();
        let client = bing::http_client().unwrap();
        fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .unwrap();

        // The downloaded image plus the two legitimate entries — nothing else.
        assert_eq!(thumb_count(&state), 3);
        for victim in [&outside_victim, &renamed_victim] {
            assert!(
                !thumbs::thumbnail_path(victim, &state).unwrap().exists(),
                "a tampered entry must not get {} decoded and cached",
                victim.display()
            );
        }
    }

    #[tokio::test]
    async fn pipeline_aborts_on_a_failed_download_but_keeps_what_it_got() {
        // The documented partial-failure contract: any HTTP failure aborts
        // the refresh (→ 1 h backoff), and whatever landed before it stays on
        // disk so the next run skips it instead of re-fetching.
        const TWO_IMAGES: &str = r#"{"images":[
            {"urlbase":"/th?id=OHR.Foo_ROW1","startdate":"20260806","fullstartdate":"202608060700","copyright":"Foo (© Bar)","copyrightlink":"https://example.com/foo","wp":true},
            {"urlbase":"/th?id=OHR.Bad_ROW2","startdate":"20260807","fullstartdate":"202608070700","copyright":"Bad (© Bar)","copyrightlink":"https://example.com/bad","wp":true}
        ]}"#;
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");

        let jpeg = crate::testutil::tiny_jpeg(64, 36);
        let base = crate::testutil::spawn_mock(move |path| {
            if path.starts_with("/HPImageArchive.aspx") {
                (200, TWO_IMAGES.as_bytes().to_vec())
            } else if path.starts_with("/th?id=OHR.Foo_ROW1") {
                (200, jpeg.clone())
            } else {
                (500, Vec::new())
            }
        });

        let client = bing::http_client().unwrap();
        let error = fetch_and_download(
            &client,
            &base,
            &Catalogue::default(),
            within(2),
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .expect_err("a failed download must abort the refresh");
        assert!(matches!(error, bing::FetchError::Status(s) if s.as_u16() == 500));

        // The image fetched before the failure survives; the failed one left
        // nothing behind, not even a `.part`.
        assert!(download_dir.join("20260806-Foo_ROW1_UHD.jpg").is_file());
        assert!(!download_dir.join("20260807-Bad_ROW2_UHD.jpg").exists());
        assert!(!download_dir.join("20260807-Bad_ROW2_UHD.jpg.part").exists());
    }

    #[tokio::test]
    async fn pipeline_survives_an_undecodable_catalogue_file() {
        // Thumbnailing is deliberately non-fatal: a corrupt or truncated
        // file in the folder must not abort the whole refresh (and leave the
        // user without new wallpapers until it is deleted by hand).
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&download_dir).unwrap();
        let corrupt = download_dir.join("20250101-Old_ROW0_UHD.jpg");
        std::fs::write(&corrupt, b"not actually a jpeg").unwrap();
        let catalogue = Catalogue::rebuild_from_folder(&download_dir);
        assert_eq!(catalogue.images.len(), 1);

        let jpeg = crate::testutil::tiny_jpeg(64, 36);
        let base = crate::testutil::spawn_mock(move |path| {
            if path.starts_with("/HPImageArchive.aspx") {
                (200, LIST_JSON.as_bytes().to_vec())
            } else {
                (200, jpeg.clone())
            }
        });

        let client = bing::http_client().unwrap();
        let fetched = fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(1),
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .expect("an undecodable file must not fail the pipeline")
        .fetched;

        assert_eq!(fetched.len(), 1);
        assert!(
            thumbs::thumbnail_path(&fetched[0].filename, &state)
                .unwrap()
                .is_file()
        );
        assert!(!thumbs::thumbnail_path(&corrupt, &state).unwrap().exists());
    }

    #[test]
    fn only_web_links_reach_xdg_open() {
        // The value comes from Bing's JSON and survives in the user-editable
        // catalogue; `xdg-open` treats a leading `-` as a flag and happily
        // launches the handler for a local path or `file:` URL.
        assert_eq!(
            web_url("https://www.bing.com/x"),
            Some("https://www.bing.com/x")
        );
        assert_eq!(web_url("http://example.com"), Some("http://example.com"));
        assert_eq!(web_url("file:///etc/passwd"), None);
        assert_eq!(web_url("/home/u/.ssh/id_ed25519"), None);
        assert_eq!(web_url("--version"), None);
        assert_eq!(web_url(""), None);
    }

    const DESKTOP: &str = include_str!("../data/io.github.ercling.cosmic-applet-daymural.desktop");
    const METAINFO: &str =
        include_str!("../data/io.github.ercling.cosmic-applet-daymural.metainfo.xml");
    const FLATPAK_MANIFEST: &str = include_str!("../io.github.ercling.cosmic-applet-daymural.json");
    const JUSTFILE: &str = include_str!("../justfile");
    const README: &str = include_str!("../README.md");
    const SCREENSHOT_PROVENANCE: &str = include_str!("../resources/screenshots/README.md");
    const STORE_SCREENSHOT: &[u8] = include_bytes!("../resources/screenshots/screenshot-main.png");
    const AGENT_GUIDE: &str = include_str!("../AGENTS.md");
    const LEGACY_GUIDE: &str = include_str!("../CLAUDE.md");
    const ACTIVE_FLATPAK_PLAN: &str =
        include_str!("../docs/plans/20260820-flatpak-distribution.md");
    const CARGO_SOURCES_SCRIPT: &str = include_str!("../flatpak/generate-cargo-sources.sh");
    const CARGO_GENERATOR: &str = include_str!("../flatpak/flatpak-cargo-generator.py");
    const GIT_MANIFEST_SCAN: &str = include_str!("../flatpak/git_manifest_scan.py");
    const GIT_MANIFEST_SCAN_TEST: &str = include_str!("../flatpak/test_git_manifest_scan.py");
    const CARGO_GENERATOR_LOCK: &str = include_str!("../flatpak/flatpak-cargo-generator.py.lock");
    const RUST_WORKFLOW: &str = include_str!("../.github/workflows/rust.yml");
    const FLATPAK_WORKFLOW: &str = include_str!("../.github/workflows/flatpak.yml");
    const CARGO_SOURCES_FILENAME: &str = "cargo-sources.json";
    const CHECKOUT_ACTION: &str = "actions/checkout@11d5960a326750d5838078e36cf38b85af677262";
    const RUST_TOOLCHAIN_ACTION: &str =
        "dtolnay/rust-toolchain@4360b52568e2003a75bf9bc1d59f33a8e3fc893c";
    const RUST_CACHE_ACTION: &str = "Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6";
    const FLATPAK_BUILDER_ACTION: &str =
        "flatpak/flatpak-github-actions/flatpak-builder@401fe28a8384095fc1531b9d320b292f0ee45adb";
    const FLATPAK_BUILDER_IMAGE: &str = "ghcr.io/flathub-infra/flatpak-github-actions:freedesktop-25.08@sha256:6f3180c6765cb55e5dcd8ee4127b82aba25163c8c655a161422b7c447c14e4af";

    fn store_screenshot_contract(readme: &str, provenance: &str, screenshot: &[u8]) -> bool {
        readme.contains(
            "![Daymural panel popup showing project-owned dawn artwork](resources/screenshots/screenshot-main.png)",
        ) && provenance.contains("not a Microsoft Bing image")
            && provenance.contains("CC0-1.0")
            && screenshot.starts_with(b"\x89PNG\r\n\x1a\n")
    }

    #[test]
    fn store_screenshot_is_present_and_has_licensed_provenance() {
        assert!(store_screenshot_contract(
            README,
            SCREENSHOT_PROVENANCE,
            STORE_SCREENSHOT
        ));
        assert!(!store_screenshot_contract(
            &README.replace("screenshot-main.png", "missing.png"),
            SCREENSHOT_PROVENANCE,
            STORE_SCREENSHOT
        ));
        assert!(!store_screenshot_contract(
            README,
            &SCREENSHOT_PROVENANCE.replace("CC0-1.0", "license-pending"),
            STORE_SCREENSHOT
        ));
        assert!(!store_screenshot_contract(
            README,
            SCREENSHOT_PROVENANCE,
            b"not a PNG"
        ));
    }

    fn just_var<'a>(justfile: &'a str, name: &str) -> Option<&'a str> {
        justfile
            .lines()
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| line.strip_prefix(name)?.trim_start().strip_prefix(":="))?
            .trim()
            .strip_prefix('\'')?
            .strip_suffix('\'')
    }

    fn just_recipe(justfile: &str, name: &str) -> String {
        justfile
            .lines()
            .skip_while(
                |line| !matches!(line.strip_prefix(name), Some(rest) if rest.starts_with(':')),
            )
            .skip(1)
            .take_while(|line| line.trim().is_empty() || line.starts_with([' ', '\t']))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn native_install_rewrites_exec(justfile: &str) -> bool {
        sans_comments(&just_recipe(justfile, "install"))
            .contains("sed -i 's|^Exec=.*|Exec={{bin-dst}}|' {{desktop-dst}}")
    }

    fn sans_comments(text: &str) -> String {
        text.lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn active_workflow_step_containing(workflow: &str, needle: &str) -> bool {
        let workflow = sans_comments(workflow);
        let lines: Vec<&str> = workflow.lines().collect();
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.starts_with("      - "))
            .any(|(start, _)| {
                let end = lines[start + 1..]
                    .iter()
                    .position(|line| line.starts_with("      - "))
                    .map_or(lines.len(), |offset| start + 1 + offset);
                let step = &lines[start..end];
                let disabled = step.iter().any(|line| {
                    line.trim().strip_prefix("if:").is_some_and(|condition| {
                        matches!(
                            condition.trim().trim_matches(['\'', '"']),
                            "false" | "${{ false }}"
                        )
                    })
                });
                !disabled && step.iter().any(|line| line.contains(needle))
            })
    }

    fn desktop_value<'a>(desktop: &'a str, key: &str) -> Option<&'a str> {
        desktop.lines().find_map(|line| {
            let (candidate, value) = line.split_once('=')?;
            (candidate == key).then_some(value)
        })
    }

    fn flatpak_identity_is_consistent(
        desktop: &str,
        metainfo: &str,
        metainfo_filename: &str,
    ) -> Result<(), String> {
        let expected_desktop = format!("{APP_ID}.desktop");
        let expected_metainfo = format!("{APP_ID}.metainfo.xml");
        let expected_icon = format!("{APP_ID}-symbolic");
        let expected_binary = env!("CARGO_PKG_NAME");
        let require = |condition: bool, message: &str| {
            condition.then_some(()).ok_or_else(|| message.to_owned())
        };

        require(
            APP_ID.bytes().all(|byte| !byte.is_ascii_uppercase()),
            "APP_ID must be lowercase",
        )?;

        require(
            desktop_value(desktop, "Exec") == Some(expected_binary),
            "desktop Exec must be the bare Cargo binary name",
        )?;
        require(
            desktop_value(desktop, "Icon") == Some(expected_icon.as_str()),
            "desktop icon must use the app-ID-prefixed symbolic name",
        )?;
        require(
            metainfo_filename == expected_metainfo,
            "metainfo filename must match APP_ID",
        )?;

        let before_provides = metainfo.split("<provides>").next().unwrap_or(metainfo);
        require(
            before_provides.contains(&format!("<id>{APP_ID}</id>")),
            "metainfo component id must match APP_ID",
        )?;
        require(
            metainfo.contains(&format!(
                "<launchable type=\"desktop-id\">{expected_desktop}</launchable>"
            )),
            "metainfo launchable must name the app-ID desktop entry",
        )?;

        let provides = metainfo
            .split("<provides>")
            .nth(1)
            .and_then(|rest| rest.split("</provides>").next())
            .ok_or_else(|| "metainfo must contain a provides block".to_owned())?;
        require(
            provides.contains("<id>com.system76.CosmicApplet</id>"),
            "metainfo must provide the COSMIC Store applet category",
        )?;
        require(
            provides.contains(&format!("<binary>{expected_binary}</binary>")),
            "metainfo binary must match the Cargo package name",
        )?;

        require(
            metainfo.contains(&format!(
                "<project_license>{}</project_license>",
                env!("CARGO_PKG_LICENSE")
            )),
            "metainfo project license must match Cargo",
        )?;
        require(
            metainfo.contains("<metadata_license>CC0-1.0</metadata_license>"),
            "metainfo metadata license must be CC0-1.0",
        )?;
        require(
            metainfo.contains(&format!(
                "<summary>{}</summary>",
                env!("CARGO_PKG_DESCRIPTION")
            )),
            "metainfo summary must match Cargo",
        )?;
        require(
            metainfo.contains("<description>") && metainfo.contains("<p>"),
            "metainfo must describe the applet",
        )?;
        require(
            metainfo.contains(&format!(
                "<name>{}</name>",
                desktop_value(desktop, "Name").unwrap_or_default()
            )),
            "metainfo name must match the desktop entry",
        )?;
        let release_prefix = format!("<release version=\"{}\" date=\"", env!("CARGO_PKG_VERSION"));
        let release_date = metainfo
            .split_once(&release_prefix)
            .and_then(|(_, rest)| rest.split_once('"').map(|(date, _)| date))
            .ok_or_else(|| "metainfo release must match the Cargo version".to_owned())?;
        require(
            release_date.len() == 10
                && release_date.chars().enumerate().all(|(index, character)| {
                    if matches!(index, 4 | 7) {
                        character == '-'
                    } else {
                        character.is_ascii_digit()
                    }
                }),
            "metainfo release must have a YYYY-MM-DD date",
        )?;
        require(
            metainfo.contains(&format!(
                "<url type=\"homepage\">{}</url>",
                env!("CARGO_PKG_REPOSITORY")
            )) && env!("CARGO_PKG_REPOSITORY") == env!("CARGO_PKG_HOMEPAGE"),
            "metainfo homepage, Cargo repository, and Cargo homepage must agree",
        )?;
        require(
            metainfo.contains("<developer id=\"io.github.ercling\">")
                && metainfo.contains("<name>Oleksandr Mykhailiuta</name>"),
            "metainfo must identify the developer",
        )?;
        require(
            metainfo.contains("<project_group>COSMIC</project_group>"),
            "metainfo project group must be COSMIC",
        )?;
        require(
            metainfo.contains("<content_rating type=\"oars-1.1\"/>"),
            "metainfo must carry an OARS 1.1 rating",
        )
    }

    fn validate_flatpak_manifest(
        manifest_text: &str,
        desktop: &str,
    ) -> Result<serde_json::Value, String> {
        let manifest: serde_json::Value =
            serde_json::from_str(manifest_text).map_err(|error| error.to_string())?;
        let binary = env!("CARGO_PKG_NAME");
        let require = |condition: bool, message: &str| {
            condition.then_some(()).ok_or_else(|| message.to_owned())
        };

        require(manifest["id"].as_str() == Some(APP_ID), "manifest id")?;
        require(
            manifest["runtime"].as_str() == Some("org.freedesktop.Platform")
                && manifest["runtime-version"].as_str() == Some("25.08")
                && manifest["sdk"].as_str() == Some("org.freedesktop.Sdk"),
            "Freedesktop 25.08 runtime and SDK",
        )?;
        require(
            manifest["sdk-extensions"]
                .as_array()
                .is_some_and(|extensions| {
                    extensions.iter().any(|extension| {
                        extension.as_str() == Some("org.freedesktop.Sdk.Extension.rust-stable")
                    })
                }),
            "Rust SDK extension",
        )?;
        let command = manifest["command"]
            .as_str()
            .ok_or_else(|| "manifest command".to_owned())?;
        require(command == binary, "manifest command")?;
        require(
            desktop_value(desktop, "Exec") == Some(command),
            "desktop Exec must match the manifest command",
        )?;

        let modules = manifest["modules"]
            .as_array()
            .ok_or_else(|| "modules array".to_owned())?;
        require(modules.len() == 1, "exactly one module")?;
        let module = &modules[0];
        require(module["name"].as_str() == Some(binary), "module name")?;
        require(
            module["buildsystem"].as_str() == Some("simple"),
            "simple buildsystem",
        )?;
        require(
            manifest["build-options"]["append-path"].as_str()
                == Some("/usr/lib/sdk/rust-stable/bin"),
            "Rust SDK path",
        )?;
        require(
            manifest["build-options"]["env"]["CARGO_HOME"].as_str()
                == Some(&format!("/run/build/{binary}/cargo")),
            "CARGO_HOME must align with the module name",
        )?;

        let commands: Vec<&str> = module["build-commands"]
            .as_array()
            .ok_or_else(|| "build-commands array".to_owned())?
            .iter()
            .map(|command| {
                command
                    .as_str()
                    .ok_or_else(|| "string build command".to_owned())
            })
            .collect::<Result<_, _>>()?;
        let expected_commands = [
            "cargo --offline fetch --locked --manifest-path Cargo.toml --verbose".to_owned(),
            "cargo --offline build --release --locked --verbose".to_owned(),
            format!("install -Dm755 target/release/{binary} /app/bin/{command}"),
            format!(
                "install -Dm644 data/{APP_ID}.desktop /app/share/applications/{APP_ID}.desktop"
            ),
            format!(
                "install -Dm644 data/{APP_ID}.metainfo.xml /app/share/metainfo/{APP_ID}.metainfo.xml"
            ),
            format!(
                "install -Dm644 data/icons/{APP_ID}-symbolic.svg /app/share/icons/hicolor/scalable/apps/{APP_ID}-symbolic.svg"
            ),
        ];
        require(
            commands.len() == expected_commands.len()
                && commands
                    .iter()
                    .zip(&expected_commands)
                    .all(|(actual, expected)| *actual == expected),
            "build commands must exactly fetch/build the lockfile and install the required output tree",
        )?;

        let sources = module["sources"]
            .as_array()
            .ok_or_else(|| "sources array".to_owned())?;
        require(
            sources
                .iter()
                .any(|source| source.as_str() == Some(CARGO_SOURCES_FILENAME)),
            "generated Cargo source",
        )?;
        let directory = sources
            .iter()
            .find(|source| source["type"].as_str() == Some("dir"))
            .ok_or_else(|| "local directory source".to_owned())?;
        require(
            directory["path"].as_str() == Some("./"),
            "local source path",
        )?;
        let skip: Vec<&str> = directory["skip"]
            .as_array()
            .ok_or_else(|| "directory skip list".to_owned())?
            .iter()
            .map(|entry| entry.as_str().ok_or_else(|| "string skip entry".to_owned()))
            .collect::<Result<_, _>>()?;
        for required in [
            ".git",
            "target",
            "examples",
            ".flatpak-builder",
            "build-dir",
        ] {
            require(
                skip.contains(&required),
                &format!("skip list entry: {required}"),
            )?;
        }

        let finish_args: Vec<&str> = manifest["finish-args"]
            .as_array()
            .ok_or_else(|| "finish-args array".to_owned())?
            .iter()
            .map(|arg| arg.as_str().ok_or_else(|| "string finish-arg".to_owned()))
            .collect::<Result<_, _>>()?;
        for required in [
            "--socket=wayland",
            "--device=dri",
            "--share=network",
            "--filesystem=~/Pictures/BingWallpaper:create",
            "--filesystem=xdg-config/cosmic:rw",
            "--filesystem=~/.local/state/cosmic:create",
            "--talk-name=com.system76.CosmicSettingsDaemon",
            "--talk-name=com.system76.CosmicSettingsDaemon.*",
            "--system-talk-name=org.freedesktop.login1",
        ] {
            require(
                finish_args.contains(&required),
                &format!("missing finish-arg: {required}"),
            )?;
        }
        require(
            !finish_args.contains(&"--filesystem=~/.local/state/cosmic:rw"),
            "COSMIC state directory must use :create, not :rw",
        )?;
        require(
            !finish_args.iter().any(|arg| {
                matches!(*arg, "--filesystem=home" | "--filesystem=host")
                    || arg.starts_with("--filesystem=home:")
                    || arg.starts_with("--filesystem=host:")
            }),
            "broad filesystem grant",
        )?;
        require(
            !finish_args.iter().any(|arg| {
                matches!(
                    *arg,
                    "--socket=x11"
                        | "--socket=fallback-x11"
                        | "--socket=session-bus"
                        | "--socket=system-bus"
                )
            }),
            "broad socket grant",
        )?;
        require(
            !finish_args.iter().any(|arg| arg.starts_with("--persist")),
            "persistence grant",
        )?;
        Ok(manifest)
    }

    #[test]
    fn flatpak_manifest_is_valid_and_resolves_every_exported_name() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(
            FLATPAK_MANIFEST,
            std::fs::read_to_string(root.join(format!("{APP_ID}.json")))
                .expect("the repository root ships a manifest named for APP_ID")
        );
        validate_flatpak_manifest(FLATPAK_MANIFEST, DESKTOP)
            .expect("the developer manifest must preserve the Flatpak contract");

        for shipped in [
            format!("data/{APP_ID}.desktop"),
            format!("data/{APP_ID}.metainfo.xml"),
            format!("data/icons/{APP_ID}-symbolic.svg"),
        ] {
            assert!(
                root.join(&shipped).is_file(),
                "manifest install source `{shipped}` must exist"
            );
        }
    }

    #[test]
    fn flatpak_manifest_rejects_an_uppercase_application_id() {
        let uppercase = FLATPAK_MANIFEST.replace(
            "io.github.ercling.cosmic-applet-daymural",
            "io.github.ercling.CosmicAppletDaymural",
        );
        assert!(
            validate_flatpak_manifest(&uppercase, DESKTOP).is_err(),
            "an uppercase Flatpak application ID unexpectedly passed"
        );
    }

    #[test]
    fn flatpak_manifest_rejects_missing_or_broadened_sandbox_permissions() {
        let valid: serde_json::Value =
            serde_json::from_str(FLATPAK_MANIFEST).expect("checked-in manifest is JSON");
        let required = valid["finish-args"]
            .as_array()
            .expect("checked-in manifest has finish-args");
        for omitted in required {
            let mut changed = valid.clone();
            changed["finish-args"] = serde_json::Value::Array(
                required
                    .iter()
                    .filter(|arg| *arg != omitted)
                    .cloned()
                    .collect(),
            );
            assert!(
                validate_flatpak_manifest(&changed.to_string(), DESKTOP).is_err(),
                "missing functional permission `{omitted}` unexpectedly passed"
            );
        }

        let state_create = "--filesystem=~/.local/state/cosmic:create";
        let mut wrong_state_mode = valid.clone();
        wrong_state_mode["finish-args"] = serde_json::Value::Array(
            required
                .iter()
                .map(|arg| {
                    if arg.as_str() == Some(state_create) {
                        serde_json::Value::String(
                            "--filesystem=~/.local/state/cosmic:rw".to_owned(),
                        )
                    } else {
                        arg.clone()
                    }
                })
                .collect(),
        );
        assert!(
            validate_flatpak_manifest(&wrong_state_mode.to_string(), DESKTOP).is_err(),
            "`:rw` must not replace the state directory's `:create` grant"
        );

        for forbidden in [
            "--filesystem=home",
            "--filesystem=host:ro",
            "--socket=x11",
            "--socket=fallback-x11",
            "--socket=session-bus",
            "--socket=system-bus",
            "--persist=.",
        ] {
            let mut broadened = valid.clone();
            broadened["finish-args"]
                .as_array_mut()
                .expect("checked-in manifest has finish-args")
                .push(serde_json::Value::String(forbidden.to_owned()));
            assert!(
                validate_flatpak_manifest(&broadened.to_string(), DESKTOP).is_err(),
                "forbidden finish-arg `{forbidden}` unexpectedly passed"
            );
        }
    }

    #[test]
    fn flatpak_manifest_rejects_commands_that_cannot_resolve_in_app_bin() {
        let mut wrong_install: serde_json::Value =
            serde_json::from_str(FLATPAK_MANIFEST).expect("checked-in manifest is JSON");
        let commands = wrong_install["modules"][0]["build-commands"]
            .as_array_mut()
            .expect("checked-in manifest has build commands");
        for command in commands {
            if let Some(text) = command.as_str()
                && text.contains("/app/bin/daymural")
            {
                *command = serde_json::Value::String(
                    text.replace("/app/bin/daymural", "/app/bin/wrong-binary"),
                );
            }
        }
        assert!(
            validate_flatpak_manifest(&wrong_install.to_string(), DESKTOP).is_err(),
            "a command without a matching /app/bin install unexpectedly passed"
        );

        let wrong_desktop = DESKTOP.replace("Exec=daymural", "Exec=another-binary");
        assert!(
            validate_flatpak_manifest(FLATPAK_MANIFEST, &wrong_desktop).is_err(),
            "an exported desktop command that differs from the manifest unexpectedly passed"
        );
        assert!(
            validate_flatpak_manifest("{not json", DESKTOP).is_err(),
            "invalid JSON unexpectedly passed"
        );

        for (valid, replacement) in [
            (
                "cargo --offline fetch --locked --manifest-path Cargo.toml --verbose",
                "echo cargo --offline fetch --locked --manifest-path Cargo.toml --verbose",
            ),
            (
                "install -Dm644 data/io.github.ercling.cosmic-applet-daymural.metainfo.xml /app/share/metainfo/io.github.ercling.cosmic-applet-daymural.metainfo.xml",
                "echo install -Dm644 data/io.github.ercling.cosmic-applet-daymural.metainfo.xml /app/share/metainfo/io.github.ercling.cosmic-applet-daymural.metainfo.xml",
            ),
            (
                "cargo --offline build --release --locked --verbose",
                "cargo --offline build --release --verbose",
            ),
        ] {
            let non_operational = FLATPAK_MANIFEST.replacen(valid, replacement, 1);
            assert!(
                validate_flatpak_manifest(&non_operational, DESKTOP).is_err(),
                "a non-operative or unlocked build command unexpectedly passed: {replacement}"
            );
        }
    }

    fn validate_flatpak_tooling(
        manifest_text: &str,
        script_text: &str,
        justfile: &str,
    ) -> Result<(), String> {
        let manifest = validate_flatpak_manifest(manifest_text, DESKTOP)?;
        let require = |condition: bool, message: &str| {
            condition.then_some(()).ok_or_else(|| message.to_owned())
        };
        let module = &manifest["modules"][0];
        let module_name = module["name"]
            .as_str()
            .ok_or_else(|| "module name".to_owned())?;

        let script = sans_comments(script_text);
        require(
            script.contains("command -v uv"),
            "vendoring script must reject missing uv",
        )?;
        require(
            script.contains("install it") && script.contains("docs.astral.sh/uv"),
            "missing-uv error must include an install hint",
        )?;
        require(
            script.contains("flatpak/flatpak-cargo-generator.py"),
            "vendoring script generator path",
        )?;
        require(
            script.contains("uv run --locked --script"),
            "vendoring script must enforce its script lockfile",
        )?;
        require(
            script.contains(" Cargo.lock "),
            "vendoring script Cargo.lock input",
        )?;
        require(
            script.contains(&format!("-o {CARGO_SOURCES_FILENAME}")),
            "vendoring output must match the manifest source name",
        )?;

        require(
            just_var(justfile, "name") == Some(module_name),
            "justfile name must match the manifest module",
        )?;
        require(
            just_var(justfile, "appid") == manifest["id"].as_str(),
            "justfile appid must match the manifest id",
        )?;
        let builder = just_var(justfile, "flatpak-builder-cmd")
            .ok_or_else(|| "shared flatpak-builder-cmd".to_owned())?;
        require(
            builder.starts_with("flatpak-builder ")
                && builder.contains("--user")
                && builder.contains("--install-deps-from=flathub")
                && builder.contains("--force-clean"),
            "shared flatpak-builder invocation",
        )?;
        for recipe in [
            "flatpak-prefetch",
            "flatpak-build",
            "flatpak-build-offline",
            "flatpak-install",
        ] {
            let body = just_recipe(justfile, recipe);
            require(
                body.contains("{{flatpak-builder-cmd}}"),
                &format!("{recipe} must use flatpak-builder-cmd"),
            )?;
            require(
                body.contains("build-dir '{{appid}}.json'"),
                &format!("{recipe} manifest path"),
            )?;
        }
        require(
            just_recipe(justfile, "flatpak-sources").contains("flatpak/generate-cargo-sources.sh"),
            "flatpak-sources script path",
        )?;
        require(
            just_recipe(justfile, "flatpak-prefetch").contains("--download-only"),
            "flatpak-prefetch must download sources",
        )?;
        require(
            just_recipe(justfile, "flatpak-build-offline").contains("--disable-download"),
            "flatpak-build-offline must disable downloads",
        )?;
        require(
            just_recipe(justfile, "flatpak-install").contains("--install"),
            "flatpak-install must install",
        )?;
        let uninstall = just_recipe(justfile, "flatpak-uninstall");
        require(
            uninstall.contains("flatpak uninstall") && uninstall.contains("'{{appid}}'"),
            "flatpak-uninstall must remove the manifest id",
        )
    }

    #[test]
    fn flatpak_vendoring_and_local_recipes_stay_aligned() {
        validate_flatpak_tooling(FLATPAK_MANIFEST, CARGO_SOURCES_SCRIPT, JUSTFILE)
            .expect("vendoring, manifest, and just recipes must agree");
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(
            root.join("flatpak/flatpak-cargo-generator.py").is_file(),
            "the wrapper's vendored generator must exist"
        );
        assert!(
            CARGO_GENERATOR.contains("https://github.com/flatpak/flatpak-builder-tools")
                && CARGO_GENERATOR.contains("f03a673abe6ce189cea1c2857e2b44af2dd79d1f"),
            "the vendored generator must document its exact upstream commit"
        );
        assert!(
            CARGO_GENERATOR.contains("aiohttp==3.12.15")
                && CARGO_GENERATOR.contains("tomlkit==0.13.3")
                && !CARGO_GENERATOR.contains("PyYAML")
                && !CARGO_GENERATOR.contains("import yaml")
                && !CARGO_GENERATOR.contains("--yaml")
                && !CARGO_GENERATOR.contains("YAML_AVAIL"),
            "the generator must remain JSON-only with exact required dependencies"
        );
        assert!(
            CARGO_GENERATOR_LOCK.contains("name = \"aiohttp\"")
                && CARGO_GENERATOR_LOCK.contains("name = \"tomlkit\"")
                && !CARGO_GENERATOR_LOCK.contains("name = \"pyyaml\""),
            "the checked-in uv lock must cover only required generator dependencies"
        );
        assert!(
            CARGO_GENERATOR.contains("[\"git\", \"fetch\", \"--depth=1\", \"origin\", commit]")
                && CARGO_GENERATOR
                    .contains("[\"git\", \"checkout\", \"--detach\", \"--force\", commit]")
                && CARGO_GENERATOR.contains("[\"git\", \"clean\", \"-ffdx\"]")
                && CARGO_GENERATOR.contains(
                    "[\"git\", \"submodule\", \"update\", \"--init\", \"--recursive\", \"--force\"]"
                )
                && CARGO_GENERATOR.contains(
                    "[\"git\", \"submodule\", \"foreach\", \"--recursive\", \"git clean -ffdx\"]"
                )
                && CARGO_GENERATOR.contains("\"--ignore-submodules=none\"")
                && CARGO_GENERATOR
                    .contains("[\"git\", \"submodule\", \"status\", \"--recursive\"]")
                && CARGO_GENERATOR.contains("if head != commit:")
                && !CARGO_GENERATOR.contains("head[:COMMIT_LEN]")
                && CARGO_GENERATOR.contains("child_manifest_directories(root_dir)")
                && GIT_MANIFEST_SCAN.contains("if child.name == \".git\":")
                && GIT_MANIFEST_SCAN.contains("child.is_dir(follow_symlinks=False)")
                && GIT_MANIFEST_SCAN_TEST
                    .contains("test_excludes_git_metadata_and_directory_symlinks")
                && GIT_MANIFEST_SCAN_TEST.contains("os.symlink"),
            "cached git metadata must be force-cleaned and fully verified before scanning"
        );
    }

    #[test]
    fn flatpak_tooling_checks_reject_path_and_offline_drift() {
        for script in [
            CARGO_SOURCES_SCRIPT.replace(" Cargo.lock ", " Wrong.lock "),
            CARGO_SOURCES_SCRIPT.replace("-o cargo-sources.json", "-o wrong.json"),
            CARGO_SOURCES_SCRIPT.replace(
                "flatpak/flatpak-cargo-generator.py",
                "flatpak/wrong-generator.py",
            ),
        ] {
            assert!(
                validate_flatpak_tooling(FLATPAK_MANIFEST, &script, JUSTFILE).is_err(),
                "a vendoring script path drift unexpectedly passed"
            );
        }

        let wrong_manifest_path = JUSTFILE.replace("'{{appid}}.json'", "'wrong.json'");
        assert!(
            validate_flatpak_tooling(FLATPAK_MANIFEST, CARGO_SOURCES_SCRIPT, &wrong_manifest_path,)
                .is_err(),
            "recipes that build a different manifest unexpectedly passed"
        );
        let no_offline_guard = JUSTFILE.replace(" --disable-download", "");
        assert!(
            validate_flatpak_tooling(FLATPAK_MANIFEST, CARGO_SOURCES_SCRIPT, &no_offline_guard,)
                .is_err(),
            "an offline recipe without --disable-download unexpectedly passed"
        );
        let unlocked_script = CARGO_SOURCES_SCRIPT.replace(" run --locked", " run");
        assert!(
            validate_flatpak_tooling(FLATPAK_MANIFEST, &unlocked_script, JUSTFILE).is_err(),
            "a vendoring wrapper that ignores its lockfile unexpectedly passed"
        );
        let unshared_build = JUSTFILE.replacen(
            "{{flatpak-builder-cmd}} build-dir '{{appid}}.json'",
            "flatpak-builder build-dir '{{appid}}.json'",
            1,
        );
        assert!(
            validate_flatpak_tooling(FLATPAK_MANIFEST, CARGO_SOURCES_SCRIPT, &unshared_build,)
                .is_err(),
            "a Flatpak recipe bypassing the shared builder command unexpectedly passed"
        );
    }

    #[test]
    fn cargo_sources_script_reports_a_missing_uv_with_an_install_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/usr/bin/dirname", dir.path().join("dirname")).unwrap();
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("flatpak/generate-cargo-sources.sh");
        let output = std::process::Command::new("/bin/sh")
            .arg(script)
            .env_clear()
            .env("PATH", dir.path())
            .output()
            .expect("the vendoring wrapper must be executable through /bin/sh");
        assert!(
            !output.status.success(),
            "missing uv unexpectedly succeeded"
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("'uv' not found"),
            "unexpected error: {stderr}"
        );
        assert!(
            stderr.contains("install it"),
            "missing install hint: {stderr}"
        );
    }

    fn validate_ci_workflows(
        rust_workflow: &str,
        flatpak_workflow: &str,
        manifest_text: &str,
        justfile: &str,
    ) -> Result<(), String> {
        let rust = sans_comments(rust_workflow);
        let flatpak = sans_comments(flatpak_workflow);
        let manifest: serde_json::Value =
            serde_json::from_str(manifest_text).map_err(|error| error.to_string())?;
        let require = |condition: bool, message: &str| {
            condition.then_some(()).ok_or_else(|| message.to_owned())
        };

        for command in just_recipe(justfile, "check")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            require(
                active_workflow_step_containing(&rust, command),
                &format!("rust.yml must run the `just check` command `{command}`"),
            )?;
        }
        for package in ["dbus", "libwayland-dev", "libxkbcommon-dev", "pkg-config"] {
            require(
                active_workflow_step_containing(&rust, package),
                &format!("rust.yml missing build/test package `{package}`"),
            )?;
        }
        require(
            active_workflow_step_containing(&rust, "dbus-run-session -- cargo test"),
            "rust.yml tests must use a hermetic session bus",
        )?;
        require(
            rust.contains("permissions:\n  contents: read"),
            "rust.yml must grant only read access to repository contents",
        )?;
        require(
            active_workflow_step_containing(&rust, CHECKOUT_ACTION)
                && active_workflow_step_containing(&rust, "persist-credentials: false"),
            "rust.yml checkout must be commit-pinned without persisted credentials",
        )?;
        for action in [RUST_TOOLCHAIN_ACTION, RUST_CACHE_ACTION] {
            require(
                active_workflow_step_containing(&rust, action),
                &format!("rust.yml must pin `{action}`"),
            )?;
        }
        require(
            !rust.contains("cargo-sources.json")
                && !rust
                    .lines()
                    .any(|line| line.split_whitespace().any(|word| word == "uv")),
            "Rust checks must not depend on Flatpak vendoring",
        )?;

        for (name, workflow) in [
            ("rust.yml", rust.as_str()),
            ("flatpak.yml", flatpak.as_str()),
        ] {
            for trigger in ["push:", "pull_request:"] {
                require(
                    workflow.contains(trigger),
                    &format!("{name} missing `{trigger}` trigger"),
                )?;
            }
        }

        let runtime_version = manifest["runtime-version"]
            .as_str()
            .ok_or_else(|| "manifest runtime-version".to_owned())?;
        require(
            FLATPAK_BUILDER_IMAGE.contains(&format!("freedesktop-{runtime_version}@sha256:"))
                && flatpak.contains(&format!("image: {FLATPAK_BUILDER_IMAGE}")),
            "flatpak.yml builder image must match the manifest runtime and pinned digest",
        )?;
        require(
            flatpak.contains("permissions:\n  contents: read"),
            "flatpak.yml must grant only read access to repository contents",
        )?;
        require(
            active_workflow_step_containing(&flatpak, CHECKOUT_ACTION)
                && active_workflow_step_containing(&flatpak, "persist-credentials: false"),
            "flatpak.yml checkout must be commit-pinned without persisted credentials",
        )?;
        for required in [
            format!("manifest-path: {APP_ID}.json"),
            "flatpak/generate-cargo-sources.sh".to_owned(),
            "python3 flatpak/test_git_manifest_scan.py".to_owned(),
            "Cargo.lock".to_owned(),
            CARGO_SOURCES_FILENAME.to_owned(),
            "astral-sh/setup-uv@08807647e7069bb48b6ef5acd8ec9567f424441b".to_owned(),
            "version: \"0.12.1\"".to_owned(),
            "appstreamcli validate --pedantic --explain --strict --no-net --override cid-contains-uppercase-letter=error data/io.github.ercling.cosmic-applet-daymural.metainfo.xml".to_owned(),
            FLATPAK_BUILDER_ACTION.to_owned(),
            format!("bundle: {}.flatpak", env!("CARGO_PKG_NAME")),
        ] {
            require(
                active_workflow_step_containing(&flatpak, &required),
                &format!("flatpak.yml missing `{required}`"),
            )?;
        }
        Ok(())
    }

    #[test]
    fn ci_workflows_match_the_manifest_vendoring_and_rust_check_contracts() {
        validate_ci_workflows(RUST_WORKFLOW, FLATPAK_WORKFLOW, FLATPAK_MANIFEST, JUSTFILE)
            .expect("the CI workflows must stay aligned with checked-in packaging and checks");
    }

    #[test]
    fn ci_workflow_checks_reject_runtime_and_source_generation_drift() {
        let wrong_runtime = FLATPAK_WORKFLOW.replace("freedesktop-25.08", "freedesktop-24.08");
        assert!(
            validate_ci_workflows(RUST_WORKFLOW, &wrong_runtime, FLATPAK_MANIFEST, JUSTFILE)
                .is_err(),
            "a builder image on a different runtime unexpectedly passed"
        );

        let no_generation = FLATPAK_WORKFLOW.replace(
            "run: flatpak/generate-cargo-sources.sh",
            "# run: flatpak/generate-cargo-sources.sh",
        );
        assert!(
            validate_ci_workflows(RUST_WORKFLOW, &no_generation, FLATPAK_MANIFEST, JUSTFILE)
                .is_err(),
            "a commented-out source generation step unexpectedly passed"
        );

        for insecure_workflow in [
            FLATPAK_WORKFLOW.replace(
                FLATPAK_BUILDER_IMAGE,
                "ghcr.io/flathub-infra/flatpak-github-actions:freedesktop-25.08",
            ),
            FLATPAK_WORKFLOW.replace(CHECKOUT_ACTION, "actions/checkout@v4"),
            FLATPAK_WORKFLOW.replace(
                FLATPAK_BUILDER_ACTION,
                "flatpak/flatpak-github-actions/flatpak-builder@v6",
            ),
            FLATPAK_WORKFLOW.replace(
                "permissions:\n  contents: read",
                "permissions:\n  contents: write",
            ),
            FLATPAK_WORKFLOW.replace("persist-credentials: false", "persist-credentials: true"),
            FLATPAK_WORKFLOW.replace(" --override cid-contains-uppercase-letter=error", ""),
        ] {
            assert!(
                validate_ci_workflows(
                    RUST_WORKFLOW,
                    &insecure_workflow,
                    FLATPAK_MANIFEST,
                    JUSTFILE
                )
                .is_err(),
                "an unpinned or over-privileged Flatpak workflow unexpectedly passed"
            );
        }

        for insecure_workflow in [
            RUST_WORKFLOW.replace(CHECKOUT_ACTION, "actions/checkout@v4"),
            RUST_WORKFLOW.replace(RUST_TOOLCHAIN_ACTION, "dtolnay/rust-toolchain@stable"),
            RUST_WORKFLOW.replace(RUST_CACHE_ACTION, "Swatinem/rust-cache@v2"),
            RUST_WORKFLOW.replace(
                "permissions:\n  contents: read",
                "permissions:\n  contents: write",
            ),
            RUST_WORKFLOW.replace("persist-credentials: false", "persist-credentials: true"),
        ] {
            assert!(
                validate_ci_workflows(
                    &insecure_workflow,
                    FLATPAK_WORKFLOW,
                    FLATPAK_MANIFEST,
                    JUSTFILE
                )
                .is_err(),
                "an unpinned or over-privileged Rust workflow unexpectedly passed"
            );
        }

        for disabled_rust in [
            RUST_WORKFLOW.replace(
                "      - name: Format\n        run: cargo fmt --check",
                "      - name: Format\n        if: false\n        run: cargo fmt --check",
            ),
            RUST_WORKFLOW.replace(
                "      - name: Test\n        run: dbus-run-session -- cargo test",
                "      - name: Test\n        if: false\n        run: dbus-run-session -- cargo test",
            ),
        ] {
            assert!(
                validate_ci_workflows(&disabled_rust, FLATPAK_WORKFLOW, FLATPAK_MANIFEST, JUSTFILE)
                    .is_err(),
                "a disabled Rust verification step unexpectedly passed"
            );
        }

        for disabled_flatpak in [
            FLATPAK_WORKFLOW.replace(
                "      - name: Test Git manifest traversal\n        run:",
                "      - name: Test Git manifest traversal\n        if: false\n        run:",
            ),
            FLATPAK_WORKFLOW.replace(
                "      - name: Generate cargo-sources.json\n        run:",
                "      - name: Generate cargo-sources.json\n        if: false\n        run:",
            ),
            FLATPAK_WORKFLOW.replace(
                "      - name: Validate AppStream metadata\n        run:",
                "      - name: Validate AppStream metadata\n        if: false\n        run:",
            ),
            FLATPAK_WORKFLOW.replace(
                "      - name: Build and bundle the Flatpak\n        uses:",
                "      - name: Build and bundle the Flatpak\n        if: false\n        uses:",
            ),
        ] {
            assert!(
                validate_ci_workflows(RUST_WORKFLOW, &disabled_flatpak, FLATPAK_MANIFEST, JUSTFILE)
                    .is_err(),
                "a disabled Flatpak verification step unexpectedly passed"
            );
        }
    }

    #[test]
    fn desktop_entry_stays_in_sync_with_the_app_id() {
        // The desktop entry is hand-maintained and never compiled; these are
        // the properties `desktop-file-validate` does not check for us.
        let mut comment_tags = std::collections::BTreeSet::new();
        let mut has_icon = false;
        for line in DESKTOP.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key == "Icon" {
                assert_eq!(value, format!("{APP_ID}-symbolic"));
                has_icon = true;
            }
            let Some(tag) = key
                .strip_prefix("Comment[")
                .and_then(|rest| rest.strip_suffix(']'))
            else {
                continue;
            };
            // Desktop-entry locale tags are POSIX (`pt_BR`, `zh_CN`), not the
            // BCP-47 spelling the Fluent catalogues use (`pt-BR`).
            assert!(
                !tag.contains('-'),
                "`Comment[{tag}]` is not a POSIX locale tag"
            );
            assert!(!value.trim().is_empty(), "`Comment[{tag}]` is empty");
            assert!(comment_tags.insert(tag), "`Comment[{tag}]` is duplicated");
        }
        assert!(has_icon, "the desktop entry must name an icon");
        assert!(!comment_tags.is_empty());
    }

    #[test]
    fn active_packaging_and_documentation_reject_legacy_identity_drift() {
        const LEGACY_APP_ID: &str = "io.github.ercling.CosmicBingWallpaper";
        const LEGACY_BINARY: &str = "cosmic-bing-wallpaper";

        let readme_without_documented_uninstall =
            README.replace(LEGACY_APP_ID, "").replace(LEGACY_BINARY, "");
        assert_eq!(
            README.matches(LEGACY_APP_ID).count(),
            3,
            "README must name the legacy desktop, icon, and Flatpak identities exactly"
        );
        assert_eq!(
            README.matches(LEGACY_BINARY).count(),
            1,
            "README must name the legacy native binary exactly once"
        );
        assert!(
            README.contains("flatpak uninstall --user io.github.ercling.CosmicBingWallpaper")
                && README.contains("$HOME/.local/bin/cosmic-bing-wallpaper")
                && README.contains("Old settings, catalogue state, thumbnails, leadership locks")
                && README.contains("coordination mailbox are not migrated")
                && README.contains("~/Pictures/BingWallpaper` image folder is\nkept")
                && README.contains("COSMIC Settings →\nDesktop → Panel"),
            "README must retain the complete legacy uninstall and migration warning"
        );

        for (name, text) in [
            ("desktop entry", DESKTOP),
            ("metainfo", METAINFO),
            ("Flatpak manifest", FLATPAK_MANIFEST),
            ("justfile", JUSTFILE),
            ("Rust workflow", RUST_WORKFLOW),
            ("Flatpak workflow", FLATPAK_WORKFLOW),
            (
                "README outside explicit uninstall instructions",
                &readme_without_documented_uninstall,
            ),
            ("AGENTS.md", AGENT_GUIDE),
            ("active Flatpak plan", ACTIVE_FLATPAK_PLAN),
        ] {
            for legacy in [
                LEGACY_APP_ID,
                LEGACY_BINARY,
                "cosmic_bing_wallpaper",
                "https://github.com/ercling/cosmic-wallpaper-applet",
            ] {
                assert!(
                    !text.contains(legacy),
                    "{name} still contains legacy identity `{legacy}`"
                );
            }
        }
        assert!(
            LEGACY_GUIDE.contains("20260807-cosmic-bing-wallpaper-applet.md")
                && LEGACY_GUIDE.matches("cosmic-bing-wallpaper").count() == 1
                && !LEGACY_GUIDE.contains("io.github.ercling.CosmicBingWallpaper")
                && !LEGACY_GUIDE.contains("cosmic_bing_wallpaper")
                && !LEGACY_GUIDE.contains("https://github.com/ercling/cosmic-wallpaper-applet"),
            "CLAUDE.md may retain the old name only in a completed historical plan filename"
        );
    }

    #[test]
    fn flatpak_metadata_stays_in_sync_with_the_crate_and_desktop_entry() {
        assert!(
            !env!("CARGO_PKG_DESCRIPTION").contains(['&', '<', '>']),
            "the Cargo description is embedded in XML and must remain unescaped"
        );
        assert_eq!(
            METAINFO,
            std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("data")
                    .join(format!("{APP_ID}.metainfo.xml"))
            )
            .expect("data/ ships metainfo named for APP_ID")
        );
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("data/icons")
                .join(format!("{APP_ID}-symbolic.svg"))
                .is_file(),
            "data/ must ship the app-ID-prefixed desktop icon"
        );
        flatpak_identity_is_consistent(DESKTOP, METAINFO, &format!("{APP_ID}.metainfo.xml"))
            .expect("the hand-written Flatpak identity fields must agree");
        assert!(
            native_install_rewrites_exec(JUSTFILE),
            "native install must rewrite the bare source Exec to its absolute binary path"
        );
        let commented_rewrite = JUSTFILE.replace(
            "    sed -i 's|^Exec=.*|Exec={{bin-dst}}|' {{desktop-dst}}",
            "    # sed -i 's|^Exec=.*|Exec={{bin-dst}}|' {{desktop-dst}}",
        );
        assert!(
            !native_install_rewrites_exec(&commented_rewrite),
            "a commented-out native Exec rewrite unexpectedly passed"
        );
    }

    #[test]
    fn flatpak_identity_checks_reject_desktop_and_metainfo_drift() {
        for invalid_exec in ["/usr/bin/daymural", "wrong-binary"] {
            let desktop = DESKTOP.replace("Exec=daymural", &format!("Exec={invalid_exec}"));
            let error = flatpak_identity_is_consistent(
                &desktop,
                METAINFO,
                &format!("{APP_ID}.metainfo.xml"),
            )
            .expect_err("an absolute or mismatched desktop Exec must fail");
            assert!(error.contains("desktop Exec"), "unexpected error: {error}");
        }

        let mismatches = [
            (
                "<id>io.github.ercling.cosmic-applet-daymural</id>",
                "<id>wrong.id</id>",
            ),
            (
                "io.github.ercling.cosmic-applet-daymural.desktop",
                "wrong.id.desktop",
            ),
            ("<binary>daymural</binary>", "<binary>wrong</binary>"),
            (
                "<id>com.system76.CosmicApplet</id>",
                "<id>com.example.NotAnApplet</id>",
            ),
            (
                "<project_license>GPL-3.0-only</project_license>",
                "<project_license>MIT</project_license>",
            ),
            (
                "<summary>Daymural: daily Microsoft Bing wallpaper applet for the COSMIC desktop</summary>",
                "<summary>Wrong summary</summary>",
            ),
            ("<release version=\"0.1.0\"", "<release version=\"9.9.9\""),
        ];
        for (valid, invalid) in mismatches {
            let metainfo = METAINFO.replacen(valid, invalid, 1);
            assert!(
                flatpak_identity_is_consistent(
                    DESKTOP,
                    &metainfo,
                    &format!("{APP_ID}.metainfo.xml")
                )
                .is_err(),
                "mismatched metainfo field unexpectedly passed: {invalid}"
            );
        }

        assert!(
            flatpak_identity_is_consistent(DESKTOP, METAINFO, "wrong.metainfo.xml").is_err(),
            "a metainfo filename that does not match APP_ID must fail"
        );
        let desktop = DESKTOP.replace(
            "Icon=io.github.ercling.cosmic-applet-daymural-symbolic",
            "Icon=wrong-symbolic",
        );
        assert!(
            flatpak_identity_is_consistent(&desktop, METAINFO, &format!("{APP_ID}.metainfo.xml"))
                .is_err(),
            "a mismatched desktop icon must fail"
        );
    }

    fn canonical_git_repo(source: &str) -> String {
        let source = source.strip_prefix("git+").unwrap_or(source);
        let without_fragment = source.split('#').next().unwrap_or(source);
        let without_query = without_fragment
            .split('?')
            .next()
            .unwrap_or(without_fragment)
            .trim_end_matches('/');
        let Some((scheme, authority_and_path)) = without_query.split_once("://") else {
            return without_query.trim_end_matches(".git").to_owned();
        };
        let (authority, path) = authority_and_path
            .split_once('/')
            .unwrap_or((authority_and_path, ""));
        let github = authority == "github.com";
        let scheme = if github { "https" } else { scheme };
        let path = if github {
            path.to_ascii_lowercase()
        } else {
            path.to_owned()
        };
        format!("{scheme}://{authority}/{}", path.trim_end_matches(".git"))
    }

    fn one_git_source_id_per_repo(lock: &str) -> Result<(), String> {
        let mut ids_by_repo: std::collections::BTreeMap<
            String,
            std::collections::BTreeSet<String>,
        > = std::collections::BTreeMap::new();
        for line in lock.lines() {
            let Some(source) = line
                .strip_prefix("source = \"git+")
                .and_then(|rest| rest.strip_suffix('"'))
            else {
                continue;
            };
            // `url?query#locked-commit`: the source id is everything before
            // the fragment; the repository is everything before the query.
            let id = source.split('#').next().expect("split has a first part");
            let repo = canonical_git_repo(source);
            ids_by_repo.entry(repo).or_default().insert(id.to_owned());
        }

        if ids_by_repo.is_empty() {
            return Err("the lock file names no git sources".to_owned());
        }
        for (repo, ids) in ids_by_repo {
            if ids.len() != 1 {
                return Err(format!(
                    "`{repo}` is named under {} source ids ({ids:?})",
                    ids.len()
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn every_git_dependency_resolves_through_one_source_id_per_repo() {
        // The Flatpak Cargo generator emits one source replacement per
        // canonical repository URL. If Cargo.lock also names that repository
        // with `?rev=...`, one source remains unvendored and an offline fetch
        // attempts the network. Keep libcosmic bare to match the transitive
        // cosmic-config declaration; Cargo.lock still pins the exact commit.
        const CARGO_LOCK: &str = include_str!("../Cargo.lock");
        one_git_source_id_per_repo(CARGO_LOCK)
            .expect("every git repository must use exactly one Cargo source id");

        let split_source_ids = r#"source = "git+https://example.invalid/repo?rev=abc#abc"
source = "git+https://example.invalid/repo#abc""#;
        let error = one_git_source_id_per_repo(split_source_ids)
            .expect_err("bare and `?rev=` spellings for one repo must fail");
        assert!(error.contains("2 source ids"), "unexpected error: {error}");

        for equivalent in [
            "git+https://github.com/Example/Repo.git/#abc",
            "git+git://github.com/example/repo#abc",
        ] {
            let lock = format!(
                "source = \"git+https://github.com/example/repo?rev=abc#abc\"\nsource = \"{equivalent}\""
            );
            let error = one_git_source_id_per_repo(&lock)
                .expect_err("equivalent GitHub spellings with split source ids must fail");
            assert!(error.contains("2 source ids"), "unexpected error: {error}");
        }
    }

    // -----------------------------------------------------------------
    // `Window::update` — the message loop's state transitions. Only the
    // branches that touch neither cosmic-bg's real config nor the real
    // state dir are exercised here (`wallpaper::apply` and the post-fetch
    // prune both write the *user's* files — see the note on
    // `wallpaper::apply`).
    // -----------------------------------------------------------------

    /// A catalogue entry that needs no file on disk (shuffle arming only
    /// counts entries).
    fn entry_in_memory(startdate: &str, name: &str) -> ImageEntry {
        ImageEntry {
            urlbase: format!("/th?id=OHR.{name}"),
            startdate: startdate.to_owned(),
            fullstartdate: format!("{startdate}0700"),
            title: format!("Title {name}"),
            copyright: "© Someone".to_owned(),
            copyrightlink: "https://example.com".to_owned(),
            filename: PathBuf::from(format!("/imgs/{startdate}-{name}_UHD.jpg")),
        }
    }

    fn window_with_images(count: usize) -> Window {
        let mut window = Window::default();
        for i in 0..count {
            window.catalogue.images.push(entry_in_memory(
                &format!("2026080{i}"),
                &format!("N{i}_ROW1"),
            ));
        }
        window
    }

    #[test]
    fn stale_refresh_ticks_are_ignored() {
        use cosmic::Application as _;

        let mut window = Window::default();
        // Arm once: the pending timer now carries generation 1.
        drop(window.schedule_refresh(Duration::from_secs(3600)));
        let armed = window.timer_generation;

        // A tick from a timer that a reschedule already replaced must not
        // start a fetch — otherwise every reschedule leaks a duplicate.
        drop(window.update(Message::RefreshDue(armed - 1)));
        assert!(!window.refresh_pending);

        drop(window.update(Message::RefreshDue(armed)));
        assert!(window.refresh_pending);

        // And a second trigger while one is in flight is debounced.
        window.timer_generation = armed;
        drop(window.update(Message::RefreshNow));
        assert!(window.refresh_pending);
    }

    #[test]
    fn stale_shuffle_ticks_are_ignored() {
        use cosmic::Application as _;

        let mut window = window_with_images(2);
        window.config.shuffle_enabled = true;
        drop(window.arm_shuffle());
        let armed = window.shuffle_generation;
        assert!(window.shuffle_armed);

        // A tick from a re-armed-over timer leaves the pending one alone;
        // consuming it would let a manual navigation's countdown reset be
        // undone by the timer it replaced.
        drop(window.update(Message::ShuffleDue(armed - 1)));
        assert!(window.shuffle_armed);
        assert_eq!(window.shuffle_generation, armed);
    }

    #[test]
    fn shuffle_runs_only_while_enabled_and_with_two_images() {
        use cosmic::Application as _;

        // Enabled but nothing to rotate through: nothing armed.
        let mut window = Window::default();
        drop(window.update(Message::SetShuffleEnabled(true)));
        assert!(window.config.shuffle_enabled, "the setting is adopted");
        assert!(!window.shuffle_armed);

        // Two images: the countdown starts.
        let mut window = window_with_images(2);
        drop(window.update(Message::SetShuffleEnabled(true)));
        assert!(window.shuffle_armed);

        // Picking an interval restarts it at the new length.
        let generation = window.shuffle_generation;
        drop(window.update(Message::SetShuffleInterval(0)));
        assert_eq!(window.config.shuffle_interval_secs, 1_800);
        assert!(window.shuffle_armed);
        assert!(window.shuffle_generation > generation);

        // Switching off disarms (and invalidates the pending tick).
        let generation = window.shuffle_generation;
        drop(window.update(Message::SetShuffleEnabled(false)));
        assert!(!window.shuffle_armed);
        assert!(window.shuffle_generation > generation);
    }

    #[test]
    fn config_updates_normalize_and_restart_the_shuffle_countdown() {
        use cosmic::Application as _;

        let mut window = window_with_images(2);
        window.config.shuffle_enabled = true;
        drop(window.arm_shuffle());
        let generation = window.shuffle_generation;

        // An external edit arrives raw: an unsupported retention must be
        // snapped before it can drive prune/fetch, and a changed interval
        // restarts the countdown at the new length.
        let external = AppletConfig {
            shuffle_enabled: true,
            shuffle_interval_secs: 3_600,
            retention_days: 1,
            ..Default::default()
        };
        drop(window.update(Message::ConfigUpdated(external)));

        assert_eq!(
            window.config.retention_days,
            AppletConfig::default().retention_days,
            "a hand-edited retention must normalize before it is adopted"
        );
        assert_eq!(window.config.shuffle_interval_secs, 3_600);
        assert!(window.shuffle_generation > generation);

        // The same config echoed back (our own write) changes nothing.
        let generation = window.shuffle_generation;
        let echoed = window.config.clone();
        drop(window.update(Message::ConfigUpdated(echoed)));
        assert_eq!(window.shuffle_generation, generation);
    }

    #[test]
    fn a_failed_refresh_records_the_error_and_backs_off() {
        use cosmic::Application as _;

        let mut window = Window::default();
        drop(window.update(Message::RefreshNow));
        assert!(window.refresh_pending);
        let generation = window.timer_generation;

        drop(
            window.update(Message::RefreshFinished(Err(RefreshError::Network(
                "boom".to_owned(),
            )))),
        );

        assert!(!window.refresh_pending, "the pipeline is no longer running");
        assert!(matches!(window.last_error, Some(RefreshError::Network(_))));
        assert!(window.last_updated.is_none(), "no successful fetch yet");
        assert!(
            window.timer_generation > generation,
            "the retry timer replaces the pending one"
        );
    }

    #[test]
    fn non_leader_drops_current_generation_automatic_messages() {
        use cosmic::Application as _;

        let mut window = Window {
            leadership: Leadership::forced(false),
            timer_generation: 7,
            shuffle_generation: 11,
            shuffle_armed: true,
            lock_poke_generation: 13,
            ..Window::default()
        };
        window.config.shuffle_enabled = true;

        drop(window.update(Message::RefreshDue(7)));
        assert!(!window.refresh_pending, "a follower cannot start a fetch");
        assert_eq!(window.timer_generation, 7);

        drop(window.update(Message::ShuffleDue(11)));
        assert!(
            window.shuffle_armed,
            "a follower cannot consume a shuffle tick"
        );
        assert_eq!(window.shuffle_generation, 11);

        drop(window.update(Message::LockEvent(lockwatch::LockEvent::Locked)));
        assert_eq!(window.lock_poke_generation, 13);
        assert!(window.due_lock_poke(13).is_none());
    }

    #[test]
    fn non_leader_refresh_completions_clear_the_producer_without_rearming_or_merging() {
        use cosmic::Application as _;

        for result in [
            Ok(RefreshBatch {
                fetched: vec![entry_in_memory("20260808", "Fresh_ROW1")],
                ineligible: Vec::new(),
                anchor: "202608080700".to_owned(),
                fallback: None,
                thumbnails_deferred: false,
            }),
            Err(RefreshError::Network("offline".to_owned())),
        ] {
            let existing = entry_in_memory("20260807", "Existing_ROW1");
            let mut window = Window {
                leadership: Leadership::forced(false),
                catalogue: Catalogue {
                    images: vec![existing.clone()],
                },
                refresh_pending: true,
                timer_generation: 9,
                last_error: Some(RefreshError::Disk("old".to_owned())),
                ..Window::default()
            };

            drop(window.update(Message::RefreshFinished(result)));

            assert!(!window.refresh_pending);
            assert_eq!(window.timer_generation, 9, "no retry or success timer");
            assert_eq!(window.catalogue.images, vec![existing], "no merge/prune");
            assert!(matches!(window.last_error, Some(RefreshError::Disk(_))));
        }
    }

    fn peer_follower(request: u64) -> Window {
        Window {
            leadership: Leadership::forced(false),
            requested_peer_refresh: Some(request),
            refresh_pending: true,
            peer_refresh_timeout_generation: request,
            ..Window::default()
        }
    }

    fn follower_reload(catalogue: Catalogue, live: wallpaper::CurrentWallpaper) -> NonLeaderReload {
        NonLeaderReload {
            catalogue,
            provenance: Provenance::Loaded,
            live,
        }
    }

    fn rebuilt_reload(live: wallpaper::CurrentWallpaper) -> NonLeaderReload {
        NonLeaderReload {
            catalogue: Catalogue {
                images: vec![entry_in_memory("20260820", "Rebuilt_ROW1")],
            },
            provenance: Provenance::Rebuilt,
            live,
        }
    }

    fn follower_with_mailbox(dir: &Path) -> Window {
        let context = cosmic_config::Config::with_custom_path(
            APP_ID,
            AppletConfig::VERSION,
            dir.join("config"),
        )
        .unwrap();
        Window {
            leadership: Leadership::forced(false),
            coordination_context: Some(context),
            coordination_state_dir: dir.join("state"),
            ..Window::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_follower_that_rebuilt_its_catalogue_asks_the_leader_to_repair_it() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        let dir = tempfile::tempdir().unwrap();
        let mut follower = Window {
            metadata_repair_due: true,
            catalogue: Catalogue {
                images: vec![entry_in_memory("20260820", "Rebuilt_ROW1")],
            },
            ..follower_with_mailbox(dir.path())
        };
        let context = follower.coordination_context.clone().unwrap();

        let startup = follower.arm_initial_duties(wallpaper::CurrentWallpaper::NoFile);
        assert!(!follower.metadata_repair_due, "consumed by the arming");
        assert!(
            follower.peer_refresh_write_pending,
            "the mailbox request is persisting"
        );
        assert!(!follower.refresh_pending, "not until the persist lands");
        assert!(
            !follower.thumbnail_pass_pending,
            "a follower produces nothing"
        );
        assert_eq!(
            startup.units(),
            2,
            "takeover retry plus the mailbox request"
        );

        let mut messages = app_messages(startup).await;
        let request = messages
            .iter()
            .position(|message| matches!(message, Message::PeerRefreshRequested(_)))
            .expect("the counter persist completes");
        let timeout = follower.update(messages.swap_remove(request));
        assert_eq!(follower.requested_peer_refresh, Some(1));
        assert!(follower.refresh_pending);
        assert_eq!(
            CoordinationConfig::get_entry(&context)
                .unwrap()
                .refresh_request,
            1,
            "the leader sees an ordinary outstanding request"
        );

        // The leader acknowledges: the reload it triggers never asks again,
        // whatever it finds — an offline leader's repair fetch fails and is
        // acknowledged as such, and polling it per acknowledgement would
        // never end.
        let reload = follower.settle_peer_refresh(PeerRefreshCompletion {
            request: 1,
            outcome: PeerRefreshOutcome::Network,
        });
        assert_eq!(reload.units(), 1);
        assert!(!follower.non_leader_reload_repairs);
        let again = follower.finish_non_leader_reload(
            follower.non_leader_reload_generation,
            Ok(rebuilt_reload(wallpaper::CurrentWallpaper::NoFile)),
        );
        assert_eq!(again.units(), 0, "settled: no second request");
        assert!(!follower.peer_refresh_write_pending);
        drop(timeout);
    }

    #[test]
    fn a_follower_empty_rebuild_or_loaded_catalogue_asks_for_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // An empty rebuild (empty folder) has nothing to repair.
        let mut empty = Window {
            metadata_repair_due: true,
            ..follower_with_mailbox(dir.path())
        };
        let startup = empty.arm_initial_duties(wallpaper::CurrentWallpaper::NoFile);
        assert!(!empty.peer_refresh_write_pending);
        assert_eq!(startup.units(), 1, "takeover retry only");

        // A loaded reload asks for nothing either.
        let mut loaded = follower_with_mailbox(dir.path());
        drop(loaded.request_non_leader_reload(true));
        let task = loaded.finish_non_leader_reload(
            loaded.non_leader_reload_generation,
            Ok(follower_reload(
                Catalogue {
                    images: vec![entry_in_memory("20260820", "Loaded_ROW1")],
                },
                wallpaper::CurrentWallpaper::NoFile,
            )),
        );
        assert_eq!(task.units(), 0);
        assert!(!loaded.peer_refresh_write_pending);
    }

    #[test]
    fn a_rebuilt_popup_reload_requests_a_repair_once_per_pending_request() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut follower = follower_with_mailbox(dir.path());

        // Popup open: the reload may repair, and a rebuilt result asks.
        let open = follower.update(Message::TogglePopup);
        assert_eq!(open.units(), 2, "popup plus the guarded reload");
        assert!(follower.non_leader_reload_repairs);
        let request = follower.finish_non_leader_reload(
            follower.non_leader_reload_generation,
            Ok(rebuilt_reload(wallpaper::CurrentWallpaper::NoFile)),
        );
        assert_eq!(request.units(), 1, "one mailbox request");
        assert!(follower.peer_refresh_write_pending);
        assert_eq!(
            follower.catalogue.images.len(),
            1,
            "the reload is adopted too"
        );

        // Coalescing: while the persist is pending, another rebuilt reload
        // asks nothing more ...
        drop(follower.request_non_leader_reload(true));
        let dup = follower.finish_non_leader_reload(
            follower.non_leader_reload_generation,
            Ok(rebuilt_reload(wallpaper::CurrentWallpaper::NoFile)),
        );
        assert_eq!(dup.units(), 0);

        // ... nor while the request waits for its acknowledgement.
        follower.peer_refresh_write_pending = false;
        follower.requested_peer_refresh = Some(4);
        follower.refresh_pending = true;
        drop(follower.request_non_leader_reload(true));
        let dup = follower.finish_non_leader_reload(
            follower.non_leader_reload_generation,
            Ok(rebuilt_reload(wallpaper::CurrentWallpaper::NoFile)),
        );
        assert_eq!(dup.units(), 0);
        assert!(!follower.peer_refresh_write_pending);
    }

    #[tokio::test(start_paused = true)]
    async fn a_follower_repair_request_retries_after_the_acknowledgement_timeout() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut follower = Window {
            metadata_repair_due: true,
            catalogue: Catalogue {
                images: vec![entry_in_memory("20260820", "Rebuilt_ROW1")],
            },
            ..follower_with_mailbox(dir.path())
        };

        let startup = follower.arm_initial_duties(wallpaper::CurrentWallpaper::NoFile);
        let mut messages = app_messages(startup).await;
        let request = messages
            .iter()
            .position(|message| matches!(message, Message::PeerRefreshRequested(_)))
            .unwrap();
        let timeout = follower.update(messages.swap_remove(request));
        assert_eq!(follower.requested_peer_refresh, Some(1));

        // No leader covers it: the established timeout fires and reloads,
        // with repair allowed — that reload is the retry point.
        let mut timeout_messages = app_messages(timeout).await;
        assert_eq!(timeout_messages.len(), 1);
        let reload = follower.update(timeout_messages.pop().unwrap());
        assert_eq!(reload.units(), 1);
        assert!(!follower.refresh_pending);
        assert_eq!(follower.requested_peer_refresh, None);
        assert!(follower.non_leader_reload_repairs);

        // Still rebuilt (the leader never wrote a catalogue): ask again.
        let retry = follower.finish_non_leader_reload(
            follower.non_leader_reload_generation,
            Ok(rebuilt_reload(wallpaper::CurrentWallpaper::NoFile)),
        );
        assert_eq!(retry.units(), 1);
        assert!(follower.peer_refresh_write_pending);
        let mut messages = app_messages(retry).await;
        drop(follower.update(messages.pop().unwrap()));
        assert_eq!(follower.requested_peer_refresh, Some(2), "a newer counter");
        assert!(follower.refresh_pending);
    }

    #[test]
    fn non_leader_popup_batches_reload_without_changing_popup_ledger() {
        use cosmic::Application as _;

        let mut follower = Window {
            leadership: Leadership::forced(false),
            dropdowns_open: 2,
            ..Window::default()
        };
        let task = follower.update(Message::TogglePopup);
        assert_eq!(
            task.units(),
            2,
            "popup action and blocking reload are batched"
        );
        assert_eq!(follower.non_leader_reload_generation, 1);
        assert_eq!(
            follower.dropdowns_open, 2,
            "opening does not edit the ledger"
        );
        assert_eq!(
            follower.popup, None,
            "the runtime still owns popup creation"
        );

        let mut leader = Window::default();
        let task = leader.update(Message::TogglePopup);
        assert_eq!(task.units(), 1, "a leader trusts its in-memory catalogue");
        assert_eq!(leader.non_leader_reload_generation, 0);
    }

    #[tokio::test]
    async fn popup_and_peer_settlement_drain_the_real_injected_reload_task() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let images_dir = dir.path().join("images");
        let catalogue_path = dir.path().join("state/catalogue.json");
        std::fs::create_dir_all(&images_dir).unwrap();
        std::fs::create_dir_all(catalogue_path.parent().unwrap()).unwrap();
        let first = entry_on_disk(&images_dir, "20260819", "Popup_ROW1");
        Catalogue {
            images: vec![first.clone()],
        }
        .save(&catalogue_path)
        .unwrap();
        let first_live = first.filename.clone();
        let mut follower = Window {
            leadership: Leadership::forced(false),
            test_snapshot_inputs: Some(TestSnapshotInputs {
                catalogue_path: catalogue_path.clone(),
                images_dir: images_dir.clone(),
                live: wallpaper::CurrentWallpaper::File(first_live.clone()),
            }),
            ..Window::default()
        };

        let task = follower.update(Message::TogglePopup);
        let mut outputs = app_messages(task).await;
        assert_eq!(
            outputs.len(),
            1,
            "surface action is ignored, reload completes"
        );
        drop(follower.update(outputs.pop().unwrap()));
        assert_eq!(follower.non_leader_reload_generation, 1);
        assert_eq!(follower.catalogue.images, vec![first]);
        assert_eq!(follower.current, Some(first_live));

        let second = entry_on_disk(&images_dir, "20260820", "Peer_ROW2");
        Catalogue {
            images: vec![second.clone()],
        }
        .save(&catalogue_path)
        .unwrap();
        let second_live = second.filename.clone();
        follower.test_snapshot_inputs.as_mut().unwrap().live =
            wallpaper::CurrentWallpaper::File(second_live.clone());
        follower.requested_peer_refresh = Some(7);
        follower.refresh_pending = true;
        let reload = follower.settle_peer_refresh(PeerRefreshCompletion {
            request: 7,
            outcome: PeerRefreshOutcome::Success,
        });
        let mut outputs = app_messages(reload).await;
        assert_eq!(outputs.len(), 1);
        drop(follower.update(outputs.pop().unwrap()));
        assert_eq!(follower.non_leader_reload_generation, 2);
        assert_eq!(follower.catalogue.images, vec![second]);
        assert_eq!(follower.current, Some(second_live));
    }

    #[test]
    fn pending_peer_refresh_suppresses_popup_reload() {
        use cosmic::Application as _;

        for mut follower in [
            Window {
                leadership: Leadership::forced(false),
                peer_refresh_write_pending: true,
                ..Window::default()
            },
            Window {
                leadership: Leadership::forced(false),
                requested_peer_refresh: Some(4),
                refresh_pending: true,
                ..Window::default()
            },
        ] {
            let task = follower.update(Message::TogglePopup);
            assert_eq!(task.units(), 1, "popup creation is never delayed");
            assert_eq!(follower.non_leader_reload_generation, 0);
        }
    }

    #[test]
    fn non_leader_reload_adopts_added_and_dropped_entries_and_live_current() {
        let dropped = entry_in_memory("20260806", "Dropped_ROW1");
        let added = entry_in_memory("20260808", "Added_ROW2");
        let live = PathBuf::from("/images/live.jpg");
        let mut follower = Window {
            leadership: Leadership::forced(false),
            catalogue: Catalogue {
                images: vec![dropped],
            },
            current: Some(PathBuf::from("/images/stale.jpg")),
            non_leader_reload_generation: 3,
            ..Window::default()
        };

        drop(follower.finish_non_leader_reload(
            3,
            Ok(follower_reload(
                Catalogue {
                    images: vec![added.clone()],
                },
                wallpaper::CurrentWallpaper::File(live.clone()),
            )),
        ));

        assert_eq!(follower.catalogue.images, vec![added]);
        assert_eq!(follower.current, Some(live));
    }

    #[test]
    fn peer_refresh_settlement_reuses_the_guarded_reload() {
        let added = entry_in_memory("20260808", "Peer_ROW2");
        let live = PathBuf::from("/images/peer-live.jpg");
        let mut follower = peer_follower(5);

        let reload_task = follower.settle_peer_refresh(PeerRefreshCompletion {
            request: 5,
            outcome: PeerRefreshOutcome::Success,
        });
        assert_eq!(reload_task.units(), 1);
        assert_eq!(follower.non_leader_reload_generation, 1);
        drop(follower.finish_non_leader_reload(
            1,
            Ok(follower_reload(
                Catalogue {
                    images: vec![added.clone()],
                },
                wallpaper::CurrentWallpaper::File(live.clone()),
            )),
        ));

        assert_eq!(follower.catalogue.images, vec![added]);
        assert_eq!(follower.current, Some(live));
    }

    #[test]
    fn stale_non_leader_reload_cannot_overwrite_apply_or_takeover_state() {
        let stale_entry = entry_in_memory("20260801", "Stale_ROW1");
        let authoritative = entry_in_memory("20260809", "Authoritative_ROW2");

        let mut after_apply = Window {
            leadership: Leadership::forced(false),
            non_leader_reload_generation: 1,
            catalogue: Catalogue {
                images: vec![authoritative.clone()],
            },
            ..Window::default()
        };
        let applied = PathBuf::from("/images/applied.jpg");
        drop(after_apply.finish_manual_apply(applied.clone()));
        drop(after_apply.finish_non_leader_reload(
            1,
            Ok(follower_reload(
                Catalogue {
                    images: vec![stale_entry.clone()],
                },
                wallpaper::CurrentWallpaper::NoFile,
            )),
        ));
        assert_eq!(after_apply.catalogue.images, vec![authoritative.clone()]);
        assert_eq!(after_apply.current, Some(applied));

        let mut after_takeover = Window {
            leadership: Leadership::forced(true),
            leader_readiness: LeaderReadiness::Ready,
            non_leader_reload_generation: 2,
            catalogue: Catalogue {
                images: vec![authoritative.clone()],
            },
            current: Some(PathBuf::from("/images/leader.jpg")),
            ..Window::default()
        };
        drop(after_takeover.finish_non_leader_reload(
            2,
            Ok(follower_reload(
                Catalogue {
                    images: vec![stale_entry],
                },
                wallpaper::CurrentWallpaper::NoFile,
            )),
        ));
        assert_eq!(after_takeover.catalogue.images, vec![authoritative]);
        assert_eq!(
            after_takeover.current,
            Some(PathBuf::from("/images/leader.jpg"))
        );
    }

    #[test]
    fn non_leader_reload_failure_and_catalogue_fallback_are_safe_and_read_only() {
        let mut follower = Window {
            leadership: Leadership::forced(false),
            non_leader_reload_generation: 1,
            catalogue: Catalogue {
                images: vec![entry_in_memory("20260807", "Kept_ROW1")],
            },
            current: Some(PathBuf::from("/images/kept.jpg")),
            ..Window::default()
        };
        let before = follower.catalogue.clone();
        drop(follower.finish_non_leader_reload(1, Err("join failed".to_owned())));
        assert_eq!(follower.catalogue, before);
        assert_eq!(follower.current, Some(PathBuf::from("/images/kept.jpg")));

        for corrupt in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let images = dir.path().join("images");
            let catalogue_path = dir.path().join("catalogue.json");
            std::fs::create_dir_all(&images).unwrap();
            let rebuilt = entry_on_disk(&images, "20260808", "Rebuilt_ROW2");
            if corrupt {
                std::fs::write(&catalogue_path, b"not json").unwrap();
            }
            let before = file_snapshot(dir.path());

            let reload = read_non_leader_reload(
                &catalogue_path,
                &images,
                wallpaper::CurrentWallpaper::Unknown,
            );

            assert_eq!(reload.catalogue.images.len(), 1);
            assert_eq!(reload.catalogue.images[0].filename, rebuilt.filename);
            assert_eq!(
                reload.provenance,
                Provenance::Rebuilt,
                "a missing or corrupt catalogue is reported as rebuilt"
            );
            assert_eq!(file_snapshot(dir.path()), before, "reload writes nothing");

            // Persisted, the same folder reloads as `Loaded`.
            reload.catalogue.save(&catalogue_path).unwrap();
            let reloaded = read_non_leader_reload(
                &catalogue_path,
                &images,
                wallpaper::CurrentWallpaper::Unknown,
            );
            assert_eq!(reloaded.provenance, Provenance::Loaded);
        }
    }

    #[test]
    fn peer_refresh_outcomes_settle_requester_status_and_request_reload() {
        use cosmic::Application as _;

        for (outcome, expected) in [
            (PeerRefreshOutcome::Success, "success"),
            (PeerRefreshOutcome::Network, "network"),
            (PeerRefreshOutcome::Disk, "disk"),
        ] {
            let mut window = peer_follower(4);
            let timeout_generation = window.peer_refresh_timeout_generation;
            drop(
                window.update(Message::CoordinationUpdated(CoordinationConfig {
                    refresh_request: 4,
                    refresh_completion: PeerRefreshCompletion {
                        request: 4,
                        outcome,
                    },
                    ..Default::default()
                })),
            );

            assert!(!window.refresh_pending);
            assert_eq!(window.requested_peer_refresh, None);
            assert!(window.peer_refresh_timeout_generation > timeout_generation);
            assert_eq!(window.non_leader_reload_generation, 1);
            match expected {
                "success" => {
                    assert!(window.last_updated.is_some());
                    assert!(window.last_error.is_none());
                }
                "network" => assert!(matches!(window.last_error, Some(RefreshError::Network(_)))),
                "disk" => assert!(matches!(window.last_error, Some(RefreshError::Disk(_)))),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn covering_completion_settles_multiple_requesters() {
        use cosmic::Application as _;

        let completion = PeerRefreshCompletion {
            request: 9,
            outcome: PeerRefreshOutcome::Success,
        };
        let mut first = peer_follower(7);
        let mut second = peer_follower(9);
        for window in [&mut first, &mut second] {
            drop(
                window.update(Message::CoordinationUpdated(CoordinationConfig {
                    refresh_request: 9,
                    refresh_completion: completion,
                    ..Default::default()
                })),
            );
            assert!(!window.refresh_pending);
            assert_eq!(window.non_leader_reload_generation, 1);
        }
    }

    #[test]
    fn leader_coalesces_requests_arriving_during_one_fetch() {
        let mut window = Window {
            coordination: CoordinationConfig {
                refresh_request: 2,
                ..Default::default()
            },
            ..Window::default()
        };

        drop(window.consume_peer_refresh_request_over(wallpaper::CurrentWallpaper::NoFile));
        assert!(window.refresh_pending);
        assert_eq!(window.peer_refresh_request, Some(2));

        window.coordination.refresh_request = 5;
        let second = window.consume_peer_refresh_request_over(wallpaper::CurrentWallpaper::NoFile);
        assert_eq!(second.units(), 0, "no second fetch is started");
        assert!(window.refresh_pending);
        assert_eq!(window.peer_refresh_request, Some(5));
    }

    #[tokio::test]
    async fn coordination_update_reads_live_wallpaper_off_update_and_guards_completion() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = Window {
            test_snapshot_inputs: Some(TestSnapshotInputs {
                catalogue_path: dir.path().join("catalogue.json"),
                images_dir: dir.path().join("images"),
                live: wallpaper::CurrentWallpaper::NoFile,
            }),
            ..Window::default()
        };
        let read = window.update(Message::CoordinationUpdated(CoordinationConfig {
            refresh_request: 3,
            ..Default::default()
        }));
        assert_eq!(read.units(), 1);
        assert_eq!(window.peer_refresh_request, Some(3));
        assert_eq!(window.peer_refresh_live_read, Some(3));
        assert!(
            !window.refresh_pending,
            "update only schedules the live read"
        );

        let mut messages = app_messages(read).await;
        let refresh = window.update(messages.pop().unwrap());
        assert!(window.refresh_pending);
        assert_eq!(window.peer_refresh_request, Some(3));
        assert_eq!(refresh.units(), 1);
        assert_eq!(window.peer_refresh_live_read, None);

        let mut stale = Window {
            test_snapshot_inputs: window.test_snapshot_inputs.clone(),
            ..Window::default()
        };
        stale.coordination.refresh_request = 4;
        assert_eq!(
            stale
                .update(Message::PeerRefreshLiveRead {
                    request: 3,
                    live: wallpaper::CurrentWallpaper::NoFile,
                })
                .units(),
            0
        );
        assert!(!stale.refresh_pending);
        stale.leadership = Leadership::forced(false);
        assert_eq!(
            stale
                .update(Message::PeerRefreshLiveRead {
                    request: 4,
                    live: wallpaper::CurrentWallpaper::NoFile,
                })
                .units(),
            0
        );
    }

    #[tokio::test]
    async fn duplicate_live_reads_and_failed_ack_never_refetch_a_covered_request() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = Window::default();
        window.coordination.refresh_request = 7;
        assert_eq!(window.consume_peer_refresh_request().units(), 1);
        assert_eq!(
            window.consume_peer_refresh_request().units(),
            0,
            "one live read per outstanding request"
        );

        let first = window.update(Message::PeerRefreshLiveRead {
            request: 7,
            live: wallpaper::CurrentWallpaper::NoFile,
        });
        assert_eq!(first.units(), 1);
        assert!(window.refresh_pending);
        let completion_tasks = window.finish_refresh(Err(RefreshError::Network("offline".into())));
        assert_eq!(
            completion_tasks.units(),
            1,
            "contextless fixture only rearms"
        );
        assert_eq!(window.peer_refresh_covered, 7);
        assert!(!window.refresh_pending);

        let context = cosmic_config::Config::with_custom_path(
            APP_ID,
            AppletConfig::VERSION,
            dir.path().join("config"),
        )
        .unwrap();
        let invalid_state = dir.path().join("not-a-directory");
        std::fs::write(&invalid_state, b"file").unwrap();
        window.coordination_context = Some(context);
        window.coordination_state_dir = invalid_state;
        let mut messages =
            app_messages(window.record_peer_refresh_completion(7, PeerRefreshOutcome::Network))
                .await;
        let Message::PeerRefreshCompletionWritten { result, .. } = messages.pop().unwrap() else {
            panic!("expected completion write result");
        };
        assert!(result.is_err(), "the acknowledgement persist really failed");
        drop(window.update(Message::PeerRefreshCompletionWritten {
            completion: PeerRefreshCompletion {
                request: 7,
                outcome: PeerRefreshOutcome::Network,
            },
            result,
        }));
        assert_eq!(
            window
                .update(Message::PeerRefreshLiveRead {
                    request: 7,
                    live: wallpaper::CurrentWallpaper::NoFile,
                })
                .units(),
            0,
            "delayed duplicate completion is stale"
        );
        assert_eq!(window.consume_peer_refresh_request().units(), 0);
        assert!(!window.refresh_pending);
    }

    #[test]
    fn request_reserved_during_live_read_is_acknowledged_by_intervening_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let context = cosmic_config::Config::with_custom_path(
            APP_ID,
            AppletConfig::VERSION,
            dir.path().join("config"),
        )
        .unwrap();
        let mut window = Window {
            coordination_context: Some(context),
            coordination_state_dir: dir.path().join("state"),
            ..Window::default()
        };
        window.coordination.refresh_request = 11;
        let delayed_read = window.consume_peer_refresh_request();
        assert_eq!(delayed_read.units(), 1);
        assert_eq!(window.peer_refresh_request, Some(11));

        let intervening = window.start_refresh_over(wallpaper::CurrentWallpaper::NoFile);
        assert_eq!(intervening.units(), 1);
        assert!(window.refresh_pending);
        let completion_tasks = window.finish_refresh(Err(RefreshError::Network("offline".into())));
        assert_eq!(
            completion_tasks.units(),
            2,
            "retry timer plus acknowledgement write"
        );
        assert_eq!(window.peer_refresh_request, None);
        assert_eq!(window.peer_refresh_covered, 11);

        assert_eq!(
            window
                .finish_peer_refresh_live_read(11, wallpaper::CurrentWallpaper::NoFile)
                .units(),
            0,
            "late live read cannot start a duplicate fetch"
        );
        assert!(!window.refresh_pending);
    }

    #[test]
    fn late_coordination_snapshots_never_regress_or_repeat_leader_work() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let newer = CoordinationConfig {
            refresh_request: 10,
            refresh_completion: PeerRefreshCompletion {
                request: 10,
                outcome: PeerRefreshOutcome::Success,
            },
            apply_notice: Some(PeerApplyNotice {
                generation: 5,
                path: PathBuf::from("/new.jpg"),
            }),
        };
        let first = window.update(Message::CoordinationUpdated(newer.clone()));
        assert_eq!(first.units(), 1, "only apply validation is armed");
        assert!(!window.refresh_pending);

        let stale = CoordinationConfig {
            refresh_request: 9,
            refresh_completion: PeerRefreshCompletion {
                request: 9,
                outcome: PeerRefreshOutcome::Network,
            },
            apply_notice: Some(PeerApplyNotice {
                generation: 4,
                path: PathBuf::from("/old.jpg"),
            }),
        };
        let duplicate = window.update(Message::CoordinationUpdated(stale));
        assert_eq!(duplicate.units(), 0, "late evidence arms no duplicate work");
        assert_eq!(window.coordination, newer);
        assert_eq!(window.peer_apply_notice_generation, 5);
        assert!(!window.refresh_pending);
    }

    #[test]
    fn follower_config_confirmation_is_generation_guarded_and_not_inline() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let (context, _) = takeover_contexts(&dir.path().join("config"));
        let mut window = Window {
            leadership: Leadership::forced(false),
            config_context: Some(context),
            ..Window::default()
        };
        let mut first = window.config.clone();
        first.retention_days = 30;
        let task = window.update(Message::ConfigUpdated(first.clone()));
        assert_eq!(task.units(), 1);
        assert_eq!(
            window.config.retention_days, 8,
            "payload is not read inline"
        );
        let first_generation = window.config_confirmation_generation;

        let mut second = first.clone();
        second.retention_days = 3;
        drop(window.update(Message::ConfigUpdated(second.clone())));
        let second_generation = window.config_confirmation_generation;
        drop(window.update(Message::ConfigConfirmed {
            generation: first_generation,
            was_active_leader: false,
            config: first,
        }));
        assert_eq!(window.config.retention_days, 8, "stale completion dropped");
        drop(window.update(Message::ConfigConfirmed {
            generation: second_generation,
            was_active_leader: false,
            config: second,
        }));
        assert_eq!(window.config.retention_days, 3);
    }

    #[test]
    fn stale_completion_and_timeout_cannot_clear_a_newer_request() {
        use cosmic::Application as _;

        let mut window = peer_follower(8);
        drop(
            window.update(Message::CoordinationUpdated(CoordinationConfig {
                refresh_request: 8,
                refresh_completion: PeerRefreshCompletion {
                    request: 7,
                    outcome: PeerRefreshOutcome::Success,
                },
                ..Default::default()
            })),
        );
        assert!(window.refresh_pending, "non-covering completion is ignored");

        drop(window.update(Message::PeerRefreshTimeout {
            generation: 7,
            request: 7,
        }));
        assert!(window.refresh_pending, "stale timeout is ignored");
        assert_eq!(window.requested_peer_refresh, Some(8));
        assert_eq!(window.non_leader_reload_generation, 0);
    }

    #[test]
    fn completion_observed_before_request_write_result_still_settles() {
        let mut window = Window {
            leadership: Leadership::forced(false),
            peer_refresh_write_pending: true,
            coordination: CoordinationConfig {
                refresh_request: 3,
                refresh_completion: PeerRefreshCompletion {
                    request: 3,
                    outcome: PeerRefreshOutcome::Network,
                },
                ..Default::default()
            },
            ..Window::default()
        };

        drop(window.finish_peer_refresh_request(Ok(3)));
        assert!(!window.refresh_pending);
        assert_eq!(window.requested_peer_refresh, None);
        assert!(matches!(window.last_error, Some(RefreshError::Network(_))));
        assert_eq!(window.non_leader_reload_generation, 1);
    }

    #[test]
    fn current_timeout_clears_pending_and_requests_reload_without_local_work() {
        use cosmic::Application as _;

        let mut window = peer_follower(6);
        drop(window.update(Message::PeerRefreshTimeout {
            generation: 6,
            request: 6,
        }));
        assert!(!window.refresh_pending);
        assert_eq!(window.requested_peer_refresh, None);
        assert_eq!(window.non_leader_reload_generation, 1);
        assert_eq!(window.timer_generation, 0, "no local refresh was armed");
    }

    async fn app_messages(task: app::Task<Message>) -> Vec<Message> {
        crate::testutil::drained_task_outputs(task, |action| match action {
            cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) => Some(message),
            _ => None,
        })
        .await
    }

    async fn deliver_app_task(window: &mut Window, task: app::Task<Message>) {
        use cosmic::Application as _;

        for message in app_messages(task).await {
            drop(window.update(message));
        }
    }

    #[tokio::test]
    async fn request_write_failure_never_shows_pending() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let config_root = dir.path().join("config");
        let context =
            cosmic_config::Config::with_custom_path(APP_ID, AppletConfig::VERSION, config_root)
                .unwrap();
        let invalid_state = dir.path().join("not-a-directory");
        std::fs::write(&invalid_state, b"file").unwrap();
        let mut window = Window {
            leadership: Leadership::forced(false),
            coordination_context: Some(context),
            coordination_state_dir: invalid_state,
            ..Window::default()
        };

        let task = window.update(Message::RefreshNow);
        assert!(!window.refresh_pending, "persist has not succeeded yet");
        let mut messages = app_messages(task).await;
        assert_eq!(messages.len(), 1);
        drop(window.update(messages.pop().unwrap()));
        assert!(!window.refresh_pending);
        assert!(!window.peer_refresh_write_pending);
        assert_eq!(window.requested_peer_refresh, None);
    }

    #[tokio::test(start_paused = true)]
    async fn successful_request_persist_starts_pending_only_after_completion() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let context = cosmic_config::Config::with_custom_path(
            APP_ID,
            AppletConfig::VERSION,
            dir.path().join("config"),
        )
        .unwrap();
        let mut window = Window {
            leadership: Leadership::forced(false),
            coordination_context: Some(context),
            coordination_state_dir: dir.path().join("state"),
            ..Window::default()
        };

        let task = window.update(Message::RefreshNow);
        assert!(!window.refresh_pending);
        let mut messages = app_messages(task).await;
        assert_eq!(messages.len(), 1);
        let timeout = window.update(messages.pop().unwrap());
        assert!(window.refresh_pending);
        assert_eq!(window.requested_peer_refresh, Some(1));

        let mut timeout_messages = app_messages(timeout).await;
        assert_eq!(timeout_messages.len(), 1);
        drop(window.update(timeout_messages.pop().unwrap()));
        assert!(!window.refresh_pending);
        assert_eq!(window.non_leader_reload_generation, 1);
    }

    #[tokio::test]
    async fn leader_persists_each_peer_outcome_and_requester_observes_it() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let context = cosmic_config::Config::with_custom_path(
            APP_ID,
            AppletConfig::VERSION,
            dir.path().join("config"),
        )
        .unwrap();
        let mut leader = Window {
            coordination_context: Some(context),
            coordination_state_dir: dir.path().join("state"),
            ..Window::default()
        };

        for (request, outcome) in [
            (1, PeerRefreshOutcome::Success),
            (2, PeerRefreshOutcome::Network),
            (3, PeerRefreshOutcome::Disk),
        ] {
            let mut messages =
                app_messages(leader.record_peer_refresh_completion(request, outcome)).await;
            assert_eq!(messages.len(), 1);
            drop(leader.update(messages.pop().unwrap()));
            assert_eq!(
                leader.coordination.refresh_completion,
                PeerRefreshCompletion { request, outcome }
            );

            let mut follower = peer_follower(request);
            drop(
                follower.update(Message::CoordinationUpdated(CoordinationConfig {
                    refresh_request: request,
                    refresh_completion: leader.coordination.refresh_completion,
                    ..Default::default()
                })),
            );
            assert!(!follower.refresh_pending);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_completion_persist_leaves_requester_for_timeout_settlement() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let context = cosmic_config::Config::with_custom_path(
            APP_ID,
            AppletConfig::VERSION,
            dir.path().join("config"),
        )
        .unwrap();
        let invalid_state = dir.path().join("not-a-directory");
        std::fs::write(&invalid_state, b"file").unwrap();
        let leader = Window {
            coordination_context: Some(context),
            coordination_state_dir: invalid_state,
            ..Window::default()
        };
        let mut requester = peer_follower(1);

        let mut messages =
            app_messages(leader.record_peer_refresh_completion(1, PeerRefreshOutcome::Network))
                .await;
        assert_eq!(messages.len(), 1);
        let Message::PeerRefreshCompletionWritten { result, .. } = messages.pop().unwrap() else {
            panic!("expected completion persist result")
        };
        assert!(result.is_err());
        assert!(
            requester.refresh_pending,
            "no acknowledgement was published"
        );

        drop(requester.update(Message::PeerRefreshTimeout {
            generation: 1,
            request: 1,
        }));
        assert!(!requester.refresh_pending);
        assert_eq!(requester.non_leader_reload_generation, 1);
    }

    #[test]
    fn non_leader_apply_failure_and_thumbnail_completion_are_non_destructive() {
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir_all(state.join("thumbs")).unwrap();
        let missing = images.join("20260807-Gone_ROW1_UHD.jpg");
        let entry = ImageEntry {
            filename: missing.clone(),
            ..entry_in_memory("20260807", "Gone_ROW1")
        };
        let orphan = state.join("thumbs/orphan.jpg");
        std::fs::write(&orphan, b"keep").unwrap();
        let mut window = Window {
            leadership: Leadership::forced(false),
            catalogue: Catalogue {
                images: vec![entry.clone()],
            },
            thumbnail_pass_pending: true,
            ..Window::default()
        };

        drop(window.on_apply_failure(&missing));
        drop(window.finish_thumbnail_pass(&state, wallpaper::CurrentWallpaper::NoFile));

        assert_eq!(window.catalogue.images, vec![entry]);
        assert!(orphan.is_file(), "a stale follower completion cannot sweep");
        assert!(!window.thumbnail_pass_pending);
    }

    #[tokio::test]
    async fn non_leader_settings_persist_only_their_own_keys() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("config");
        let context =
            cosmic_config::Config::with_custom_path(APP_ID, AppletConfig::VERSION, root.clone())
                .unwrap();
        let snapshot = AccentSnapshot {
            light: Some([1, 2, 3]),
            dark: None,
        };
        let last_written = AccentPair {
            light: [4, 5, 6],
            dark: [7, 8, 9],
        };
        let seeded = AppletConfig {
            accent_enabled: true,
            accent_snapshot: Some(snapshot),
            accent_last_written: Some(last_written),
            ..Default::default()
        };
        seeded.write_entry(&context).unwrap();
        let version_dir = root.join("cosmic").join(APP_ID).join("v1");
        let snapshot_path = version_dir.join("accent_snapshot");
        let last_written_path = version_dir.join("accent_last_written");
        let accent_bytes = || {
            (
                std::fs::read(&snapshot_path).unwrap(),
                std::fs::read(&last_written_path).unwrap(),
            )
        };
        let before = accent_bytes();
        let mut window = Window {
            leadership: Leadership::forced(false),
            config: seeded,
            config_context: Some(context),
            ..Window::default()
        };
        let timer_generation = window.timer_generation;
        let shuffle_generation = window.shuffle_generation;

        for message in [
            Message::SetShuffleEnabled(true),
            Message::SetShuffleInterval(0),
            Message::SetRetention(0),
        ] {
            let mut outputs = app_messages(window.update(message)).await;
            assert_eq!(outputs.len(), 1);
            drop(window.update(outputs.pop().unwrap()));
        }

        assert!(window.config.shuffle_enabled);
        assert_eq!(window.config.shuffle_interval_secs, 1_800);
        assert_eq!(window.config.retention_days, 3);
        assert_eq!(window.timer_generation, timer_generation);
        assert_eq!(window.shuffle_generation, shuffle_generation);
        assert_eq!(accent_bytes(), before, "accent key bytes are leader-owned");
        let persisted = AppletConfig::load(window.config_context.as_ref().unwrap());
        assert_eq!(persisted.accent_snapshot, Some(snapshot));
        assert_eq!(persisted.accent_last_written, Some(last_written));
    }

    #[tokio::test]
    async fn non_leader_settings_handle_missing_and_failing_config_contexts() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        let mut memory_only = Window {
            leadership: Leadership::forced(false),
            ..Window::default()
        };
        drop(memory_only.update(Message::SetShuffleEnabled(true)));
        drop(memory_only.update(Message::SetShuffleInterval(0)));
        drop(memory_only.update(Message::SetRetention(0)));
        assert!(memory_only.config.shuffle_enabled);
        assert_eq!(memory_only.config.shuffle_interval_secs, 1_800);
        assert_eq!(memory_only.config.retention_days, 3);

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("config");
        let context =
            cosmic_config::Config::with_custom_path(APP_ID, AppletConfig::VERSION, root.clone())
                .unwrap();
        AppletConfig::default().write_entry(&context).unwrap();
        let version_dir = root.join("cosmic").join(APP_ID).join("v1");
        std::fs::remove_dir_all(&version_dir).unwrap();
        std::fs::write(&version_dir, b"not a directory").unwrap();
        let mut failing = Window {
            leadership: Leadership::forced(false),
            config_context: Some(context),
            ..Window::default()
        };
        let before = failing.config.clone();

        for message in [
            Message::SetShuffleEnabled(true),
            Message::SetShuffleInterval(0),
            Message::SetRetention(0),
        ] {
            let mut outputs = app_messages(failing.update(message)).await;
            assert_eq!(outputs.len(), 1);
            drop(failing.update(outputs.pop().unwrap()));
        }

        assert_eq!(failing.config, before, "failed persists are not adopted");
        assert!(!failing.shuffle_armed);
    }

    #[tokio::test]
    async fn follower_setting_writes_are_serialized_and_latest_value_wins() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let (context, _) = takeover_contexts(&dir.path().join("config"));
        let mut window = Window {
            leadership: Leadership::forced(false),
            config_context: Some(context),
            ..Window::default()
        };

        let first = window.update(Message::SetShuffleEnabled(true));
        let second = window.update(Message::SetShuffleEnabled(false));
        assert_eq!(first.units(), 1);
        assert_eq!(second.units(), 0, "newer write waits behind the first");
        let mut outputs = app_messages(first).await;
        let next = window.update(outputs.pop().unwrap());
        assert!(
            !window.config.shuffle_enabled,
            "stale completion is not adopted"
        );
        let mut outputs = app_messages(next).await;
        drop(window.update(outputs.pop().unwrap()));

        assert!(!window.config.shuffle_enabled);
        assert!(!AppletConfig::load(window.config_context.as_ref().unwrap()).shuffle_enabled);
        assert!(!window.setting_write_inflight);
        assert!(window.setting_write_queue.is_empty());
    }

    #[test]
    fn a_prune_landing_mid_pass_keeps_what_that_pass_is_writing() {
        // `SetRetention`, an external retention edit and a failed apply all
        // prune on the UI thread — which sweeps the thumbnail cache down to
        // the *live* catalogue. A pass writing into that cache is always
        // ahead of the catalogue: the pipeline's downloads join it only when
        // `RefreshFinished` merges them, and the startup pass runs on a
        // blocking pool. Sweeping inside either window deletes the preview of
        // the image that is about to be applied, and nothing regenerates it
        // until the next successful refresh.
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&images).unwrap();
        let kept = entry_on_disk(&images, "20260807", "Kept_ROW1");
        let cat_path = state.join(catalogue::CATALOGUE_FILENAME);

        let mut window = Window::default();
        // Retention "forever": this is about the sweep, not about age.
        window.config.retention_days = 0;
        window.catalogue.images.push(kept);

        let in_flight = images.join("20260808-Fresh_ROW2_UHD.jpg");
        let fresh_thumb = thumbs::thumbnail_path(&in_flight, &state).unwrap();
        std::fs::create_dir_all(fresh_thumb.parent().unwrap()).unwrap();
        std::fs::write(&fresh_thumb, b"thumb").unwrap();

        let prune = |window: &mut Window| {
            drop(window.prune_over(
                wallpaper::CurrentWallpaper::NoFile,
                &images,
                &state,
                &cat_path,
            ));
        };

        window.refresh_pending = true;
        prune(&mut window);
        assert!(
            fresh_thumb.is_file(),
            "a fetch's fresh thumbnail must survive a concurrent prune"
        );

        window.refresh_pending = false;
        window.thumbnail_pass_pending = true;
        prune(&mut window);
        assert!(
            fresh_thumb.is_file(),
            "so must the startup pass's — same race, other producer"
        );

        // With nothing in flight the sweep works exactly as before: an
        // artefact no live entry names is collected.
        window.thumbnail_pass_pending = false;
        prune(&mut window);
        assert!(!fresh_thumb.exists(), "the orphan is still collected");
    }

    // -----------------------------------------------------------------
    // Eligibility reconciliation at `RefreshFinished` (Task 1b).
    // -----------------------------------------------------------------

    /// Tempdir roots for a `finish_refresh_over` run: the images folder,
    /// the state dir and the catalogue path inside it.
    struct RefreshRoots {
        _dir: tempfile::TempDir,
        images: PathBuf,
        state: PathBuf,
        catalogue: PathBuf,
    }

    fn refresh_roots() -> RefreshRoots {
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let catalogue = state.join(catalogue::CATALOGUE_FILENAME);
        RefreshRoots {
            _dir: dir,
            images,
            state,
            catalogue,
        }
    }

    /// A successful fetch that downloaded nothing and marks `urlbases`
    /// explicitly ineligible — the shape of a refresh on a day Bing
    /// restricts an image the applet already holds.
    fn batch_marking_ineligible(urlbases: &[&str]) -> Result<RefreshBatch, RefreshError> {
        Ok(RefreshBatch {
            ineligible: urlbases.iter().map(|u| (*u).to_owned()).collect(),
            anchor: "202608080700".to_owned(),
            ..RefreshBatch::default()
        })
    }

    fn finish_over(
        window: &mut Window,
        roots: &RefreshRoots,
        live: wallpaper::CurrentWallpaper,
        result: Result<RefreshBatch, RefreshError>,
    ) {
        window.refresh_pending = true;
        drop(window.finish_refresh_over(
            result,
            live,
            &roots.images,
            &roots.state,
            &roots.catalogue,
        ));
    }

    #[test]
    fn an_explicitly_ineligible_image_is_removed_durably_entry_and_file() {
        // `wp: false` on an image already on disk: the entry *and* the JPEG
        // go, and because the file is gone a later rebuild from the folder
        // (missing `catalogue.json`) cannot bring the image back.
        let roots = refresh_roots();
        let kept = entry_on_disk(&roots.images, "20260807", "Kept_ROW1");
        let restricted = entry_on_disk(&roots.images, "20260806", "Restricted_ROW2");
        let mut window = Window::default();
        window.config.retention_days = 0;
        window.catalogue.images = vec![restricted.clone(), kept.clone()];
        window.last_error = Some(RefreshError::Network("old".to_owned()));

        // The ineligible list is what the parser produces for an explicit
        // `false`, so the chain parser → batch → removal is the one pinned.
        let archive = bing::parse_image_list(&format!(
            r#"{{"images":[{{"startdate":"20260806","fullstartdate":"202608060700","urlbase":"{}","copyright":"x (© y)","copyrightlink":"https://example.com","title":"Info","wp":false}}]}}"#,
            restricted.urlbase
        ))
        .unwrap();
        assert_eq!(archive.ineligible, vec![restricted.urlbase.clone()]);
        let batch = Ok(RefreshBatch {
            fetched: Vec::new(),
            ineligible: archive.ineligible,
            anchor: archive.anchor.unwrap(),
            fallback: None,
            thumbnails_deferred: false,
        });
        finish_over(
            &mut window,
            &roots,
            wallpaper::CurrentWallpaper::File(kept.filename.clone()),
            batch,
        );

        assert_eq!(window.catalogue.images, vec![kept.clone()]);
        assert!(!restricted.filename.exists(), "the JPEG is unlinked");
        assert!(kept.filename.is_file());
        assert!(
            window.last_error.is_none(),
            "a no-download refresh is a success"
        );
        assert!(window.last_updated.is_some());
        assert!(!window.refresh_pending);

        // Restart with the catalogue lost: the rebuild scans the folder and
        // must not find the restricted image.
        std::fs::remove_file(&roots.catalogue).unwrap();
        let rebuilt = Catalogue::load_or_rebuild(&roots.catalogue, &roots.images).catalogue;
        assert_eq!(
            rebuilt
                .images
                .iter()
                .map(|e| &e.filename)
                .collect::<Vec<_>>(),
            vec![&kept.filename],
            "a removed image must not resurrect through a rebuild"
        );
    }

    #[test]
    fn an_image_whose_wp_is_absent_keeps_its_entry_and_file() {
        // Absent `wp` blocks downloads only (Task 1a) — it is never a reason
        // to delete: the parser lists nothing as ineligible, so the refresh
        // completion removes nothing.
        let roots = refresh_roots();
        let held = entry_on_disk(&roots.images, "20260807", "Held_ROW1");
        let mut window = Window::default();
        window.config.retention_days = 0;
        window.catalogue.images = vec![held.clone()];

        let archive = bing::parse_image_list(&format!(
            r#"{{"images":[{{"startdate":"20260807","fullstartdate":"202608070700","urlbase":"{}","copyright":"x (© y)","copyrightlink":"https://example.com","title":"Info"}}]}}"#,
            held.urlbase
        ))
        .unwrap();
        assert!(archive.ineligible.is_empty());
        assert_eq!(archive.absent_wp, 1);
        let batch = Ok(RefreshBatch {
            fetched: Vec::new(),
            ineligible: archive.ineligible,
            anchor: archive.anchor.unwrap(),
            fallback: None,
            thumbnails_deferred: false,
        });
        finish_over(
            &mut window,
            &roots,
            wallpaper::CurrentWallpaper::NoFile,
            batch,
        );

        assert_eq!(window.catalogue.images, vec![held.clone()]);
        assert!(held.filename.is_file());
        assert!(window.last_error.is_none());
    }

    #[test]
    fn an_ineligible_image_whose_unlink_fails_keeps_its_entry_until_a_retry_succeeds() {
        use std::os::unix::fs::PermissionsExt as _;

        // A read-only folder: the unlink fails, so the entry must stay —
        // dropping it while the JPEG remains would open the resurrection
        // window the file-level guarantee exists to close. The next refresh
        // (folder writable again) retries and removes both.
        let roots = refresh_roots();
        let restricted = entry_on_disk(&roots.images, "20260806", "Restricted_ROW2");
        let mut window = Window::default();
        window.config.retention_days = 0;
        window.catalogue.images = vec![restricted.clone()];

        // The mode bits must actually bite (they do not for root, or on a
        // filesystem ignoring them) — otherwise the scenario cannot be
        // staged here and the test is skipped rather than passed.
        let probe = roots.images.join("probe");
        std::fs::write(&probe, b"x").unwrap();
        std::fs::set_permissions(&roots.images, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::remove_file(&probe).is_ok() {
            std::fs::set_permissions(&roots.images, std::fs::Permissions::from_mode(0o755))
                .unwrap();
            eprintln!("skipping: a read-only directory does not block unlinks here");
            return;
        }
        finish_over(
            &mut window,
            &roots,
            wallpaper::CurrentWallpaper::NoFile,
            batch_marking_ineligible(&[&restricted.urlbase]),
        );
        std::fs::set_permissions(&roots.images, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_file(&probe).unwrap();

        assert_eq!(
            window.catalogue.images,
            vec![restricted.clone()],
            "the entry is retained while its file cannot be removed"
        );
        assert!(restricted.filename.is_file());
        assert!(window.last_error.is_none(), "still a successful refresh");

        finish_over(
            &mut window,
            &roots,
            wallpaper::CurrentWallpaper::NoFile,
            batch_marking_ineligible(&[&restricted.urlbase]),
        );
        assert!(window.catalogue.images.is_empty(), "the retry removes it");
        assert!(!restricted.filename.exists());
    }

    #[test]
    fn an_unknowable_wallpaper_blocks_ineligibility_removal_like_it_blocks_the_prune() {
        // Mirror of `classify_maps_the_three_cosmic_bg_states`: only
        // `Unknown` (per-output mode, unreadable config) withholds the
        // evidence; `File` and `NoFile` are both knowable.
        for (live, deleted) in [
            (wallpaper::CurrentWallpaper::Unknown, false),
            (wallpaper::CurrentWallpaper::NoFile, true),
            (
                wallpaper::CurrentWallpaper::File(PathBuf::from("/usr/share/backgrounds/x.jpg")),
                true,
            ),
        ] {
            let roots = refresh_roots();
            let restricted = entry_on_disk(&roots.images, "20260806", "Restricted_ROW2");
            let mut window = Window::default();
            window.config.retention_days = 0;
            window.catalogue.images = vec![restricted.clone()];
            // What a stale `self.current` would "protect" in `Unknown`:
            // nothing relevant, which is exactly why nothing may be deleted.
            window.current = None;

            finish_over(
                &mut window,
                &roots,
                live.clone(),
                batch_marking_ineligible(&[&restricted.urlbase]),
            );

            assert_eq!(restricted.filename.exists(), !deleted, "{live:?}");
            assert_eq!(window.catalogue.images.is_empty(), deleted, "{live:?}");
            assert!(window.last_error.is_none(), "{live:?}");
        }
    }

    #[test]
    fn the_live_wallpaper_survives_its_own_ineligibility_until_replaced() {
        // The image on screen is exempt, entry and file, so the popup keeps
        // attributing what is actually displayed (`view::displayed` would
        // otherwise fall back to the newest entry). Once another image is
        // applied, the next refresh removes it.
        let roots = refresh_roots();
        let shown = entry_on_disk(&roots.images, "20260806", "Shown_ROW2");
        let newer = entry_on_disk(&roots.images, "20260807", "Newer_ROW1");
        let mut window = Window::default();
        window.config.retention_days = 0;
        window.catalogue.images = vec![shown.clone(), newer.clone()];

        finish_over(
            &mut window,
            &roots,
            wallpaper::CurrentWallpaper::File(shown.filename.clone()),
            batch_marking_ineligible(&[&shown.urlbase]),
        );

        assert_eq!(window.catalogue.images, vec![shown.clone(), newer.clone()]);
        assert!(shown.filename.is_file());
        assert_eq!(window.current.as_deref(), Some(shown.filename.as_path()));
        assert_eq!(
            view::displayed(&window.catalogue, window.current.as_deref()),
            Some(&shown),
            "the popup still attributes the image on screen"
        );

        // Another image is applied: the first refresh after that removes it.
        finish_over(
            &mut window,
            &roots,
            wallpaper::CurrentWallpaper::File(newer.filename.clone()),
            batch_marking_ineligible(&[&shown.urlbase]),
        );
        assert_eq!(window.catalogue.images, vec![newer.clone()]);
        assert!(!shown.filename.exists());
        assert!(newer.filename.is_file());
    }

    #[test]
    fn an_all_ineligible_response_on_a_warm_catalogue_is_a_daily_no_op() {
        // Nothing downloadable today: history and the current wallpaper
        // stay, the error clears, cold start stays armed, and the next
        // refresh is scheduled off the *response* anchor — the normal daily
        // delay, not the ~6-minute out-of-range reset a stale catalogue
        // entry would produce.
        // Fixed instants: a `Utc::now()` here would hit `next_refresh`'s
        // out-of-range reset itself whenever the test runs before the
        // anchor's hour (due = anchor + 24 h is then more than 24 h away).
        let now = Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap();
        let stale = entry_in_memory("20200101", "Ancient_ROW0");
        let today = "202608070700".to_owned();

        let plan = refresh_success_plan(true, None, true, false, &today, now);
        assert!(
            plan.auto_apply,
            "the live catalogue still has an image to apply"
        );
        // Due tomorrow 07:00, i.e. 19 h away, plus the reference +300 s fudge.
        assert_eq!(plan.delay, Duration::from_secs(19 * 3_600 + 300));
        assert_eq!(
            schedule::next_refresh(Some(&stale.fullstartdate), now),
            Duration::from_secs(360),
            "the stale entry would have hit the out-of-range reset"
        );

        // Through the completion handler, warm: an image we hold, nothing
        // fetched, the ineligible image is one we never downloaded. A
        // foreign live wallpaper keeps `wallpaper::apply` (real cosmic-bg
        // config) out of the test.
        let roots = refresh_roots();
        let held = entry_on_disk(&roots.images, "20200101", "Ancient_ROW0");
        let mut window = Window::default();
        window.config.retention_days = 0;
        window.catalogue.images = vec![held.clone()];
        window.last_error = Some(RefreshError::Network("old".to_owned()));
        let generation = window.timer_generation;
        let foreign = PathBuf::from("/usr/share/backgrounds/x.jpg");

        finish_over(
            &mut window,
            &roots,
            wallpaper::CurrentWallpaper::File(foreign.clone()),
            Ok(RefreshBatch {
                fetched: Vec::new(),
                ineligible: vec!["/th?id=OHR.Other_ROW3".to_owned()],
                anchor: today.clone(),
                fallback: None,
                thumbnails_deferred: false,
            }),
        );
        assert_eq!(window.catalogue.images, vec![held.clone()]);
        assert!(held.filename.is_file());
        assert_eq!(window.current.as_deref(), Some(foreign.as_path()));
        assert!(window.last_error.is_none());
        assert!(window.timer_generation > generation, "rescheduled");

        // And cold: nothing held, nothing downloadable. Nothing is applied,
        // so the cold-start flag stays armed for the fetch that finally
        // delivers, and the reschedule still comes from the anchor (the
        // plan's `None` branch — the 1 h back-off — is not taken).
        let roots = refresh_roots();
        let mut window = Window {
            cold_start: ColdStart::Pending,
            ..Window::default()
        };
        window.last_error = Some(RefreshError::Network("old".to_owned()));
        let generation = window.timer_generation;
        finish_over(
            &mut window,
            &roots,
            wallpaper::CurrentWallpaper::NoFile,
            Ok(RefreshBatch {
                fetched: Vec::new(),
                ineligible: vec!["/th?id=OHR.Other_ROW3".to_owned()],
                anchor: today,
                fallback: None,
                thumbnails_deferred: false,
            }),
        );
        assert!(window.catalogue.images.is_empty());
        assert_eq!(window.cold_start, ColdStart::Pending, "still armed");
        assert!(window.last_error.is_none());
        assert!(window.timer_generation > generation, "rescheduled");
    }

    // -----------------------------------------------------------------
    // Full-window request, horizon selection and the bounded fallback
    // (Task 2).
    // -----------------------------------------------------------------

    /// The `(startdate, fullstartdate)` of archive position `position` in
    /// the test window: newest first, position `i` dated `2026-08-(07-i)`
    /// at Bing's 07:00 UTC publish time.
    fn window_day(position: usize) -> (String, String) {
        let day = 7 - position;
        (format!("202608{day:02}"), format!("202608{day:02}0700"))
    }

    fn archive_image(position: usize, name: &str, wp: bool) -> ArchiveImage {
        let (startdate, fullstartdate) = window_day(position);
        ArchiveImage {
            position,
            image: bing::BingImage {
                startdate,
                fullstartdate,
                urlbase: format!("/th?id=OHR.{name}"),
                copyright: "x (© y)".to_owned(),
                copyrightlink: "https://example.com".to_owned(),
                wp: Some(wp),
            },
        }
    }

    /// An eight-entry HPImageArchive body, newest first, dated per
    /// [`window_day`]; `eligible` names the positions marked `wp: true`,
    /// every other one is an explicit `false`.
    fn window_json(eligible: &[usize]) -> String {
        let entries: Vec<String> = (0..usize::from(bing::ARCHIVE_WINDOW))
            .map(|i| {
                let (startdate, fullstartdate) = window_day(i);
                format!(
                    r#"{{"startdate":"{startdate}","fullstartdate":"{fullstartdate}","urlbase":"/th?id=OHR.Pos{i}_ROW{i}","copyright":"x (© y)","copyrightlink":"https://example.com","title":"Info","wp":{}}}"#,
                    eligible.contains(&i)
                )
            })
            .collect();
        format!(r#"{{"images":[{}]}}"#, entries.join(","))
    }

    /// Loopback server answering the list with `json` and every image GET
    /// with a tiny JPEG, recording the image paths it was asked for.
    fn window_server(json: String) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = std::sync::Arc::clone(&requests);
        let jpeg = crate::testutil::tiny_jpeg(32, 18);
        let base = crate::testutil::spawn_mock(move |path| {
            if path.starts_with("/HPImageArchive.aspx") {
                assert!(
                    path.contains("idx=0&n=8&"),
                    "always the full supported window, never paginated: {path}"
                );
                (200, json.clone().into_bytes())
            } else {
                seen.lock().unwrap().push(path.to_owned());
                (200, jpeg.clone())
            }
        });
        (base, requests)
    }

    #[test]
    fn select_downloads_takes_the_horizon_or_exactly_one_fallback() {
        let urlbases = |slots: &[&ArchiveImage]| -> Vec<String> {
            slots.iter().map(|s| s.image.urlbase.clone()).collect()
        };
        let all = |h: u8, fallback: bool| Downloads {
            horizon: h,
            fallback,
        };

        // Newest eligible: the horizon is served, nothing beyond it.
        let eligible = vec![
            archive_image(0, "A", true),
            archive_image(1, "B", true),
            archive_image(5, "F", true),
        ];
        let sel = select_downloads(&eligible, all(2, true));
        assert_eq!(urlbases(&sel.in_window), ["/th?id=OHR.A", "/th?id=OHR.B"]);
        assert!(sel.fallback.is_none(), "a served horizon never falls back");

        // Several older eligible images beyond an empty horizon: only the
        // newest one of them is the fallback, the rest stay undownloaded.
        let eligible = vec![
            archive_image(3, "D", true),
            archive_image(4, "E", true),
            archive_image(7, "H", true),
        ];
        let sel = select_downloads(&eligible, all(1, true));
        assert!(sel.in_window.is_empty());
        assert_eq!(sel.fallback.map(|s| s.position), Some(3));

        // Auto-apply suppressed (foreign wallpaper): no fallback at all.
        let sel = select_downloads(&eligible, all(1, false));
        assert!(sel.in_window.is_empty());
        assert!(sel.fallback.is_none());

        // Nothing eligible anywhere: nothing to download, nothing to fall
        // back to — the caller completes as a successful no-op.
        let sel = select_downloads(&[], all(8, true));
        assert!(sel.in_window.is_empty() && sel.fallback.is_none());

        // Retention 0 / ≥ 8 considers the whole window, so a position-7
        // image is in-window, not a fallback.
        let sel = select_downloads(&eligible, all(schedule::download_horizon(0), true));
        assert_eq!(sel.in_window.len(), 3);
        assert!(sel.fallback.is_none());
    }

    #[tokio::test]
    async fn an_empty_horizon_downloads_only_the_newest_eligible_fallback() {
        // Retention 1 on a day whose newest image is restricted, with two
        // older eligible images: exactly one image GET, for the newer of the
        // two, and the batch names it as the fallback.
        let (base, requests) = window_server(window_json(&[2, 5]));
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        let state = dir.path().join("state");
        let client = bing::http_client().unwrap();

        let batch = fetch_and_download(
            &client,
            &base,
            &Catalogue::default(),
            Downloads {
                horizon: schedule::download_horizon(1),
                fallback: true,
            },
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .unwrap();

        let expected = download_dir.join("20260805-Pos2_ROW2_UHD.jpg");
        assert_eq!(
            *requests.lock().unwrap(),
            vec![bing::image_url("", "/th?id=OHR.Pos2_ROW2")],
            "one GET: the newest eligible image beyond the horizon"
        );
        assert_eq!(batch.fallback.as_deref(), Some(expected.as_path()));
        assert_eq!(batch.fetched.len(), 1);
        assert_eq!(batch.fetched[0].filename, expected);
        assert!(expected.is_file());
        assert!(thumbs::is_cached(&expected, &state));
        assert_eq!(batch.ineligible.len(), 6, "every explicit false is carried");
        assert_eq!(batch.anchor, "202608070700");
    }

    #[tokio::test]
    async fn a_served_horizon_never_downloads_beyond_it() {
        // Retention 3 with the newest image eligible: the horizon's eligible
        // images are fetched and the eligible position-5 image is not — no
        // out-of-retention download happens just because one exists.
        let (base, requests) = window_server(window_json(&[0, 2, 5]));
        let dir = tempfile::tempdir().unwrap();
        let client = bing::http_client().unwrap();

        let batch = fetch_and_download(
            &client,
            &base,
            &Catalogue::default(),
            within(schedule::download_horizon(3)),
            &dir.path().join("images"),
            &dir.path().join("state"),
            &test_backfill(),
        )
        .await
        .unwrap();

        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                bing::image_url("", "/th?id=OHR.Pos0_ROW0"),
                bing::image_url("", "/th?id=OHR.Pos2_ROW2"),
            ]
        );
        assert!(batch.fallback.is_none());
        assert_eq!(batch.fetched.len(), 2);
    }

    #[tokio::test]
    async fn an_all_ineligible_window_downloads_nothing_and_succeeds() {
        let (base, requests) = window_server(window_json(&[]));
        let dir = tempfile::tempdir().unwrap();
        let client = bing::http_client().unwrap();

        let batch = fetch_and_download(
            &client,
            &base,
            &Catalogue::default(),
            within(1),
            &dir.path().join("images"),
            &dir.path().join("state"),
            &test_backfill(),
        )
        .await
        .expect("a valid all-ineligible window is a successful no-op");

        assert!(requests.lock().unwrap().is_empty(), "no image GET at all");
        assert!(batch.fetched.is_empty());
        assert!(batch.fallback.is_none());
        assert_eq!(batch.ineligible.len(), 8);
        assert_eq!(
            batch.anchor, "202608070700",
            "still anchors the normal daily schedule"
        );
    }

    #[tokio::test]
    async fn a_warm_retention_one_catalogue_reuses_the_fallback_it_already_holds() {
        // Retention 1, yesterday's image on disk and on screen, today's
        // restricted: the fallback *is* yesterday's image. It is hydrated
        // from disk (no GET) and still reported as the fallback, so the
        // prune keeps protecting it — and the day is not a permanent no-op.
        let (base, requests) = window_server(window_json(&[1, 4]));
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("images");
        std::fs::create_dir_all(&download_dir).unwrap();
        let held = download_dir.join("20260806-Pos1_ROW1_UHD.jpg");
        std::fs::write(&held, crate::testutil::tiny_jpeg(32, 18)).unwrap();
        let mut catalogue = Catalogue::default();
        catalogue.images.push(ImageEntry {
            urlbase: "/th?id=OHR.Pos1_ROW1".to_owned(),
            startdate: "20260806".to_owned(),
            fullstartdate: "202608060700".to_owned(),
            title: String::new(),
            copyright: String::new(),
            copyrightlink: String::new(),
            filename: held.clone(),
        });
        let client = bing::http_client().unwrap();

        let batch = fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(schedule::download_horizon(1)),
            &download_dir,
            &dir.path().join("state"),
            &test_backfill(),
        )
        .await
        .unwrap();

        assert!(requests.lock().unwrap().is_empty(), "held file: no GET");
        assert_eq!(batch.fallback.as_deref(), Some(held.as_path()));
        assert_eq!(batch.fetched.len(), 1);
        assert_eq!(batch.fetched[0].filename, held);
        assert_eq!(
            batch.fetched[0].copyright, "© y",
            "and the merge will refill its metadata"
        );
    }

    #[tokio::test]
    async fn a_foreign_wallpaper_suppresses_the_fallback_without_churn() {
        // The user's own wallpaper is up, so nothing would be applied: the
        // fallback is not downloaded — across two refreshes, zero image GETs
        // and nothing to prune and re-fetch daily.
        let (base, requests) = window_server(window_json(&[3]));
        let dir = tempfile::tempdir().unwrap();
        let client = bing::http_client().unwrap();
        let suppressed = Downloads {
            horizon: schedule::download_horizon(1),
            fallback: wallpaper::should_auto_apply(
                false,
                Some(Path::new("/usr/share/backgrounds/user.jpg")),
            ),
        };
        assert!(!suppressed.fallback);

        for _ in 0..2 {
            let batch = fetch_and_download(
                &client,
                &base,
                &Catalogue::default(),
                suppressed,
                &dir.path().join("images"),
                &dir.path().join("state"),
                &test_backfill(),
            )
            .await
            .unwrap();
            assert!(batch.fetched.is_empty());
            assert!(batch.fallback.is_none());
        }
        assert!(requests.lock().unwrap().is_empty());
        assert!(
            !dir.path().join("images").exists()
                || std::fs::read_dir(dir.path().join("images"))
                    .unwrap()
                    .next()
                    .is_none(),
            "nothing landed on disk"
        );
    }

    #[tokio::test]
    async fn an_unknowable_wallpaper_suppresses_the_fallback_like_the_completion_does() {
        // Warm catalogue, per-output mode (`CurrentWallpaper::Unknown`),
        // nothing eligible inside the horizon. `sync_current` keeps the
        // stale path of our own last apply under `Unknown`, which
        // `is_ours` would pass — but the completion maps `Unknown` to
        // `None` and applies nothing warm, so a fallback downloaded here
        // would only be pruned and re-fetched daily. The start must
        // decide from the same live mapping: no fallback, zero image GETs.
        let (base, requests) = window_server(window_json(&[3]));
        let dir = tempfile::tempdir().unwrap();
        let client = bing::http_client().unwrap();
        let ours = wallpaper::download_dir().join("20260807-Ours_ROW1_UHD.jpg");
        let mut window = window_with_images(1);
        window.cold_start = ColdStart::Done;
        window.current = Some(ours.clone());
        let live = wallpaper::CurrentWallpaper::Unknown;
        window.sync_current(&live);
        assert_eq!(window.current, Some(ours.clone()), "the stale path is kept");
        assert!(
            wallpaper::should_auto_apply(false, window.current.as_deref()),
            "premise: the stale path alone would permit the fallback"
        );
        assert!(
            !window.fallback_permitted(&live),
            "but the start decides from the live state, as the completion does"
        );
        let plan = refresh_success_plan(
            false,
            live.clone().into_file().as_deref(),
            true,
            true,
            "202608080700",
            Utc::now(),
        );
        assert!(!plan.auto_apply, "the completion would not have applied it");

        let batch = fetch_and_download(
            &client,
            &base,
            &window.catalogue,
            Downloads {
                horizon: schedule::download_horizon(1),
                fallback: window.fallback_permitted(&live),
            },
            &dir.path().join("images"),
            &dir.path().join("state"),
            &test_backfill(),
        )
        .await
        .unwrap();
        assert!(batch.fetched.is_empty());
        assert!(batch.fallback.is_none());
        assert!(requests.lock().unwrap().is_empty(), "no image GET");

        // The rule is the completion's, so every other state agrees with
        // `should_auto_apply` over `live.into_file()`.
        window.current = None;
        assert!(
            window.fallback_permitted(&wallpaper::CurrentWallpaper::File(ours)),
            "our own wallpaper is up: the fallback is applied, so it is fetched"
        );
        assert!(
            !window.fallback_permitted(&wallpaper::CurrentWallpaper::File(PathBuf::from(
                "/usr/share/bg/u.jpg"
            )))
        );
        assert!(!window.fallback_permitted(&wallpaper::CurrentWallpaper::NoFile));
        window.cold_start = ColdStart::Pending;
        assert!(
            window.fallback_permitted(&wallpaper::CurrentWallpaper::Unknown),
            "cold start applies over anything, so the fallback is worth fetching"
        );
    }

    #[tokio::test]
    async fn a_deferred_refresh_that_fetches_nothing_owes_no_thumbnail_pass() {
        // All-ineligible response under the write interlock: no download
        // loop iteration, so no debt — booking one would buy an extra
        // pass, sweep and accent recompute for no preview.
        let roots = refresh_roots();
        let (base, requests) = window_server(window_json(&[]));
        let client = bing::http_client().unwrap();
        let batch = fetch_and_download(
            &client,
            &base,
            &Catalogue::default(),
            within(8),
            &roots.images,
            &roots.state,
            &Backfill {
                deferred: true,
                ..test_backfill()
            },
        )
        .await
        .unwrap();
        assert!(batch.fetched.is_empty());
        assert!(requests.lock().unwrap().is_empty());
        assert!(!batch.thumbnails_deferred, "nothing fetched, nothing owed");
    }

    #[test]
    fn a_downloaded_fallback_is_protected_until_applied_or_reselected() {
        // Retention 1; the refresh brought back an out-of-retention fallback
        // it could not apply (here: auto-apply suppressed by a foreign live
        // wallpaper, which also keeps `wallpaper::apply` — real cosmic-bg
        // config — out of the test). The same completion's prune must keep
        // it, so must any prune before the next refresh, and the next
        // refresh's selection replaces the protection.
        let roots = refresh_roots();
        let fallback = entry_on_disk(&roots.images, "20200105", "Fallback_ROW5");
        let stale = entry_on_disk(&roots.images, "20200101", "Stale_ROW1");
        let mut window = Window::default();
        window.config.retention_days = 1;
        window.catalogue.images = vec![stale.clone()];
        let foreign = wallpaper::CurrentWallpaper::File(PathBuf::from("/usr/share/bg/u.jpg"));
        let anchor = Utc::now().format("%Y%m%d0700").to_string();

        finish_over(
            &mut window,
            &roots,
            foreign.clone(),
            Ok(RefreshBatch {
                fetched: vec![fallback.clone()],
                ineligible: Vec::new(),
                anchor: anchor.clone(),
                fallback: Some(fallback.filename.clone()),
                thumbnails_deferred: false,
            }),
        );
        assert_eq!(
            window.catalogue.images,
            vec![fallback.clone()],
            "the fallback survives the prune that deleted the stale entry"
        );
        assert!(fallback.filename.is_file());
        assert!(!stale.filename.exists());
        assert_eq!(
            window.protected_fallback.as_deref(),
            Some(fallback.filename.as_path())
        );
        assert_eq!(
            Catalogue::load(&roots.catalogue).unwrap(),
            window.catalogue,
            "persisted with the fallback"
        );

        // An interim prune (retention edit, failed-apply cleanup) keeps it too.
        drop(window.prune_over(
            foreign.clone(),
            &roots.images,
            &roots.state,
            &roots.catalogue,
        ));
        assert_eq!(window.catalogue.images, vec![fallback.clone()]);
        assert!(fallback.filename.is_file());

        // The next ordinary refresh selects no fallback: protection ends and
        // the out-of-retention file goes with the prune.
        finish_over(
            &mut window,
            &roots,
            foreign,
            Ok(RefreshBatch {
                fetched: Vec::new(),
                ineligible: Vec::new(),
                anchor,
                fallback: None,
                thumbnails_deferred: false,
            }),
        );
        assert!(window.protected_fallback.is_none());
        assert!(window.catalogue.images.is_empty());
        assert!(!fallback.filename.exists());
    }

    #[test]
    fn an_applied_fallback_hands_over_to_the_current_wallpaper_protection() {
        // Once the fallback is what cosmic-bg displays, the prune's
        // current-wallpaper exemption covers it and the fallback
        // protection is dropped at the next completion like any other.
        let roots = refresh_roots();
        let fallback = entry_on_disk(&roots.images, "20200105", "Fallback_ROW5");
        let mut window = Window::default();
        window.config.retention_days = 1;
        window.catalogue.images = vec![fallback.clone()];
        window.protected_fallback = Some(fallback.filename.clone());
        let anchor = Utc::now().format("%Y%m%d0700").to_string();

        finish_over(
            &mut window,
            &roots,
            wallpaper::CurrentWallpaper::File(fallback.filename.clone()),
            Ok(RefreshBatch {
                fetched: Vec::new(),
                ineligible: Vec::new(),
                anchor,
                fallback: None,
                thumbnails_deferred: false,
            }),
        );
        assert!(window.protected_fallback.is_none());
        assert_eq!(window.catalogue.images, vec![fallback.clone()]);
        assert!(fallback.filename.is_file(), "kept as the live wallpaper");
        assert_eq!(window.current.as_deref(), Some(fallback.filename.as_path()));
    }

    // -----------------------------------------------------------------
    // Catalogue provenance, the leader's metadata repair and the producer
    // write interlock (Task 3).
    // -----------------------------------------------------------------

    /// A "recovered image pack": `positions` of the eight-entry window
    /// already on disk as decodable UHD JPEGs, plus one historical image
    /// Bing no longer lists, with no `catalogue.json` — exactly what
    /// `load_or_rebuild` rescans into blank-metadata entries.
    fn rebuilt_pack(roots: &RefreshRoots, positions: &[usize]) -> CatalogueRestore {
        let jpeg = crate::testutil::tiny_jpeg(32, 18);
        for i in positions {
            let day = 7 - i;
            std::fs::write(
                roots
                    .images
                    .join(format!("202608{day:02}-Pos{i}_ROW{i}_UHD.jpg")),
                &jpeg,
            )
            .unwrap();
        }
        std::fs::write(roots.images.join("20260701-Historical_ROW9_UHD.jpg"), &jpeg).unwrap();
        let restore = Catalogue::load_or_rebuild(&roots.catalogue, &roots.images);
        assert_eq!(restore.provenance, Provenance::Rebuilt);
        assert_eq!(restore.catalogue.images.len(), positions.len() + 1);
        assert!(
            restore.catalogue.images.iter().all(|e| e.title.is_empty()),
            "a rebuilt entry has no metadata"
        );
        restore
    }

    /// Every cache artefact under `state/thumbs`: `(name, bytes)`.
    fn thumb_dir_listing(state: &Path) -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(state.join("thumbs"))
            .map(|read| {
                read.flatten()
                    .map(|e| {
                        (
                            e.file_name().to_string_lossy().into_owned(),
                            std::fs::read(e.path()).unwrap(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn a_rebuilt_pack_is_hydrated_by_the_repair_refresh_without_downloads() {
        // The reported regression: a recovered pack shows filenames until
        // the user presses "Check for new images now". With the leader's
        // repair refresh every title and author Bing still serves comes
        // back automatically — including entries beyond the download
        // horizon, which are hydrated from the response without a GET —
        // and the result is persisted. Only the historical image outside
        // Bing's window keeps the honest filename fallback.
        let roots = refresh_roots();
        let restore = rebuilt_pack(&roots, &[0, 5]);
        let mut window = Window {
            metadata_repair_due: true,
            catalogue: restore.catalogue,
            ..Window::default()
        };
        window.config.retention_days = 0; // keep the historical image
        for entry in &window.catalogue.images {
            assert_eq!(
                view::display_title(entry),
                entry.filename.file_stem().unwrap().to_string_lossy(),
                "before the repair the popup shows the filename"
            );
        }

        let (base, requests) = window_server(window_json(&[0, 1, 2, 5]));
        let client = bing::http_client().unwrap();
        // Retention-2 horizon: position 0 is in-window and on disk,
        // position 1 is in-window and missing (the one real download),
        // position 2 is eligible beyond the horizon and *not* on disk (no
        // download solely for repair), position 5 is eligible beyond the
        // horizon and on disk — hydrate, never fetch.
        let batch = fetch_and_download(
            &client,
            &base,
            &window.catalogue,
            within(schedule::download_horizon(2)),
            &roots.images,
            &roots.state,
            &test_backfill(),
        )
        .await
        .unwrap();
        assert_eq!(
            *requests.lock().unwrap(),
            vec![bing::image_url("", "/th?id=OHR.Pos1_ROW1")],
            "only the missing in-window image is downloaded"
        );
        let mut urlbases: Vec<_> = batch.fetched.iter().map(|e| e.urlbase.clone()).collect();
        urlbases.sort();
        assert_eq!(
            urlbases,
            [
                "/th?id=OHR.Pos0_ROW0",
                "/th?id=OHR.Pos1_ROW1",
                "/th?id=OHR.Pos5_ROW5",
            ]
        );

        finish_over(
            &mut window,
            &roots,
            wallpaper::CurrentWallpaper::File(PathBuf::from("/elsewhere/foreign.jpg")),
            Ok(batch),
        );

        let titled: Vec<_> = window
            .catalogue
            .images
            .iter()
            .filter(|e| !e.title.is_empty())
            .map(|e| e.urlbase.as_str())
            .collect();
        assert_eq!(titled.len(), 3, "every image Bing still lists is repaired");
        let historical = window
            .catalogue
            .images
            .iter()
            .find(|e| e.urlbase == "/th?id=OHR.Historical_ROW9")
            .expect("the historical image is retained");
        assert_eq!(
            view::display_title(historical),
            "20260701-Historical_ROW9_UHD",
            "outside Bing's response the filename fallback stays"
        );
        let pos5 = window
            .catalogue
            .images
            .iter()
            .find(|e| e.urlbase == "/th?id=OHR.Pos5_ROW5")
            .unwrap();
        assert_eq!(view::display_title(pos5), "x");
        assert_eq!(pos5.copyright, "© y");
        assert_eq!(pos5.copyrightlink, "https://example.com");
        assert_eq!(
            pos5.fullstartdate, "202608020700",
            "real time replaces …0000"
        );
        assert_eq!(
            pos5.filename,
            roots.images.join("20260802-Pos5_ROW5_UHD.jpg")
        );

        // Persisted atomically: a restart loads it with provenance `Loaded`
        // and every title intact.
        let reloaded = Catalogue::load_or_rebuild(&roots.catalogue, &roots.images);
        assert_eq!(reloaded.provenance, Provenance::Loaded);
        assert_eq!(reloaded.catalogue, window.catalogue);
    }

    #[tokio::test]
    async fn an_offline_rebuilt_start_still_gets_thumbnails_from_the_pass() {
        // Rebuilt catalogue, unreachable server: the repair refresh fails at
        // the list fetch and the startup pass — armed regardless — is what
        // fills the cache. The failure keeps every reconstructed entry and
        // JPEG, persists no empty history, and takes the ordinary retry.
        let roots = refresh_roots();
        let restore = rebuilt_pack(&roots, &[0, 3]);
        let catalogue = restore.catalogue;
        let live = wallpaper::CurrentWallpaper::NoFile;

        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            format!("http://127.0.0.1:{port}")
        };
        let client = bing::http_client().unwrap();
        let refresh = fetch_and_download(
            &client,
            &dead,
            &catalogue,
            within(8),
            &roots.images,
            &roots.state,
            &Backfill {
                deferred: true,
                ..test_backfill()
            },
        )
        .await;
        assert!(matches!(refresh, Err(bing::FetchError::Http(_))));

        run_thumbnail_pass(
            catalogue.clone(),
            Backfill::new(&live, 0, None),
            &roots.images,
            &roots.state,
        )
        .await;
        for entry in &catalogue.images {
            assert!(
                thumbs::is_cached(&entry.filename, &roots.state),
                "{} cached by the pass alone",
                entry.filename.display()
            );
        }

        let mut window = Window {
            catalogue: catalogue.clone(),
            ..Window::default()
        };
        let before = file_snapshot(&roots.images);
        finish_over(
            &mut window,
            &roots,
            live,
            Err(RefreshError::Network("connection refused".to_owned())),
        );
        assert_eq!(
            window.catalogue, catalogue,
            "reconstructed entries retained"
        );
        assert_eq!(file_snapshot(&roots.images), before, "no JPEG deleted");
        assert!(
            !roots.catalogue.exists(),
            "a failed repair persists nothing, least of all an empty history"
        );
        assert!(window.last_error.is_some(), "the ordinary 1 h retry path");
    }

    #[tokio::test]
    async fn a_rebuilt_entry_whose_file_is_not_a_jpeg_is_neither_hydrated_nor_fetched() {
        // The hydration's reject branch: an on-disk file beyond the
        // horizon that fails the magic-byte test (a `.part` leftover
        // renamed by hand, a truncated download) is not a usable image, so
        // it must not be dressed up with a title — and, being out of the
        // horizon, is not re-downloaded for repair either.
        let roots = refresh_roots();
        let bogus = roots.images.join("20260802-Pos5_ROW5_UHD.jpg");
        std::fs::write(&bogus, b"not a jpeg").unwrap();
        let restore = Catalogue::load_or_rebuild(&roots.catalogue, &roots.images);
        assert_eq!(restore.provenance, Provenance::Rebuilt);
        assert_eq!(restore.catalogue.images.len(), 1);

        let (base, requests) = window_server(window_json(&[5]));
        let client = bing::http_client().unwrap();
        let batch = fetch_and_download(
            &client,
            &base,
            &restore.catalogue,
            Downloads {
                horizon: schedule::download_horizon(2),
                fallback: false,
            },
            &roots.images,
            &roots.state,
            &test_backfill(),
        )
        .await
        .unwrap();

        assert!(requests.lock().unwrap().is_empty(), "no GET for repair");
        assert!(batch.fetched.is_empty(), "nothing hydrated");
        assert!(bogus.is_file(), "the user's file is left alone");
    }

    #[test]
    fn the_apply_target_is_the_fallback_entry_else_the_newest() {
        let older = entry_in_memory("20260801", "Older_ROW1");
        let newest = entry_in_memory("20260808", "Newest_ROW2");
        let catalogue = Catalogue {
            images: vec![older.clone(), newest.clone()],
        };

        assert_eq!(apply_target(&catalogue, None), Some(&newest));
        assert_eq!(
            apply_target(&catalogue, Some(&older.filename)),
            Some(&older),
            "a downloaded fallback is applied even though it is not the newest"
        );
        assert_eq!(
            apply_target(&catalogue, Some(Path::new("/images/vanished.jpg"))),
            Some(&newest),
            "a fallback with no surviving entry degrades to the newest"
        );
        assert_eq!(apply_target(&Catalogue::default(), None), None);
    }

    #[tokio::test]
    async fn two_producers_over_one_rebuilt_catalogue_leave_one_intact_cache_slot_each() {
        // The startup pass and a *deferred* repair refresh compose over
        // the same rebuilt catalogue: the refresh writes nothing into the
        // cache (`Backfill::deferred`, pinned on its own by
        // `a_deferred_refresh_writes_no_thumbnails_…`), so the cache ends
        // with exactly one thumbnail and one `cached` sidecar per entry
        // and no stray `.part` — and the refresh still hydrates every
        // entry. This is the composition, not the write race the interlock
        // exists for: under the single-threaded test runtime `join!`
        // interleaves cooperatively, so an *undeferred* refresh here would
        // not reproduce the collision either.
        let roots = refresh_roots();
        let restore = rebuilt_pack(&roots, &[0, 1, 2, 3]);
        let catalogue = restore.catalogue;
        let (base, requests) = window_server(window_json(&[0, 1, 2, 3]));
        let client = bing::http_client().unwrap();
        let live = wallpaper::CurrentWallpaper::NoFile;

        let pass = run_thumbnail_pass(
            catalogue.clone(),
            Backfill::new(&live, 0, None),
            &roots.images,
            &roots.state,
        );
        let deferred = Backfill {
            deferred: true,
            ..test_backfill()
        };
        let refresh = fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(8),
            &roots.images,
            &roots.state,
            &deferred,
        );
        let ((), batch) = tokio::join!(pass, refresh);
        let batch = batch.unwrap();

        assert!(
            requests.lock().unwrap().is_empty(),
            "everything was on disk"
        );
        assert_eq!(batch.fetched.len(), 4, "every listed entry is hydrated");
        let listing = thumb_dir_listing(&roots.state);
        assert!(
            listing.keys().all(|name| !name.ends_with(".part")),
            "no stray .part: {listing:?}"
        );
        let mut expected = BTreeMap::new();
        for entry in &catalogue.images {
            if entry.urlbase.contains("Historical") {
                continue;
            }
            assert!(thumbs::is_cached(&entry.filename, &roots.state));
            let thumb = thumbs::thumbnail_path(&entry.filename, &roots.state).unwrap();
            let name = thumb.file_name().unwrap().to_string_lossy().into_owned();
            expected.insert(format!("{name}.meta"), ());
            expected.insert(name, ());
        }
        // The historical image is cached too (retention 0 keeps it); its
        // two artefacts complete the expected set.
        let historical = catalogue
            .images
            .iter()
            .find(|e| e.urlbase.contains("Historical"))
            .unwrap();
        let thumb = thumbs::thumbnail_path(&historical.filename, &roots.state).unwrap();
        let name = thumb.file_name().unwrap().to_string_lossy().into_owned();
        expected.insert(format!("{name}.meta"), ());
        expected.insert(name, ());
        let actual: BTreeMap<String, ()> = listing.keys().map(|k| (k.clone(), ())).collect();
        assert_eq!(
            actual, expected,
            "exactly one thumbnail + sidecar per entry"
        );
    }

    #[tokio::test]
    async fn a_deferred_refresh_writes_no_thumbnails_and_an_undeferred_one_does() {
        // The write interlock itself: with the pass pending the refresh
        // touches the cache neither per download nor in its tail backfill;
        // the next (undeferred) refresh backfills what it downloaded.
        let roots = refresh_roots();
        let (base, _requests) = window_server(window_json(&[0]));
        let client = bing::http_client().unwrap();
        let catalogue = Catalogue::default();

        let batch = fetch_and_download(
            &client,
            &base,
            &catalogue,
            within(8),
            &roots.images,
            &roots.state,
            &Backfill {
                deferred: true,
                ..test_backfill()
            },
        )
        .await
        .unwrap();
        let downloaded = batch.fetched[0].filename.clone();
        assert!(downloaded.is_file());
        assert!(!thumbs::is_cached(&downloaded, &roots.state));
        assert!(
            batch.thumbnails_deferred,
            "the batch reports the debt it leaves behind"
        );
        assert!(
            thumb_dir_listing(&roots.state).is_empty(),
            "nothing written into the cache while deferred"
        );

        let mut merged = catalogue;
        merged.merge(batch.fetched, &roots.images);
        fetch_and_download(
            &client,
            &base,
            &merged,
            within(8),
            &roots.images,
            &roots.state,
            &test_backfill(),
        )
        .await
        .unwrap();
        assert!(
            thumbs::is_cached(&downloaded, &roots.state),
            "the next refresh backfills it"
        );
    }

    #[test]
    fn a_repair_refresh_runs_beside_the_pass_and_blocks_the_sweep() {
        // `arm_leader_duties` arms the pass before the repair refresh, so
        // both producers are in flight and no sweep may run. (That the
        // refresh then writes no thumbnails is the `Backfill::deferred`
        // contract, pinned by `a_deferred_refresh_writes_no_thumbnails_…`;
        // the flag's capture in `start_refresh_over` is not observable
        // here — the policy travels inside the spawned task.)
        let mut armed = Window {
            metadata_repair_due: true,
            ..window_with_images(1)
        };
        drop(armed.arm_initial_duties(wallpaper::CurrentWallpaper::NoFile));
        assert!(armed.thumbnail_pass_pending && armed.refresh_pending);
        assert!(!armed.may_sweep_thumbnails(), "both producers in flight");
    }

    fn deferred_batch(entry: &ImageEntry) -> Result<RefreshBatch, RefreshError> {
        Ok(RefreshBatch {
            fetched: vec![entry.clone()],
            anchor: "202608080700".to_owned(),
            thumbnails_deferred: true,
            ..RefreshBatch::default()
        })
    }

    #[test]
    fn a_deferred_refresh_owes_a_pass_that_the_running_pass_pays_when_it_ends() {
        // The headline scenario: a repair refresh lands while the startup
        // pass is still decoding. Its downloads have no previews and the
        // pass ran over a snapshot taken before they existed, so the
        // pass's end re-arms one more pass over the merged catalogue.
        let roots = refresh_roots();
        let fresh = entry_on_disk(&roots.images, "20260808", "Fresh_ROW1");
        let mut window = window_with_images(1);
        window.config.retention_days = 0;
        window.thumbnail_pass_pending = true;
        let foreign = wallpaper::CurrentWallpaper::File(PathBuf::from("/usr/share/bg/u.jpg"));

        finish_over(&mut window, &roots, foreign.clone(), deferred_batch(&fresh));
        assert!(window.thumbnails_owed, "the debt is booked");
        assert!(
            window.thumbnail_pass_pending,
            "the running pass is not doubled while it still owns the cache"
        );
        assert!(window.catalogue.images.contains(&fresh));

        drop(window.finish_thumbnail_pass(&roots.state, foreign));
        assert!(!window.thumbnails_owed, "paid");
        assert!(
            window.thumbnail_pass_pending,
            "one more pass armed over the merged catalogue"
        );
    }

    #[test]
    fn a_deferred_refresh_that_outlives_the_pass_arms_the_owed_pass_itself() {
        // Other end of the overlap: the pass ended while the refresh was
        // still in flight, so nothing later would re-arm it — the refresh
        // completion arms the owed pass directly.
        let roots = refresh_roots();
        let fresh = entry_on_disk(&roots.images, "20260808", "Fresh_ROW1");
        let mut window = window_with_images(1);
        window.thumbnail_pass_pending = true;
        let foreign = wallpaper::CurrentWallpaper::File(PathBuf::from("/usr/share/bg/u.jpg"));
        drop(window.finish_thumbnail_pass(&roots.state, foreign.clone()));
        assert!(!window.thumbnail_pass_pending && !window.thumbnails_owed);

        finish_over(&mut window, &roots, foreign, deferred_batch(&fresh));
        assert!(!window.thumbnails_owed, "paid on the spot");
        assert!(window.thumbnail_pass_pending, "the owed pass is running");
    }

    #[tokio::test]
    async fn the_owed_pass_decodes_the_out_of_retention_fallback_it_was_armed_for() {
        // Deferred refresh + bounded fallback + pass already ended: the
        // owed pass is armed from `finish_refresh_over` *before* its apply
        // arm, i.e. with the pre-apply live wallpaper, and the fallback is
        // out of retention by construction — so only the fallback
        // exemption makes the pass decode exactly the image the refresh
        // then applies (otherwise: placeholder and no accent for ~24 h).
        let roots = refresh_roots();
        let fallback = entry_on_disk(&roots.images, "20260701", "Fallback_ROW1");
        std::fs::write(&fallback.filename, crate::testutil::tiny_jpeg(64, 36)).unwrap();
        let mut window = window_with_images(1);
        window.config.retention_days = 7;
        window.thumbnail_pass_pending = true;
        let foreign = wallpaper::CurrentWallpaper::File(PathBuf::from("/usr/share/bg/u.jpg"));
        drop(window.finish_thumbnail_pass(&roots.state, foreign.clone()));

        finish_over(
            &mut window,
            &roots,
            foreign.clone(),
            Ok(RefreshBatch {
                fallback: Some(fallback.filename.clone()),
                ..deferred_batch(&fallback).unwrap()
            }),
        );
        assert!(window.thumbnail_pass_pending, "the owed pass is running");
        assert!(
            window.catalogue.images.contains(&fallback),
            "the fallback survived the prune"
        );

        // Exactly what the armed task runs, with the roots injected: the
        // pre-apply live state, and the fallback the window protected when
        // the debt was settled.
        let now = Utc::now();
        assert!(
            !fallback.within_retention(7, now),
            "out of retention by construction"
        );
        run_thumbnail_pass(
            window.catalogue.clone(),
            Backfill {
                protected_fallback: Some(fallback.filename.clone()),
                ..Backfill::new(&foreign, 7, None)
            },
            &roots.images,
            &roots.state,
        )
        .await;
        assert!(
            thumbs::is_cached(&fallback.filename, &roots.state),
            "the fallback gets its thumbnail from the pass it is owed"
        );

        // Without the exemption the same pass skips it — the bug this pins.
        let unprotected = refresh_roots();
        let twin = entry_on_disk(&unprotected.images, "20260701", "Fallback_ROW1");
        std::fs::write(&twin.filename, crate::testutil::tiny_jpeg(64, 36)).unwrap();
        let mut catalogue = Catalogue::default();
        catalogue.merge(vec![twin.clone()], &unprotected.images);
        run_thumbnail_pass(
            catalogue,
            Backfill::new(&foreign, 7, None),
            &unprotected.images,
            &unprotected.state,
        )
        .await;
        assert!(!thumbs::is_cached(&twin.filename, &unprotected.state));
    }

    #[test]
    fn an_undeferred_refresh_owes_nothing_and_a_follower_never_pays() {
        let roots = refresh_roots();
        let fresh = entry_on_disk(&roots.images, "20260808", "Fresh_ROW1");
        let foreign = wallpaper::CurrentWallpaper::File(PathBuf::from("/usr/share/bg/u.jpg"));

        let mut window = window_with_images(1);
        finish_over(
            &mut window,
            &roots,
            foreign.clone(),
            Ok(RefreshBatch {
                thumbnails_deferred: false,
                ..deferred_batch(&fresh).unwrap()
            }),
        );
        assert!(!window.thumbnails_owed && !window.thumbnail_pass_pending);

        // A debt booked before leadership was lost is never paid by a
        // follower: the pass is leader-owned work.
        let mut follower = Window {
            leadership: Leadership::forced(false),
            thumbnails_owed: true,
            thumbnail_pass_pending: true,
            ..Window::default()
        };
        drop(follower.finish_thumbnail_pass(&roots.state, foreign));
        assert!(!follower.thumbnail_pass_pending);
        assert!(follower.thumbnails_owed, "left for a takeover's own pass");
    }

    #[test]
    fn the_startup_thumbnail_pass_defers_the_sweep_until_it_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");

        let mut window = Window::default();
        assert!(window.may_sweep_thumbnails(), "nothing in flight at rest");

        drop(window.start_thumbnail_pass_over(wallpaper::CurrentWallpaper::NoFile));
        assert!(window.thumbnail_pass_pending);
        assert!(!window.may_sweep_thumbnails());

        // Whatever a prune skipped meanwhile is collected when the pass ends,
        // so deferring never leaks an orphan.
        let orphan = state.join("thumbs").join("20250101-Gone_ROW9_UHD.jpg");
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, b"thumb").unwrap();

        drop(window.finish_thumbnail_pass(&state, wallpaper::CurrentWallpaper::NoFile));

        assert!(window.may_sweep_thumbnails());
        assert!(!orphan.exists(), "the deferred sweep runs at the end");
    }

    use crate::testutil::surface::{
        Emitted, app_dropdown_create, dropdown_create, dropdown_destroy, emitted, tooltip_arm,
        tooltip_destroy,
    };

    /// Our popup's own close: the id is dropped, the ledger is reset to zero
    /// (every child dies with it, whatever the count held), and a
    /// possibly-orphaned tooltip is swept.
    ///
    /// The sweep is for the compositor path — `…/handlers/shell/xdg_popup.rs`'s
    /// `done` collects the dismissed popup's *ancestors* and no children, so a
    /// mapped tooltip is left in the runtime's list with a dead parent. On the
    /// self-initiated path the runtime already took it and the destroy is the
    /// documented no-op.
    #[tokio::test]
    async fn popup_closed_for_our_popup_clears_the_ledger_and_sweeps_the_tooltip() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let ours = window::Id::unique();
        window.popup = Some(ours);
        window.dropdowns_open = 1;

        let task = window.update(Message::PopupClosed(ours));

        assert_eq!(window.popup, None);
        assert!(!window.dropdown_open());
        assert_eq!(
            window.stale_menu_closes, 1,
            "the menu still owes a `Done`; our popup's is this very event"
        );
        assert!(
            window.closing_popups.is_empty(),
            "our popup was closed for us, so there is nothing left to await"
        );
        assert_eq!(emitted(task).await, vec![Emitted::DestroyTooltip]);
    }

    /// The tooltip's own close names the shared tooltip surface. It says
    /// nothing about menus, so the ledger must not move — a decrement here
    /// would un-pause tooltips beside a mapped menu.
    #[tokio::test]
    async fn popup_closed_for_the_tooltip_surface_changes_nothing() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let ours = window::Id::unique();
        window.popup = Some(ours);
        window.dropdowns_open = 1;

        let task = window.update(Message::PopupClosed(crate::tooltip::window_id()));

        assert_eq!(window.popup, Some(ours), "a tooltip close it is, not ours");
        assert!(window.dropdown_open(), "a dropdown close it is not");
        assert!(emitted(task).await.is_empty(), "nothing to destroy");
    }

    /// A dropdown menu is identified by elimination — its window id is minted
    /// inside the widget and never visible here. Grab-loss dismissal publishes
    /// no `DestroyPopup`, so this close is the only signal the ledger gets.
    #[tokio::test]
    async fn popup_closed_for_an_unknown_surface_decrements_the_dropdown_count() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let ours = window::Id::unique();
        window.popup = Some(ours);
        window.dropdowns_open = 1;

        let task = window.update(Message::PopupClosed(window::Id::unique()));

        assert_eq!(window.popup, Some(ours), "another surface is not ours");
        assert!(!window.dropdown_open());
        assert_eq!(
            emitted(task).await,
            vec![Emitted::DestroyTooltip],
            "the menu is gone, so a tooltip under it is topmost again"
        );
    }

    /// The overlapping-lifetimes case a single bool cannot survive: a `Done`
    /// for a menu that is already gone is delivered *after* a newer menu was
    /// created. Clearing on it would leave tooltips un-paused beside the live
    /// menu — the two-children state the invariant forbids — so the ledger
    /// counts instead, and the newer create still stands after the older
    /// close. The trailing unpaired close pins the saturating floor: an extra
    /// `Done` (a stale id for something already gone) can never push the
    /// count below "nothing open", which is what keeps the by-elimination
    /// misclassification harmless.
    #[tokio::test]
    async fn a_close_for_an_earlier_menu_leaves_a_newer_one_recorded() {
        use cosmic::Application as _;

        let mut window = Window::default();
        window.popup = Some(window::Id::unique());

        drop(window.update(Message::DropdownSurface(dropdown_create())));
        drop(window.update(Message::DropdownSurface(dropdown_create())));
        assert_eq!(window.dropdowns_open, 2, "two creates, two menus");

        let task = window.update(Message::PopupClosed(window::Id::unique()));
        assert_eq!(emitted(task).await, vec![Emitted::DestroyTooltip]);
        assert!(
            window.dropdown_open(),
            "the older menu's close says nothing about the newer one"
        );

        drop(window.update(Message::PopupClosed(window::Id::unique())));
        assert!(!window.dropdown_open(), "now every menu is accounted for");

        drop(window.update(Message::PopupClosed(window::Id::unique())));
        assert_eq!(window.dropdowns_open, 0, "the count saturates at zero");
    }

    /// The stale-id case the by-elimination rule cannot distinguish: the
    /// `Done` for a popup `TogglePopup` already took out of `self.popup`
    /// arrives later and is read as a menu. Harmless because `TogglePopup`
    /// booked it by name as a close owed by the session it ended
    /// ([`Window::closing_popups`]), so it is settled rather than charged to
    /// the live count.
    #[tokio::test]
    async fn a_late_close_for_a_popup_we_already_took_is_harmless() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let ours = window::Id::unique();
        window.popup = Some(ours);
        drop(window.update(Message::TogglePopup));
        assert_eq!(window.popup, None, "taken before the destroy is emitted");
        assert_eq!(
            window.closing_popups,
            vec![ClosingPopup {
                id: ours,
                menus_owed: 0
            }],
            "our popup still owes its own, and it is owed by name"
        );
        assert_eq!(window.stale_menu_closes, 0, "no menu was mapped");

        let task = window.update(Message::PopupClosed(ours));

        assert_eq!(window.popup, None);
        assert!(!window.dropdown_open());
        assert!(window.closing_popups.is_empty(), "the debt is settled");
        assert_eq!(emitted(task).await, vec![Emitted::DestroyTooltip]);
    }

    /// The keyed half of the debt earning its keep: a popup create that never
    /// mapped leaves `self.popup` holding an id nothing can destroy, so its
    /// `Done` never comes. (`self.popup` is set in the create's *settings*
    /// closure, which libcosmic runs before it asks the sctk thread for the
    /// surface; a failed or five-times-deferred create is only logged; and
    /// destroying an unmapped id logs `"No popup to destroy"` and emits
    /// nothing — see [`Window::closing_popups`].)
    ///
    /// Booked anonymously, that never-paid unit would swallow the *next*
    /// session's live menu close and strand `dropdowns_open` at one with no
    /// menu mapped — tooltips paused for the rest of the session. Booked by
    /// name it just sits there.
    #[tokio::test]
    async fn a_popup_that_never_mapped_owes_a_debt_no_live_close_can_pay() {
        use cosmic::Application as _;

        let mut window = Window::default();
        // A create whose surface the runtime never produced: the id is ours,
        // but nothing is mapped and no `Done` will ever name it.
        let phantom = window::Id::unique();
        window.popup = Some(phantom);
        drop(window.update(Message::TogglePopup));
        assert_eq!(
            window.closing_popups,
            vec![ClosingPopup {
                id: phantom,
                menus_owed: 0
            }],
            "owed by name, so no other close can settle it"
        );

        // Reopened, with a menu of its own.
        drop(window.update(Message::TogglePopup));
        window.popup = Some(window::Id::unique());
        drop(window.update(Message::DropdownSurface(dropdown_create())));
        assert!(window.dropdown_open(), "a live menu is mapped");

        // The live menu closes. Its `Done` must be charged to the live count,
        // not consumed by the phantom's outstanding debt.
        drop(window.update(Message::PopupClosed(window::Id::unique())));

        assert!(
            !window.dropdown_open(),
            "the live menu is gone, so tooltips must be un-paused"
        );
        assert_eq!(
            window.closing_popups,
            vec![ClosingPopup {
                id: phantom,
                menus_owed: 0
            }],
            "the unpayable debt is still outstanding, and cost nothing"
        );
    }

    /// The create-side twin of the late-close hazard: `TogglePopup` decides its
    /// branch in `update()`, but the popup id is minted a message round later,
    /// in the create's settings closure. libcosmic drains *every* queued
    /// message before running any of the actions they produced
    /// (`iced/winit/src/lib.rs`), and the create is one of those actions, so
    /// two panel clicks in one drain both see `self.popup == None` and both
    /// open. The second closure then displaces the first popup — which upstream
    /// destroys for real (the `parent_mismatch` path), announcing a `Done`.
    /// Unbooked, that `Done` would be charged to the live count by elimination.
    #[tokio::test]
    async fn a_second_toggle_in_one_drain_books_the_popup_it_displaces() {
        use cosmic::Application as _;

        let mut window = Window::default();

        drop(window.update(Message::TogglePopup));
        drop(window.update(Message::TogglePopup));
        assert_eq!(
            window.popup, None,
            "both toggles take the create branch: no closure has run yet"
        );

        // Both settings closures, back to back, as the next round runs them.
        let first = window::Id::unique();
        let second = window::Id::unique();
        window.adopt_popup(first);
        window.adopt_popup(second);

        assert_eq!(window.popup, Some(second), "the later create wins");
        assert_eq!(
            window.closing_popups,
            vec![ClosingPopup {
                id: first,
                menus_owed: 0
            }],
            "the displaced popup is owed by name, not dropped on the floor"
        );

        // A menu opened in the surviving popup, and only then does upstream's
        // parent-mismatch destroy of the displaced popup come back.
        drop(window.update(Message::DropdownSurface(dropdown_create())));
        assert_eq!(window.dropdowns_open, 1, "the live session's menu");

        drop(window.update(Message::PopupClosed(first)));

        assert!(
            window.dropdown_open(),
            "the displaced popup's close must not decrement a live menu"
        );
        assert!(window.closing_popups.is_empty(), "the debt is settled");
    }

    /// A displacement does **not** reset the menu count, unlike `TogglePopup`.
    /// Nothing is subtracted, so a menu that dies with the displaced popup is
    /// still counted and is paid for by its own `Done` through the
    /// by-elimination row — exact arithmetic, no anonymous debt, and the count
    /// never dips below the number of mapped menus.
    #[tokio::test]
    async fn a_displaced_popup_leaves_its_menus_counted() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let first = window::Id::unique();
        window.popup = Some(first);
        drop(window.update(Message::DropdownSurface(dropdown_create())));
        assert_eq!(window.dropdowns_open, 1, "a menu is up under the first");

        window.adopt_popup(window::Id::unique());
        assert_eq!(
            window.dropdowns_open, 1,
            "the menu is still mapped and still owes its close"
        );
        assert_eq!(
            window.stale_menu_closes, 0,
            "nothing was subtracted, so nothing is owed anonymously"
        );

        // The menu dies with the popup it hung under; its own close pays for
        // it, and the displaced popup's close settles the keyed debt.
        drop(window.update(Message::PopupClosed(window::Id::unique())));
        assert!(!window.dropdown_open(), "every menu is accounted for");
        drop(window.update(Message::PopupClosed(first)));
        assert!(window.closing_popups.is_empty(), "and so is the popup");
    }

    /// The disputed interleaving, settled against the pinned libcosmic rev and
    /// pinned here so it is not re-litigated: a self-initiated destroy's
    /// `Done`s are delivered across a thread and four queues
    /// ([`Window::stale_menu_closes`] carries the trace), so a reopen *and* a
    /// fresh menu create can be processed before they land. Those late closes
    /// belong to the session that ended, and must not decrement the new one —
    /// a zeroed count with a menu mapped un-pauses tooltips beside a live
    /// sibling, which is exactly the two-children state the branch exists to
    /// forbid.
    #[tokio::test]
    async fn late_closes_from_an_ended_session_never_decrement_a_reopened_one() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let first = window::Id::unique();
        window.popup = Some(first);
        drop(window.update(Message::DropdownSurface(dropdown_create())));
        assert_eq!(
            window.dropdowns_open, 1,
            "a menu is up in the first session"
        );

        // Panel icon clicked: our popup and its menu are torn down, and each
        // owes a `Done` that has not been delivered yet.
        drop(window.update(Message::TogglePopup));
        assert_eq!(
            window.closing_popups,
            vec![ClosingPopup {
                id: first,
                menus_owed: 1
            }],
            "our popup by name, its menu anonymously on the same entry"
        );

        // Reopened and a new menu opened, all before those `Done`s are
        // drained. The open branch's popup id is minted inside the runtime's
        // settings closure, which tests never run, so `adopt_popup` — the only
        // thing that closure does to the ledger — is called directly.
        drop(window.update(Message::TogglePopup));
        window.adopt_popup(window::Id::unique());
        drop(window.update(Message::DropdownSurface(dropdown_create())));
        assert_eq!(window.dropdowns_open, 1, "the second session's menu");
        assert_eq!(
            window.closing_popups[0].menus_owed, 1,
            "a new session discards no debt that is still keyed to a popup"
        );

        // Now the first session's two `Done`s arrive: the menu's (an id we
        // never saw) and our old popup's (an id `self.popup` no longer holds).
        drop(window.update(Message::PopupClosed(window::Id::unique())));
        drop(window.update(Message::PopupClosed(first)));

        assert!(
            window.dropdown_open(),
            "the live menu is still mapped, so tooltips stay paused"
        );
        assert!(
            window.closing_popups.is_empty(),
            "the menu debt is paid and ours is settled"
        );

        // And the live menu's own close still lands normally.
        drop(window.update(Message::PopupClosed(window::Id::unique())));
        assert!(!window.dropdown_open(), "now every menu is accounted for");
    }

    /// The other side of that debt, and the bound it owes the user: a menu
    /// create the runtime **drops** (deferred behind a non-empty
    /// `state.destroyed` — which the interlock's own tooltip destroy populates
    /// — and then abandoned after five 30 ms retries) is counted but never
    /// mapped, so no `Done` will ever pay for it.
    ///
    /// Unbounded, that phantom unit is re-booked as an owed close when the
    /// session ends, swallows the *next* session's live menu close, is re-booked
    /// again when that session ends… and hover tooltips are dead for the rest of
    /// the process after one transient upstream hiccup. Here the session's debt
    /// is keyed to the popup that ended it, and our popup's own `Done` comes
    /// back only after every child close that teardown will emit
    /// ([`ClosingPopup::menus_owed`]) — so what is left over is discarded with
    /// it. Three sessions, because the damage this pins is precisely a debt that
    /// outlives its own session.
    #[tokio::test]
    async fn a_dropped_menu_create_owes_nothing_past_the_session_that_leaked_it() {
        use cosmic::Application as _;

        let mut window = Window::default();
        window.adopt_popup(window::Id::unique());

        // Session 1: a create the runtime silently drops. The ledger cannot
        // know, so tooltips stay paused for the rest of this session — the
        // accepted, safe direction.
        drop(window.update(Message::DropdownSurface(dropdown_create())));
        assert!(
            window.dropdown_open(),
            "counted optimistically, as designed"
        );

        for session in 2..=3 {
            // The panel icon ends the session, booking whatever was counted.
            let ended = window.popup.expect("a session is open");
            drop(window.update(Message::TogglePopup));

            // Our popup's `Done`, after every child close that teardown emits.
            drop(window.update(Message::PopupClosed(ended)));
            assert!(
                window.closing_popups.is_empty(),
                "session {session}: the phantom cannot be owed to anyone"
            );

            // The next session, with a menu that really maps.
            drop(window.update(Message::TogglePopup));
            window.adopt_popup(window::Id::unique());
            drop(window.update(Message::DropdownSurface(dropdown_create())));
            assert!(window.dropdown_open(), "session {session}: a live menu");

            drop(window.update(Message::PopupClosed(window::Id::unique())));
            assert!(
                !window.dropdown_open(),
                "session {session}: the live menu's close must reach the count, \
                 not pay off a phantom — tooltips are dead for good otherwise"
            );
        }
    }

    /// Same phantom, ended the other way: a compositor dismissal reaches the
    /// "ours" row, which has no id to key its menu debt to and can only count
    /// it. That debt is discarded when the next session's popup id is adopted —
    /// sound because `…/handlers/shell/xdg_popup.rs::done` queues the whole
    /// dismissed chain's closes in one call, ahead of the click that opens the
    /// next popup ([`Window::stale_menu_closes`]).
    #[tokio::test]
    async fn a_dismissed_session_owes_no_menu_closes_once_the_next_popup_opens() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let ours = window::Id::unique();
        window.adopt_popup(ours);
        drop(window.update(Message::DropdownSurface(dropdown_create())));

        // Clicked outside: the compositor `done`s our popup itself.
        drop(window.update(Message::PopupClosed(ours)));
        assert_eq!(
            window.stale_menu_closes, 1,
            "the menu's close is still owed, anonymously"
        );

        window.adopt_popup(window::Id::unique());
        assert_eq!(
            window.stale_menu_closes, 0,
            "by now the dismissal's closes have all been delivered"
        );

        // So the new session's own menu is accounted for exactly.
        drop(window.update(Message::DropdownSurface(dropdown_create())));
        assert!(window.dropdown_open());
        drop(window.update(Message::PopupClosed(window::Id::unique())));
        assert!(
            !window.dropdown_open(),
            "the live close must decrement, not pay off a dead session's debt"
        );
    }

    /// With no menu mapped the tooltip is the topmost popup, so everything it
    /// publishes is legal and goes straight through — including a shape the
    /// ledger does not classify (the catch-all), which must still be
    /// forwarded rather than swallowed.
    #[tokio::test]
    async fn tooltip_actions_are_forwarded_when_no_dropdown_is_open() {
        use cosmic::Application as _;

        let mut window = Window::default();
        window.popup = Some(window::Id::unique());

        for (action, expected) in [
            (tooltip_arm(), Emitted::Arm),
            (tooltip_destroy(), Emitted::DestroyTooltip),
            (dropdown_create(), Emitted::Create),
        ] {
            let task = window.update(Message::TooltipSurface(action));
            assert_eq!(emitted(task).await, vec![expected]);
            assert!(!window.dropdown_open(), "the tooltip route never opens one");
        }
    }

    /// The crash case: while a menu is mapped the tooltip is a *sibling* under
    /// it on one xdg-shell stack. Arming would map a second child; destroying
    /// would target a non-topmost popup, which is fatal. So everything the
    /// tooltip publishes is dropped — the menu's own close re-emits the
    /// destroy unconditionally.
    #[tokio::test]
    async fn every_tooltip_action_is_dropped_while_a_dropdown_is_open() {
        use cosmic::Application as _;

        let mut window = Window::default();
        window.popup = Some(window::Id::unique());
        window.dropdowns_open = 1;

        for action in [tooltip_arm(), tooltip_destroy(), dropdown_create()] {
            let task = window.update(Message::TooltipSurface(action));
            assert!(
                emitted(task).await.is_empty(),
                "nothing may reach the runtime under an open menu"
            );
            assert!(window.dropdown_open());
        }
    }

    /// The interlock, and the only ordering that matters: the create arriving
    /// is the last moment a tooltip is still topmost, so its destroy is
    /// chained *ahead* of the forwarded create.
    #[tokio::test]
    async fn a_dropdown_create_destroys_the_tooltip_first() {
        use cosmic::Application as _;

        for create in [dropdown_create(), app_dropdown_create()] {
            let mut window = Window::default();
            window.popup = Some(window::Id::unique());

            let task = window.update(Message::DropdownSurface(create));

            assert!(window.dropdown_open());
            assert_eq!(
                emitted(task).await,
                vec![Emitted::DestroyTooltip, Emitted::Create],
                "the destroy must precede the create, and never be batched with it"
            );
        }
    }

    /// A create arriving while a menu is already mapped keeps the interlock.
    /// Any tooltip that reached the surface then was mapped *after* the menu,
    /// i.e. above it, so destroying it is still legal; if none is, the destroy
    /// is the runtime's documented no-op.
    #[tokio::test]
    async fn a_dropdown_create_over_an_open_menu_still_interlocks() {
        use cosmic::Application as _;

        let mut window = Window::default();
        window.popup = Some(window::Id::unique());
        window.dropdowns_open = 1;

        let task = window.update(Message::DropdownSurface(dropdown_create()));

        assert!(window.dropdown_open());
        assert_eq!(
            emitted(task).await,
            vec![Emitted::DestroyTooltip, Emitted::Create]
        );
    }

    /// A shape the dropdown route does not classify goes through untouched and
    /// leaves the ledger alone — the catch-all must forward, not swallow.
    #[tokio::test]
    async fn an_unclassified_dropdown_action_is_forwarded_untouched() {
        use cosmic::Application as _;

        let mut window = Window::default();
        window.popup = Some(window::Id::unique());

        let task = window.update(Message::DropdownSurface(tooltip_arm()));

        assert!(!window.dropdown_open(), "no create, no menu");
        assert_eq!(emitted(task).await, vec![Emitted::Arm]);
    }

    /// The menu's own destroy: forwarded first, then the tooltip sweep — only
    /// once the menu is gone is a tooltip beneath it topmost again. This is
    /// what makes dropping a tooltip destroy during the menu safe.
    ///
    /// The count stays put: a destroy *request* is not evidence the menu died
    /// (see the field doc). `PopupClosed` is what decrements it, and the
    /// runtime sends one for every popup this destroy really tears down.
    #[tokio::test]
    async fn a_dropdown_destroy_sweeps_the_tooltip_afterwards() {
        use cosmic::Application as _;

        let mut window = Window::default();
        window.popup = Some(window::Id::unique());
        window.dropdowns_open = 1;

        let task = window.update(Message::DropdownSurface(dropdown_destroy()));

        assert!(
            window.dropdown_open(),
            "the request is not the close; only `PopupClosed` decrements the count"
        );
        assert_eq!(
            emitted(task).await,
            vec![Emitted::DestroyOther, Emitted::DestroyTooltip],
            "the menu goes first; only then is the tooltip topmost"
        );
    }

    /// The two-rows regression: both dropdowns publish through one
    /// `DropdownSurface`, and `Row::update` hands the same `ButtonPressed` to
    /// every child regardless of `capture_event`. A row whose widget kept a
    /// stale `is_open` — grab-loss dismissal destroys the menu without ever
    /// reaching that widget's `ButtonPressed` arm, so nothing resets it —
    /// therefore emits a destroy for an already-dead popup in the *same* pass
    /// as the other row's create, and tree order (interval before retention in
    /// `view.rs`) puts the create first. Decrementing on the destroy would end
    /// that pass with a menu mapped and tooltips un-paused, which is the
    /// two-children state the invariant forbids.
    #[tokio::test]
    async fn a_stale_destroy_after_a_create_leaves_the_menu_recorded() {
        use cosmic::Application as _;

        let mut window = Window::default();
        window.popup = Some(window::Id::unique());

        let task = window.update(Message::DropdownSurface(dropdown_create()));
        assert_eq!(
            emitted(task).await,
            vec![Emitted::DestroyTooltip, Emitted::Create]
        );

        // The other row's stale destroy, same pass. A runtime no-op ("No popup
        // to destroy"), so it produces no `Done` and must not move the ledger.
        let task = window.update(Message::DropdownSurface(dropdown_destroy()));
        assert_eq!(
            emitted(task).await,
            vec![Emitted::DestroyOther, Emitted::DestroyTooltip]
        );

        assert!(
            window.dropdown_open(),
            "the menu the create mapped is still up, so tooltips stay paused"
        );
    }

    /// Clicking the panel icon closes our popup with an explicit destroy. The
    /// runtime *does* announce that back as `PopupClosed`, but asynchronously
    /// and with an id `self.popup` no longer holds, so the ledger is cleared
    /// on the spot — a stale count would pause every later tooltip — and the
    /// closes still in flight are booked as owed by the ended session.
    #[tokio::test]
    async fn closing_our_own_popup_clears_the_ledger() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let ours = window::Id::unique();
        window.popup = Some(ours);
        window.dropdowns_open = 1;

        let task = window.update(Message::TogglePopup);

        assert_eq!(window.popup, None);
        assert!(
            !window.dropdown_open(),
            "a stale dropdown would pause tooltips for the rest of the session"
        );
        assert_eq!(
            window.stale_menu_closes, 0,
            "a self-initiated ending keys its menu debt to the popup it closed"
        );
        assert_eq!(
            window.closing_popups,
            vec![ClosingPopup {
                id: ours,
                menus_owed: 1
            }],
            "ours is owed by name, its menu's `Done` counted on the same entry"
        );
        assert_eq!(
            emitted(task).await,
            vec![Emitted::DestroyOther],
            "our own popup, not the tooltip surface"
        );
    }

    /// The full crash sequence end to end: hover arms a tooltip, a menu opens
    /// over it, the pointer leaves, the menu closes. At no point does a
    /// destroy for a non-topmost popup reach the runtime.
    #[tokio::test]
    async fn the_hover_then_menu_sequence_never_emits_an_illegal_destroy() {
        use cosmic::Application as _;

        let mut window = Window::default();
        window.popup = Some(window::Id::unique());

        let task = window.update(Message::TooltipSurface(tooltip_arm()));
        assert_eq!(emitted(task).await, vec![Emitted::Arm]);

        let task = window.update(Message::DropdownSurface(dropdown_create()));
        assert_eq!(
            emitted(task).await,
            vec![Emitted::DestroyTooltip, Emitted::Create]
        );

        // The pointer leaves while the menu is up: this destroy is the crash
        // if forwarded, so it is dropped.
        let task = window.update(Message::TooltipSurface(tooltip_destroy()));
        assert!(emitted(task).await.is_empty());

        let task = window.update(Message::DropdownSurface(dropdown_destroy()));
        assert_eq!(
            emitted(task).await,
            vec![Emitted::DestroyOther, Emitted::DestroyTooltip],
            "the dropped destroy is re-emitted once it is legal"
        );
        assert!(window.dropdown_open(), "the request is not yet the close");

        // The runtime announces the teardown, and only that re-opens the
        // tooltip window.
        let task = window.update(Message::PopupClosed(window::Id::unique()));
        assert_eq!(emitted(task).await, vec![Emitted::DestroyTooltip]);
        assert!(!window.dropdown_open());
    }

    #[test]
    fn set_config_adopts_only_real_changes() {
        let mut window = Window::default();
        let same = window.config.clone();
        window.set_config(same.clone());
        assert_eq!(window.config, same);

        let mut changed = same.clone();
        changed.shuffle_interval_secs = 1_800;
        window.set_config(changed.clone());
        assert_eq!(window.config, changed);
    }

    #[tokio::test]
    async fn leader_and_follower_setting_writes_do_not_clobber_other_keys() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        let dir = tempfile::tempdir().unwrap();
        let context = cosmic_config::Config::with_custom_path(
            APP_ID,
            AppletConfig::VERSION,
            dir.path().to_path_buf(),
        )
        .unwrap();
        AppletConfig::default().write_entry(&context).unwrap();
        let mut leader = Window {
            config_context: Some(context.clone()),
            ..Window::default()
        };
        let mut follower = Window {
            leadership: Leadership::forced(false),
            config_context: Some(context.clone()),
            ..Window::default()
        };

        // Both processes begin from the same stale full snapshot, then write
        // unrelated controls in the adversarial follower-before-leader order.
        let follower_write = follower.update(Message::SetRetention(2));
        let leader_write = leader.update(Message::SetShuffleEnabled(true));
        let mut follower_messages = app_messages(follower_write).await;
        drop(follower.update(follower_messages.pop().unwrap()));
        let mut leader_messages = app_messages(leader_write).await;
        drop(leader.update(leader_messages.pop().unwrap()));

        let disk = AppletConfig::load(&context);
        assert_eq!(disk.retention_days, 30);
        assert!(disk.shuffle_enabled);
    }

    // -----------------------------------------------------------------
    // Accent-from-wallpaper wiring. The theme configs are TempDir-rooted
    // (`accent::ThemeHandles::sandboxed`) and the applet config context is
    // either absent (in-memory only) or TempDir-rooted — nothing ever
    // touches the real user theme or settings.
    // -----------------------------------------------------------------

    use crate::accent::{AccentPair, AccentSnapshot};
    use crate::testutil::{dark_palette, light_palette, read_only_trees, restore_dir_permissions};

    /// A window with the accent feature on, sandboxed theme handles, and a
    /// real (TempDir-rooted) applet-config context — enabling refuses a
    /// memory-only config, and "persisted" assertions can mean on-disk.
    /// The enabled state is persisted like the production toggler would
    /// have: the executor's own persists are per-key (checked setters), so
    /// they never re-write `accent_enabled` themselves.
    fn accent_window(dir: &tempfile::TempDir) -> Window {
        use cosmic_config::CosmicConfigEntry as _;

        let mut window = Window {
            accent_handles: Some(accent::ThemeHandles::sandboxed(dir.path()).unwrap()),
            config_context: Some(
                cosmic_config::Config::with_custom_path(
                    APP_ID,
                    AppletConfig::VERSION,
                    dir.path().join("applet-config"),
                )
                .unwrap(),
            ),
            ..Window::default()
        };
        window.config.accent_enabled = true;
        window
            .config
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();
        window
    }

    /// Rewind an [`accent_window`] to the not-yet-enabled state, in memory
    /// *and* on disk — tests that start from "feature off" must not leave a
    /// stray persisted `accent_enabled = true` behind.
    fn start_disabled(window: &mut Window) {
        use cosmic_config::CosmicConfigEntry as _;

        window.config.accent_enabled = false;
        window
            .config
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();
    }

    /// The applet config as persisted in `window`'s TempDir-rooted context.
    fn persisted_config(window: &Window) -> AppletConfig {
        AppletConfig::load(window.config_context.as_ref().unwrap())
    }

    #[tokio::test]
    async fn non_leader_accent_toggle_persists_only_the_raw_flag() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        window.leadership = Leadership::forced(false);
        window.config.accent_snapshot = Some(AccentSnapshot {
            light: Some([1, 2, 3]),
            dark: Some([4, 5, 6]),
        });
        window.config.accent_last_written = Some(AccentPair {
            light: [7, 8, 9],
            dark: [10, 11, 12],
        });
        window
            .config
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();
        let snapshot = window.config.accent_snapshot;
        let last_written = window.config.accent_last_written;
        let write_generation = window.accent_write_generation;

        let mut outputs = app_messages(window.update(Message::SetAccentEnabled(false))).await;
        assert_eq!(outputs.len(), 1);
        drop(window.update(outputs.pop().unwrap()));

        assert!(!window.config.accent_enabled);
        let disk = persisted_config(&window);
        assert!(!disk.accent_enabled);
        assert_eq!(disk.accent_snapshot, snapshot);
        assert_eq!(disk.accent_last_written, last_written);
        assert!(window.accent_inflight.is_none());
        assert_eq!(window.accent_write_generation, write_generation);
    }

    #[tokio::test]
    async fn non_leader_accent_toggle_requires_a_successful_persist() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        let mut absent = Window {
            leadership: Leadership::forced(false),
            ..Window::default()
        };
        drop(absent.update(Message::SetAccentEnabled(true)));
        assert!(!absent.config.accent_enabled);
        assert!(absent.accent_inflight.is_none());

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("config");
        let context =
            cosmic_config::Config::with_custom_path(APP_ID, AppletConfig::VERSION, root.clone())
                .unwrap();
        AppletConfig::default().write_entry(&context).unwrap();
        let version_dir = root.join("cosmic").join(APP_ID).join("v1");
        std::fs::remove_dir_all(&version_dir).unwrap();
        std::fs::write(&version_dir, b"not a directory").unwrap();
        let mut failing = Window {
            leadership: Leadership::forced(false),
            config_context: Some(context),
            ..Window::default()
        };
        let mut outputs = app_messages(failing.update(Message::SetAccentEnabled(true))).await;
        assert_eq!(outputs.len(), 1);
        drop(failing.update(outputs.pop().unwrap()));
        assert!(!failing.config.accent_enabled);
        assert!(failing.accent_inflight.is_none());
    }

    #[tokio::test]
    async fn non_leader_config_echo_uses_fresh_flag_and_keeps_accent_records() {
        use cosmic::Application as _;
        use cosmic_config::ConfigSet as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        window.leadership = Leadership::forced(false);
        let snapshot = Some(AccentSnapshot {
            light: Some([1, 2, 3]),
            dark: None,
        });
        let last_written = Some(AccentPair {
            light: [4, 5, 6],
            dark: [7, 8, 9],
        });
        window.config.accent_snapshot = snapshot;
        window.config.accent_last_written = last_written;
        window
            .config_context
            .as_ref()
            .unwrap()
            .set("accent_enabled", false)
            .unwrap();

        let mut stale = window.config.clone();
        stale.accent_enabled = true;
        stale.accent_snapshot = None;
        stale.accent_last_written = None;
        let mut outputs = app_messages(window.update(Message::ConfigUpdated(stale))).await;
        assert_eq!(outputs.len(), 1);
        drop(window.update(outputs.pop().unwrap()));

        assert!(!window.config.accent_enabled, "fresh disk flag wins");
        assert_eq!(window.config.accent_snapshot, snapshot);
        assert_eq!(window.config.accent_last_written, last_written);
        assert!(window.accent_inflight.is_none());
    }

    #[tokio::test]
    async fn follower_apply_updates_navigation_and_persists_notice_off_thread() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let context = cosmic_config::Config::with_custom_path(
            APP_ID,
            AppletConfig::VERSION,
            dir.path().join("config"),
        )
        .unwrap();
        let mut window = Window {
            leadership: Leadership::forced(false),
            coordination_context: Some(context),
            coordination_state_dir: dir.path().join("state"),
            cold_start: ColdStart::Pending,
            config: AppletConfig {
                accent_enabled: true,
                ..Default::default()
            },
            ..Window::default()
        };
        let applied = PathBuf::from("/images/peer-applied.jpg");

        let task = window.finish_manual_apply(applied.clone());
        assert_eq!(window.current, Some(applied.clone()));
        assert_eq!(window.cold_start, ColdStart::Pending, "leader owns it");
        assert_eq!(window.non_leader_reload_generation, 1);
        assert!(window.accent_inflight.is_none());
        assert_eq!(window.shuffle_generation, 0);

        let mut messages = app_messages(task).await;
        assert_eq!(messages.len(), 1);
        drop(window.update(messages.pop().unwrap()));
        let notice = CoordinationConfig::load(window.coordination_context.as_ref().unwrap())
            .apply_notice
            .unwrap();
        assert_eq!(notice.generation, 1);
        assert_eq!(notice.path, applied);
    }

    #[tokio::test]
    async fn missing_or_failed_apply_notice_does_not_start_follower_lifecycle() {
        let applied = PathBuf::from("/images/peer-applied.jpg");
        let mut absent = Window {
            leadership: Leadership::forced(false),
            cold_start: ColdStart::Pending,
            ..Window::default()
        };
        let task = absent.finish_manual_apply(applied.clone());
        assert_eq!(task.units(), 0);
        assert_eq!(absent.current, Some(applied.clone()));
        assert_eq!(absent.cold_start, ColdStart::Pending);
        assert!(absent.accent_inflight.is_none());

        let dir = tempfile::tempdir().unwrap();
        let context = cosmic_config::Config::with_custom_path(
            APP_ID,
            AppletConfig::VERSION,
            dir.path().join("config"),
        )
        .unwrap();
        let invalid_state = dir.path().join("not-a-directory");
        std::fs::write(&invalid_state, b"file").unwrap();
        let mut failing = Window {
            leadership: Leadership::forced(false),
            coordination_context: Some(context),
            coordination_state_dir: invalid_state,
            cold_start: ColdStart::Pending,
            ..Window::default()
        };
        let mut messages = app_messages(failing.finish_manual_apply(applied.clone())).await;
        assert_eq!(messages.len(), 1);
        let Message::PeerApplyNoticeWritten(Err(_)) = messages.pop().unwrap() else {
            panic!("expected a failed notice persist")
        };
        assert_eq!(failing.current, Some(applied));
        assert_eq!(failing.cold_start, ColdStart::Pending);
        assert!(failing.accent_inflight.is_none());
    }

    #[test]
    fn stale_and_non_file_peer_apply_validations_cannot_regress_leader() {
        let original = PathBuf::from("/images/current.jpg");
        let stale = PathBuf::from("/images/stale.jpg");
        let mut leader = Window {
            current: Some(original.clone()),
            cold_start: ColdStart::Pending,
            ..Window::default()
        };

        assert_eq!(
            leader
                .consume_peer_apply_notice(Some(PeerApplyNotice {
                    generation: 2,
                    path: stale.clone(),
                }))
                .units(),
            1
        );
        assert_eq!(
            leader
                .consume_peer_apply_notice(Some(PeerApplyNotice {
                    generation: 3,
                    path: PathBuf::from("/images/newer.jpg"),
                }))
                .units(),
            1
        );
        drop(leader.finish_peer_apply_validation(2, wallpaper::CurrentWallpaper::File(stale)));
        drop(leader.finish_peer_apply_validation(3, wallpaper::CurrentWallpaper::NoFile));
        assert_eq!(leader.current, Some(original.clone()));
        assert_eq!(leader.cold_start, ColdStart::Pending);

        assert_eq!(
            leader
                .consume_peer_apply_notice(Some(PeerApplyNotice {
                    generation: 2,
                    path: PathBuf::from("/images/older.jpg"),
                }))
                .units(),
            0,
            "stale watcher payload is inert"
        );
        leader.peer_apply_notice_generation = 4;
        drop(leader.finish_peer_apply_validation(4, wallpaper::CurrentWallpaper::Unknown));
        assert_eq!(leader.current, Some(original));
        assert_eq!(leader.cold_start, ColdStart::Pending);
    }

    #[tokio::test]
    async fn two_windows_leave_accent_ownership_with_the_leader_after_peer_apply() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut leader = accent_window(&dir);
        start_disabled(&mut leader);
        drop(leader.update(Message::SetAccentEnabled(true)));
        let snapshot = leader.config.accent_snapshot;
        assert!(snapshot.is_some());
        leader.cold_start = ColdStart::Pending;
        let old = PathBuf::from("/images/old.jpg");
        leader.current = Some(old.clone());

        let coordination_context = cosmic_config::Config::with_custom_path(
            APP_ID,
            AppletConfig::VERSION,
            dir.path().join("coordination-config"),
        )
        .unwrap();
        let mut follower = Window {
            leadership: Leadership::forced(false),
            config: leader.config.clone(),
            config_context: leader.config_context.clone(),
            coordination_context: Some(coordination_context.clone()),
            coordination_state_dir: dir.path().join("coordination-state"),
            accent_handles: leader.accent_handles.clone(),
            current: Some(old),
            cold_start: ColdStart::Pending,
            ..Window::default()
        };

        // The diagnosed fight: late/torn echoes and every follower compute
        // entry point are display-only/inert.
        let mut echo = follower.config.clone();
        echo.accent_snapshot = None;
        echo.accent_last_written = Some(expected_pair(Some(30.0)));
        drop(follower.update(Message::ConfigUpdated(echo)));
        assert_eq!(follower.config.accent_snapshot, snapshot);
        assert_eq!(
            follower
                .start_accent_compute(PathBuf::from("/images/ignored.jpg"))
                .units(),
            0
        );
        drop(follower.update(Message::AccentComputed {
            source: PathBuf::from("/images/ignored.jpg"),
            hue: Some(30.0),
        }));
        drop(follower.update(Message::AccentWriteFinished {
            generation: 1,
            success: true,
        }));
        assert!(follower.accent_inflight.is_none());
        assert_eq!(follower.config.accent_last_written, None);

        let applied = PathBuf::from("/images/new.jpg");
        let mut notice_messages = app_messages(follower.finish_manual_apply(applied.clone())).await;
        drop(follower.update(notice_messages.pop().unwrap()));
        assert_eq!(follower.current, Some(applied.clone()));
        assert_eq!(follower.cold_start, ColdStart::Pending);

        let mailbox = CoordinationConfig::load(&coordination_context);
        let validation = leader.update(Message::CoordinationUpdated(mailbox.clone()));
        assert_eq!(validation.units(), 1, "only the leader reads live state");
        let generation = mailbox.apply_notice.unwrap().generation;
        let accent_task = leader.update(Message::PeerApplyValidated {
            generation,
            live: wallpaper::CurrentWallpaper::File(applied.clone()),
        });
        assert_eq!(leader.current, Some(applied.clone()));
        assert_eq!(leader.cold_start, ColdStart::Done);
        assert_eq!(accent_task.units(), 1, "the leader recomputes once");

        drop(leader.update(Message::AccentComputed {
            source: applied,
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut leader);
        assert!(leader.config.accent_enabled);
        assert_eq!(leader.config.accent_snapshot, snapshot);
        assert_eq!(
            leader.config.accent_last_written,
            Some(expected_pair(Some(200.0)))
        );
        assert_eq!(follower.config.accent_snapshot, snapshot);
        assert!(follower.accent_inflight.is_none());
    }

    /// The sandboxed theme-config trees for the given config ids read-only,
    /// so accent writes into them fail.
    fn read_only_config_dirs(dir: &tempfile::TempDir, ids: &[&str]) -> Vec<PathBuf> {
        let roots: Vec<PathBuf> = ids
            .iter()
            .map(|id| dir.path().join("cosmic").join(id))
            .collect();
        read_only_trees(&roots)
    }

    /// The applet-config tree read-only, so the accent state persists fail
    /// while the theme configs stay writable — the swallowed-persist
    /// failure injection.
    fn read_only_applet_config(dir: &tempfile::TempDir) -> Vec<PathBuf> {
        read_only_trees(&[dir.path().join("applet-config")])
    }

    /// All four theme configs (light/dark × builder/theme) read-only — the
    /// all-or-nothing failure case.
    fn read_only_theme_dirs(dir: &tempfile::TempDir) -> Vec<PathBuf> {
        use cosmic::cosmic_theme::{
            DARK_THEME_BUILDER_ID, DARK_THEME_ID, LIGHT_THEME_BUILDER_ID, LIGHT_THEME_ID,
        };
        read_only_config_dirs(
            dir,
            &[
                LIGHT_THEME_BUILDER_ID,
                DARK_THEME_BUILDER_ID,
                LIGHT_THEME_ID,
                DARK_THEME_ID,
            ],
        )
    }

    fn current_accents(window: &Window) -> accent::BuilderAccents {
        accent::read_current_accents(window.accent_handles.as_ref().unwrap())
    }

    /// The transplant's own answer for `hue`, per mode — what a `Write` is
    /// expected to put on disk.
    fn expected_pair(hue: Option<f32>) -> AccentPair {
        AccentPair {
            light: accent::quantize(accent::accent_for(light_palette(), hue)),
            dark: accent::quantize(accent::accent_for(dark_palette(), hue)),
        }
    }

    /// Run the pending accent theme task synchronously and feed its
    /// completion back through `update` — the test-side stand-in for the
    /// blocking pool, using the *production* job derivation
    /// ([`accent_job`] + [`run_accent_job`]) so it cannot drift from what
    /// the spawned task would have done. Loops because a completion can
    /// chain another task (a rollback, a reconciled disable's restore).
    fn settle_accent_tasks(window: &mut Window) {
        use cosmic::Application as _;

        while let Some(inflight) = window.accent_inflight.clone() {
            let handles = window
                .accent_handles
                .clone()
                .expect("an in-flight accent task requires theme handles");
            let success = run_accent_job(&handles, accent_job(&inflight));
            let generation = window.accent_write_generation;
            drop(window.update(Message::AccentWriteFinished {
                generation,
                success,
            }));
        }
    }

    #[test]
    fn accent_computed_writes_snapshots_and_persists_last_written() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());

        let hue = Some(200.0);
        drop(window.update(Message::AccentComputed { source, hue }));
        settle_accent_tasks(&mut window);

        // Both builders hold the transplanted colours…
        let expected = expected_pair(hue);
        assert_eq!(
            current_accents(&window),
            (Some(expected.light), Some(expected.dark))
        );
        // …the don't-clobber pair is persisted…
        assert_eq!(window.config.accent_last_written, Some(expected));
        // …and the first write captured the user's accents (palette default
        // on both modes here) before anything of ours landed.
        assert_eq!(
            window.config.accent_snapshot,
            Some(AccentSnapshot {
                light: None,
                dark: None,
            })
        );
        // "Persisted" means on disk, not just the in-memory struct.
        assert_eq!(persisted_config(&window), window.config);
    }

    #[test]
    fn stale_accent_results_are_dropped() {
        use cosmic::Application as _;

        // The wallpaper changed while the extraction ran: the result must
        // change nothing at all — the newer apply armed its own compute.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        window.current = Some(PathBuf::from("/imgs/20260808-New_ROW1_UHD.jpg"));

        drop(window.update(Message::AccentComputed {
            source: PathBuf::from("/imgs/20260807-Old_ROW1_UHD.jpg"),
            hue: Some(120.0),
        }));

        assert!(
            window.accent_inflight.is_none(),
            "a stale result must not even spawn a write"
        );
        assert_eq!(current_accents(&window), (None, None));
        assert_eq!(window.config.accent_snapshot, None);
        assert_eq!(window.config.accent_last_written, None);
    }

    #[test]
    fn an_external_accent_change_disarms_without_restoring() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());

        // Steady state: we wrote once, snapshot + last-written persisted.
        let ours = AccentPair {
            light: [1, 2, 3],
            dark: [4, 5, 6],
        };
        let handles = window.accent_handles.as_ref().unwrap();
        accent::write_accents(
            handles,
            accent::read_builders(handles),
            ours.light,
            ours.dark,
        )
        .unwrap();
        window.config.accent_snapshot = Some(AccentSnapshot {
            light: None,
            dark: None,
        });
        window.config.accent_last_written = Some(ours);

        // The user picks a new dark accent in Settings…
        let picked = AccentSnapshot {
            light: Some(ours.light),
            dark: Some([99, 88, 77]),
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), picked).unwrap();

        // …and the next recompute disarms: toggle off, state cleared, and
        // crucially *no* restore — the user's choice stands.
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(10.0),
        }));

        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, None);
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(current_accents(&window), (picked.light, picked.dark));
        // The disarm survives a restart: the cleared state is on disk.
        assert_eq!(persisted_config(&window), window.config);
        assert!(!persisted_config(&window).accent_enabled);
    }

    #[test]
    fn accent_toggle_snapshots_on_enable_and_restores_on_disable() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);

        // The user's pre-feature accents: an explicit light one, dark on the
        // palette default — the mixed case a restore must reproduce exactly.
        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: None,
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), user).unwrap();

        drop(window.update(Message::SetAccentEnabled(true)));
        assert!(window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, Some(user));
        assert_eq!(window.config.accent_last_written, None);

        // A compute lands (the enable-time snapshot must survive it).
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(30.0),
        }));
        settle_accent_tasks(&mut window);
        assert_eq!(
            window.config.accent_last_written,
            Some(expected_pair(Some(30.0)))
        );
        assert_eq!(window.config.accent_snapshot, Some(user));
        assert_ne!(current_accents(&window), (user.light, user.dark));

        // Toggle off: the setting flips immediately, the restore runs as a
        // task, and the snapshot comes back verbatim — including the
        // palette-default `None` dark — with the feature state cleared, on
        // disk too.
        drop(window.update(Message::SetAccentEnabled(false)));
        assert!(
            !window.config.accent_enabled,
            "the toggle must not wait on theme I/O"
        );
        settle_accent_tasks(&mut window);
        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, None);
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(current_accents(&window), (user.light, user.dark));
        assert_eq!(persisted_config(&window), window.config);
    }

    #[test]
    fn enabling_without_theme_handles_refuses() {
        use cosmic::Application as _;

        // No theme configs: nothing could ever write or restore an accent,
        // so the toggle must stay off instead of pretending.
        let mut window = Window::default();
        assert!(window.accent_handles.is_none());

        drop(window.update(Message::SetAccentEnabled(true)));

        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, None);
    }

    #[test]
    fn enabling_without_a_config_context_refuses() {
        use cosmic::Application as _;

        // Theme handles exist, but the applet config cannot be persisted:
        // the feature state would be memory-only while the written accents
        // outlive the process — reversibility breaks on restart, so refuse.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        window.config.accent_enabled = false;
        window.config_context = None;

        drop(window.update(Message::SetAccentEnabled(true)));

        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, None);
    }

    #[test]
    fn an_echoed_enable_does_not_resnapshot() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);

        // The user's pre-feature accents, then enable + a landed write.
        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: Some([40, 50, 60]),
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), user).unwrap();
        drop(window.update(Message::SetAccentEnabled(true)));
        assert_eq!(window.config.accent_snapshot, Some(user));
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);
        assert_ne!(current_accents(&window), (user.light, user.dark));

        // A repeated SetAccentEnabled(true) — an echo, not a flip — must not
        // re-snapshot: the builders now hold *our* accents, and capturing
        // them as the user's would make disable restore our own colours.
        drop(window.update(Message::SetAccentEnabled(true)));
        assert_eq!(window.config.accent_snapshot, Some(user));

        // Disable still restores the true pre-feature accents.
        drop(window.update(Message::SetAccentEnabled(false)));
        settle_accent_tasks(&mut window);
        assert_eq!(current_accents(&window), (user.light, user.dark));
    }

    #[test]
    fn a_failed_accent_write_leaves_the_feature_armed_for_a_retry() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());

        let locked = read_only_theme_dirs(&dir);
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);
        restore_dir_permissions(&locked);

        // The write failed: nothing recorded as written (so the next
        // recompute retries instead of skipping), the enable-time snapshot
        // is retained, the feature stays enabled, and the theme configs are
        // exactly as they were.
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(
            window.config.accent_snapshot,
            Some(AccentSnapshot {
                light: None,
                dark: None,
            })
        );
        assert!(window.config.accent_enabled);
        assert_eq!(current_accents(&window), (None, None));
        assert_eq!(persisted_config(&window), window.config);
    }

    #[test]
    fn a_partial_accent_write_rolls_back_and_stays_armed() {
        use cosmic::Application as _;
        use cosmic::cosmic_theme::{DARK_THEME_BUILDER_ID, DARK_THEME_ID};

        // Light is written before dark: locking only the dark configs leaves
        // the reachable half-way state — light landed, dark failed. The
        // rollback must put the light mode back; a half-write left in place
        // would read as user intervention to the next recompute's
        // don't-clobber guard, which disarms and clears the snapshot without
        // restoring — permanently destroying the user's pre-feature accent.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);

        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: Some([40, 50, 60]),
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), user).unwrap();
        drop(window.update(Message::SetAccentEnabled(true)));

        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());
        let locked = read_only_config_dirs(&dir, &[DARK_THEME_BUILDER_ID, DARK_THEME_ID]);
        drop(window.update(Message::AccentComputed {
            source: source.clone(),
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);
        restore_dir_permissions(&locked);

        // The feature stays enabled and armed for a retry, snapshot kept…
        assert!(window.config.accent_enabled, "must not have disarmed");
        assert_eq!(window.config.accent_snapshot, Some(user));
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(persisted_config(&window), window.config);
        // …and the half-landed light accent was rolled back to the user's.
        assert_eq!(current_accents(&window), (user.light, user.dark));

        // "Armed" is real: the same recompute now lands on both modes.
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);
        assert!(window.config.accent_enabled);
        assert_eq!(
            window.config.accent_last_written,
            Some(expected_pair(Some(200.0)))
        );
        assert_eq!(window.config.accent_snapshot, Some(user));
    }

    #[test]
    fn a_failed_disable_time_restore_keeps_the_snapshot_for_the_next_enable() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);

        // Enable over explicit user accents, land a write.
        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: Some([40, 50, 60]),
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), user).unwrap();
        drop(window.update(Message::SetAccentEnabled(true)));
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);
        let ours = current_accents(&window);
        assert_ne!(ours, (user.light, user.dark));

        // Disable while the theme configs are unwritable: the restore fails
        // and our accents stay on disk — so the snapshot, the only record of
        // the user's pre-feature accents, must survive (clearing it would
        // make the loss permanent). The feature still turns off; the next
        // enable's deferred restore is the retry mechanism.
        let locked = read_only_theme_dirs(&dir);
        drop(window.update(Message::SetAccentEnabled(false)));
        settle_accent_tasks(&mut window);
        restore_dir_permissions(&locked);

        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, Some(user), "snapshot kept");
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(current_accents(&window), ours, "no restore happened yet");
        assert_eq!(persisted_config(&window), window.config);

        // The retry is real: re-enabling runs the deferred restore, and a
        // clean disable then ends on the user's pre-feature accents.
        drop(window.update(Message::SetAccentEnabled(true)));
        settle_accent_tasks(&mut window);
        assert!(window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, Some(user));
        assert_eq!(current_accents(&window), (user.light, user.dark));
        drop(window.update(Message::SetAccentEnabled(false)));
        settle_accent_tasks(&mut window);
        assert_eq!(window.config.accent_snapshot, None);
        assert_eq!(current_accents(&window), (user.light, user.dark));
    }

    #[tokio::test]
    async fn a_refused_external_enable_is_persisted_back_off() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        // An external `accent_enabled = true` lands on disk *before* the
        // `ConfigUpdated` handler runs. When the enable then refuses (here:
        // the deferred restore of a kept snapshot fails), disk and memory
        // must agree again — the flag back to false on disk — or the next
        // startup would arm the feature over state the toggler never built,
        // and its reconciliation would destroy the kept snapshot without
        // restoring.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);
        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: None,
        };
        window.config.accent_snapshot = Some(user);
        window
            .config
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();

        // The external editor flips the flag on disk, then the watcher
        // echoes the new config into the handler.
        let mut external = window.config.clone();
        external.accent_enabled = true;
        external
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();
        assert!(persisted_config(&window).accent_enabled);

        let locked = read_only_theme_dirs(&dir);
        let task = window.update(Message::ConfigUpdated(external));
        deliver_app_task(&mut window, task).await;
        settle_accent_tasks(&mut window);
        restore_dir_permissions(&locked);

        // Refused: off in memory *and* on disk, snapshot kept in both.
        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, Some(user));
        let persisted = persisted_config(&window);
        assert!(!persisted.accent_enabled, "the refusal must reach the disk");
        assert_eq!(persisted.accent_snapshot, Some(user), "snapshot survives");
    }

    #[test]
    fn an_enable_that_cannot_persist_its_snapshot_refuses() {
        use cosmic::Application as _;

        // The enable-time snapshot is the record every later restore depends
        // on. If it cannot reach the disk, arming anyway would leave our
        // colours unprotected across a restart — refuse, adopting nothing
        // (not even in memory).
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);

        let locked = read_only_applet_config(&dir);
        drop(window.update(Message::SetAccentEnabled(true)));
        restore_dir_permissions(&locked);

        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, None, "nothing half-adopted");
        assert!(!persisted_config(&window).accent_enabled);
        assert_eq!(persisted_config(&window).accent_snapshot, None);

        // The refusal is transient: once the config is writable, enabling
        // works.
        drop(window.update(Message::SetAccentEnabled(true)));
        assert!(window.config.accent_enabled);
        assert_eq!(persisted_config(&window), window.config);
    }

    #[test]
    fn a_failed_snapshot_persist_aborts_the_accent_write() {
        use cosmic::Application as _;

        // `snapshot_now` means nothing of ours has landed yet. The snapshot
        // must be safely on disk *before* the theme write it exists to undo:
        // when its persist fails, the whole write is aborted — themes
        // untouched, nothing adopted in memory — and the next recompute
        // simply retries.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir); // enabled, no snapshot yet
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());

        let locked = read_only_applet_config(&dir);
        drop(window.update(Message::AccentComputed {
            source: source.clone(),
            hue: Some(200.0),
        }));
        restore_dir_permissions(&locked);

        assert_eq!(current_accents(&window), (None, None), "themes untouched");
        assert_eq!(window.config.accent_snapshot, None);
        assert_eq!(window.config.accent_last_written, None);
        assert!(window.config.accent_enabled, "still armed for a retry");

        // The retry lands whole once the config is writable again.
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);
        assert_eq!(
            window.config.accent_last_written,
            Some(expected_pair(Some(200.0)))
        );
        assert_eq!(
            window.config.accent_snapshot,
            Some(AccentSnapshot {
                light: None,
                dark: None,
            })
        );
        assert_eq!(persisted_config(&window), window.config);
    }

    #[test]
    fn a_failed_last_written_persist_rolls_back_the_theme_write() {
        use cosmic::Application as _;

        // The theme write landed but its don't-clobber record could not be
        // persisted. Left in place, a restart would find our colours with
        // `last_written = None` — a state whose reconciliation can only
        // disarm. The executor rolls the themes back to the accents the plan
        // compared instead: the guard still holds and the next recompute
        // retries.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);

        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: Some([40, 50, 60]),
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), user).unwrap();
        drop(window.update(Message::SetAccentEnabled(true)));
        assert_eq!(persisted_config(&window).accent_snapshot, Some(user));

        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());
        let locked = read_only_applet_config(&dir);
        drop(window.update(Message::AccentComputed {
            source: source.clone(),
            hue: Some(200.0),
        }));
        // The write task lands, its record's persist fails, and the chained
        // rollback task undoes the write — all before the guard clears.
        settle_accent_tasks(&mut window);
        restore_dir_permissions(&locked);

        // Themes rolled back to the user's accents, nothing recorded, still
        // enabled and armed.
        assert_eq!(current_accents(&window), (user.light, user.dark));
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(window.config.accent_snapshot, Some(user));
        assert!(window.config.accent_enabled);
        assert_eq!(persisted_config(&window).accent_last_written, None);

        // The same recompute lands whole once the config is writable.
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);
        assert_eq!(
            window.config.accent_last_written,
            Some(expected_pair(Some(200.0)))
        );
        assert_ne!(current_accents(&window), (user.light, user.dark));
        assert_eq!(persisted_config(&window), window.config);
    }

    #[test]
    fn a_double_failure_rollback_repairs_the_on_disk_record() {
        use cosmic::Application as _;

        // The write lands, its record's persist fails (rollback chained),
        // and the rollback's theme restore fails too: the themes keep the
        // new pair while the on-disk record still holds the old one. Memory
        // adopting the new pair keeps this session Skipping — nothing would
        // ever re-persist the record — so a restart inside that window
        // would find themes ≠ record and hit the destructive
        // `Disarm { keep_snapshot: false }`, silently discarding the
        // snapshot without a restore. The rollback completion must repair
        // the on-disk record into a shape whose startup plan is Skip or a
        // snapshot-keeping disarm — never `keep_snapshot: false`.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());

        // A first write lands cleanly: the disk records the old pair.
        drop(window.update(Message::AccentComputed {
            source: source.clone(),
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);
        let old_pair = expected_pair(Some(200.0));
        assert_eq!(
            persisted_config(&window).accent_last_written,
            Some(old_pair)
        );

        // A second write for a new hue: the theme write itself succeeds…
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(30.0),
        }));
        let handles = window.accent_handles.clone().unwrap();
        let inflight = window.accent_inflight.clone().unwrap();
        assert!(run_accent_job(&handles, accent_job(&inflight)));
        // …its record's persist fails (the rollback is chained)…
        let locked_config = read_only_applet_config(&dir);
        drop(window.update(Message::AccentWriteFinished {
            generation: window.accent_write_generation,
            success: true,
        }));
        assert!(matches!(
            window.accent_inflight,
            Some(AccentInflight::Rollback { .. })
        ));
        // …and the rollback's theme restore fails as well; the persist
        // failure was transient and has passed by completion time.
        let locked_themes = read_only_theme_dirs(&dir);
        let inflight = window.accent_inflight.clone().unwrap();
        assert!(!run_accent_job(&handles, accent_job(&inflight)));
        restore_dir_permissions(&locked_config);
        drop(window.update(Message::AccentWriteFinished {
            generation: window.accent_write_generation,
            success: false,
        }));
        restore_dir_permissions(&locked_themes);

        // The themes hold the new pair, and memory *and the repaired disk
        // record* match them — the steady state is Skip, not a stranded
        // divergence…
        let new_pair = expected_pair(Some(30.0));
        assert_eq!(
            current_accents(&window),
            (Some(new_pair.light), Some(new_pair.dark))
        );
        assert_eq!(window.config.accent_last_written, Some(new_pair));
        let persisted = persisted_config(&window);
        assert_eq!(persisted.accent_last_written, Some(new_pair));
        // …so a restart can never hit the destructive disarm: the startup
        // plan over the persisted state is Skip.
        let plan = accent::accent_plan(
            persisted.accent_enabled,
            persisted.accent_snapshot,
            persisted.accent_last_written,
            &accent::read_builders(&handles),
            Some(30.0),
        );
        assert_eq!(plan, accent::AccentAction::Skip);
    }

    #[test]
    fn a_gap_disarm_keeps_the_snapshot() {
        use cosmic::Application as _;

        // Enabled with a persisted snapshot but no `last_written`, and
        // builders matching neither: a user pick in the enable→first-write
        // gap *or* our own write whose record was lost (the crash window) —
        // indistinguishable. The disarm turns the feature off and leaves the
        // themes alone (a genuine pick stands), but keeps the snapshot: the
        // pre-feature record must survive for the next enable's deferred
        // restore instead of being destroyed over what may be our own
        // leftovers.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: Some([40, 50, 60]),
        };
        window.config.accent_snapshot = Some(user);
        use cosmic_config::ConfigSet as _;
        window
            .config_context
            .as_ref()
            .unwrap()
            .set("accent_snapshot", Some(user))
            .unwrap();
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());

        let foreign = AccentSnapshot {
            light: Some([99, 88, 77]),
            dark: Some([40, 50, 60]),
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), foreign).unwrap();

        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(200.0),
        }));

        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, Some(user), "snapshot kept");
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(
            current_accents(&window),
            (foreign.light, foreign.dark),
            "no restore on disarm — a genuine gap pick stands"
        );
        assert_eq!(persisted_config(&window), window.config);
    }

    #[test]
    fn disabling_without_theme_handles_keeps_the_snapshot() {
        use cosmic::Application as _;

        // The config says enabled (from a previous run) but the theme
        // configs failed to open this run: disabling cannot restore, so it
        // must keep the snapshot — the only record of the user's pre-feature
        // accents — rather than silently discarding it.
        let mut window = Window::default();
        window.config.accent_enabled = true;
        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: None,
        };
        window.config.accent_snapshot = Some(user);
        window.config.accent_last_written = Some(AccentPair {
            light: [1, 2, 3],
            dark: [4, 5, 6],
        });
        assert!(window.accent_handles.is_none());

        drop(window.update(Message::SetAccentEnabled(false)));

        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, Some(user), "snapshot kept");
        assert_eq!(window.config.accent_last_written, None);
    }

    #[test]
    fn enable_after_a_kept_snapshot_restores_it_instead_of_resnapshotting() {
        use cosmic::Application as _;

        // Run A: the feature wrote accents. Run B: the theme configs failed
        // to open, so disabling kept the snapshot (deferred restore) while
        // *our* accents stayed on disk. Run C (healthy): enable must not
        // re-capture the on-disk accents — they are ours from run A, and
        // snapshotting them would clobber the only record of the user's
        // pre-feature accents, making a later disable "restore" our own
        // colours. It honours the deferred restore and keeps the snapshot.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());

        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: Some([40, 50, 60]),
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), user).unwrap();

        // Run A: enable + a landed write.
        drop(window.update(Message::SetAccentEnabled(true)));
        drop(window.update(Message::AccentComputed {
            source: source.clone(),
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);
        let ours = current_accents(&window);
        assert_ne!(ours, (user.light, user.dark));

        // Run B: theme handles unavailable; disable keeps the snapshot.
        window.accent_handles = None;
        drop(window.update(Message::SetAccentEnabled(false)));
        assert_eq!(window.config.accent_snapshot, Some(user), "snapshot kept");

        // Run C: the theme configs open again; our accents are still on disk.
        window.accent_handles = Some(accent::ThemeHandles::sandboxed(dir.path()).unwrap());
        assert_eq!(current_accents(&window), ours);

        drop(window.update(Message::SetAccentEnabled(true)));
        settle_accent_tasks(&mut window);
        // The kept snapshot survives (not clobbered by our on-disk accents)…
        assert_eq!(window.config.accent_snapshot, Some(user));
        assert_eq!(window.config.accent_last_written, None);
        // …and the deferred restore reconciled the disk, so the next
        // recompute's guard sees the snapshot, not a foreign accent.
        assert_eq!(current_accents(&window), (user.light, user.dark));

        // The feature works — a compute lands without disarming…
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(30.0),
        }));
        settle_accent_tasks(&mut window);
        assert!(window.config.accent_enabled, "must not have disarmed");
        assert_ne!(current_accents(&window), (user.light, user.dark));

        // …and disable restores the true pre-feature accents, not run A's.
        drop(window.update(Message::SetAccentEnabled(false)));
        settle_accent_tasks(&mut window);
        assert_eq!(current_accents(&window), (user.light, user.dark));
        assert_eq!(window.config.accent_snapshot, None);
    }

    #[test]
    fn enable_with_a_kept_snapshot_refuses_when_the_restore_fails() {
        use cosmic::Application as _;

        // A kept snapshot means the disk may still hold our accents from an
        // earlier run; enabling without reconciling would let the next
        // recompute's guard disarm and destroy the snapshot. So when the
        // deferred restore fails, the enable must refuse — off, snapshot
        // kept, nothing armed.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);
        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: None,
        };
        window.config.accent_snapshot = Some(user);

        let locked = read_only_theme_dirs(&dir);
        drop(window.update(Message::SetAccentEnabled(true)));
        settle_accent_tasks(&mut window);
        restore_dir_permissions(&locked);

        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, Some(user), "snapshot kept");
        assert_eq!(window.config.accent_last_written, None);
    }

    #[tokio::test]
    async fn config_updated_accent_flips_run_the_toggle_lifecycle() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        // External edits of `accent_enabled` (RON file, another tool) arrive
        // via ConfigUpdated and must behave exactly like the popup toggler —
        // not silently adopt the flag. A genuine external edit is on the
        // *disk* before the watcher fires — the handler verifies the payload
        // flip against a fresh disk read (a payload alone can be a stale
        // echo), so the test writes the edit like the external editor would.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);

        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: Some([40, 50, 60]),
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), user).unwrap();

        // External enable: snapshot taken from the live accents.
        let mut external = window.config.clone();
        external.accent_enabled = true;
        external
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();
        let task = window.update(Message::ConfigUpdated(external));
        deliver_app_task(&mut window, task).await;
        assert!(window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, Some(user));
        assert_eq!(window.config.accent_last_written, None);

        // A write lands.
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);
        assert_ne!(current_accents(&window), (user.light, user.dark));

        // External disable: the snapshot is restored and the state cleared,
        // exactly like the toggler's off path.
        let mut external = window.config.clone();
        external.accent_enabled = false;
        external
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();
        let task = window.update(Message::ConfigUpdated(external));
        deliver_app_task(&mut window, task).await;
        settle_accent_tasks(&mut window);
        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, None);
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(current_accents(&window), (user.light, user.dark));
    }

    #[test]
    fn re_enable_re_snapshots_so_disable_restores_the_later_accents() {
        use cosmic::Application as _;

        // enable → manual change → disarm → re-enable → disable must end on
        // the accents from *re-enable time*, not the pre-feature originals —
        // the re-snapshot rule, driven through the real executor.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());

        let original = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: Some([40, 50, 60]),
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), original).unwrap();

        // Enable + first write.
        drop(window.update(Message::SetAccentEnabled(true)));
        assert_eq!(window.config.accent_snapshot, Some(original));
        drop(window.update(Message::AccentComputed {
            source: source.clone(),
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);

        // The user picks new accents in Settings; the next recompute disarms
        // without restoring.
        let manual = AccentSnapshot {
            light: Some([90, 90, 0]),
            dark: Some([0, 90, 90]),
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), manual).unwrap();
        drop(window.update(Message::AccentComputed {
            source: source.clone(),
            hue: Some(200.0),
        }));
        assert!(!window.config.accent_enabled);
        assert_eq!(current_accents(&window), (manual.light, manual.dark));

        // Re-enable snapshots *now* — the manual accents — and a write lands.
        drop(window.update(Message::SetAccentEnabled(true)));
        assert_eq!(window.config.accent_snapshot, Some(manual));
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(30.0),
        }));
        settle_accent_tasks(&mut window);

        // Disable restores what re-enable captured, not the originals.
        drop(window.update(Message::SetAccentEnabled(false)));
        settle_accent_tasks(&mut window);
        assert_eq!(current_accents(&window), (manual.light, manual.dark));
        assert_ne!(current_accents(&window), (original.light, original.dark));
    }

    // -----------------------------------------------------------------
    // The write guard (2026-08-08 btrfs incident): while an accent theme
    // task is in flight, echoes must not run the toggle lifecycle, results
    // are deferred, stale completions are ignored, and the completion
    // reconciles against a fresh disk read.
    // -----------------------------------------------------------------

    /// An [`accent_window`] with a `Write` task in flight for `hue` 200°:
    /// the state the incident's echoes arrived in.
    fn window_with_inflight_write(dir: &tempfile::TempDir) -> (Window, PathBuf) {
        use cosmic::Application as _;

        let mut window = accent_window(dir);
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());
        drop(window.update(Message::AccentComputed {
            source: source.clone(),
            hue: Some(200.0),
        }));
        assert!(
            matches!(window.accent_inflight, Some(AccentInflight::Write { .. })),
            "the compute must have spawned a write task"
        );
        (window, source)
    }

    #[tokio::test]
    async fn a_config_echo_during_an_inflight_write_is_not_routed_through_the_lifecycle() {
        use cosmic::Application as _;
        use cosmic_config::ConfigSet as _;

        // The incident's oscillation: our own multi-key persists echoed back
        // stale/torn while the minute-long theme write flew, the apparent
        // `accent_enabled` flip ran the disable lifecycle, and the
        // enable→disarm→re-enable loop rewrote both themes each round. An
        // echo arriving mid-flight must leave every accent field alone —
        // while still adopting genuine non-accent changes.
        let dir = tempfile::tempdir().unwrap();
        let (mut window, _) = window_with_inflight_write(&dir);
        let snapshot = window.config.accent_snapshot;

        let mut echo = window.config.clone();
        echo.accent_enabled = false;
        echo.accent_snapshot = None;
        echo.accent_last_written = None;
        echo.retention_days = 30;
        window
            .config_context
            .as_ref()
            .unwrap()
            .set("retention_days", 30_u16)
            .unwrap();
        let task = window.update(Message::ConfigUpdated(echo));
        deliver_app_task(&mut window, task).await;

        assert!(window.config.accent_enabled, "no disable routed");
        assert_eq!(window.config.accent_snapshot, snapshot, "snapshot kept");
        assert!(
            matches!(window.accent_inflight, Some(AccentInflight::Write { .. })),
            "the in-flight write survives the echo"
        );
        assert_eq!(window.config.retention_days, 30, "non-accent fields adopt");

        // Completion: the write lands once, the reconcile's fresh disk read
        // agrees with memory (the disk still says enabled) — no second write
        // cycle, no disarm.
        settle_accent_tasks(&mut window);
        assert!(window.config.accent_enabled);
        assert_eq!(
            window.config.accent_last_written,
            Some(expected_pair(Some(200.0)))
        );
        let persisted = persisted_config(&window);
        assert!(persisted.accent_enabled);
        assert_eq!(
            persisted.accent_last_written,
            window.config.accent_last_written
        );
        assert_eq!(persisted.accent_snapshot, window.config.accent_snapshot);
    }

    #[test]
    fn a_stale_echo_after_completion_does_not_regress_accent_state() {
        use cosmic::Application as _;

        // Watcher payloads are read at event time and can be delivered a
        // message late: an echo carrying mid-flight state (`last_written:
        // None`) can arrive *after* `AccentWriteFinished` already retired
        // the guard. Adopting its accent fields would strand the
        // just-persisted record — the next recompute would then Disarm
        // spuriously (the builders hold the written pair, memory says
        // nothing was written): feature off, accent stranded, the
        // oscillation class under I/O pressure. In-memory accent fields
        // are authoritative; payload accent fields are never adopted.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());
        drop(window.update(Message::AccentComputed {
            source: source.clone(),
            hue: Some(200.0),
        }));
        // What the watcher read mid-flight: enabled, snapshot persisted,
        // the write's record not yet.
        let stale = window.config.clone();
        assert_eq!(stale.accent_last_written, None);
        settle_accent_tasks(&mut window);
        let written = window.config.accent_last_written;
        assert!(written.is_some());
        let snapshot = window.config.accent_snapshot;
        assert!(snapshot.is_some());

        // The stale no-flip echo lands one message after the completion:
        // nothing regresses.
        drop(window.update(Message::ConfigUpdated(stale.clone())));
        assert_eq!(window.config.accent_last_written, written, "no regress");
        assert_eq!(window.config.accent_snapshot, snapshot);

        // A stale/torn payload that *does* claim a flip is verified against
        // a fresh disk read (which still says enabled): no lifecycle runs.
        let mut torn = stale;
        torn.accent_enabled = false;
        drop(window.update(Message::ConfigUpdated(torn)));
        assert!(window.config.accent_enabled, "stale flip not adopted");
        assert!(window.accent_inflight.is_none(), "no lifecycle routed");
        assert_eq!(window.config.accent_last_written, written);

        // And no disarm follows: the next recompute is the steady-state
        // Skip, not a spurious `Disarm`.
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(200.0),
        }));
        assert!(window.accent_inflight.is_none(), "Skip — nothing rewritten");
        assert!(window.config.accent_enabled, "no spurious disarm");
        assert_eq!(persisted_config(&window), window.config);
    }

    #[test]
    fn a_stale_accent_write_completion_is_ignored() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let (mut window, _) = window_with_inflight_write(&dir);

        // A completion from a superseded generation — even one claiming
        // success — must not retire the guard or persist a record for a
        // write that never ran.
        drop(window.update(Message::AccentWriteFinished {
            generation: 0,
            success: true,
        }));
        assert!(
            window.accent_inflight.is_some(),
            "the guard must survive a stale completion"
        );
        assert_eq!(
            window.config.accent_last_written, None,
            "a stale completion must not persist a record"
        );

        // The real completion still lands normally.
        settle_accent_tasks(&mut window);
        assert_eq!(
            window.config.accent_last_written,
            Some(expected_pair(Some(200.0)))
        );
    }

    #[test]
    fn an_accent_result_during_an_inflight_write_is_deferred_and_rearmed() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let (mut window, source) = window_with_inflight_write(&dir);

        // A second result while the first write flies: acting on it would
        // plan against builders our own task is mutating. It is dropped and
        // queued instead — no second task, nothing persisted for it.
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(30.0),
        }));
        assert!(window.accent_recompute_queued, "deferred, not executed");
        assert!(
            matches!(window.accent_inflight, Some(AccentInflight::Write { .. })),
            "still the first write, no second task"
        );

        // The completion re-arms a fresh compute (the task itself is opaque
        // here; the cleared flag is the observable hand-off) and the first
        // write's record is what landed.
        settle_accent_tasks(&mut window);
        assert!(!window.accent_recompute_queued);
        assert_eq!(
            window.config.accent_last_written,
            Some(expected_pair(Some(200.0)))
        );
    }

    #[test]
    fn the_post_task_reconcile_adopts_the_fresh_disk_state() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        let dir = tempfile::tempdir().unwrap();
        let (mut window, _) = window_with_inflight_write(&dir);

        // A genuine external disable lands on the disk while the write
        // flies; its echo is suppressed (see the echo test), so only the
        // disk records it.
        let mut external = persisted_config(&window);
        external.accent_enabled = false;
        external
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();
        drop(window.update(Message::ConfigUpdated(external)));
        assert!(window.config.accent_enabled, "echo suppressed mid-flight");

        // The completion reconciles against the fresh disk read — not the
        // echo payload — and routes the disable through the real lifecycle:
        // off, snapshot restored and cleared, record cleared, themes back to
        // the user's accents.
        settle_accent_tasks(&mut window);
        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, None);
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(
            current_accents(&window),
            (None, None),
            "the routed disable undid the landed write"
        );
        assert!(!persisted_config(&window).accent_enabled);
    }

    #[test]
    fn a_toggle_during_an_inflight_write_is_deferred_to_the_reconcile() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let (mut window, _) = window_with_inflight_write(&dir);

        // The user flips the toggler off mid-write: the lifecycle must not
        // race the running task, so the request is recorded (the toggler
        // renders it immediately) and pinned onto the disk config for the
        // completion's fresh-read reconcile.
        drop(window.update(Message::SetAccentEnabled(false)));
        assert!(
            window.config.accent_enabled,
            "the lifecycle itself is deferred"
        );
        assert!(
            !window.accent_toggler_state(),
            "the row answers immediately"
        );
        assert!(!persisted_config(&window).accent_enabled, "pinned on disk");
        assert!(window.accent_inflight.is_some());

        // Completion: the reconcile routes the real disable — restore,
        // clear, all persisted.
        settle_accent_tasks(&mut window);
        assert!(!window.config.accent_enabled);
        assert_eq!(window.config.accent_snapshot, None);
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(current_accents(&window), (None, None));
        assert_eq!(persisted_config(&window), window.config);
    }

    #[tokio::test]
    async fn an_external_disable_landing_during_an_enable_restore_is_routed_not_stomped() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        // An external enable routed a deferred restore (kept snapshot);
        // while it flew, the external editor flipped the flag back off on
        // disk. The completion's own `arm_accent_enable` persists
        // `accent_enabled = true` — a reconcile disk read taken *after*
        // that persist reads our own write back, and the external disable
        // is silently overwritten. It must be routed through the disable
        // lifecycle instead.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);
        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: Some([40, 50, 60]),
        };
        window.config.accent_snapshot = Some(user);
        window
            .config
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();
        // Our accents are still on the themes from the run that kept the
        // snapshot.
        let handles = window.accent_handles.clone().unwrap();
        accent::write_accents(
            &handles,
            accent::read_builders(&handles),
            [1, 2, 3],
            [4, 5, 6],
        )
        .unwrap();

        // The external enable lands on disk, then routes: the deferred
        // restore takes flight.
        let mut external = persisted_config(&window);
        external.accent_enabled = true;
        external
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();
        let task = window.update(Message::ConfigUpdated(external));
        deliver_app_task(&mut window, task).await;
        assert!(matches!(
            window.accent_inflight,
            Some(AccentInflight::EnableRestore { .. })
        ));

        // The external disable lands on disk mid-flight; its echo is
        // suppressed by the write guard.
        let mut flipped = persisted_config(&window);
        flipped.accent_enabled = false;
        flipped
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();
        let task = window.update(Message::ConfigUpdated(flipped));
        deliver_app_task(&mut window, task).await;
        assert!(
            window.accent_inflight.is_some(),
            "echo suppressed, the restore still flying"
        );

        settle_accent_tasks(&mut window);

        // The disable won: off in memory *and* on disk — not stomped back
        // to enabled — and the routed disable lifecycle ran (snapshot
        // restored and cleared).
        assert!(!window.config.accent_enabled);
        assert!(!persisted_config(&window).accent_enabled);
        assert_eq!(window.config.accent_snapshot, None);
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(current_accents(&window), (user.light, user.dark));
    }

    #[test]
    fn an_external_enable_landing_during_a_disable_restore_is_routed_not_stomped() {
        use cosmic::Application as _;
        use cosmic_config::CosmicConfigEntry as _;

        // The mirror case: a disable's restore is in flight when the
        // external editor enables the feature on disk. The completion's
        // `finish_disable_restore` persists the full config from memory
        // (`accent_enabled = false`) — a disk read taken *after* that
        // persist reads our own write back, and the external enable is
        // lost. It must be routed through the enable lifecycle instead.
        let dir = tempfile::tempdir().unwrap();
        let mut window = accent_window(&dir);
        start_disabled(&mut window);
        let user = AccentSnapshot {
            light: Some([10, 20, 30]),
            dark: Some([40, 50, 60]),
        };
        accent::restore_accents(window.accent_handles.as_ref().unwrap(), user).unwrap();

        // Enable + a landed write, then toggle off: the restore takes
        // flight.
        drop(window.update(Message::SetAccentEnabled(true)));
        let source = PathBuf::from("/imgs/20260808-Foo_ROW1_UHD.jpg");
        window.current = Some(source.clone());
        drop(window.update(Message::AccentComputed {
            source,
            hue: Some(200.0),
        }));
        settle_accent_tasks(&mut window);
        drop(window.update(Message::SetAccentEnabled(false)));
        assert!(matches!(
            window.accent_inflight,
            Some(AccentInflight::DisableRestore { .. })
        ));

        // The external enable lands on disk mid-flight; its echo is
        // suppressed by the write guard.
        let mut flipped = persisted_config(&window);
        flipped.accent_enabled = true;
        flipped
            .write_entry(window.config_context.as_ref().unwrap())
            .unwrap();
        drop(window.update(Message::ConfigUpdated(flipped)));
        assert!(!window.config.accent_enabled, "echo suppressed mid-flight");

        settle_accent_tasks(&mut window);

        // The enable won: on in memory *and* on disk — the completion's
        // changed-field persists did not bury it — and the enable lifecycle
        // ran: a fresh snapshot of the just-restored user accents.
        assert!(window.config.accent_enabled);
        assert!(persisted_config(&window).accent_enabled);
        assert_eq!(window.config.accent_snapshot, Some(user));
        assert_eq!(window.config.accent_last_written, None);
        assert_eq!(current_accents(&window), (user.light, user.dark));
    }

    #[tokio::test]
    async fn accent_extraction_reads_only_the_cached_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("images");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&images).unwrap();
        let source = images.join("20260808-Foo_ROW1_UHD.jpg");

        // No thumbnail cached: nothing to act on (and no `ensure_thumbnail`
        // in disguise — there is no source file to decode either).
        assert_eq!(extract_accent_hue(&source, &state).await, None);

        // The discriminating case for the never-`ensure_thumbnail` rule: a
        // perfectly decodable source whose thumbnail is simply not cached
        // yet. A regression to `ensure_thumbnail` would generate the
        // thumbnail and answer `Some`; the contract is `None`, cache
        // untouched.
        let fresh = images.join("20260806-Fresh_ROW1_UHD.jpg");
        image::RgbImage::from_pixel(64, 36, image::Rgb([40, 180, 60]))
            .save(&fresh)
            .unwrap();
        assert_eq!(extract_accent_hue(&fresh, &state).await, None);
        assert!(
            !thumbs::is_cached(&fresh, &state),
            "extraction must not have generated the thumbnail"
        );
        assert!(!thumbs::thumbnail_path(&fresh, &state).unwrap().exists());

        // A recorded decode failure: still nothing to act on, for free.
        std::fs::write(&source, b"not actually a jpeg").unwrap();
        assert!(thumbs::ensure_thumbnail(&source, &state).is_err());
        assert!(thumbs::decode_failed(&source, &state));
        assert_eq!(extract_accent_hue(&source, &state).await, None);

        // A repaired, cached, vibrant wallpaper yields its hue. Synthetic
        // solid image saved as a real JPEG (not `testutil::tiny_jpeg`, whose
        // gradient is a poor substrate for hue assertions).
        image::RgbImage::from_pixel(64, 36, image::Rgb([200, 30, 40]))
            .save(&source)
            .unwrap();
        thumbs::ensure_thumbnail(&source, &state).unwrap();
        let hue = extract_accent_hue(&source, &state)
            .await
            .expect("cached thumbnail must extract")
            .expect("a solid vibrant red is not grey");
        // JPEG chroma subsampling shifts hue a little; the reference value is
        // the extractor's own answer for the raw colour.
        let reference = accent::dominant_hue(&image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([200, 30, 40]),
        ))
        .unwrap();
        let diff = (hue - reference).rem_euclid(360.0);
        assert!(
            diff.min(360.0 - diff) <= 10.0,
            "hue {hue} too far from the source colour's {reference}"
        );

        // A cache slot that *says* cached while the thumbnail bytes are
        // ruined (sidecar stamps the source, not the thumb): the decode
        // fails and the answer is the outer `None` — a failure, not grey.
        let thumb = thumbs::thumbnail_path(&source, &state).unwrap();
        std::fs::write(&thumb, b"ruined bytes").unwrap();
        assert!(
            thumbs::is_cached(&source, &state),
            "the slot must still read as cached for this to hit the decode branch"
        );
        assert_eq!(extract_accent_hue(&source, &state).await, None);

        // An effectively grey wallpaper is a real answer (`Some(None)`), not
        // a failure — the plan writes the palette's warm grey for it.
        let grey = images.join("20260807-Grey_ROW1_UHD.jpg");
        image::RgbImage::from_pixel(64, 36, image::Rgb([128, 128, 128]))
            .save(&grey)
            .unwrap();
        thumbs::ensure_thumbnail(&grey, &state).unwrap();
        assert_eq!(extract_accent_hue(&grey, &state).await, Some(None));
    }

    // -----------------------------------------------------------------
    // Lock-screen poke wiring (the cosmic-greeter#511 workaround). The
    // cosmic-bg state handle is TempDir-rooted — nothing ever touches the
    // real ~/.local/state/cosmic/com.system76.CosmicBackground.
    // -----------------------------------------------------------------

    use crate::testutil::{
        bg_path_source, bg_state_config, bg_wallpapers_inode, bg_wallpapers_key_file,
    };
    use cosmic_bg_config::Source;

    /// A window whose poke handle is injected tempdir-rooted (mirroring how
    /// [`accent_window`] injects `config_context`; the shared
    /// [`bg_state_config`] builder mirrors production's key layout).
    fn poke_window(dir: &tempfile::TempDir) -> Window {
        Window {
            poke_config: Some(bg_state_config(dir.path())),
            ..Window::default()
        }
    }

    fn seed_bg_wallpapers(window: &Window, list: &[(String, Source)]) {
        use cosmic_config::ConfigSet as _;
        window
            .poke_config
            .as_ref()
            .expect("poke handle must be injected")
            .set("wallpapers", list)
            .expect("seed wallpapers key");
    }

    fn read_bg_wallpapers(window: &Window) -> Vec<(String, Source)> {
        use cosmic_config::ConfigGet as _;
        window
            .poke_config
            .as_ref()
            .expect("poke handle must be injected")
            .get("wallpapers")
            .expect("read wallpapers key")
    }

    /// Deliver one rung of the poke ladder synchronously — the test-side
    /// stand-in for the spawned poke task, in the shape of
    /// [`settle_accent_tasks`]: a `Task` returned from `update()` is never
    /// polled in unit tests, so on-disk assertions must come through here.
    /// Feeds `LockPokeDue(generation)` through `update` (the bookkeeping the
    /// real timer tick hits), then runs the *production* decision
    /// ([`Window::due_lock_poke`]) and the *production* poke body
    /// ([`run_lock_poke`]) so neither can drift, and feeds
    /// `LockPokeFinished` back exactly when production would (a stale or
    /// handle-less rung sends no completion). Returns whether a write
    /// happened. The explicit `generation` (the plan sketched a bare
    /// `&mut Window`) is what lets the staleness tests settle a *dead* rung.
    fn settle_lock_pokes(window: &mut Window, generation: u64) -> bool {
        use cosmic::Application as _;

        drop(window.update(Message::LockPokeDue(generation)));
        let Some(config) = window.due_lock_poke(generation) else {
            return false;
        };
        let wrote = run_lock_poke(&config);
        drop(window.update(Message::LockPokeFinished(wrote)));
        wrote
    }

    #[test]
    fn settled_lock_poke_toggles_the_injected_state() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = poke_window(&dir);
        let canonical = vec![
            ("DP-1".to_owned(), bg_path_source("a")),
            ("HDMI-1".to_owned(), bg_path_source("b")),
        ];
        seed_bg_wallpapers(&window, &canonical);
        let seeded_inode = bg_wallpapers_inode(dir.path());

        drop(window.update(Message::LockEvent(lockwatch::LockEvent::Locked)));
        let generation = window.lock_poke_generation;
        assert!(
            settle_lock_pokes(&mut window, generation),
            "the ladder's first rung must write"
        );
        assert_ne!(
            bg_wallpapers_inode(dir.path()),
            seeded_inode,
            "a write must have landed (atomic rename = new inode)"
        );
        // A *value change* reached the injected state — the wiring's job.
        // The exact toggled bytes are `wallpaper.rs`'s poke tests' pin
        // (`poke_toggles_the_wallpapers_key` / `second_poke_restores_..`),
        // not re-asserted here.
        assert_ne!(read_bg_wallpapers(&window), canonical);

        // The same ladder's second rung writes again (a full toggle, never
        // deduped-away).
        assert!(settle_lock_pokes(&mut window, generation));
    }

    #[test]
    fn a_stale_poke_due_is_a_noop() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = poke_window(&dir);
        let canonical = vec![("all".to_owned(), bg_path_source("a"))];
        seed_bg_wallpapers(&window, &canonical);
        let seeded_inode = bg_wallpapers_inode(dir.path());

        drop(window.update(Message::LockEvent(lockwatch::LockEvent::Locked)));
        let stale = window.lock_poke_generation;
        drop(window.update(Message::LockEvent(lockwatch::LockEvent::Locked)));
        assert!(
            window.lock_poke_generation > stale,
            "a fresh event must bump the generation (that bump *is* the cancel)"
        );

        assert!(
            !settle_lock_pokes(&mut window, stale),
            "a rung from the replaced ladder must not poke"
        );
        assert!(window.due_lock_poke(stale).is_none());
        assert_eq!(
            bg_wallpapers_inode(dir.path()),
            seeded_inode,
            "the state file must be untouched"
        );
        assert_eq!(read_bg_wallpapers(&window), canonical);
    }

    #[test]
    fn a_second_lock_event_invalidates_the_first_ladder() {
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = poke_window(&dir);
        let canonical = vec![("all".to_owned(), bg_path_source("a"))];
        seed_bg_wallpapers(&window, &canonical);

        drop(window.update(Message::LockEvent(lockwatch::LockEvent::Locked)));
        let first = window.lock_poke_generation;
        // A rapid re-lock (or a resume racing a lock): the last event wins.
        drop(window.update(Message::LockEvent(lockwatch::LockEvent::Resumed)));
        let second = window.lock_poke_generation;
        assert!(second > first);

        // Every rung of the first ladder is dead…
        assert!(!settle_lock_pokes(&mut window, first));
        assert!(!settle_lock_pokes(&mut window, first));
        assert_eq!(read_bg_wallpapers(&window), canonical);
        // …while the second ladder pokes normally.
        assert!(settle_lock_pokes(&mut window, second));
        assert_ne!(read_bg_wallpapers(&window), canonical);
    }

    #[test]
    fn resumed_pokes_like_locked() {
        use crate::lockwatch::toggle_wallpapers;
        use cosmic::Application as _;

        let dir = tempfile::tempdir().unwrap();
        let mut window = poke_window(&dir);
        let canonical = vec![("all".to_owned(), bg_path_source("a"))];
        seed_bg_wallpapers(&window, &canonical);

        // The suspend path may never emit a session `Lock` signal — the
        // resume edge must arm the identical ladder.
        drop(window.update(Message::LockEvent(lockwatch::LockEvent::Resumed)));
        let generation = window.lock_poke_generation;
        assert!(settle_lock_pokes(&mut window, generation));
        assert_eq!(
            read_bg_wallpapers(&window),
            toggle_wallpapers(canonical).expect("canonical toggles")
        );
    }

    #[test]
    fn poke_with_no_config_handle_is_a_noop() {
        use cosmic::Application as _;

        // No handle (cosmic-bg state context failed to open in `init`):
        // events still arm the ladder harmlessly, and every rung no-ops.
        let mut window = Window::default();
        assert!(window.poke_config.is_none());

        drop(window.update(Message::LockEvent(lockwatch::LockEvent::Locked)));
        let generation = window.lock_poke_generation;
        assert_eq!(generation, 1, "the event still bumps the generation");
        assert!(window.due_lock_poke(generation).is_none());
        assert!(!settle_lock_pokes(&mut window, generation));
        // The completion path stays harmless too (log-only).
        drop(window.update(Message::LockPokeFinished(false)));
    }

    /// Every `LockPokeDue` generation the task's rungs will deliver, in
    /// order — the only way to see what [`Window::arm_lock_pokes`] actually
    /// armed (its `Task` is otherwise dropped unpolled by every other test,
    /// so a ladder of zero rungs or rungs minted with a pre-bump generation
    /// — every rung permanently stale — would pass the whole suite).
    /// Paused tokio time auto-advances the rung sleeps.
    async fn armed_rung_generations(task: app::Task<Message>) -> Vec<u64> {
        crate::testutil::drained_task_outputs(task, |action| match action {
            cosmic::iced::runtime::Action::Output(cosmic::Action::App(Message::LockPokeDue(
                generation,
            ))) => Some(generation),
            _ => None,
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_lock_event_arms_one_fresh_rung_per_delay() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let task = window.update(Message::LockEvent(lockwatch::LockEvent::Locked));

        let rungs = armed_rung_generations(task).await;
        assert_eq!(
            rungs.len(),
            lockwatch::POKE_DELAYS.len(),
            "one rung per ladder delay"
        );
        assert!(
            rungs.iter().all(|g| *g == window.lock_poke_generation),
            "every rung must carry the *bumped* generation ({}), got {rungs:?} — \
             a pre-bump capture would make the whole ladder stillborn",
            window.lock_poke_generation
        );
    }

    #[test]
    fn a_failed_poke_settles_as_not_written() {
        use cosmic::Application as _;

        // A read-only version dir fails the state write, driving
        // `run_lock_poke`'s warn-and-`false` branch (the completion then
        // carries `wrote == false`).
        let dir = tempfile::tempdir().unwrap();
        let mut window = poke_window(&dir);
        let canonical = vec![("all".to_owned(), bg_path_source("a"))];
        seed_bg_wallpapers(&window, &canonical);
        let seeded_inode = bg_wallpapers_inode(dir.path());

        drop(window.update(Message::LockEvent(lockwatch::LockEvent::Locked)));
        let generation = window.lock_poke_generation;

        let key_dir = bg_wallpapers_key_file(dir.path())
            .parent()
            .expect("key file has a version dir")
            .to_path_buf();
        let locked = crate::testutil::read_only_trees(&[key_dir]);
        let wrote = settle_lock_pokes(&mut window, generation);
        crate::testutil::restore_dir_permissions(&locked);

        assert!(!wrote, "a failed write must settle as not-written");
        assert_eq!(
            bg_wallpapers_inode(dir.path()),
            seeded_inode,
            "the seeded state must be untouched"
        );
        assert_eq!(read_bg_wallpapers(&window), canonical);
    }
}
