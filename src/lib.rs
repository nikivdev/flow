pub mod cli;
pub mod config;
pub mod doctor;
pub mod hub;
pub mod indexer;
pub mod logs;
pub mod palette;
pub mod screen;
pub mod secrets;
pub mod server;
pub mod servers;
pub mod servers_tui;
pub mod setup;
pub mod tasks;
pub mod terminal;
pub mod trace;
pub mod watchers;

/// Initialize tracing with a default filter if `RUST_LOG` is unset.
pub fn init_tracing() {
    let default_filter = "flowd=info,axum=warn,tower=warn";
    let filter_layer = std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_string());

    tracing_subscriber::fmt()
        .with_env_filter(filter_layer)
        .with_target(false)
        .compact()
        .init();
}
