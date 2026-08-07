// Minimal COSMIC applet skeleton: a panel icon button that toggles an
// (empty, placeholder) popup. Patterned on `cosmic-applet-power` from
// pop-os/cosmic-applets @ ec8ffdc (current popup idiom:
// `cosmic::surface::action::app_popup` / `destroy_popup` via `surface_task`).

use cosmic::{
    Element, Task, app, cosmic_config,
    iced::{self, window},
    widget,
};

use crate::config::AppletConfig;

/// One name everywhere: cosmic-config app ID, state dir, desktop entry.
pub const APP_ID: &str = "io.github.ercling.CosmicBingWallpaper";

/// Symbolic icon shown in the panel.
const PANEL_ICON: &str = "preferences-desktop-wallpaper-symbolic";

pub fn run() -> cosmic::iced::Result {
    cosmic::applet::run::<Window>(())
}

#[derive(Default)]
pub struct Window {
    core: cosmic::app::Core,
    popup: Option<window::Id>,
    /// Applet settings (shuffle, retention). Defaults when the config context
    /// is unavailable.
    config: AppletConfig,
    /// cosmic-config context used to persist setting changes. `None` only if
    /// the config directory could not be created — the applet still runs with
    /// defaults, changes just don't persist.
    config_context: Option<cosmic_config::Config>,
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(window::Id),
    /// Settings changed on disk (external edit or our own write echoed back).
    ConfigUpdated(AppletConfig),
}

impl Window {
    /// Write-on-change: adopt `config` and persist it if it differs from the
    /// current settings. Used by the shuffle/retention controls (Tasks 9-10).
    #[allow(dead_code)]
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

        (
            Self {
                core,
                popup: None,
                config,
                config_context,
            },
            Task::none(),
        )
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
                self.config = config;
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
            let content = widget::text::body("Hello").center();
            self.core.applet.popup_container(content).into()
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
}
