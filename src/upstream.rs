use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use axum::http::{
    StatusCode,
    header::{ACCEPT, CONTENT_TYPE},
};
use reqwest::Client;
use serde_json::{Map, Value, json};
use tracing::{debug, error, warn};

use crate::config::{ReconnectConfig, ServerConfig};
use crate::constants::MCP_SESSION_ID;
use crate::state::AppState;

#[derive(Debug)]
struct UpstreamResponseError {
    message: String,
    session_reset_reason: Option<&'static str>,
}

impl std::fmt::Display for UpstreamResponseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UpstreamResponseError {}

fn session_reset_reason_from_error(error: &anyhow::Error) -> Option<&'static str> {
    error
        .downcast_ref::<UpstreamResponseError>()
        .and_then(|error| error.session_reset_reason)
}

pub(crate) async fn server_names(state: &AppState) -> Vec<String> {
    state
        .registry
        .read()
        .await
        .servers
        .keys()
        .cloned()
        .collect()
}

pub(crate) async fn manage_server(state: AppState, name: String) {
    let mut session_id: Option<String> = None;
    let mut failures = 0_u32;

    loop {
        let config = match get_server_config(&state, &name).await {
            Some(config) => config,
            None => return,
        };

        let result = if let Some(session) = session_id.as_deref() {
            load_tools(&state, &config, Some(session)).await
        } else {
            initialize_and_load_tools(&state, &config).await
        };

        match result {
            Ok((next_session_id, tools)) => {
                if failures > 0 {
                    record_upstream_reconnect_attempt(&state, &name, "success").await;
                }
                record_upstream_health_check(&state, &name, "success").await;
                failures = 0;
                if next_session_id.is_some() {
                    session_id = next_session_id;
                }
                set_server_connected(&state, &name, session_id.clone(), tools).await;
                debug!(server = %name, "upstream healthy");
                tokio::time::sleep(Duration::from_secs(config.tools.ttl_seconds.max(1))).await;
            }
            Err(err) => {
                record_upstream_reconnect_attempt(&state, &name, "error").await;
                record_upstream_health_check(&state, &name, "failure").await;
                failures = failures.saturating_add(1);
                session_id = None;
                set_server_disconnected(&state, &name, err.to_string()).await;
                if config.required
                    && config.reconnect.max_attempts > 0
                    && failures >= config.reconnect.max_attempts
                {
                    error!(server = %name, failures, "required upstream exhausted reconnect attempts");
                } else {
                    warn!(server = %name, failures, error = %err, "upstream unavailable");
                }
                let delay = backoff_delay(&config.reconnect, failures);
                record_upstream_backoff_start(&state, &name, delay.as_secs_f64()).await;
                tokio::time::sleep(delay).await;
                record_upstream_backoff_complete(&state, &name, delay.as_secs_f64()).await;
            }
        }
    }
}

fn backoff_delay(config: &ReconnectConfig, failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(20);
    let multiplier = 1_u64 << exponent;
    let ms = config
        .backoff_base_ms
        .saturating_mul(multiplier)
        .min(config.backoff_max_ms)
        .max(1);
    Duration::from_millis(ms)
}

async fn get_server_config(state: &AppState, name: &str) -> Option<ServerConfig> {
    state
        .registry
        .read()
        .await
        .servers
        .get(name)
        .map(|server| server.config.clone())
}

async fn set_server_connected(
    state: &AppState,
    name: &str,
    session_id: Option<String>,
    tools: Vec<Value>,
) {
    let mut registry = state.registry.write().await;
    if let Some(server) = registry.servers.get_mut(name) {
        server.connected = true;
        server.last_error = None;
        server.session_id = session_id;
        server.tools = tools;
        server.last_seen = Some(Instant::now());
    }
}

async fn set_server_disconnected(state: &AppState, name: &str, error: String) {
    let mut registry = state.registry.write().await;
    if let Some(server) = registry.servers.get_mut(name) {
        server.connected = false;
        server.last_error = Some(error);
        server.session_id = None;
        server.tools.clear();
        server.reconnects = server.reconnects.saturating_add(1);
    }
}

