mod app;
// Consumed by the fetch pipeline from Task 3 onwards; until then only tests use it.
#[allow(dead_code)]
mod bing;

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tracing::info!("starting {} {}", app::APP_ID, env!("CARGO_PKG_VERSION"));

    app::run()
}
