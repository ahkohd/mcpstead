use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::config::{Config, McpAuthMode, ServerConfig};

pub(crate) const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];
pub(crate) const SESSION_DURATION_BUCKETS: &[f64] = &[
    1.0, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 21_600.0,
];

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) registry: Arc<RwLock<Registry>>,
    pub(crate) metrics: Arc<Mutex<Metrics>>,
    pub(crate) sessions: Arc<RwLock<HashMap<String, DownstreamSession>>>,
    pub(crate) upstream_session_reset_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    pub(crate) accepting_new_sessions: Arc<AtomicBool>,
    pub(crate) session_idle_ttl: Duration,
    pub(crate) session_gc_interval: Duration,
    pub(crate) session_shutdown_grace: Duration,
    pub(crate) client: Client,
    pub(crate) insecure_client: Client,
    pub(crate) mcp_auth_mode: McpAuthMode,
    pub(crate) mcp_bearer_token: Option<String>,
    pub(crate) start: Instant,
    pub(crate) start_unix_seconds: f64,
    pub(crate) metrics_enabled: bool,
}

#[derive(Clone)]
pub(crate) struct DownstreamSession {
    pub(crate) created_at: Instant,
    pub(crate) last_activity: Instant,
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
    pub(crate) mcp_requests: BTreeMap<(String, String), u64>,
    pub(crate) mcp_request_duration: BTreeMap<String, HistogramMetric>,
    pub(crate) downstream_sessions_total: u64,
    pub(crate) downstream_session_duration: HistogramMetric,
    pub(crate) downstream_session_terminations: BTreeMap<String, u64>,
    pub(crate) upstream_initialize: BTreeMap<(String, String), u64>,
    pub(crate) upstream_initialize_duration: BTreeMap<String, HistogramMetric>,
    pub(crate) upstream_health_checks: BTreeMap<(String, String), u64>,
    pub(crate) upstream_reconnect_attempts: BTreeMap<(String, String), u64>,
    pub(crate) upstream_backoff_seconds_total: BTreeMap<String, f64>,
    pub(crate) upstream_in_backoff: BTreeMap<String, u64>,
    pub(crate) upstream_current_backoff_seconds: BTreeMap<String, f64>,
    pub(crate) upstream_session_resets: BTreeMap<(String, String), u64>,
    pub(crate) upstream_tools_refresh: BTreeMap<(String, String), u64>,
    pub(crate) upstream_tools_refresh_duration: BTreeMap<String, HistogramMetric>,
    pub(crate) upstream_tools_last_refresh_timestamp_seconds: BTreeMap<String, f64>,
    pub(crate) mcp_auth_attempts: BTreeMap<String, u64>,
    pub(crate) mcp_auth_failures: BTreeMap<String, u64>,
    pub(crate) upstream_bytes: BTreeMap<(String, String), u64>,
}

impl Metrics {
    pub(crate) fn record_tool_call(
        &mut self,
        server: &str,
        tool: &str,
        duration_seconds: f64,
        error_reason: Option<&str>,
    ) {
        let metric = self
            .tool_calls
            .entry((server.to_string(), tool.to_string()))
            .or_default();
        metric.count = metric.count.saturating_add(1);
        metric.duration.observe(duration_seconds, DURATION_BUCKETS);
        if let Some(reason) = error_reason {
            *metric
                .errors_by_reason
                .entry(reason.to_string())
                .or_insert(0) += 1;
        }
    }

    pub(crate) fn record_mcp_request(&mut self, method: &str, result: &str, duration_seconds: f64) {
        *self
            .mcp_requests
            .entry((method.to_string(), result.to_string()))
            .or_insert(0) += 1;
        self.mcp_request_duration
            .entry(method.to_string())
            .or_default()
            .observe(duration_seconds, DURATION_BUCKETS);
    }

    pub(crate) fn record_session_opened(&mut self) {
        self.downstream_sessions_total = self.downstream_sessions_total.saturating_add(1);
    }

    pub(crate) fn record_session_termination(&mut self, reason: &str, duration_seconds: f64) {
        *self
            .downstream_session_terminations
            .entry(reason.to_string())
            .or_insert(0) += 1;
        self.downstream_session_duration
            .observe(duration_seconds, SESSION_DURATION_BUCKETS);
    }