async fn replace_server_session_id(state: &AppState, name: &str, session_id: Option<String>) {
    let mut registry = state.registry.write().await;
    if let Some(server) = registry.servers.get_mut(name) {
        server.session_id = session_id;
        server.connected = true;
        server.last_error = None;
        server.last_seen = Some(Instant::now());
    }
}

async fn current_server_session_id(state: &AppState, name: &str) -> Option<String> {
    state
        .registry
        .read()
        .await
        .servers
        .get(name)
        .and_then(|server| server.session_id.clone())
}

async fn upstream_session_reset_lock(state: &AppState, name: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = state.upstream_session_reset_locks.lock().await;
    locks
        .entry(name.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

async fn record_upstream_health_check(state: &AppState, server: &str, result: &str) {
    state
        .metrics
        .lock()
        .await
        .record_upstream_health_check(server, result);
}

async fn record_upstream_reconnect_attempt(state: &AppState, server: &str, result: &str) {
    state
        .metrics
        .lock()
        .await
        .record_upstream_reconnect_attempt(server, result);
}

async fn record_upstream_backoff_start(state: &AppState, server: &str, seconds: f64) {
    state
        .metrics
        .lock()
        .await
        .set_upstream_backoff(server, seconds);
}

async fn record_upstream_backoff_complete(state: &AppState, server: &str, seconds: f64) {
    state
        .metrics
        .lock()
        .await
        .record_upstream_backoff_complete(server, seconds);
}

async fn record_upstream_bytes(state: &AppState, server: &str, direction: &str, bytes: u64) {
    state
        .metrics
        .lock()
        .await
        .record_upstream_bytes(server, direction, bytes);
}

async fn record_upstream_session_reset(state: &AppState, server: &str, reason: &str) {
    state
        .metrics
        .lock()
        .await
        .record_upstream_session_reset(server, reason);
}

fn upstream_client<'a>(state: &'a AppState, config: &ServerConfig) -> &'a Client {
    if config.tls_skip_verify {
        &state.insecure_client
    } else {
        &state.client
    }
}

async fn initialize_and_load_tools(
    state: &AppState,
    config: &ServerConfig,
) -> Result<(Option<String>, Vec<Value>)> {
    let session_id = initialize_upstream_session(state, config).await?;
    load_tools(state, config, session_id.as_deref()).await
}

async fn initialize_upstream_session(
    state: &AppState,
    config: &ServerConfig,
) -> Result<Option<String>> {
    let params = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {
            "name": "mcpstead",
            "version": env!("CARGO_PKG_VERSION")
        }
    });
    let started = Instant::now();
    let initialize =
        upstream_request_raw(state, config, None, json!(1), "initialize", params).await;
    let duration = started.elapsed().as_secs_f64();
    match &initialize {
        Ok(_) => {
            state
                .metrics
                .lock()
                .await
                .record_upstream_initialize(&config.name, "success", duration)
        }
        Err(_) => {
            state
                .metrics
                .lock()
                .await
                .record_upstream_initialize(&config.name, "error", duration)
        }
    }
    let (session_id, _) = initialize?;
    let _ = upstream_notification(
        state,
        config,
        session_id.as_deref(),
        "notifications/initialized",
        Value::Null,
    )
    .await;
    Ok(session_id)
}

async fn load_tools(
    state: &AppState,
    config: &ServerConfig,
    session_id: Option<&str>,
) -> Result<(Option<String>, Vec<Value>)> {
    let started = Instant::now();
    let refresh =
        upstream_request(state, config, session_id, json!(2), "tools/list", json!({})).await;
    let duration = started.elapsed().as_secs_f64();
    let (next_session_id, result) = match refresh {
        Ok(value) => value,
        Err(err) => {
            state.metrics.lock().await.record_upstream_tools_refresh(
                &config.name,
                "error",
                duration,
            );
            return Err(err);
        }
    };
    let tools = match result.get("tools").and_then(Value::as_array).cloned() {
        Some(tools) => {
            state.metrics.lock().await.record_upstream_tools_refresh(
                &config.name,
                "success",
                duration,
            );
            tools
        }
        None => {
            state.metrics.lock().await.record_upstream_tools_refresh(
                &config.name,
                "error",
                duration,
            );
            return Err(anyhow!(
                "upstream '{}' tools/list returned no tools array",
                config.name
            ));
        }
    };
    Ok((
        next_session_id.or_else(|| session_id.map(ToOwned::to_owned)),
        tools,
    ))
}

