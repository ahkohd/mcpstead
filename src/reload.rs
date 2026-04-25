use std::{collections::BTreeMap, sync::atomic::Ordering};

use anyhow::Result;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tracing::{error, info, warn};

use crate::{
    config::{Config, ServerConfig, load_config, load_mcp_bearer_token, validate_config},
    downstream::{AuthDecision, authorize_mcp, record_auth_decision, unauthorized_response},
    state::{AppState, ManagedServer, McpAuthState},
    upstream::{abort_server_manager, drop_upstream_session_reset_lock, spawn_server_manager},
};

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub(crate) struct ReloadSummary {
    added: usize,
    removed: usize,
    restarted: usize,
    updated: usize,
    unchanged: usize,
}

pub(crate) async fn reload_http(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let auth = authorize_mcp(&headers, &state).await;
    record_auth_decision(&state, auth).await;
    if let AuthDecision::Deny(_) = auth {
        return unauthorized_response();
    }

    match reload_config(&state).await {
        Ok(summary) => (StatusCode::OK, Json(summary)).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    }
}

pub(crate) async fn reload_config(state: &AppState) -> Result<ReloadSummary> {
    let _reload_guard = state.reload_lock.lock().await;
    let result = reload_config_inner(state).await;
    match &result {
        Ok(_) => record_reload_result(state, "success").await,
        Err(err) => {
            error!(error = %err, "config reload failed; keeping current config");
            record_reload_result(state, "error").await;
        }
    }
    result
}

async fn reload_config_inner(state: &AppState) -> Result<ReloadSummary> {
    let loaded = load_config(state.config_path.as_ref()).await?;
    validate_config(&loaded)?;
    let bearer_token = load_mcp_bearer_token(&loaded)?;

    let old_config = state.live_config.read().await.clone();
    let new_config = reloadable_config(&old_config, loaded);
    let summary = apply_upstream_diff(state, &old_config, &new_config).await;

    *state.live_config.write().await = new_config.clone();
    *state.mcp_auth.write().await = McpAuthState {
        mode: new_config.mcp.auth.mode,
        bearer_token,
    };
    state
        .metrics_enabled
        .store(new_config.metrics.enabled, Ordering::Relaxed);

    info!(
        added = summary.added,
        removed = summary.removed,
        restarted = summary.restarted,
        updated = summary.updated,
        unchanged = summary.unchanged,
        "config reloaded"
    );
    Ok(summary)
}

fn reloadable_config(old: &Config, mut new: Config) -> Config {
    if old.host != new.host || old.port != new.port {
        warn!(
            old_host = %old.host,
            old_port = old.port,
            new_host = %new.host,
            new_port = new.port,
            "config reload ignored listen address change; restart required"
        );
        new.host.clone_from(&old.host);
        new.port = old.port;
    }
    if old.logging != new.logging {
        warn!("config reload ignored logging change; restart required");
        new.logging = old.logging.clone();
    }
    if old.mcp.session != new.mcp.session {
        warn!("config reload ignored mcp.session change; restart required");
        new.mcp.session = old.mcp.session.clone();
    }
    new
}

async fn apply_upstream_diff(
    state: &AppState,
    old_config: &Config,
    new_config: &Config,
) -> ReloadSummary {
    let old_servers = servers_by_name(old_config);
    let new_servers = servers_by_name(new_config);
    let mut summary = ReloadSummary::default();

    for name in old_servers.keys() {
        if !new_servers.contains_key(name) {
            abort_server_manager(state, name).await;
            state.registry.write().await.servers.remove(name);
            drop_upstream_session_reset_lock(state, name).await;
            summary.removed += 1;
        }
    }

    for (name, new_server) in &new_servers {
        match old_servers.get(name) {
            None => {
                state
                    .registry
                    .write()
                    .await
                    .servers
                    .insert(name.clone(), ManagedServer::new(new_server.clone()));
                spawn_server_manager(state, name.clone()).await;
                summary.added += 1;
            }
            Some(old_server) if old_server == new_server => {
                summary.unchanged += 1;
            }
            Some(old_server) if old_server.requires_fresh_upstream_session(new_server) => {
                abort_server_manager(state, name).await;
                state
                    .registry
                    .write()
                    .await
                    .servers
                    .insert(name.clone(), ManagedServer::new(new_server.clone()));
                drop_upstream_session_reset_lock(state, name).await;
                spawn_server_manager(state, name.clone()).await;
                summary.restarted += 1;
            }
            Some(_) => {
                if let Some(server) = state.registry.write().await.servers.get_mut(name) {
                    server.config = new_server.clone();
                }
                summary.updated += 1;
            }
        }
    }

    summary
}

fn servers_by_name(config: &Config) -> BTreeMap<String, ServerConfig> {
    config
        .servers
        .iter()
        .cloned()
        .map(|server| (server.name.clone(), server))
        .collect()
}