    pub(crate) fn record_upstream_initialize(
        &mut self,
        server: &str,
        result: &str,
        duration_seconds: f64,
    ) {
        *self
            .upstream_initialize
            .entry((server.to_string(), result.to_string()))
            .or_insert(0) += 1;
        self.upstream_initialize_duration
            .entry(server.to_string())
            .or_default()
            .observe(duration_seconds, DURATION_BUCKETS);
    }

    pub(crate) fn record_upstream_health_check(&mut self, server: &str, result: &str) {
        *self
            .upstream_health_checks
            .entry((server.to_string(), result.to_string()))
            .or_insert(0) += 1;
    }

    pub(crate) fn record_upstream_reconnect_attempt(&mut self, server: &str, result: &str) {
        *self
            .upstream_reconnect_attempts
            .entry((server.to_string(), result.to_string()))
            .or_insert(0) += 1;
    }

    pub(crate) fn set_upstream_backoff(&mut self, server: &str, seconds: f64) {
        self.upstream_in_backoff.insert(server.to_string(), 1);
        self.upstream_current_backoff_seconds
            .insert(server.to_string(), seconds);
    }

    pub(crate) fn record_upstream_backoff_complete(&mut self, server: &str, seconds: f64) {
        *self
            .upstream_backoff_seconds_total
            .entry(server.to_string())
            .or_insert(0.0) += seconds;
        self.upstream_in_backoff.insert(server.to_string(), 0);
        self.upstream_current_backoff_seconds
            .insert(server.to_string(), 0.0);
    }

    pub(crate) fn record_upstream_session_reset(&mut self, server: &str, reason: &str) {
        *self
            .upstream_session_resets
            .entry((server.to_string(), reason.to_string()))
            .or_insert(0) += 1;
    }

    pub(crate) fn record_upstream_tools_refresh(
        &mut self,
        server: &str,
        result: &str,
        duration_seconds: f64,
    ) {
        *self
            .upstream_tools_refresh
            .entry((server.to_string(), result.to_string()))
            .or_insert(0) += 1;
        self.upstream_tools_refresh_duration
            .entry(server.to_string())
            .or_default()
            .observe(duration_seconds, DURATION_BUCKETS);
        if result == "success" {
            self.upstream_tools_last_refresh_timestamp_seconds
                .insert(server.to_string(), unix_now_seconds());
        }
    }

    pub(crate) fn record_auth_success(&mut self) {
        *self
            .mcp_auth_attempts
            .entry("success".to_string())
            .or_insert(0) += 1;
    }

    pub(crate) fn record_auth_failure(&mut self, reason: &str) {
        *self
            .mcp_auth_attempts
            .entry("failure".to_string())
            .or_insert(0) += 1;
        *self
            .mcp_auth_failures
            .entry(reason.to_string())
            .or_insert(0) += 1;
    }

    pub(crate) fn record_upstream_bytes(&mut self, server: &str, direction: &str, bytes: u64) {
        *self
            .upstream_bytes
            .entry((server.to_string(), direction.to_string()))
            .or_insert(0) += bytes;
    }
}

#[derive(Default)]
pub(crate) struct ToolCallMetric {
    pub(crate) count: u64,
    pub(crate) errors_by_reason: BTreeMap<String, u64>,
    pub(crate) duration: HistogramMetric,
}

#[derive(Default)]
pub(crate) struct HistogramMetric {
    pub(crate) buckets: Vec<u64>,
    pub(crate) count: u64,
    pub(crate) sum: f64,
}

impl HistogramMetric {
    pub(crate) fn observe(&mut self, value: f64, buckets: &[f64]) {
        if self.buckets.len() != buckets.len() + 1 {
            self.buckets = vec![0; buckets.len() + 1];
        }
        for (index, bucket) in buckets.iter().enumerate() {
            if value <= *bucket {
                self.buckets[index] = self.buckets[index].saturating_add(1);
            }
        }
        let inf_index = buckets.len();
        self.buckets[inf_index] = self.buckets[inf_index].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.sum += value;
    }
}

pub(crate) fn unix_now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

pub(crate) fn accepting_new_sessions(state: &AppState) -> bool {
    state.accepting_new_sessions.load(Ordering::Relaxed)
}

pub(crate) fn stop_accepting_new_sessions(state: &AppState) {
    state.accepting_new_sessions.store(false, Ordering::Relaxed);
}

pub(crate) async fn touch_session(state: &AppState, session_id: &str) -> bool {
    let mut sessions = state.sessions.write().await;
    let Some(session) = sessions.get_mut(session_id) else {
        return false;
    };
    session.last_activity = Instant::now();
    true
}

