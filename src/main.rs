mod app;
// Consumed by the fetch pipeline from Task 7 onwards; until then only tests use them.
#[allow(dead_code)]
mod bing;
#[allow(dead_code)]
mod catalogue;
// dead_code: the derive-generated per-field setters are called from the
// shuffle/retention UI (Tasks 9-10).
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod thumbs;
#[allow(dead_code)]
mod wallpaper;

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tracing::info!("starting {} {}", app::APP_ID, env!("CARGO_PKG_VERSION"));

    app::run()
}