async fn record_reload_result(state: &AppState, result: &str) {
    state.metrics.lock().await.record_config_reload(result);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{
        fs,
        path::PathBuf,
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        config::{ReconnectConfig, ToolsConfig},
        state::build_state_with_config_path,
    };

    fn temp_config_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mcpstead-reload-{name}-{}-{stamp}.yaml",
            std::process::id()
        ))
    }

    fn config_with_servers(servers: Vec<ServerConfig>) -> Config {
        Config {
            servers,
            ..Config::default()
        }
    }

    fn server(name: &str, url: &str) -> ServerConfig {
        let mut server = ServerConfig::default();
        server.name = name.to_string();
        server.url = url.to_string();
        server
    }

    fn write_config(path: &PathBuf, config: &str) {
        fs::write(path, config).unwrap();
    }

    #[tokio::test]
    async fn reload_parse_error_keeps_running_config() {
        let path = temp_config_path("bad");
        write_config(&path, "not: [valid");
        let initial = config_with_servers(vec![server("a", "http://127.0.0.1:1")]);
        let state = build_state_with_config_path(&initial, None, path).unwrap();

        let err = reload_config(&state).await.unwrap_err();

        assert!(err.to_string().contains("parse config"));
        assert_eq!(state.live_config.read().await.servers[0].name, "a");
        assert_eq!(
            state.metrics.lock().await.config_reloads.get("error"),
            Some(&1)
        );
    }

    #[tokio::test]
    async fn reload_identical_config_is_noop() {
        let path = temp_config_path("same");
        write_config(
            &path,
            r#"
servers:
  - name: a
    url: http://127.0.0.1:1
"#,
        );
        let initial = config_with_servers(vec![server("a", "http://127.0.0.1:1")]);
        let state = build_state_with_config_path(&initial, None, path).unwrap();

        let summary = reload_config(&state).await.unwrap();

        assert_eq!(summary.unchanged, 1);
        assert_eq!(summary.added, 0);
        assert_eq!(
            state.metrics.lock().await.config_reloads.get("success"),
            Some(&1)
        );
    }

    #[tokio::test]
    async fn reload_adds_upstream_and_spawns_task() {
        let path = temp_config_path("add");
        write_config(
            &path,
            r#"
servers:
  - name: a
    url: http://127.0.0.1:1
  - name: b
    url: http://127.0.0.1:2
"#,
        );
        let initial = config_with_servers(vec![server("a", "http://127.0.0.1:1")]);
        let state = build_state_with_config_path(&initial, None, path).unwrap();

        let summary = reload_config(&state).await.unwrap();

        assert_eq!(summary.added, 1);
        assert_eq!(summary.unchanged, 1);
        assert!(state.registry.read().await.servers.contains_key("b"));
        assert!(state.upstream_tasks.lock().await.contains_key("b"));
        abort_server_manager(&state, "b").await;
    }

    #[tokio::test]
    async fn reload_removes_upstream_task_registry_and_reset_lock() {
        let path = temp_config_path("remove");
        write_config(
            &path,
            r#"
servers:
  - name: a
    url: http://127.0.0.1:1
"#,
        );
        let initial = config_with_servers(vec![
            server("a", "http://127.0.0.1:1"),
            server("b", "http://127.0.0.1:2"),
        ]);
        let state = build_state_with_config_path(&initial, None, path).unwrap();
        state.upstream_tasks.lock().await.insert(
            "b".to_string(),
            tokio::spawn(async { std::future::pending::<()>().await }),
        );
        state
            .upstream_session_reset_locks
            .lock()
            .await
            .insert("b".to_string(), Arc::new(tokio::sync::Mutex::new(())));

        let summary = reload_config(&state).await.unwrap();

        assert_eq!(summary.removed, 1);
        assert_eq!(summary.unchanged, 1);
        assert!(!state.registry.read().await.servers.contains_key("b"));
        assert!(!state.upstream_tasks.lock().await.contains_key("b"));
        assert!(
            !state
                .upstream_session_reset_locks
                .lock()
                .await
                .contains_key("b")
        );
    }

    #[tokio::test]
    async fn reload_hard_changes_restart_upstream() {
        let path = temp_config_path("hard");
        write_config(
            &path,
            r#"
servers:
  - name: a
    url: http://127.0.0.1:2
"#,
        );
        let initial = config_with_servers(vec![server("a", "http://127.0.0.1:1")]);
        let state = build_state_with_config_path(&initial, None, path).unwrap();
        {
            let mut registry = state.registry.write().await;
            let managed = registry.servers.get_mut("a").unwrap();
            managed.connected = true;
            managed.session_id = Some("old-session".to_string());
            managed.tools = vec![serde_json::json!({"name": "old"})];
        }
        state
            .upstream_session_reset_locks
            .lock()
            .await
            .insert("a".to_string(), Arc::new(tokio::sync::Mutex::new(())));

        let summary = reload_config(&state).await.unwrap();

        assert_eq!(summary.restarted, 1);
        let registry = state.registry.read().await;
        let managed = &registry.servers["a"];
        assert_eq!(managed.config.url, "http://127.0.0.1:2");
        assert!(!managed.connected);
        assert!(managed.session_id.is_none());
        assert!(managed.tools.is_empty());
        drop(registry);
        assert!(state.upstream_tasks.lock().await.contains_key("a"));
        assert!(
            !state
                .upstream_session_reset_locks
                .lock()
                .await
                .contains_key("a")
        );
        abort_server_manager(&state, "a").await;
    }

    #[tokio::test]
    async fn reload_updates_soft_upstream_fields_in_place() {
        let path = temp_config_path("soft");
        write_config(
            &path,
            r#"
servers:
  - name: a
    url: http://127.0.0.1:1
    reconnect:
      backoff_max_ms: 42
    tools:
      ttl_seconds: 7
"#,
        );
        let mut initial_server = server("a", "http://127.0.0.1:1");
        initial_server.reconnect = ReconnectConfig::default();
        initial_server.tools = ToolsConfig::default();
        let initial = config_with_servers(vec![initial_server]);
        let state = build_state_with_config_path(&initial, None, path).unwrap();

        let summary = reload_config(&state).await.unwrap();

        assert_eq!(summary.updated, 1);
        let registry = state.registry.read().await;
        assert_eq!(registry.servers["a"].config.reconnect.backoff_max_ms, 42);
        assert_eq!(registry.servers["a"].config.tools.ttl_seconds, 7);
    }
}
