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
use crate::fl;
use crate::{bing, schedule, thumbs, view, wallpaper};

/// One name everywhere: cosmic-config app ID, state dir, desktop entry.
pub const APP_ID: &str = "io.github.ercling.CosmicBingWallpaper";

/// Symbolic icon shown in the panel.
const PANEL_ICON: &str = "preferences-desktop-wallpaper-symbolic";

/// Hover tooltip on the panel button. A function, not a const: the text is
/// localized and must be read after `localize::localize()` has run.
fn panel_tooltip() -> String {
    fl!("panel-tooltip")
}

pub fn run() -> cosmic::iced::Result {
    cosmic::applet::run::<Window>(())
}

/// The applet's state dir (`~/.local/state/<APP_ID>/`): catalogue JSON +
/// cached thumbnails. Resolved once — the view asks for it on every
/// re-render and must not repeat env/home lookups per frame.
pub fn state_dir() -> &'static Path {
    static STATE_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
        dirs::state_dir()
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".local")
                    .join("state")
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
    /// Cold-start auto-apply state (see [`ColdStart`]). Spent by *any*
    /// successful apply (auto, manual navigation, shuffle tick): see
    /// [`on_apply_success`].
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
                // per-output mode) — same ground the reviewer's rule
                // treats as safe to apply over.
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
    /// Forwarded surface actions (the dropdown's menu opens as its own
    /// wayland popup and drives it through these).
    Surface(cosmic::surface::Action),
}

impl Window {
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
    /// wallpaper changes), prune against the *current* retention, drop the
    /// pruned images' cached thumbnails (later prunes never report these
    /// entries again — skipping this would orphan them permanently), and
    /// persist the catalogue. Returns the live wallpaper source that was
    /// read (`None` for every non-file state).
    ///
    /// When what is displayed is *unknowable* (per-output mode, unreadable
    /// config), `wallpaper::prune_retention` disables age-based deletion
    /// entirely — protecting only the possibly stale `self.current` could
    /// delete a Bing image some output actually displays.
    fn prune_and_persist(&mut self) -> Option<PathBuf> {
        let live = wallpaper::current_wallpaper();
        self.current = wallpaper::synced_current(&live, self.current.take());
        let removed = self.catalogue.prune(
            &wallpaper::download_dir(),
            wallpaper::prune_retention(&live, self.config.retention_days),
            self.current.as_deref(),
            Utc::now(),
        );
        thumbs::remove_thumbnails(&removed, state_dir());
        if let Err(error) = self.catalogue.save(&catalogue_path()) {
            // Non-fatal: the catalogue is rebuildable from the folder scan.
            tracing::warn!("failed to persist catalogue after prune: {error}");
        }
        match live {
            wallpaper::CurrentWallpaper::File(path) => Some(path),
            _ => None,
        }
    }