pub(crate) async fn upstream_request(
    state: &AppState,
    config: &ServerConfig,
    session_id: Option<&str>,
    id: Value,
    method: &str,
    params: Value,
) -> Result<(Option<String>, Value)> {
    let result = upstream_request_raw(
        state,
        config,
        session_id,
        id.clone(),
        method,
        params.clone(),
    )
    .await;
    let Err(error) = result else {
        return result;
    };

    let Some(reason) = session_reset_reason_from_error(&error) else {
        return Err(error);
    };
    if session_id.is_none() || method == "initialize" {
        return Err(error);
    }

    let reset_lock = upstream_session_reset_lock(state, &config.name).await;
    let _reset_guard = reset_lock.lock().await;

    if let Some(current_session) = current_server_session_id(state, &config.name).await
        && Some(current_session.as_str()) != session_id
    {
        let retry = upstream_request_raw(
            state,
            config,
            Some(&current_session),
            id.clone(),
            method,
            params.clone(),
        )
        .await;
        match retry {
            Ok((next_session_id, result)) => {
                let session = next_session_id.or(Some(current_session));
                replace_server_session_id(state, &config.name, session.clone()).await;
                return Ok((session, result));
            }
            Err(retry_error) if session_reset_reason_from_error(&retry_error).is_none() => {
                return Err(retry_error);
            }
            Err(_) => {}
        }
    }

    record_upstream_session_reset(state, &config.name, reason).await;
    replace_server_session_id(state, &config.name, None).await;

    let session = match initialize_upstream_session(state, config).await {
        Ok(session) => session,
        Err(_) => return Err(error),
    };
    replace_server_session_id(state, &config.name, session.clone()).await;

    let retry = upstream_request_raw(state, config, session.as_deref(), id, method, params).await;
    match retry {
        Ok((next_session_id, result)) => {
            let session = next_session_id.or(session);
            replace_server_session_id(state, &config.name, session.clone()).await;
            Ok((session, result))
        }
        Err(retry_error) => {
            if session_reset_reason_from_error(&retry_error).is_some() {
                replace_server_session_id(state, &config.name, None).await;
                Err(error)
            } else {
                Err(retry_error)
            }
        }
    }
}

async fn upstream_request_raw(
    state: &AppState,
    config: &ServerConfig,
    session_id: Option<&str>,
    id: Value,
    method: &str,
    params: Value,
) -> Result<(Option<String>, Value)> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let (next_session_id, response) = send_upstream_body(state, config, session_id, body).await?;
    let response =
        response.ok_or_else(|| anyhow!("upstream '{}' returned no body", config.name))?;
    if let Some(error) = response.get("error") {
        return Err(UpstreamResponseError {
            message: format!("upstream '{}' JSON-RPC error: {}", config.name, error),
            session_reset_reason: classify_json_rpc_session_reset(error),
        }
        .into());
    }
    Ok((
        next_session_id,
        response.get("result").cloned().unwrap_or(Value::Null),
    ))
}

async fn upstream_notification(
    state: &AppState,
    config: &ServerConfig,
    session_id: Option<&str>,
    method: &str,
    params: Value,
) -> Result<()> {
    let mut object = Map::new();
    object.insert("jsonrpc".to_string(), json!("2.0"));
    object.insert("method".to_string(), json!(method));
    if !params.is_null() {
        object.insert("params".to_string(), params);
    }
    send_upstream_body(state, config, session_id, Value::Object(object)).await?;
    Ok(())
}

async fn send_upstream_body(
    state: &AppState,
    config: &ServerConfig,
    session_id: Option<&str>,
    body: Value,
) -> Result<(Option<String>, Option<Value>)> {
    let client = upstream_client(state, config);
    let body_bytes = serde_json::to_vec(&body).context("serialize upstream request body")?;
    record_upstream_bytes(state, &config.name, "sent", body_bytes.len() as u64).await;
    let mut request = client
        .post(&config.url)
        .header(CONTENT_TYPE.as_str(), "application/json")
        .header(ACCEPT.as_str(), config.accept_header())
        .body(body_bytes);

    if let Some(session_id) = session_id {
        request = request.header(MCP_SESSION_ID, session_id);
    }

    if let Some(token) = config.bearer_token()? {
        request = request.bearer_auth(token);
    }

    for (name, value) in &config.headers {
        request = request.header(name.as_str(), value.as_str());
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("upstream '{}' request failed", config.name))?;
    parse_upstream_response(state, config, response).await
}