pub(crate) async fn terminate_session(state: &AppState, session_id: &str, reason: &str) -> bool {
    let session = state.sessions.write().await.remove(session_id);
    let Some(session) = session else {
        return false;
    };
    state
        .metrics
        .lock()
        .await
        .record_session_termination(reason, session.created_at.elapsed().as_secs_f64());
    true
}

pub(crate) async fn prune_idle_sessions(state: &AppState) -> usize {
    let now = Instant::now();
    let mut sessions = state.sessions.write().await;
    let ids: Vec<String> = sessions
        .iter()
        .filter(|(_, session)| now.duration_since(session.last_activity) > state.session_idle_ttl)
        .map(|(id, _)| id.clone())
        .collect();
    if ids.is_empty() {
        return 0;
    }

    let mut metrics = state.metrics.lock().await;
    let mut count = 0;
    for id in ids {
        if let Some(session) = sessions.remove(&id) {
            metrics.record_session_termination(
                "idle_timeout",
                now.duration_since(session.created_at).as_secs_f64(),
            );
            count += 1;
        }
    }
    count
}

pub(crate) async fn terminate_all_sessions(state: &AppState, reason: &str) -> usize {
    let now = Instant::now();
    let sessions = {
        let mut sessions = state.sessions.write().await;
        std::mem::take(&mut *sessions)
    };
    let count = sessions.len();
    if count > 0 {
        let mut metrics = state.metrics.lock().await;
        for session in sessions.into_values() {
            metrics.record_session_termination(
                reason,
                now.duration_since(session.created_at).as_secs_f64(),
            );
        }
    }
    count
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
        sessions: Arc::new(RwLock::new(HashMap::new())),
        upstream_session_reset_locks: Arc::new(Mutex::new(HashMap::new())),
        accepting_new_sessions: Arc::new(AtomicBool::new(true)),
        session_idle_ttl: Duration::from_secs(config.mcp.session.idle_ttl_seconds.max(1)),
        session_gc_interval: Duration::from_secs(config.mcp.session.gc_interval_seconds.max(1)),
        session_shutdown_grace: Duration::from_secs(
            config.mcp.session.shutdown_grace_seconds.max(1),
        ),
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
        start_unix_seconds: unix_now_seconds(),
        metrics_enabled: config.metrics.enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::{Config, McpConfig, McpSessionConfig};

    fn test_state() -> AppState {
        let config = Config {
            mcp: McpConfig {
                session: McpSessionConfig {
                    idle_ttl_seconds: 1,
                    gc_interval_seconds: 1,
                    shutdown_grace_seconds: 1,
                },
                ..McpConfig::default()
            },
            ..Config::default()
        };
        build_state(&config, None).unwrap()
    }

    #[tokio::test]
    async fn idle_gc_evicts_stale_sessions() {
        let state = test_state();
        let now = Instant::now();
        state.sessions.write().await.insert(
            "stale".to_string(),
            DownstreamSession {
                created_at: now - Duration::from_secs(3),
                last_activity: now - Duration::from_secs(2),
            },
        );

        assert_eq!(prune_idle_sessions(&state).await, 1);
        assert!(state.sessions.read().await.is_empty());
        assert_eq!(
            state
                .metrics
                .lock()
                .await
                .downstream_session_terminations
                .get("idle_timeout"),
            Some(&1)
        );
    }

    #[tokio::test]
    async fn touch_session_prevents_idle_gc() {
        let state = test_state();
        let now = Instant::now();
        state.sessions.write().await.insert(
            "active".to_string(),
            DownstreamSession {
                created_at: now - Duration::from_secs(3),
                last_activity: now - Duration::from_secs(2),
            },
        );

        assert!(touch_session(&state, "active").await);
        assert_eq!(prune_idle_sessions(&state).await, 0);
        assert_eq!(state.sessions.read().await.len(), 1);
    }

    #[tokio::test]
    async fn shutdown_sweep_terminates_all_sessions() {
        let state = test_state();
        let now = Instant::now();
        for id in ["a", "b"] {
            state.sessions.write().await.insert(
                id.to_string(),
                DownstreamSession {
                    created_at: now - Duration::from_secs(2),
                    last_activity: now,
                },
            );
        }

        assert_eq!(terminate_all_sessions(&state, "gateway_shutdown").await, 2);
        assert!(state.sessions.read().await.is_empty());
        assert_eq!(
            state
                .metrics
                .lock()
                .await
                .downstream_session_terminations
                .get("gateway_shutdown"),
            Some(&2)
        );
    }
}
