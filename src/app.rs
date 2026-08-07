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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use cosmic::{
    Element, Task, app, cosmic_config,
    iced::{self, window},
    widget,
};

use crate::catalogue::{self, Catalogue, ImageEntry};
use crate::config::AppletConfig;
use crate::{bing, schedule, thumbs, wallpaper};

/// One name everywhere: cosmic-config app ID, state dir, desktop entry.
pub const APP_ID: &str = "io.github.ercling.CosmicBingWallpaper";

/// Symbolic icon shown in the panel.
const PANEL_ICON: &str = "preferences-desktop-wallpaper-symbolic";

pub fn run() -> cosmic::iced::Result {
    cosmic::applet::run::<Window>(())
}

/// The applet's state dir (`~/.local/state/<APP_ID>/`): catalogue JSON +
/// cached thumbnails.
pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".local")
                .join("state")
        })
        .join(APP_ID)
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
    /// Cold start: the catalogue was empty at startup and no fetch has
    /// succeeded yet — the first successful fetch auto-applies
    /// unconditionally.
    first_fetch_pending: bool,
    /// Generation counter for the one-shot refresh timer; `RefreshDue`
    /// messages carrying a stale generation are ignored.
    timer_generation: u64,
    /// Generation counter for the one-shot shuffle timer (same stale-tick
    /// scheme as the refresh timer).
    shuffle_generation: u64,
    /// A shuffle tick is currently scheduled.
    shuffle_armed: bool,
    /// The last user action that resets the shuffle countdown (enabling
    /// shuffle, changing the interval, manual navigation). Cleared after
    /// each tick so the following cycle waits one full interval again.
    last_user_action: Option<Instant>,
    /// When the last successful fetch completed (status footer).
    last_updated: Option<DateTime<Utc>>,
    /// The last fetch error, cleared on success (status footer).
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(window::Id),
    /// Settings changed on disk (external edit or our own write echoed back).
    ConfigUpdated(AppletConfig),
    /// The refresh timer fired (payload: the generation it was armed with).
    RefreshDue(u64),
    /// The fetch pipeline finished.
    RefreshFinished(Result<Catalogue, String>),
    /// Apply this downloaded file as the wallpaper (prev/next/newest
    /// buttons — browsing applies immediately).
    ApplyImage(PathBuf),
    /// The popup's refresh button (debounced while a fetch is pending).
    RefreshNow,
    /// Open a file or URL with the default handler (`xdg-open`): thumbnail
    /// click → full image, "About this image" → copyright link.
    Open(String),
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
    fn schedule_refresh(&mut self, delay: Duration) -> app::Task<Message> {
        self.timer_generation += 1;
        let generation = self.timer_generation;
        tracing::info!("next refresh in {}s", delay.as_secs());
        cosmic::task::future(async move {
            tokio::time::sleep(delay).await;
            Message::RefreshDue(generation)
        })
    }

    /// Arm (or re-arm) the one-shot shuffle timer, invalidating any pending
    /// tick. The delay counts down from the last user action.
    fn arm_shuffle(&mut self) -> app::Task<Message> {
        self.shuffle_generation += 1;
        self.shuffle_armed = true;
        let generation = self.shuffle_generation;
        let delay = schedule::next_shuffle_delay(
            self.config.shuffle_interval_secs,
            self.last_user_action,
            Instant::now(),
        );
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
        // Refresh our idea of what is applied (cheap config read), so prune
        // protects the right file even after external wallpaper changes.
        if let Some(live) = wallpaper::current_source() {
            self.current = Some(live);
        }
        self.catalogue.prune(
            self.config.retention_days,
            self.current.as_deref(),
            Utc::now(),
        );
        if let Err(error) = self.catalogue.save(&catalogue_path()) {
            // Non-fatal: the catalogue is rebuildable from the folder scan.
            tracing::warn!("failed to persist catalogue after prune: {error}");
        }
        // Pruning may leave fewer than two images — disarm shuffle if so.
        self.sync_shuffle(false)
    }

    /// Kick off the fetch pipeline unless one is already running.
    fn start_refresh(&mut self) -> app::Task<Message> {
        if self.refresh_pending {
            return Task::none();
        }
        self.refresh_pending = true;
        // Refresh our idea of what is applied (cheap config read), so prune
        // protects the right file even after external wallpaper changes.
        if let Some(live) = wallpaper::current_source() {
            self.current = Some(live);
        }
        let catalogue = self.catalogue.clone();
        let retention_days = self.config.retention_days;
        let currently_applied = self.current.clone();
        cosmic::task::future(async move {
            Message::RefreshFinished(
                run_refresh(catalogue, retention_days, currently_applied).await,
            )
        })
    }

    /// Status footer text.
    pub(crate) fn status_line(&self) -> String {
        if self.refresh_pending {
            "Checking for new images…".to_owned()
        } else if self.last_error.is_some() {
            "Bing unreachable — retrying in 1 h".to_owned()
        } else if let Some(updated) = self.last_updated {
            crate::view::format_updated(
                updated.with_timezone(&chrono::Local).naive_local(),
                chrono::Local::now().naive_local(),
            )
        } else if self.catalogue.images.is_empty() {
            "No images yet — fetching…".to_owned()
        } else {
            // Restored from the catalogue; no fetch has completed yet this
            // session.
            "Up to date".to_owned()
        }
    }
}

