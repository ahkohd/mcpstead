use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use axum::{
    Router,
    routing::{get, post},
};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod constants;
mod downstream;
mod metrics;
mod state;
mod upstream;

use config::{load_config, load_mcp_bearer_token, validate_config};
use downstream::{delete_mcp, get_mcp, post_mcp};
use metrics::{health, metrics};
use state::{
    AppState, build_state, prune_idle_sessions, stop_accepting_new_sessions, terminate_all_sessions,
};
use upstream::{manage_server, server_names};

#[derive(Parser, Debug)]
struct Args {
    #[arg(
        short,
        long,
        env = "MCPSTEAD_CONFIG",
        default_value = "config/mcpstead.yaml"
    )]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = load_config(&args.config).await?;
    init_logging(&config.logging.level);
    validate_config(&config)?;

    let state = build_state(&config, load_mcp_bearer_token(&config)?)?;

    for name in server_names(&state).await {
        let state = state.clone();
        tokio::spawn(async move {
            manage_server(state, name).await;
        });
    }

    let shutdown_token = CancellationToken::new();
    tokio::spawn(session_gc_task(state.clone(), shutdown_token.clone()));

    let app = build_app(state.clone());

    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .with_context(|| format!("invalid bind address '{}:{}'", config.host, config.port))?;

    info!(%addr, "starting mcpstead");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state, shutdown_token))
        .await?;
    Ok(())
}

fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/mcp", post(post_mcp).get(get_mcp).delete(delete_mcp))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn init_logging(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}

async fn session_gc_task(state: AppState, shutdown_token: CancellationToken) {
    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => break,
            _ = tokio::time::sleep(state.session_gc_interval) => {
                prune_idle_sessions(&state).await;
            }
        }
    }
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(err) => {
            error!(%err, "failed to install sigterm handler");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };

    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(err) = result {
                error!(%err, "failed to install ctrl-c handler");
            }
        }
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        error!(%err, "failed to install ctrl-c handler");
    }
}

async fn shutdown_signal(state: AppState, shutdown_token: CancellationToken) {
    wait_for_shutdown_signal().await;
    shutdown_token.cancel();
    stop_accepting_new_sessions(&state);
    let sweep = terminate_all_sessions(&state, "gateway_shutdown");
    if tokio::time::timeout(state.session_shutdown_grace, sweep)
        .await
        .is_err()
    {
        error!(timeout = ?state.session_shutdown_grace, "session shutdown sweep timed out");
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::{Config, McpConfig, McpSessionConfig};

    #[tokio::test]
    async fn session_gc_task_stops_on_cancel() {
        let config = Config {
            mcp: McpConfig {
                session: McpSessionConfig {
                    idle_ttl_seconds: 60,
                    gc_interval_seconds: 60,
                    shutdown_grace_seconds: 1,
                },
                ..McpConfig::default()
            },
            ..Config::default()
        };
        let state = build_state(&config, None).unwrap();
        let token = CancellationToken::new();
        let handle = tokio::spawn(session_gc_task(state, token.clone()));

        token.cancel();

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("gc task should stop")
            .expect("gc task should not panic");
    }
}
