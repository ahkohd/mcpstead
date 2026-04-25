use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::Instant,
};

use anyhow::Result;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::config::{Config, McpAuthMode, ServerConfig};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) registry: Arc<RwLock<Registry>>,
    pub(crate) metrics: Arc<Mutex<Metrics>>,
    pub(crate) sessions: Arc<RwLock<HashSet<String>>>,
    pub(crate) client: Client,
    pub(crate) insecure_client: Client,
    pub(crate) mcp_auth_mode: McpAuthMode,
    pub(crate) mcp_bearer_token: Option<String>,
    pub(crate) start: Instant,
    pub(crate) metrics_enabled: bool,
}

#[derive(Default)]
pub(crate) struct Registry {
    pub(crate) servers: BTreeMap<String, ManagedServer>,
}

pub(crate) struct ManagedServer {
    pub(crate) config: ServerConfig,
    pub(crate) connected: bool,
    pub(crate) last_error: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) tools: Vec<Value>,
    pub(crate) last_seen: Option<Instant>,
    pub(crate) reconnects: u64,
}

impl ManagedServer {
    pub(crate) fn new(config: ServerConfig) -> Self {
        Self {
            config,
            connected: false,
            last_error: None,
            session_id: None,
            tools: Vec::new(),
            last_seen: None,
            reconnects: 0,
        }
    }
}

#[derive(Default)]
pub(crate) struct Metrics {
    pub(crate) tool_calls: BTreeMap<(String, String), ToolCallMetric>,
}

#[derive(Default)]
pub(crate) struct ToolCallMetric {
    pub(crate) count: u64,
    pub(crate) errors: u64,
    pub(crate) duration_sum_seconds: f64,
}

pub(crate) fn build_state(config: &Config, mcp_bearer_token: Option<String>) -> Result<AppState> {
    let registry = Registry {
        servers: config
            .servers
            .iter()
            .cloned()
            .map(|server| (server.name.clone(), ManagedServer::new(server)))
            .collect(),
    };

    Ok(AppState {
        registry: Arc::new(RwLock::new(registry)),
        metrics: Arc::new(Mutex::new(Metrics::default())),
        sessions: Arc::new(RwLock::new(HashSet::new())),
        client: Client::builder()
            .user_agent(format!("mcpstead/{}", env!("CARGO_PKG_VERSION")))
            .build()?,
        insecure_client: Client::builder()
            .user_agent(format!("mcpstead/{}", env!("CARGO_PKG_VERSION")))
            .danger_accept_invalid_certs(true)
            .build()?,
        mcp_auth_mode: config.mcp.auth.mode,
        mcp_bearer_token,
        start: Instant::now(),
        metrics_enabled: config.metrics.enabled,
    })
}
