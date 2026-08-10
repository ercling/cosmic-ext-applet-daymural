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

use std::collections::HashSet;
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

use crate::catalogue::{self, Catalogue, ImageEntry};
use crate::config::AppletConfig;
// No `fl!` here: every user-visible string this applet renders lives in the
// popup (`view.rs`). The panel contributes an icon and nothing else.
use crate::{accent, bing, schedule, thumbs, tooltip, view, wallpaper};

/// One name everywhere: cosmic-config app ID, state dir, desktop entry.
pub const APP_ID: &str = "io.github.ercling.CosmicBingWallpaper";

/// Symbolic icon shown in the panel.
const PANEL_ICON: &str = "preferences-desktop-wallpaper-symbolic";

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
    /// Whether a dropdown menu popup is mapped — the whole popup ledger (see
    /// "UI conventions → Popup stack" in `CLAUDE.md`).
    ///
    /// One bit is enough because the only thing the invariant needs is "may a
    /// tooltip be arming or destroyed right now?", and the *other* side of
    /// every decision — the tooltip surface — is destroyed by an idempotent
    /// task ([`destroy_tooltip`]) that needs no state of its own. A menu's
    /// window id is minted inside the widget and cannot be read at creation,
    /// so presence is also all that can be tracked.
    ///
    /// Biased toward `true`: a spurious `true` only pauses tooltips, while a
    /// spurious `false` lets one arm beside a mapped menu — the two-children
    /// state the invariant forbids. It is set optimistically on the create,
    /// which the runtime can still drop (it retries a deferred create five
    /// times at 30 ms and then gives up, and `get_popup` failures only log);
    /// tooltips then stay paused until the next thing that clears the bit —
    /// clicking the dropdown again, a `PopupClosed`, or closing the popup.
    pub(crate) dropdown_open: bool,
    /// Applet settings (shuffle, retention). Defaults when the config context
    /// is unavailable.
    pub(crate) config: AppletConfig,
    /// cosmic-config context used to persist setting changes. `None` only if
    /// the config directory could not be created — the applet still runs with
    /// defaults, changes just don't persist.
    config_context: Option<cosmic_config::Config>,
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
    /// Cold-start auto-apply state (see [`ColdStart`]). Spent by *any*
    /// successful apply (auto, manual navigation, shuffle tick): see
    /// [`Window::on_apply_success`].
    cold_start: ColdStart,
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
    /// The refresh timer fired (payload: the generation it was armed with).
    RefreshDue(u64),
    /// The fetch pipeline finished (payload: the freshly fetched entries,
    /// merged into the live catalogue on the UI thread).
    RefreshFinished(Result<Vec<ImageEntry>, RefreshError>),
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
/// with a single bit: the runtime's `Action::Destroy` arm logs
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
            tracing::debug!("popup closed: the tooltip");
            return Task::none();
        }
        if self.popup == Some(id) {
            tracing::debug!("popup closed: our own popup");
            self.popup = None;
        } else {
            // By elimination: a dropdown menu. Its window id is minted inside
            // the widget (`window::Id::unique()` into private state) and is
            // never visible to us at creation. A *stale* id lands here too —
            // the `Done` for a popup `TogglePopup` already took out of
            // `self.popup`, or one for a menu that closed before another
            // opened — which is why both branches end the same way: clearing
            // the bit is the conservative direction (a menu still mapped is
            // recovered by the runtime, which destroys popups above a create's
            // requested parent), and the tooltip destroy is a no-op unless one
            // is really mapped.
            tracing::debug!("popup closed: a dropdown menu");
        }
        self.dropdown_open = false;
        // Our popup dying does *not* take a mapped tooltip with it on the
        // compositor path (`…/handlers/shell/xdg_popup.rs::done` walks only up
        // the parent chain), so the orphan is cleaned up here; on the
        // self-initiated path it is already gone and this is the documented
        // no-op.
        destroy_tooltip()
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
        if self.dropdown_open {
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
                tracing::debug!("dropdown opened");
                self.dropdown_open = true;
                before = destroy_tooltip();
            }
            cosmic::surface::Action::DestroyPopup(_) => {
                tracing::debug!("dropdown destroyed");
                self.dropdown_open = false;
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

    /// Write-on-change: adopt `config` and persist it if it differs from the
    /// current settings. Used by the shuffle/retention controls.
    fn set_config(&mut self, config: AppletConfig) {
        use cosmic_config::CosmicConfigEntry as _;

        if self.config == config {
            return;
        }
        self.config = config;
        if let Some(context) = &self.config_context
            && let Err(error) = self.config.write_entry(context)
        {
            tracing::warn!("failed to persist applet config change: {error}");
        }
    }

    /// Arm the one-shot refresh timer for `delay` from now, invalidating any
    /// previously armed timer via the generation counter.
    ///
    /// Accepted v1 limitation (same as the GNOME reference): the sleep is
    /// monotonic and does not advance during system suspend, so a refresh
    /// due while suspended fires late after resume instead of immediately.
    fn schedule_refresh(&mut self, delay: Duration) -> app::Task<Message> {
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
        self.shuffle_generation += 1;
        self.shuffle_armed = false;
    }

    /// Bring the shuffle timer in line with the current state: it runs only
    /// while shuffle is enabled and at least two images exist. Pass
    /// `reset_countdown` to force a re-arm even when a tick is already
    /// pending (manual navigation, settings changes).
    fn sync_shuffle(&mut self, reset_countdown: bool) -> app::Task<Message> {
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
        self.prune_over(
            wallpaper::current_wallpaper(),
            &wallpaper::download_dir(),
            state_dir(),
            &catalogue_path(),
        )
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
        self.current = wallpaper::synced_current(&live, self.current.take());
        self.catalogue.prune(
            download_dir,
            wallpaper::prune_retention(&live, self.config.retention_days),
            self.current.as_deref(),
            Utc::now(),
        );
        self.sweep_thumbnails(state_dir);
        if let Err(error) = self.catalogue.save(catalogue_path) {
            // Non-fatal: the catalogue is rebuildable from the folder scan.
            tracing::warn!("failed to persist catalogue after prune: {error}");
        }
        live.into_file()
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
        if self.refresh_pending {
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

    /// [`Window::start_refresh`] against an already-read cosmic-bg state
    /// (injected so tests can stage a wallpaper the applet has not seen
    /// itself apply). The caller has established that no fetch is in flight.
    fn start_refresh_over(&mut self, live: wallpaper::CurrentWallpaper) -> app::Task<Message> {
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
        let current = wallpaper::synced_current(&live, self.current.take());
        self.current = current.clone();
        cosmic::task::future(async move {
            Message::RefreshFinished(run_refresh(catalogue, retention_days, &live, current).await)
        })
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
        self.thumbnail_pass_pending = true;
        let catalogue = self.catalogue.clone();
        let retention_days = self.config.retention_days;
        // As in `start_refresh_over`: the protected file must be the one the
        // *prune* protects, i.e. the live state, not our own last apply.
        let current = wallpaper::synced_current(&live, self.current.take());
        self.current = current.clone();
        let download_dir = wallpaper::download_dir();
        cosmic::task::future(async move {
            run_thumbnail_pass(
                catalogue,
                retention_days,
                &live,
                current,
                &download_dir,
                state_dir(),
            )
            .await;
            Message::ThumbnailsReady
        })
    }

    /// The startup thumbnail pass finished: nothing writes into the cache
    /// any more, so collect whatever a prune skipped while it ran (see
    /// [`Window::may_sweep_thumbnails`]).
    fn finish_thumbnail_pass(&mut self, state_dir: &Path) {
        self.thumbnail_pass_pending = false;
        self.sweep_thumbnails(state_dir);
    }

    /// React to the fetch pipeline finishing: merge the fetched entries
    /// into the live catalogue, prune + persist, auto-apply per the plan,
    /// and reschedule the next refresh.
    fn finish_refresh(
        &mut self,
        result: Result<Vec<ImageEntry>, RefreshError>,
    ) -> app::Task<Message> {
        self.refresh_pending = false;
        let fetched = match result {
            Ok(fetched) => fetched,
            Err(error) => {
                tracing::warn!("refresh failed: {error}");
                self.last_error = Some(error);
                return self.schedule_refresh(schedule::ERROR_RETRY_DELAY);
            }
        };

        // Merge into the *live* catalogue: a wholesale replacement from
        // the pipeline's snapshot would resurrect entries a concurrent
        // prune removed. The prune likewise runs here on the UI thread,
        // against what is applied *right now* and the *current* retention
        // — the pipeline's start-of-fetch snapshot may be stale on both
        // counts, and the currently applied file must never be deleted.
        self.catalogue.merge(fetched);
        let live = self.prune_and_persist();

        self.last_updated = Some(Utc::now());
        self.last_error = None;

        // Bound once: the newest entry answers all three questions below —
        // whether the fetch delivered anything at all, when the next refresh
        // is due, and what to auto-apply. Cloned because the apply arm
        // mutates `self`.
        let newest = self.catalogue.newest().cloned();
        let plan = refresh_success_plan(
            self.cold_start.applies_over(live.as_deref()),
            live.as_deref(),
            newest.as_ref().map(|e| e.fullstartdate.as_str()),
            Utc::now(),
        );
        let mut apply_failed = false;
        let mut accent = Task::none();
        if plan.auto_apply
            && let Some(newest) = &newest
        {
            let path = newest.filename.clone();
            match wallpaper::apply(&path) {
                Ok(()) => accent = self.on_apply_success(path),
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
        // A "success" that somehow delivered no images keeps the flag armed
        // for the fetch that finally does.
        if newest.is_some() && !apply_failed {
            self.cold_start = ColdStart::Done;
        }

        let refresh_timer = self.schedule_refresh(plan.delay);
        // A grown catalogue may unlock a waiting shuffle (≥2 images); a
        // pending tick keeps its countdown.
        let shuffle = self.sync_shuffle(false);
        Task::batch([refresh_timer, shuffle, accent])
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

    /// Arm the async accent extraction for `source` (a freshly applied
    /// wallpaper, or the startup-restored current). Gated on the setting and
    /// on usable theme handles; finishes in [`Message::AccentComputed`],
    /// whose handler re-checks everything against live state.
    fn start_accent_compute(&self, source: PathBuf) -> app::Task<Message> {
        if !self.config.accent_enabled || self.accent_handles.is_none() {
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
        // `finish_disable_restore`'s full `set_config` rewrites it `false`
        // from memory) — reading after them would read our own write back
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
    ///    known. It was pinned onto the disk too, but a concurrent
    ///    `set_config` full-entry write (retention/shuffle changed while the
    ///    task flew) rewrites the flag from stale memory, so the in-memory
    ///    record, not the disk, carries the user's intent.
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
fn restore_catalogue(path: &Path, images_dir: &Path, state_dir: &Path) -> Catalogue {
    let mut catalogue = Catalogue::load_or_rebuild(path, images_dir);
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
    catalogue
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
    retention_days: u16,
    live: &wallpaper::CurrentWallpaper,
    current: Option<PathBuf>,
) -> Result<Vec<ImageEntry>, RefreshError> {
    let client = bing::http_client()?;
    fetch_and_download(
        &client,
        bing::BING_BASE_URL,
        &catalogue,
        schedule::fetch_count(retention_days),
        &wallpaper::download_dir(),
        state_dir(),
        &Backfill::new(live, retention_days, current.as_deref()),
    )
    .await
    .map_err(RefreshError::from)
}

/// Fetch the latest `count` images from `base_url` and download the
/// missing ones into `download_dir` (thumbnails cached under `state_dir`),
/// then backfill thumbnails for older catalogue entries per `backfill`.
/// All roots, the endpoint and the backfill policy are injected so tests can
/// run the whole pipeline against tempdirs and a loopback mock server.
async fn fetch_and_download(
    client: &reqwest::Client,
    base_url: &str,
    catalogue: &Catalogue,
    count: u8,
    download_dir: &Path,
    state_dir: &Path,
    backfill: &Backfill<'_>,
) -> Result<Vec<ImageEntry>, bing::FetchError> {
    // A crash mid-download leaves an orphaned `.part` behind; sweep first.
    bing::sweep_part_files(download_dir);

    let archive = bing::fetch_image_list(client, base_url, count).await?;

    let mut fetched = Vec::with_capacity(archive.images.len());
    for image in &archive.images {
        // A rebuilt entry may already hold this image at a different
        // resolution suffix — that file stays authoritative (no
        // re-download); the merge refills its metadata.
        //
        // Unless it never was an image: this lookup skips the download just
        // as permanently as `bing::download_image`'s own existence check
        // does, so a catalogued file failing the same magic-byte test the
        // download applies to a fresh body would otherwise stay the entry's
        // wallpaper for good. Unlinking it *after* the replacement lands is
        // what lets the merge heal the entry — an entry's file claim only
        // counts as dead once the file is gone — while a failed download
        // leaves the user's folder exactly as it was.
        let path = match catalogue.existing_file(&image.urlbase, download_dir) {
            Some(existing) if bing::is_jpeg_file(&existing) => existing,
            existing => {
                let fresh = bing::download_image(client, base_url, image, download_dir).await?;
                if let Some(corrupt) = existing.filter(|path| *path != fresh)
                    && let Err(error) = std::fs::remove_file(&corrupt)
                {
                    tracing::warn!("failed to remove {}: {error}", corrupt.display());
                }
                fresh
            }
        };
        ensure_thumbnail_logged(&path, state_dir).await;
        fetched.push(ImageEntry::from_bing(image, path));
    }

    // Backfill thumbnails for catalogue entries outside this fetch window —
    // rebuilt or older entries would otherwise show the placeholder forever.
    // Files the fetch loop above just handled are skipped.
    let handled: HashSet<&Path> = fetched.iter().map(|e| e.filename.as_path()).collect();
    backfill_thumbnails(catalogue, &handled, download_dir, state_dir, backfill).await;

    Ok(fetched)
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
    backfill: &Backfill<'_>,
) {
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
    retention_days: u16,
    live: &wallpaper::CurrentWallpaper,
    current: Option<PathBuf>,
    download_dir: &Path,
    state_dir: &Path,
) {
    backfill_thumbnails(
        &catalogue,
        &HashSet::new(),
        download_dir,
        state_dir,
        &Backfill::new(live, retention_days, current.as_deref()),
    )
    .await;
}

/// Policy for the out-of-window thumbnail backfill in [`fetch_and_download`]:
/// how much decoding one refresh may do, and which entries are worth it.
struct Backfill<'a> {
    /// Decode attempts this refresh may spend (see
    /// [`MAX_THUMBNAIL_BACKFILL`]).
    budget: usize,
    /// The *effective* retention in days the prune after this refresh will
    /// apply (`0` = keep forever) — [`wallpaper::prune_retention`] of the
    /// configured value, never the configured value itself.
    retention_days: u16,
    /// The currently applied wallpaper, if known — protected from the
    /// retention skip just as the prune protects it from deletion.
    current: Option<&'a Path>,
    /// Reference time for the retention cutoff (injected for tests).
    now: DateTime<Utc>,
}

impl<'a> Backfill<'a> {
    /// The retention is derived here, from the same live cosmic-bg state the
    /// prune will consult, so the two predicates cannot be handed different
    /// numbers by a caller.
    fn new(
        live: &wallpaper::CurrentWallpaper,
        configured_days: u16,
        current: Option<&'a Path>,
    ) -> Self {
        Self {
            budget: MAX_THUMBNAIL_BACKFILL,
            retention_days: wallpaper::prune_retention(live, configured_days),
            current,
            now: Utc::now(),
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
        entry.within_retention(self.retention_days, self.now)
            || self.current == Some(entry.filename.as_path())
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

/// Pure decisions after a successful fetch (tested): whether to auto-apply
/// the newest image, and when the next refresh is due.
struct RefreshSuccessPlan {
    auto_apply: bool,
    delay: Duration,
}

fn refresh_success_plan(
    cold_start_pending: bool,
    live_current: Option<&Path>,
    newest_fullstartdate: Option<&str>,
    now: DateTime<Utc>,
) -> RefreshSuccessPlan {
    let has_images = newest_fullstartdate.is_some();
    RefreshSuccessPlan {
        auto_apply: has_images && wallpaper::should_auto_apply(cold_start_pending, live_current),
        delay: match newest_fullstartdate {
            Some(date) => schedule::next_refresh(Some(date), now),
            // A success that leaves the catalogue empty must not reuse the
            // 5 s cold-start delay — that would tight-loop against Bing.
            None => schedule::ERROR_RETRY_DELAY,
        },
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
        let catalogue =
            restore_catalogue(&catalogue_path(), &wallpaper::download_dir(), state_dir());
        // Read once and handed to the thumbnail pass below: it must protect
        // the applied file's preview exactly as the prune protects its file.
        let live = wallpaper::current_wallpaper();
        let current = live.clone().into_file();
        let cold_start = if catalogue.images.is_empty() {
            ColdStart::Pending
        } else {
            ColdStart::Done
        };

        // Empty catalogue (cold start) → fetch fires ~5 s after startup;
        // otherwise the next refresh derives from the newest fullstartdate.
        let delay = schedule::next_refresh(
            catalogue.newest().map(|e| e.fullstartdate.as_str()),
            Utc::now(),
        );

        let mut window = Self {
            core,
            popup: None,
            dropdown_open: false,
            config,
            config_context,
            catalogue,
            current,
            refresh_pending: false,
            thumbnail_pass_pending: false,
            cold_start,
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
        };
        let timer = window.schedule_refresh(delay);
        // Shuffle restored as enabled starts a fresh full-interval cycle.
        let shuffle = window.sync_shuffle(false);
        // Previews come from the cache, so the cache is filled *now*, from
        // the files already on disk — not by a refresh that may be ~24 h out
        // (or never, offline).
        let thumbnails = window.start_thumbnail_pass_over(live);
        // Startup reconciliation: startup does *not* pass through
        // `on_apply_success` (`current` was restored above), and the next
        // apply can be ~24 h out — or never, offline — while the wallpaper or
        // the accent may have changed when the applet was down. The same
        // extraction task catches both: an external accent change disarms,
        // anything else re-applies. Gated inside on `accent_enabled`.
        let accent = window.accent_compute_for_current();
        (window, Task::batch([timer, shuffle, thumbnails, accent]))
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
                    // [`Window::on_popup_closed`]), but only asynchronously,
                    // and by then the id no longer matches `self.popup`. So
                    // the ledger is cleared here rather than left to a row
                    // that can no longer fire: a stale `dropdown_open` would
                    // pause every tooltip for the rest of the session.
                    self.dropdown_open = false;
                    return cosmic::surface::surface_task(cosmic::surface::action::destroy_popup(
                        popup_id,
                    ));
                }
                return cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                    |_: &Window| Default::default(),
                    |window: &mut Window| {
                        let new_id = window::Id::unique();
                        window.popup.replace(new_id);

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
            }
            Message::PopupClosed(id) => return self.on_popup_closed(id),
            Message::ConfigUpdated(config) => {
                // Our own setter writes echo back here unchanged (no-op);
                // an *external* edit of the shuffle settings restarts the
                // countdown against the new values, and an externally
                // reduced retention prunes immediately. Watched configs
                // arrive raw — normalize like `AppletConfig::load` does,
                // so a hand-edited retention outside the dropdown's
                // choices never drives prune/fetch.
                let mut config = config.normalize();
                let shuffle_changed = config.shuffle_enabled != self.config.shuffle_enabled
                    || config.shuffle_interval_secs != self.config.shuffle_interval_secs;
                let retention_reduced =
                    schedule::retention_reduced(self.config.retention_days, config.retention_days);
                // An external flip of `accent_enabled` must run the same
                // lifecycle as the popup toggler — snapshot + compute on
                // enable, restore + clear on disable — not silently adopt
                // the flag (which would orphan the snapshot on disable and
                // never snapshot on enable). The three accent fields
                // themselves NEVER adopt from a watcher payload: payloads
                // are read at event time and can be delivered late, so even
                // with no task in flight a payload can carry stale
                // mid-flight state (a `last_written: None` adopted after
                // the write completed makes the next recompute disarm
                // spuriously — the incident's oscillation class). Under the
                // single-instance assumption the in-memory accent fields
                // are authoritative; a routed flip persists the resulting
                // accent state itself.
                //
                // A payload flip is only *evidence* of an external edit —
                // verified against a fresh disk read before routing (a
                // stale echo's flag disagrees with memory but the disk
                // agrees; a genuine external flip lives on the disk). While
                // an accent theme task is in flight no flip is routed at
                // all — the completion reconciles against the disk itself.
                let accent_flip = if self.accent_inflight.is_some() {
                    None
                } else if config.accent_enabled != self.config.accent_enabled {
                    match &self.config_context {
                        Some(context) => {
                            let disk = AppletConfig::load(context).accent_enabled;
                            (disk != self.config.accent_enabled).then_some(disk)
                        }
                        // No disk to verify against — but a memory-only
                        // config has no persists of ours to echo either, so
                        // the payload is taken at face value.
                        None => Some(config.accent_enabled),
                    }
                } else {
                    None
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
                if !tasks.is_empty() {
                    return Task::batch(tasks);
                }
            }
            Message::RefreshDue(generation) => {
                // Stale timers (replaced by a newer reschedule) are ignored.
                if generation == self.timer_generation {
                    return self.start_refresh();
                }
            }
            Message::RefreshNow => return self.start_refresh(),
            Message::ApplyImage(path) => {
                // Browsing is setting: prev/next/newest apply immediately.
                // Manual navigation also resets the shuffle countdown —
                // and spends the cold-start flag (any successful apply
                // fulfills its purpose; see `on_apply_success`).
                match wallpaper::apply(&path) {
                    Ok(()) => {
                        let accent = self.on_apply_success(path);
                        let shuffle = self.sync_shuffle(true);
                        return Task::batch([accent, shuffle]);
                    }
                    Err(error) => {
                        tracing::warn!("failed to apply {}: {error}", path.display());
                        // If the file vanished externally (the common way
                        // apply refuses), prune right away: the routine
                        // prune drops entries whose file is gone, so the
                        // dead image leaves the popup instead of failing
                        // on every further click until the next fetch.
                        if !path.is_file() {
                            return self.prune_immediately();
                        }
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
                if generation != self.shuffle_generation {
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
                let mut config = self.config.clone();
                config.shuffle_enabled = enabled;
                self.set_config(config);
                // Enabling starts a fresh full-interval countdown.
                return self.sync_shuffle(true);
            }
            Message::SetShuffleInterval(index) => {
                let mut config = self.config.clone();
                config.shuffle_interval_secs = view::shuffle_interval_secs(index);
                self.set_config(config);
                // Picking an interval restarts the countdown at that length.
                return self.sync_shuffle(true);
            }
            Message::SetRetention(index) => {
                let new_days = view::retention_days(index);
                let reduced = schedule::retention_reduced(self.config.retention_days, new_days);
                let mut config = self.config.clone();
                config.retention_days = new_days;
                self.set_config(config);
                if reduced {
                    // Reduced retention prunes immediately (the routine
                    // post-fetch prune would otherwise leave over-limit
                    // files around for up to a day).
                    return self.prune_immediately();
                }
            }
            Message::SetAccentEnabled(enabled) => return self.set_accent_enabled(enabled),
            Message::AccentComputed { source, hue } => {
                return self.finish_accent_compute(source, hue);
            }
            Message::AccentWriteFinished {
                generation,
                success,
            } => return self.finish_accent_task(generation, success),
            Message::TooltipSurface(action) => return self.on_tooltip_surface(action),
            Message::DropdownSurface(action) => return self.on_dropdown_surface(action),
            Message::RefreshFinished(result) => return self.finish_refresh(result),
            // Returning to the message loop re-renders the popup, so a
            // preview generated while it was open shows up by itself.
            Message::ThumbnailsReady => {
                self.finish_thumbnail_pass(state_dir());
                // The startup (or enable-time) accent compute may have found
                // a cold cache and dropped its answer; the pass that just
                // ended is what writes those thumbnails, so this is the
                // moment a retry can succeed. Free when disabled or already
                // answered (a cached decode plus the steady-state Skip).
                return self.accent_compute_for_current();
            }
        }
        Task::none()
    }

    fn subscription(&self) -> iced::Subscription<Self::Message> {
        // Keep `self.config` in sync with on-disk changes (our own setter
        // writes echo back through here too, which is harmless).
        self.core
            .watch_config::<AppletConfig>(APP_ID)
            .map(|update| Message::ConfigUpdated(update.config))
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

    #[test]
    fn app_id_is_reverse_dns() {
        assert_eq!(APP_ID, "io.github.ercling.CosmicBingWallpaper");
        assert_eq!(<Window as cosmic::Application>::APP_ID, APP_ID);
        assert!(APP_ID.split('.').count() >= 3);
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
            Some("202608070700"),
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
            Some("202608070700"),
            now,
        );
        assert!(!plan.auto_apply);
    }

    #[test]
    fn refresh_success_plan_without_images_backs_off() {
        // A "successful" fetch that still leaves no images: no 5 s
        // cold-start delay (that would tight-loop against Bing) and no
        // auto-apply. `finish_refresh` also keeps the cold-start flag armed
        // for the fetch that finally delivers (it only spends the flag on a
        // successful apply — see `any_successful_apply_spends_the_cold_start_flag`).
        let plan = refresh_success_plan(true, None, None, Utc::now());
        assert!(!plan.auto_apply);
        assert_eq!(plan.delay, schedule::ERROR_RETRY_DELAY);
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
            Some("202608070700"),
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
            Some("202608070700"),
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

        let restored = restore_catalogue(&cat_path, &images, &state);

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

        let restored = restore_catalogue(&cat_path, &images, &state);

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

        let restored = restore_catalogue(&cat_path, &images, &state);

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
        let restored = restore_catalogue(&cat_path, &images, &state);

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

        let restored = restore_catalogue(&cat_path, &images, &state);

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

        let restored = restore_catalogue(&cat_path, &images, &state);

        assert!(victim.is_file(), "tampered entry must not delete the file");
        assert_eq!(restored.images, vec![kept]);
        assert_eq!(Catalogue::load(&cat_path).unwrap(), restored);
    }

    /// One-image HPImageArchive response for the pipeline tests below.
    const LIST_JSON: &str = r#"{"images":[{"urlbase":"/th?id=OHR.Foo_ROW1","startdate":"20260807","fullstartdate":"202608070700","copyright":"Foo place (© Bar)","copyrightlink":"https://example.com/foo"}]}"#;

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
            1,
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .unwrap();

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
            1,
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .expect("existing file must be reused, not re-downloaded");

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
            1,
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .unwrap();

        let fresh = download_dir.join("20260807-Foo_ROW1_UHD.jpg");
        assert_eq!(fetched[0].filename, fresh);
        assert!(bing::is_jpeg_file(&fresh));
        assert!(thumbs::thumbnail_path(&fresh, &state).unwrap().is_file());
        // …and the merge adopts the replacement, which it only does for an
        // entry whose file claim is dead — hence the unlink.
        assert!(!corrupt.exists(), "the dead file must not survive");
        let mut healed = catalogue.clone();
        healed.merge(fetched);
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
            1,
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .unwrap();

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
            0,
            &wallpaper::CurrentWallpaper::NoFile,
            None,
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
    fn capped(budget: usize) -> Backfill<'static> {
        Backfill {
            budget,
            retention_days: 0,
            current: None,
            now: Utc::now(),
        }
    }

    /// [`capped`] at the production budget, which no test staging a handful
    /// of files can reach — i.e. "backfill everything".
    fn test_backfill() -> Backfill<'static> {
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
            1,
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
            1,
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
            1,
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
            1,
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
            1,
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
            1,
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
            1,
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
            current: Some(&applied),
            now,
        };
        fetch_and_download(
            &client,
            &base,
            &catalogue,
            1,
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
            1,
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
            {"urlbase":"/th?id=OHR.Foo_ROW1","startdate":"20260806","fullstartdate":"202608060700","copyright":"Foo (© Bar)","copyrightlink":"https://example.com/foo"},
            {"urlbase":"/th?id=OHR.Bad_ROW2","startdate":"20260807","fullstartdate":"202608070700","copyright":"Bad (© Bar)","copyrightlink":"https://example.com/bad"}
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
            2,
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
            1,
            &download_dir,
            &state,
            &test_backfill(),
        )
        .await
        .expect("an undecodable file must not fail the pipeline");

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

    #[test]
    fn desktop_entry_stays_in_sync_with_the_app_id() {
        // The desktop entry is hand-maintained and never compiled; these are
        // the properties `desktop-file-validate` does not check for us.
        const DESKTOP: &str = include_str!("../data/io.github.ercling.CosmicBingWallpaper.desktop");
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

        window.finish_thumbnail_pass(&state);

        assert!(window.may_sweep_thumbnails());
        assert!(!orphan.exists(), "the deferred sweep runs at the end");
    }

    use crate::testutil::surface::{
        Emitted, app_dropdown_create, dropdown_create, dropdown_destroy, emitted, tooltip_arm,
        tooltip_destroy,
    };

    /// Our popup's own close: the id is dropped, the ledger is cleared, and a
    /// possibly-orphaned tooltip is swept.
    ///
    /// The sweep is for the compositor path — `…/handlers/shell/xdg_popup.rs`'s
    /// `done` walks only *up* the parent chain, so a mapped tooltip is left in
    /// the runtime's list with a dead parent. On the self-initiated path the
    /// runtime already took it and the destroy is the documented no-op.
    #[tokio::test]
    async fn popup_closed_for_our_popup_clears_the_ledger_and_sweeps_the_tooltip() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let ours = window::Id::unique();
        window.popup = Some(ours);
        window.dropdown_open = true;

        let task = window.update(Message::PopupClosed(ours));

        assert_eq!(window.popup, None);
        assert!(!window.dropdown_open);
        assert_eq!(emitted(task).await, vec![Emitted::DestroyTooltip]);
    }

    /// The tooltip's own close names the shared tooltip surface. It says
    /// nothing about menus, so the ledger must not move — a cleared
    /// `dropdown_open` would un-pause tooltips beside a mapped menu.
    #[tokio::test]
    async fn popup_closed_for_the_tooltip_surface_changes_nothing() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let ours = window::Id::unique();
        window.popup = Some(ours);
        window.dropdown_open = true;

        let task = window.update(Message::PopupClosed(crate::tooltip::window_id()));

        assert_eq!(window.popup, Some(ours), "a tooltip close it is, not ours");
        assert!(window.dropdown_open, "a dropdown close it is not");
        assert!(emitted(task).await.is_empty(), "nothing to destroy");
    }

    /// A dropdown menu is identified by elimination — its window id is minted
    /// inside the widget and never visible here. Grab-loss dismissal publishes
    /// no `DestroyPopup`, so this close is the only signal the ledger gets.
    #[tokio::test]
    async fn popup_closed_for_an_unknown_surface_clears_the_dropdown() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let ours = window::Id::unique();
        window.popup = Some(ours);
        window.dropdown_open = true;

        let task = window.update(Message::PopupClosed(window::Id::unique()));

        assert_eq!(window.popup, Some(ours), "another surface is not ours");
        assert!(!window.dropdown_open);
        assert_eq!(
            emitted(task).await,
            vec![Emitted::DestroyTooltip],
            "the menu is gone, so a tooltip under it is topmost again"
        );
    }

    /// The stale-id case the by-elimination rule cannot distinguish: the
    /// `Done` for a popup `TogglePopup` already took out of `self.popup`
    /// arrives later and is read as a menu. Harmless by construction — both
    /// branches end in the same clear-and-sweep.
    #[tokio::test]
    async fn a_late_close_for_a_popup_we_already_took_is_harmless() {
        use cosmic::Application as _;

        let mut window = Window::default();
        let ours = window::Id::unique();
        window.popup = Some(ours);
        drop(window.update(Message::TogglePopup));
        assert_eq!(window.popup, None, "taken before the destroy is emitted");

        let task = window.update(Message::PopupClosed(ours));

        assert_eq!(window.popup, None);
        assert!(!window.dropdown_open);
        assert_eq!(emitted(task).await, vec![Emitted::DestroyTooltip]);
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
            assert!(!window.dropdown_open, "the tooltip route never opens one");
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
        window.dropdown_open = true;

        for action in [tooltip_arm(), tooltip_destroy(), dropdown_create()] {
            let task = window.update(Message::TooltipSurface(action));
            assert!(
                emitted(task).await.is_empty(),
                "nothing may reach the runtime under an open menu"
            );
            assert!(window.dropdown_open);
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

            assert!(window.dropdown_open);
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
        window.dropdown_open = true;

        let task = window.update(Message::DropdownSurface(dropdown_create()));

        assert!(window.dropdown_open);
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

        assert!(!window.dropdown_open, "no create, no menu");
        assert_eq!(emitted(task).await, vec![Emitted::Arm]);
    }

    /// The menu's own destroy: forwarded first, then the tooltip sweep — only
    /// once the menu is gone is a tooltip beneath it topmost again. This is
    /// what makes dropping a tooltip destroy during the menu safe.
    #[tokio::test]
    async fn a_dropdown_destroy_sweeps_the_tooltip_afterwards() {
        use cosmic::Application as _;

        let mut window = Window::default();
        window.popup = Some(window::Id::unique());
        window.dropdown_open = true;

        let task = window.update(Message::DropdownSurface(dropdown_destroy()));

        assert!(!window.dropdown_open);
        assert_eq!(
            emitted(task).await,
            vec![Emitted::DestroyOther, Emitted::DestroyTooltip],
            "the menu goes first; only then is the tooltip topmost"
        );
    }

    /// Clicking the panel icon closes our popup with an explicit destroy. The
    /// runtime *does* announce that back as `PopupClosed`, but asynchronously
    /// and with an id `self.popup` no longer holds, so the ledger is cleared
    /// on the spot — a stale `dropdown_open` would pause every later tooltip.
    #[tokio::test]
    async fn closing_our_own_popup_clears_the_ledger() {
        use cosmic::Application as _;

        let mut window = Window::default();
        window.popup = Some(window::Id::unique());
        window.dropdown_open = true;

        let task = window.update(Message::TogglePopup);

        assert_eq!(window.popup, None);
        assert!(
            !window.dropdown_open,
            "a stale dropdown would pause tooltips for the rest of the session"
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
        assert!(!window.dropdown_open);
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

    #[test]
    fn a_refused_external_enable_is_persisted_back_off() {
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
        drop(window.update(Message::ConfigUpdated(external)));
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

    #[test]
    fn config_updated_accent_flips_run_the_toggle_lifecycle() {
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
        drop(window.update(Message::ConfigUpdated(external)));
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
        drop(window.update(Message::ConfigUpdated(external)));
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

    #[test]
    fn a_config_echo_during_an_inflight_write_is_not_routed_through_the_lifecycle() {
        use cosmic::Application as _;

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
        drop(window.update(Message::ConfigUpdated(echo)));

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

    #[test]
    fn an_external_disable_landing_during_an_enable_restore_is_routed_not_stomped() {
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
        drop(window.update(Message::ConfigUpdated(external)));
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
        drop(window.update(Message::ConfigUpdated(flipped)));
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
        // full-entry persist did not bury it — and the enable lifecycle
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
}
