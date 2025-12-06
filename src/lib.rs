pub mod cli;
pub mod config;
pub mod db;
pub mod discover;
pub mod doctor;
pub mod flox;
pub mod history;
pub mod hub;
pub mod init;
pub mod lin_runtime;
pub mod log_server;
pub mod log_store;
pub mod palette;
pub mod processes;
pub mod projects;
pub mod running;
pub mod tasks;
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
