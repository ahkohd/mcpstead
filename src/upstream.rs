use std::time::{Duration, Instant};

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
    let params = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {
            "name": "mcpstead",
            "version": env!("CARGO_PKG_VERSION")
        }
    });
    let started = Instant::now();
    let initialize = upstream_request(state, config, None, json!(1), "initialize", params).await;
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
    load_tools(state, config, session_id.as_deref()).await
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
        bail!("upstream '{}' JSON-RPC error: {}", config.name, error);
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
        bail!(
            "upstream '{}' HTTP {}: {}",
            config.name,
            status.as_u16(),
            text.chars().take(512).collect::<String>()
        );
    }

    if bytes.is_empty() {
        return Ok((session_id, None));
    }

    let value = parse_mcp_body(content_type.as_deref(), &bytes)?;
    Ok((session_id, Some(value)))
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
}
