use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use axum::{
    Router,
    routing::{get, post},
};
use clap::Parser;
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
use state::{AppState, build_state};
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

    let app = build_app(state);

    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .with_context(|| format!("invalid bind address '{}:{}'", config.host, config.port))?;

    info!(%addr, "starting mcpstead");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
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

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        error!(%err, "failed to install ctrl-c handler");
    }
}
