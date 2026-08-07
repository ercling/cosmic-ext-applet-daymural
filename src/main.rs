mod app;
mod bing;
mod catalogue;
// dead_code: the CosmicConfigEntry derive generates per-field
// `set_<field>` setters the applet never calls (settings persist through
// `write_entry` in `Window::set_config`); they are exercised only by
// config.rs's tests. The allow must sit here on the module — the derive
// output cannot be annotated more narrowly.
#[allow(dead_code)]
mod config;
mod fsutil;
mod schedule;
#[cfg(test)]
mod testutil;
mod thumbs;
mod view;
mod wallpaper;

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tracing::info!("starting {} {}", app::APP_ID, env!("CARGO_PKG_VERSION"));

    app::run()
}