async fn parse_upstream_response(
    state: &AppState,
    config: &ServerConfig,
    response: reqwest::Response,
) -> Result<(Option<String>, Option<Value>)> {
    let status = response.status();
    let headers = response.headers().clone();
    let session_id = headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let content_type = headers
        .get(CONTENT_TYPE.as_str())
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let bytes = response.bytes().await?;
    record_upstream_bytes(state, &config.name, "received", bytes.len() as u64).await;

    if status == StatusCode::ACCEPTED && bytes.is_empty() {
        return Ok((session_id, None));
    }

    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes);
        let body = text.chars().take(512).collect::<String>();
        return Err(UpstreamResponseError {
            message: format!(
                "upstream '{}' HTTP {}: {}",
                config.name,
                status.as_u16(),
                body
            ),
            session_reset_reason: classify_http_session_reset(status, &text),
        }
        .into());
    }

    if bytes.is_empty() {
        return Ok((session_id, None));
    }

    let value = parse_mcp_body(content_type.as_deref(), &bytes)?;
    Ok((session_id, Some(value)))
}

fn classify_http_session_reset(status: StatusCode, body: &str) -> Option<&'static str> {
    let value = serde_json::from_str::<Value>(body).ok();
    if status == StatusCode::NOT_FOUND
        && value
            .as_ref()
            .and_then(json_rpc_error_code)
            .is_some_and(|code| code == -32001)
    {
        return Some("unknown_session");
    }

    if !matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE
    ) {
        return None;
    }

    if let Some(message) = value.as_ref().and_then(json_rpc_error_message)
        && let Some(reason) = classify_session_message(message)
    {
        return Some(reason);
    }
    classify_session_message(body)
}

fn classify_json_rpc_session_reset(error: &Value) -> Option<&'static str> {
    if json_rpc_error_code(error).is_some_and(|code| code == -32001) {
        return Some("unknown_session");
    }
    error
        .get("message")
        .and_then(Value::as_str)
        .and_then(classify_session_message)
}

fn json_rpc_error_code(value: &Value) -> Option<i64> {
    let error = value.get("error").unwrap_or(value);
    error.get("code").and_then(Value::as_i64)
}

fn json_rpc_error_message(value: &Value) -> Option<&str> {
    let error = value.get("error").unwrap_or(value);
    error.get("message").and_then(Value::as_str)
}

fn classify_session_message(message: &str) -> Option<&'static str> {
    let message = message.to_ascii_lowercase();
    if message.contains("unknown mcp session") {
        Some("unknown_session")
    } else if message.contains("session expired") {
        Some("expired")
    } else if message.contains("session terminated") {
        Some("terminated")
    } else {
        None
    }
}

fn parse_mcp_body(content_type: Option<&str>, bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes).context("response body is not utf-8")?;
    let trimmed = text.trim_start();
    let is_sse = content_type
        .map(|content_type| content_type.contains("text/event-stream"))
        .unwrap_or(false)
        || trimmed.starts_with("event:")
        || trimmed.starts_with("data:");

    if is_sse {
        parse_sse_body(text)
    } else {
        serde_json::from_slice(bytes).context("response body is not valid JSON")
    }
}

#[derive(Debug)]
struct SseEvent {
    event_type: Option<String>,
    data_lines: Vec<String>,
}

impl SseEvent {
    fn data(&self) -> String {
        self.data_lines.join("\n")
    }
}

