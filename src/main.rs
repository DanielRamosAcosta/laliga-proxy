mod config;
mod detector;
mod health;
mod proxy;
mod state;

use std::sync::Arc;

use anyhow::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, LogFormat};
use crate::state::SharedState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::from_env().context("invalid configuration")?;
    init_tracing(&cfg);

    let cfg = Arc::new(cfg);
    let state = Arc::new(SharedState::new());

    tracing::info!(
        event = "startup",
        version = env!("CARGO_PKG_VERSION"),
        listen = %cfg.listen,
        health = %cfg.health_listen,
        upstream = %cfg.upstream,
        mode = cfg.mode_name(),
        "laliga-proxy starting"
    );

    // Background detector: keeps the shared routing state up to date.
    {
        let cfg = cfg.clone();
        let state = state.clone();
        tokio::spawn(async move { detector::run(cfg, state).await });
    }

    // Health/status endpoint.
    {
        let cfg = cfg.clone();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = health::run(cfg, state).await {
                tracing::error!(event = "health_error", error = %e, "health server stopped");
            }
        });
    }

    proxy::run(cfg, state).await
}

fn init_tracing(cfg: &Config) {
    // Filter precedence: LALIGA_PROXY_LOG_LEVEL, then RUST_LOG, then "info".
    let filter = std::env::var("LALIGA_PROXY_LOG_LEVEL")
        .ok()
        .and_then(|value| EnvFilter::try_new(value).ok())
        .or_else(|| EnvFilter::try_from_default_env().ok())
        .unwrap_or_else(|| EnvFilter::new("info"));

    let registry = tracing_subscriber::registry().with(filter);
    match cfg.log_format {
        LogFormat::Json => registry
            .with(tracing_subscriber::fmt::layer().json().flatten_event(true))
            .init(),
        LogFormat::Pretty => registry.with(tracing_subscriber::fmt::layer()).init(),
    }
}
