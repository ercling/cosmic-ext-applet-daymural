mod accent;
mod app;
mod bing;
mod catalogue;
mod config;
mod fsutil;
mod localize;
mod schedule;
#[cfg(test)]
mod testutil;
mod thumbs;
mod tooltip;
mod view;
mod wallpaper;

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tracing::info!("starting {} {}", app::APP_ID, env!("CARGO_PKG_VERSION"));

    // Must happen before the first `fl!` in the view: it layers the desktop's
    // languages onto the `en` fallback (see `localize.rs` for why nothing
    // else in the crate calls it).
    localize::localize();

    app::run()
}