fn parse_sse_body(text: &str) -> Result<Value> {
    let events = collect_sse_events(text);
    let mut first_data_event = None;

    for event in events {
        let event_type = event
            .event_type
            .clone()
            .unwrap_or_else(|| "message".to_string());
        let data = event.data();
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }

        match event_type.as_str() {
            "ping" | "keepalive" | "heartbeat" => continue,
            "message" => {
                return serde_json::from_str(data).context("SSE message data is not valid JSON");
            }
            "error" => bail!("SSE error event: {data}"),
            _ if first_data_event.is_none() => first_data_event = Some(event),
            _ => {}
        }
    }

    if let Some(event) = first_data_event {
        let event_type = event.event_type.as_deref().unwrap_or("message");
        let data = event.data();
        return serde_json::from_str(data.trim())
            .with_context(|| format!("SSE event '{event_type}' data is not valid JSON"));
    }

    bail!("SSE response had no data event")
}

fn collect_sse_events(text: &str) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut event_type = None;
    let mut data_lines = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            push_sse_event(&mut events, &mut event_type, &mut data_lines);
            continue;
        }
        if line.starts_with(':') {
            continue;
        }

        let (field, value) = line.split_once(':').map_or((line, ""), |(field, value)| {
            (field, value.strip_prefix(' ').unwrap_or(value))
        });
        match field {
            "event" => event_type = Some(value.to_string()),
            "data" => data_lines.push(value.to_string()),
            _ => {}
        }
    }
    push_sse_event(&mut events, &mut event_type, &mut data_lines);
    events
}