    /// Kick off the fetch pipeline unless one is already running. The
    /// pipeline only fetches and downloads; merge/prune/save happen back
    /// on the UI thread in `RefreshFinished` against the live state.
    fn start_refresh(&mut self) -> app::Task<Message> {
        if self.refresh_pending {
            return Task::none();
        }
        self.refresh_pending = true;
        let catalogue = self.catalogue.clone();
        let retention_days = self.config.retention_days;
        cosmic::task::future(async move {
            Message::RefreshFinished(run_refresh(catalogue, retention_days).await)
        })
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

        let plan = refresh_success_plan(
            self.cold_start.applies_over(live.as_deref()),
            live.as_deref(),
            self.catalogue.newest().map(|e| e.fullstartdate.as_str()),
            Utc::now(),
        );
        let mut apply_failed = false;
        if plan.auto_apply
            && let Some(newest) = self.catalogue.newest()
        {
            let path = newest.filename.clone();
            match wallpaper::apply(&path) {
                Ok(()) => on_apply_success(&mut self.current, &mut self.cold_start, path),
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
        if plan.clear_cold_start && !apply_failed {
            self.cold_start = ColdStart::Done;
        }

        let refresh_timer = self.schedule_refresh(plan.delay);
        // A grown catalogue may unlock a waiting shuffle (≥2 images); a
        // pending tick keeps its countdown.
        let shuffle = self.sync_shuffle(false);
        Task::batch([refresh_timer, shuffle])
    }
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
fn on_apply_success(current: &mut Option<PathBuf>, cold_start: &mut ColdStart, path: PathBuf) {
    *current = Some(path);
    *cold_start = ColdStart::Done;
}

/// Restore the catalogue at startup and drop entries whose file vanished
/// while the applet wasn't running (folder cleaned out by the user, moved
/// drive, …). A catalogue that is valid JSON but points at nothing must
/// not count as "images exist" — the cold start would stay unarmed
/// and the popup/actions would trust dead paths until a much later
/// refresh. Pruning with retention `0` deletes nothing: it only drops
/// vanished entries — and scrubs tampered entries pointing outside
/// `images_dir` (their files stay untouched) — reporting them so their
/// thumbnails go too.
fn restore_catalogue(path: &Path, images_dir: &Path, state_dir: &Path) -> Catalogue {
    let mut catalogue = Catalogue::load_or_rebuild(path, images_dir);
    let removed = catalogue.prune(images_dir, 0, None, Utc::now());
    if !removed.is_empty() {
        tracing::info!(
            "dropped {} catalogue entr{} whose file vanished",
            removed.len(),
            if removed.len() == 1 { "y" } else { "ies" }
        );
        thumbs::remove_thumbnails(&removed, state_dir);
        if let Err(error) = catalogue.save(path) {
            // Non-fatal: the catalogue is rebuildable from the folder scan.
            tracing::warn!("failed to persist catalogue after startup sweep: {error}");
        }
    }
    catalogue
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
) -> Result<Vec<ImageEntry>, RefreshError> {
    let client = bing::http_client()?;
    fetch_and_download(
        &client,
        bing::BING_BASE_URL,
        &catalogue,
        schedule::fetch_count(retention_days),
        &wallpaper::download_dir(),
        state_dir(),
    )
    .await
    .map_err(RefreshError::from)
}

/// Fetch the latest `count` images from `base_url` and download the
/// missing ones into `download_dir` (thumbnails cached under `state_dir`).
/// All roots and the endpoint are injected so tests can run the whole
/// pipeline against tempdirs and a loopback mock server.
async fn fetch_and_download(
    client: &reqwest::Client,
    base_url: &str,
    catalogue: &Catalogue,
    count: u8,
    download_dir: &Path,
    state_dir: &Path,
) -> Result<Vec<ImageEntry>, bing::FetchError> {
    // A crash mid-download leaves an orphaned `.part` behind; sweep first.
    bing::sweep_part_files(download_dir);

    let archive = bing::fetch_image_list(client, base_url, count).await?;

    let mut fetched = Vec::with_capacity(archive.images.len());
    for image in &archive.images {
        // A rebuilt entry may already hold this image at a different
        // resolution suffix — that file stays authoritative (no
        // re-download); the merge refills its metadata.
        let path = match catalogue.existing_file(&image.urlbase) {
            Some(existing) => existing,
            None => bing::download_image(client, base_url, image, download_dir).await?,
        };
        ensure_thumbnail_logged(&path, state_dir);
        fetched.push(ImageEntry::from_bing(image, path));
    }

    // Backfill thumbnails for catalogue entries outside this fetch window —
    // rebuilt or older entries would otherwise show the placeholder forever.
    // Files the fetch loop above just handled are skipped.
    let handled: std::collections::HashSet<&Path> =
        fetched.iter().map(|e| e.filename.as_path()).collect();
    for entry in &catalogue.images {
        if !handled.contains(entry.filename.as_path()) && entry.filename.is_file() {
            ensure_thumbnail_logged(&entry.filename, state_dir);
        }
    }

    Ok(fetched)
}

/// A failed thumbnail is not fatal: `ensure_thumbnail` regenerates missing
/// thumbs on the next refresh.
fn ensure_thumbnail_logged(path: &Path, state_dir: &Path) {
    if let Err(error) = thumbs::ensure_thumbnail(path, state_dir) {
        tracing::warn!(
            "thumbnail generation failed for {}: {error}",
            path.display()
        );
    }
}

/// Pure decisions after a successful fetch (tested): whether to auto-apply
/// the newest image, whether the cold-start flag is spent, and when the
/// next refresh is due.
struct RefreshSuccessPlan {
    auto_apply: bool,
    clear_cold_start: bool,
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
        // The one-shot cold-start auto-apply is spent only once images
        // actually arrived; a success that somehow yielded none keeps it
        // armed for the fetch that finally delivers.
        clear_cold_start: has_images,
        delay: match newest_fullstartdate {
            Some(date) => schedule::next_refresh(Some(date), now),
            // A success that leaves the catalogue empty must not reuse the
            // 5 s cold-start delay — that would tight-loop against Bing.
            None => schedule::ERROR_RETRY_DELAY,
        },
    }
}

/// Pure diff of an incoming (externally edited or echoed-back) config
/// against the current one — which reactions the update handler owes.
struct ConfigDiff {
    shuffle_changed: bool,
    retention_reduced: bool,
}

fn config_diff(old: &AppletConfig, new: &AppletConfig) -> ConfigDiff {
    ConfigDiff {
        shuffle_changed: new.shuffle_enabled != old.shuffle_enabled
            || new.shuffle_interval_secs != old.shuffle_interval_secs,
        retention_reduced: schedule::retention_reduced(old.retention_days, new.retention_days),
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

        // Restore instantly from disk — no network involved. A corrupt or
        // missing catalogue rebuilds from the download folder scan; entries
        // whose file vanished while we weren't running are dropped so a
        // hollow catalogue still counts as a cold start.
        let catalogue =
            restore_catalogue(&catalogue_path(), &wallpaper::download_dir(), state_dir());
        let current = wallpaper::current_source();
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
            config,
            config_context,
            catalogue,
            current,
            refresh_pending: false,
            cold_start,
            timer_generation: 0,
            shuffle_generation: 0,
            shuffle_armed: false,
            last_updated: None,
            last_error: None,
        };
        let timer = window.schedule_refresh(delay);
        // Shuffle restored as enabled starts a fresh full-interval cycle.
        let shuffle = window.sync_shuffle(false);
        (window, Task::batch([timer, shuffle]))
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::TogglePopup => {
                if let Some(popup_id) = self.popup.take() {
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
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                }
            }
            Message::ConfigUpdated(config) => {
                // Our own setter writes echo back here unchanged (no-op);
                // an *external* edit of the shuffle settings restarts the
                // countdown against the new values, and an externally
                // reduced retention prunes immediately. Watched configs
                // arrive raw — normalize like `AppletConfig::load` does,
                // so a hand-edited retention outside the dropdown's
                // choices never drives prune/fetch.
                let config = config.normalize();
                let diff = config_diff(&self.config, &config);
                self.config = config;
                let mut tasks = Vec::new();
                if diff.retention_reduced {
                    tasks.push(self.prune_immediately());
                }
                if diff.shuffle_changed {
                    tasks.push(self.sync_shuffle(true));
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
                        on_apply_success(&mut self.current, &mut self.cold_start, path);
                        return self.sync_shuffle(true);
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
            Message::OpenUrl(url) => open_detached(url.into()),
            Message::OpenFile(path) => open_detached(path.into_os_string()),
            Message::ShuffleDue(generation) => {
                // Stale ticks (replaced by a newer re-arm) are ignored.
                if generation != self.shuffle_generation {
                    return Task::none();
                }
                self.shuffle_armed = false;
                if !self.config.shuffle_enabled {
                    return Task::none();
                }
                if let Some(pick) = self.catalogue.random_other(self.current.as_deref()) {
                    let path = pick.filename.clone();
                    match wallpaper::apply(&path) {
                        Ok(()) => on_apply_success(&mut self.current, &mut self.cold_start, path),
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
                return self.sync_shuffle(false);
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
            Message::Surface(action) => {
                return cosmic::surface::surface_task(action);
            }
            Message::RefreshFinished(result) => return self.finish_refresh(result),
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

    fn view(&self) -> Element<'_, Self::Message> {
        let button = self
            .core
            .applet
            .icon_button(PANEL_ICON)
            .on_press_down(Message::TogglePopup);
        // Panel-level tooltip: parented to the panel (`parent_id: None`) and
        // suppressed while our popup is open (`has_popup`), per libcosmic's
        // own applet example.
        self.core
            .applet
            .applet_tooltip::<Message>(
                button,
                panel_tooltip(),
                self.popup.is_some(),
                Message::Surface,
                None,
            )
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
    fn panel_tooltip_names_the_applet() {
        // Guards the English copy of the one string the panel shows on
        // hover (the test loader is pinned to `en` — see `localize.rs`).
        assert_eq!(panel_tooltip(), "Bing Wallpaper of the Day");
    }

    #[test]
    fn state_paths_live_under_the_app_id() {
        if dirs::home_dir().is_none() {
            eprintln!("skipping: no home dir in this environment");
            return;
        }
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
    fn config_diff_detects_shuffle_and_retention_changes() {
        let base = AppletConfig::default();

        // Echoed-back identical config: nothing owed.
        let diff = config_diff(&base, &base.clone());
        assert!(!diff.shuffle_changed);
        assert!(!diff.retention_reduced);

        // Shuffle toggled.
        let mut toggled = base.clone();
        toggled.shuffle_enabled = true;
        assert!(config_diff(&base, &toggled).shuffle_changed);

        // Interval changed.
        let mut interval = base.clone();
        interval.shuffle_interval_secs = 1_800;
        assert!(config_diff(&base, &interval).shuffle_changed);

        // Retention reduced (8 → 3) prunes; loosened (8 → 30) does not.
        let mut reduced = base.clone();
        reduced.retention_days = 3;
        let diff = config_diff(&base, &reduced);
        assert!(diff.retention_reduced);
        assert!(!diff.shuffle_changed);
        let mut loosened = base.clone();
        loosened.retention_days = 30;
        assert!(!config_diff(&base, &loosened).retention_reduced);
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
        assert!(plan.clear_cold_start);
        assert_eq!(
            plan.delay,
            schedule::next_refresh(Some("202608070700"), now)
        );

        // Warm, foreign wallpaper: never clobber, but the flag is spent.
        let plan = refresh_success_plan(
            false,
            Some(std::path::Path::new("/usr/share/backgrounds/x.jpg")),
            Some("202608070700"),
            now,
        );
        assert!(!plan.auto_apply);
        assert!(plan.clear_cold_start);
    }

    #[test]
    fn refresh_success_plan_without_images_backs_off_and_keeps_cold_start() {
        // A "successful" fetch that still leaves no images: no 5 s
        // cold-start delay (tight loop against Bing), no auto-apply, and
        // the cold-start flag stays armed for the fetch that delivers.
        let plan = refresh_success_plan(true, None, None, Utc::now());
        assert!(!plan.auto_apply);
        assert!(!plan.clear_cold_start);
        assert_eq!(plan.delay, schedule::ERROR_RETRY_DELAY);
    }

    #[test]
    fn any_successful_apply_spends_the_cold_start_flag() {
        // Scenario from review: cold start stays armed after the first
        // fetch's auto-apply *failed*; the user then navigates (or a
        // shuffle tick fires) and an apply succeeds. That apply fulfills
        // the cold-start purpose — the flag must be spent, or the next
        // refresh's unconditional cold-start branch would clobber a
        // wallpaper the user picked in COSMIC Settings in between.
        let mut current = None;
        let mut cold_start = ColdStart::Pending;

        on_apply_success(
            &mut current,
            &mut cold_start,
            PathBuf::from("/imgs/20260807-Foo_ROW1_UHD.jpg"),
        );

        assert_eq!(
            current.as_deref(),
            Some(Path::new("/imgs/20260807-Foo_ROW1_UHD.jpg"))
        );
        assert_eq!(cold_start, ColdStart::Done);

        // With the flag spent, a later refresh over a foreign (user-picked)
        // wallpaper no longer auto-applies.
        let user_choice = Path::new("/usr/share/backgrounds/user-choice.jpg");
        let plan = refresh_success_plan(
            cold_start.applies_over(Some(user_choice)),
            Some(user_choice),
            Some("202608070700"),
            Utc::now(),
        );
        assert!(!plan.auto_apply);
    }

    #[test]
    fn cold_start_retry_never_clobbers_a_wallpaper_picked_after_the_failure() {
        // Iteration-5 scenario: the cold-start auto-apply failed while the
        // system default was displayed; before the retry the user picks a
        // different (foreign) wallpaper in COSMIC Settings. The retry must
        // not fire — the user's choice wins.
        let default_bg = Path::new("/usr/share/backgrounds/cosmic/default.jpg");
        let user_choice = Path::new("/usr/share/backgrounds/user-choice.jpg");
        let retry = ColdStart::RetryOver(Some(default_bg.to_path_buf()));

        // Display unchanged since the failure: the retry still fires (a
        // fresh install with a transient failure must not end up
        // wallpaper-less — the iteration-3 retry decision).
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
        assert!(plan.clear_cold_start);
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
        let orphan_thumb = thumbs::thumbnail_path(&gone.filename, &state);
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
    fn restore_catalogue_of_valid_json_pointing_at_nothing_is_empty() {
        // The cold-start case codex flagged: valid catalogue JSON, every
        // file gone. Startup must see an *empty* catalogue (so the cold
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
        )
        .await
        .unwrap();

        assert_eq!(fetched.len(), 1);
        let path = &fetched[0].filename;
        assert_eq!(path, &download_dir.join("20260807-Foo_ROW1_UHD.jpg"));
        assert_eq!(std::fs::read(path).unwrap(), expected);
        assert_eq!(fetched[0].title, "Foo place");
        assert!(thumbs::thumbnail_path(path, &state).is_file());
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
        let fetched = fetch_and_download(&client, &base, &catalogue, 1, &download_dir, &state)
            .await
            .expect("existing file must be reused, not re-downloaded");

        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].filename, existing);
        assert!(!download_dir.join("20260807-Foo_ROW1_UHD.jpg").exists());
        // The thumbnail backfill covered the pre-existing entry too.
        assert!(thumbs::thumbnail_path(&existing, &state).is_file());
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
        std::fs::write(&old, crate::testutil::tiny_jpeg(64, 36)).unwrap();
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
        let fetched = fetch_and_download(&client, &base, &catalogue, 1, &download_dir, &state)
            .await
            .unwrap();

        // The fetch window holds only the new image…
        assert_eq!(fetched.len(), 1);
        assert!(thumbs::thumbnail_path(&fetched[0].filename, &state).is_file());
        // …yet the older catalogue entry got a thumbnail all the same.
        assert!(
            thumbs::thumbnail_path(&old, &state).is_file(),
            "out-of-window entry must be backfilled"
        );
        assert!(!thumbs::thumbnail_path(&foreign, &state).exists());
    }
}
