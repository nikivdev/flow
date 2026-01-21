pub mod agent_setup;
pub mod agents;
pub mod ai;
pub mod ai_context;
pub mod archive;
pub mod auth;
pub mod changes;
pub mod cli;
pub mod code;
pub mod commit;
pub mod commits;
pub mod config;
pub mod daemon;
pub mod db;
pub mod deploy;
pub mod deploy_setup;
pub mod deps;
pub mod discover;
pub mod docs;
pub mod doctor;
pub mod env;
pub mod env_setup;
pub mod ext;
pub mod fixup;
pub mod flox;
pub mod gh_release;
pub mod help_search;
pub mod history;
pub mod health;
pub mod home;
pub mod hub;
pub mod info;
pub mod init;
pub mod install;
pub mod jazz_state;
pub mod latest;
pub mod lmstudio;
pub mod log_server;
pub mod log_store;
pub mod notify;
pub mod palette;
pub mod parallel;
pub mod processes;
pub mod projects;
pub mod publish;
pub mod registry;
pub mod release;
pub mod repos;
pub mod running;
pub mod services;
pub mod setup;
pub mod skills;
pub mod ssh;
pub mod ssh_keys;
pub mod start;
pub mod storage;
pub mod supervisor;
pub mod sync;
pub mod task_match;
pub mod tasks;
pub mod todo;
pub mod tools;
pub mod traces;
pub mod upgrade;
pub mod upstream;
pub mod vcs;
pub mod watchers;
pub mod web;

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