fn push_sse_event(
    events: &mut Vec<SseEvent>,
    event_type: &mut Option<String>,
    data_lines: &mut Vec<String>,
) {
    if event_type.is_some() || !data_lines.is_empty() {
        events.push(SseEvent {
            event_type: event_type.take(),
            data_lines: std::mem::take(data_lines),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        extract::State,
        http::HeaderMap,
        response::{IntoResponse, Response},
        routing::post,
    };

    use crate::{config::Config, constants::MCP_SESSION_ID, state::build_state};

    #[test]
    fn parses_plain_json_body() {
        let value = parse_mcp_body(
            Some("application/json"),
            br#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        )
        .unwrap();
        assert_eq!(value["id"], 1);
    }

    #[test]
    fn parses_sse_body_with_event_lines() {
        let value = parse_mcp_body(
            Some("text/event-stream"),
            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n",
        )
        .unwrap();
        assert_eq!(value["id"], 1);
    }

    #[test]
    fn skips_non_message_sse_events() {
        let value = parse_mcp_body(
            Some("text/event-stream"),
            b"event: ping\ndata: keepalive\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\n",
        )
        .unwrap();
        assert_eq!(value["id"], 2);
    }

    #[test]
    fn ignores_bare_ping_sse_events() {
        let err = parse_mcp_body(
            Some("text/event-stream"),
            b"event: ping\ndata: keepalive\n\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("SSE response had no data event"));
    }

    #[test]
    fn parses_custom_sse_event_when_no_message_exists() {
        let value = parse_mcp_body(
            Some("text/event-stream"),
            b"event: result\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}\n\n",
        )
        .unwrap();
        assert_eq!(value["id"], 3);
    }

    #[test]
    fn reports_sse_error_events() {
        let err =
            parse_mcp_body(Some("text/event-stream"), b"event: error\ndata: bad\n\n").unwrap_err();
        assert!(err.to_string().contains("SSE error event: bad"));
    }

    #[test]
    fn computes_backoff() {
        let config = ReconnectConfig {
            max_attempts: 0,
            backoff_base_ms: 100,
            backoff_max_ms: 1_000,
        };
        assert_eq!(backoff_delay(&config, 1), Duration::from_millis(100));
        assert_eq!(backoff_delay(&config, 2), Duration::from_millis(200));
        assert_eq!(backoff_delay(&config, 20), Duration::from_millis(1_000));
    }

    #[test]
    fn classifies_session_reset_reasons() {
        assert_eq!(
            classify_http_session_reset(
                StatusCode::NOT_FOUND,
                r#"{"code":-32001,"message":"Unknown MCP session"}"#,
            ),
            Some("unknown_session")
        );
        assert_eq!(
            classify_http_session_reset(
                StatusCode::UNAUTHORIZED,
                r#"{"error":{"message":"session expired"}}"#,
            ),
            Some("expired")
        );
        assert_eq!(
            classify_http_session_reset(StatusCode::FORBIDDEN, "session terminated"),
            Some("terminated")
        );
    }

    #[derive(Clone)]
    struct SessionResetServer {
        initialize_calls: Arc<AtomicUsize>,
        tool_calls: Arc<AtomicUsize>,
        always_reject_tools: bool,
    }

    async fn session_reset_handler(
        State(server): State<SessionResetServer>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        let method = body.get("method").and_then(Value::as_str).unwrap_or("");
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                server.initialize_calls.fetch_add(1, Ordering::Relaxed);
                let mut response = Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {}
                }))
                .into_response();
                response
                    .headers_mut()
                    .insert(MCP_SESSION_ID, "fresh-session".parse().unwrap());
                response
            }
            "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
            "tools/call" => {
                server.tool_calls.fetch_add(1, Ordering::Relaxed);
                let session = headers
                    .get(MCP_SESSION_ID)
                    .and_then(|value| value.to_str().ok());
                if server.always_reject_tools || session != Some("fresh-session") {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({
                            "code": -32001,
                            "message": "Unknown MCP session"
                        })),
                    )
                        .into_response();
                }
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"ok": true}
                }))
                .into_response()
            }
            _ => Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {}
            }))
            .into_response(),
        }
    }

    async fn session_reset_test_state(
        always_reject_tools: bool,
    ) -> (AppState, ServerConfig, SessionResetServer) {
        let server = SessionResetServer {
            initialize_calls: Arc::new(AtomicUsize::new(0)),
            tool_calls: Arc::new(AtomicUsize::new(0)),
            always_reject_tools,
        };
        let observed_server = server.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/", post(session_reset_handler))
            .with_state(server);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut config = ServerConfig::default();
        config.name = "example".to_string();
        config.url = url;
        let state = build_state(
            &Config {
                servers: vec![config.clone()],
                ..Config::default()
            },
            None,
        )
        .unwrap();
        {
            let mut registry = state.registry.write().await;
            let server = registry.servers.get_mut("example").unwrap();
            server.connected = true;
            server.session_id = Some("stale-session".to_string());
        }
        (state, config, observed_server)
    }

    #[tokio::test]
    async fn resets_invalid_upstream_session_and_retries_request() {
        let (state, config, _server) = session_reset_test_state(false).await;

        let (session_id, result) = upstream_request(
            &state,
            &config,
            Some("stale-session"),
            json!(7),
            "tools/call",
            json!({}),
        )
        .await
        .unwrap();

        assert_eq!(session_id.as_deref(), Some("fresh-session"));
        assert_eq!(result, json!({"ok": true}));
        let registry = state.registry.read().await;
        assert_eq!(
            registry.servers["example"].session_id.as_deref(),
            Some("fresh-session")
        );
        drop(registry);
        assert_eq!(
            state
                .metrics
                .lock()
                .await
                .upstream_session_resets
                .get(&("example".to_string(), "unknown_session".to_string())),
            Some(&1)
        );
    }

    #[tokio::test]
    async fn concurrent_session_resets_share_one_reinitialize() {
        let (state, config, server) = session_reset_test_state(false).await;

        let first = upstream_request(
            &state,
            &config,
            Some("stale-session"),
            json!(8),
            "tools/call",
            json!({}),
        );
        let second = upstream_request(
            &state,
            &config,
            Some("stale-session"),
            json!(9),
            "tools/call",
            json!({}),
        );
        let (first, second) = tokio::join!(first, second);

        assert_eq!(first.unwrap().1, json!({"ok": true}));
        assert_eq!(second.unwrap().1, json!({"ok": true}));
        assert_eq!(server.initialize_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            state
                .metrics
                .lock()
                .await
                .upstream_session_resets
                .get(&("example".to_string(), "unknown_session".to_string())),
            Some(&1)
        );
    }

    #[tokio::test]
    async fn only_retries_session_reset_once() {
        let (state, config, _server) = session_reset_test_state(true).await;

        let error = upstream_request(
            &state,
            &config,
            Some("stale-session"),
            json!(8),
            "tools/call",
            json!({}),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("HTTP 404"));
        assert_eq!(
            state.registry.read().await.servers["example"]
                .session_id
                .as_deref(),
            None
        );
        assert_eq!(
            state
                .metrics
                .lock()
                .await
                .upstream_session_resets
                .get(&("example".to_string(), "unknown_session".to_string())),
            Some(&1)
        );
    }
}