/// The whole refresh pipeline, run off the UI thread: fetch the image list,
/// download what's missing (+ thumbnails), merge, prune, persist. Any
/// HTTP/parse failure aborts with an error string (→ 1 h retry); files
/// downloaded before the failure stay on disk and are skipped next time.
async fn run_refresh(
    mut catalogue: Catalogue,
    retention_days: u16,
    currently_applied: Option<PathBuf>,
) -> Result<Catalogue, String> {
    let download_dir = wallpaper::download_dir();
    let state = state_dir();

    let client = bing::http_client().map_err(|e| e.to_string())?;
    let archive = bing::fetch_image_list(&client, schedule::fetch_count(retention_days))
        .await
        .map_err(|e| e.to_string())?;

    let mut fetched = Vec::with_capacity(archive.images.len());
    for image in &archive.images {
        let path = bing::download_image(&client, image, &download_dir)
            .await
            .map_err(|e| e.to_string())?;
        // A failed thumbnail is not fatal: `ensure_thumbnail` regenerates
        // missing thumbs on the next refresh.
        if let Err(error) = thumbs::ensure_thumbnail(&path, &state) {
            tracing::warn!(
                "thumbnail generation failed for {}: {error}",
                path.display()
            );
        }
        fetched.push(ImageEntry::from_bing(image, path));
    }

    catalogue.merge(fetched);
    catalogue.prune(retention_days, currently_applied.as_deref(), Utc::now());
    if let Err(error) = catalogue.save(&catalogue_path()) {
        // Non-fatal: the catalogue is rebuildable from the folder scan.
        tracing::warn!("failed to persist catalogue: {error}");
    }
    Ok(catalogue)
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
        // missing catalogue rebuilds from the download folder scan.
        let catalogue = Catalogue::load_or_rebuild(&catalogue_path(), &wallpaper::download_dir());
        let current = wallpaper::current_source();
        let first_fetch_pending = catalogue.images.is_empty();

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
            first_fetch_pending,
            timer_generation: 0,
            shuffle_generation: 0,
            shuffle_armed: false,
            last_user_action: None,
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
                // reduced retention prunes immediately.
                let shuffle_changed = config.shuffle_enabled != self.config.shuffle_enabled
                    || config.shuffle_interval_secs != self.config.shuffle_interval_secs;
                let newly_enabled = config.shuffle_enabled && !self.config.shuffle_enabled;
                let retention_reduced =
                    schedule::retention_reduced(self.config.retention_days, config.retention_days);
                self.config = config;
                if newly_enabled {
                    // First fire one full interval after enabling.
                    self.last_user_action = Some(Instant::now());
                }
                let mut tasks = Vec::new();
                if retention_reduced {
                    tasks.push(self.prune_immediately());
                }
                if shuffle_changed {
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
                // Manual navigation also resets the shuffle countdown.
                match wallpaper::apply(&path) {
                    Ok(()) => {
                        self.current = Some(path);
                        self.last_user_action = Some(Instant::now());
                        return self.sync_shuffle(true);
                    }
                    Err(error) => {
                        tracing::warn!("failed to apply {}: {error}", path.display());
                    }
                }
            }
            Message::Open(target) => {
                // Detached viewer/browser; a thread reaps the child so no
                // zombie lingers per click.
                match std::process::Command::new("xdg-open").arg(&target).spawn() {
                    Ok(mut child) => {
                        std::thread::spawn(move || {
                            let _ = child.wait();
                        });
                    }
                    Err(error) => tracing::warn!("xdg-open {target} failed: {error}"),
                }
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
                if let Some(pick) = self.catalogue.random_other(self.current.as_deref()) {
                    let path = pick.filename.clone();
                    match wallpaper::apply(&path) {
                        Ok(()) => self.current = Some(path),
                        Err(error) => {
                            tracing::warn!("shuffle failed to apply {}: {error}", path.display());
                        }
                    }
                }
                // A tick starts a fresh cycle: the next one comes a full
                // interval from now, not from the last user action.
                self.last_user_action = None;
                return self.sync_shuffle(false);
            }
            Message::SetShuffleEnabled(enabled) => {
                let mut config = self.config.clone();
                config.shuffle_enabled = enabled;
                self.set_config(config);
                if enabled {
                    // First fire one full interval after enabling.
                    self.last_user_action = Some(Instant::now());
                }
                return self.sync_shuffle(true);
            }
            Message::SetShuffleInterval(index) => {
                let mut config = self.config.clone();
                config.shuffle_interval_secs = crate::view::shuffle_interval_secs(index);
                self.set_config(config);
                // Picking an interval restarts the countdown at that length.
                self.last_user_action = Some(Instant::now());
                return self.sync_shuffle(true);
            }
            Message::SetRetention(index) => {
                let new_days = crate::view::retention_days(index);
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
            Message::RefreshFinished(result) => {
                self.refresh_pending = false;
                match result {
                    Ok(catalogue) => {
                        self.catalogue = catalogue;
                        self.last_updated = Some(Utc::now());
                        self.last_error = None;

                        // Auto-apply per the "don't clobber" rule, judged
                        // against what is applied *right now*.
                        let live = wallpaper::current_source();
                        if let Some(live) = &live {
                            self.current = Some(live.clone());
                        }
                        if let Some(newest) = self.catalogue.newest()
                            && schedule::should_auto_apply(
                                self.first_fetch_pending,
                                live.as_deref(),
                            )
                        {
                            match wallpaper::apply(&newest.filename) {
                                Ok(()) => self.current = Some(newest.filename.clone()),
                                Err(error) => {
                                    tracing::warn!("failed to apply wallpaper: {error}");
                                }
                            }
                        }
                        self.first_fetch_pending = false;

                        let delay = schedule::next_refresh(
                            self.catalogue.newest().map(|e| e.fullstartdate.as_str()),
                            Utc::now(),
                        );
                        let refresh_timer = self.schedule_refresh(delay);
                        // A grown catalogue may unlock a waiting shuffle
                        // (≥2 images); a pending tick keeps its countdown.
                        let shuffle = self.sync_shuffle(false);
                        return Task::batch([refresh_timer, shuffle]);
                    }
                    Err(error) => {
                        tracing::warn!("refresh failed: {error}");
                        self.last_error = Some(error);
                        return self.schedule_refresh(schedule::ERROR_RETRY_DELAY);
                    }
                }
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

    fn view(&self) -> Element<'_, Self::Message> {
        self.core
            .applet
            .icon_button(PANEL_ICON)
            .on_press_down(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, id: window::Id) -> Element<'_, Self::Message> {
        if matches!(self.popup, Some(popup_id) if popup_id == id) {
            crate::view::popup_view(self)
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
        let state = state_dir();
        assert!(state.ends_with(APP_ID));
        assert!(state.is_absolute());
        assert_eq!(state_dir().join("catalogue.json"), catalogue_path());
    }

    #[test]
    fn status_line_reflects_the_fetch_lifecycle() {
        let mut window = Window::default();

        // Fresh cold start: nothing on disk, nothing fetched yet.
        assert_eq!(window.status_line(), "No images yet — fetching…");

        // Pipeline running.
        window.refresh_pending = true;
        assert_eq!(window.status_line(), "Checking for new images…");
        window.refresh_pending = false;

        // Fetch failed → the plan's exact error footer.
        window.last_error = Some("boom".to_owned());
        assert_eq!(window.status_line(), "Bing unreachable — retrying in 1 h");

        // Success clears the error and records the time (relative wording
        // itself is covered by `view::format_updated`'s tests).
        window.last_error = None;
        window.last_updated = Some(Utc::now());
        assert!(window.status_line().starts_with("Updated today at "));
    }

    #[test]
    fn status_line_restored_catalogue_without_fetch_is_up_to_date() {
        let mut window = Window::default();
        window.catalogue.images.push(ImageEntry {
            urlbase: "/th?id=OHR.Foo_ROW1".to_owned(),
            startdate: "20260807".to_owned(),
            fullstartdate: "202608070700".to_owned(),
            title: "Foo".to_owned(),
            copyright: "© Bar".to_owned(),
            copyrightlink: String::new(),
            filename: PathBuf::from("/x/20260807-Foo_ROW1_UHD.jpg"),
        });
        assert_eq!(window.status_line(), "Up to date");
    }
}
