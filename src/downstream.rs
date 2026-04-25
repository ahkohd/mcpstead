use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{
        HeaderMap, StatusCode,
        header::{ACCEPT, CONTENT_TYPE, WWW_AUTHENTICATE},
    },
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::config::{McpAuthMode, ServerConfig};
use crate::constants::MCP_SESSION_ID;
use crate::state::{
    AppState, DownstreamSession, accepting_new_sessions, terminate_session, touch_session,
};
use crate::upstream::upstream_request;

const JSONRPC_PARSE_ERROR: i64 = -32700;
const JSONRPC_INVALID_REQUEST: i64 = -32600;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const JSONRPC_INVALID_PARAMS: i64 = -32602;
const JSONRPC_UPSTREAM_ERROR: i64 = -32000;

fn get_header(headers: &HeaderMap, key: &str) -> Option<String> {
    headers
        .get(key)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string())
}

#[derive(Clone, Copy)]
pub(crate) enum AuthDecision {
    Allow,
    Deny(&'static str),
}

pub(crate) async fn authorize_mcp(headers: &HeaderMap, state: &AppState) -> AuthDecision {
    let auth_state = state.mcp_auth.read().await.clone();
    match auth_state.mode {
        McpAuthMode::None => AuthDecision::Allow,
        McpAuthMode::Bearer => {
            let Some(token) = &auth_state.bearer_token else {
                return AuthDecision::Deny("internal");
            };
            let Some(auth) = get_header(headers, "authorization") else {
                return AuthDecision::Deny("missing_header");
            };
            let Some(candidate) = auth.strip_prefix("Bearer ") else {
                return AuthDecision::Deny("bad_format");
            };
            if candidate.is_empty() {
                return AuthDecision::Deny("bad_format");
            }
            if bool::from(candidate.as_bytes().ct_eq(token.as_bytes())) {
                AuthDecision::Allow
            } else {
                AuthDecision::Deny("wrong_token")
            }
        }
    }
}

pub(crate) async fn record_auth_decision(state: &AppState, decision: AuthDecision) {
    if state.mcp_auth.read().await.mode != McpAuthMode::Bearer {
        return;
    }
    let mut metrics = state.metrics.lock().await;
    match decision {
        AuthDecision::Allow => metrics.record_auth_success(),
        AuthDecision::Deny(reason) => metrics.record_auth_failure(reason),
    }
}

pub(crate) fn unauthorized_response() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(WWW_AUTHENTICATE, "Bearer")
        .body(Body::empty())
        .expect("response build failed")
}

pub(crate) async fn post_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_started = Instant::now();
    let auth = authorize_mcp(&headers, &state).await;
    record_auth_decision(&state, auth).await;
    if let AuthDecision::Deny(_) = auth {
        record_mcp_request(
            &state,
            "unknown",
            "error",
            request_started.elapsed().as_secs_f64(),
        )
        .await;
        return unauthorized_response();
    }

    if let Some(session_id) = get_header(&headers, MCP_SESSION_ID) {
        touch_session(&state, &session_id).await;
    }

    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            record_mcp_request(
                &state,
                "invalid",
                "error",
                request_started.elapsed().as_secs_f64(),
            )
            .await;
            return downstream_response(
                &headers,
                StatusCode::OK,
                Some(json_rpc_error(
                    Value::Null,
                    JSONRPC_PARSE_ERROR,
                    err.to_string(),
                )),
                None,
            );
        }
    };

    let saw_initialize = value_has_method(&value, "initialize");
    if saw_initialize && !accepting_new_sessions(&state) {
        record_mcp_request(
            &state,
            "initialize",
            "error",
            request_started.elapsed().as_secs_f64(),
        )
        .await;
        return (StatusCode::SERVICE_UNAVAILABLE, "gateway shutting down").into_response();
    }

    let session_id = saw_initialize.then(|| Uuid::new_v4().to_string());
    if let Some(session_id) = &session_id {
        let now = Instant::now();
        state.sessions.write().await.insert(
            session_id.clone(),
            DownstreamSession {
                created_at: now,
                last_activity: now,
            },
        );
        state.metrics.lock().await.record_session_opened();
    }

    let mut response_session_id = session_id.clone();
    let response = if let Some(items) = value.as_array() {
        let mut responses = Vec::new();
        for item in items {
            if let Some(response) = handle_message_with_metrics(&state, item.clone()).await {
                responses.push(response);
            }
        }
        if responses.is_empty() {
            if let Some(session_id) = &session_id {
                terminate_session(&state, session_id, "init_failed").await;
                response_session_id = None;
            }
            return downstream_response(&headers, StatusCode::ACCEPTED, None, response_session_id);
        }
        Value::Array(responses)
    } else {
        match handle_message_with_metrics(&state, value).await {
            Some(response) => response,
            None => {
                if let Some(session_id) = &session_id {
                    terminate_session(&state, session_id, "init_failed").await;
                    response_session_id = None;
                }
                return downstream_response(
                    &headers,
                    StatusCode::ACCEPTED,
                    None,
                    response_session_id,
                );
            }
        }
    };

    if let Some(session_id) = &session_id
        && !initialize_response_succeeded(&response)
    {
        terminate_session(&state, session_id, "init_failed").await;
        response_session_id = None;
    }

    downstream_response(
        &headers,
        StatusCode::OK,
        Some(response),
        response_session_id,
    )
}

async fn handle_message_with_metrics(state: &AppState, message: Value) -> Option<Value> {
    let method = method_label(&message);
    let started = Instant::now();
    let response = handle_message(state, message).await;
    let result = if response.as_ref().is_some_and(is_json_rpc_error) {
        "error"
    } else {
        "success"
    };
    record_mcp_request(state, method, result, started.elapsed().as_secs_f64()).await;
    response
}

async fn record_mcp_request(state: &AppState, method: &str, result: &str, duration: f64) {
    state
        .metrics
        .lock()
        .await
        .record_mcp_request(method, result, duration);
}

fn method_label(message: &Value) -> &'static str {
    match message.get("method").and_then(Value::as_str) {
        Some("initialize") => "initialize",
        Some("notifications/initialized") => "notifications/initialized",
        Some("tools/list") => "tools/list",
        Some("tools/call") => "tools/call",
        Some(_) => "unknown",
        None => "invalid",
    }
}

fn is_json_rpc_error(value: &Value) -> bool {
    value.get("error").is_some()
}

async fn handle_message(state: &AppState, message: Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let response_id = id.clone().unwrap_or(Value::Null);
    let is_notification = id.is_none();

    let Some(object) = message.as_object() else {
        return Some(json_rpc_error(
            Value::Null,
            JSONRPC_INVALID_REQUEST,
            "request must be an object",
        ));
    };

    if object.get("jsonrpc") != Some(&json!("2.0")) {
        if is_notification {
            return None;
        }
        return Some(json_rpc_error(
            response_id,
            JSONRPC_INVALID_REQUEST,
            "jsonrpc must be '2.0'",
        ));
    }

    let Some(method) = object.get("method").and_then(Value::as_str) else {
        if is_notification {
            return None;
        }
        return Some(json_rpc_error(
            response_id,
            JSONRPC_INVALID_REQUEST,
            "method is required",
        ));
    };

    match method {
        "initialize" => {
            if is_notification {
                None
            } else {
                Some(json_rpc_result(response_id, initialize_result(&message)))
            }
        }
        "notifications/initialized" => None,
        "tools/list" => {
            if is_notification {
                None
            } else {
                Some(json_rpc_result(
                    response_id,
                    list_downstream_tools(state).await,
                ))
            }
        }
        "tools/call" => {
            if is_notification {
                None
            } else {
                Some(call_tool(state, response_id, &message).await)
            }
        }
        _ => {
            if is_notification {
                None
            } else {
                Some(json_rpc_error(
                    response_id,
                    JSONRPC_METHOD_NOT_FOUND,
                    format!("method '{method}' not found"),
                ))
            }
        }
    }
}

fn initialize_result(message: &Value) -> Value {
    let protocol_version = message
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("2024-11-05");
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": "mcpstead",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

async fn list_downstream_tools(state: &AppState) -> Value {
    let registry = state.registry.read().await;
    let mut tools = Vec::new();

    for (server_name, server) in &registry.servers {
        if !server.connected {
            continue;
        }
        for tool in &server.tools {
            if let Some(tool) = namespaced_tool(server_name, tool) {
                tools.push(tool);
            }
        }
    }

    json!({ "tools": tools })
}

fn namespaced_tool(server_name: &str, tool: &Value) -> Option<Value> {
    let object = tool.as_object()?;
    let name = object.get("name")?.as_str()?;
    let mut object = object.clone();
    object.insert(
        "name".to_string(),
        Value::String(format!("{server_name}__{name}")),
    );
    match object.get("description").and_then(Value::as_str) {
        Some(description) => {
            object.insert(
                "description".to_string(),
                Value::String(format!("[{server_name}] {description}")),
            );
        }
        None => {
            object.insert(
                "description".to_string(),
                Value::String(format!("Tool from {server_name}")),
            );
        }
    }
    Some(Value::Object(object))
}

async fn call_tool(state: &AppState, response_id: Value, message: &Value) -> Value {
    let Some(params) = message.get("params") else {
        return json_rpc_error(response_id, JSONRPC_INVALID_PARAMS, "params are required");
    };
    let Some(full_name) = params.get("name").and_then(Value::as_str) else {
        return json_rpc_error(
            response_id,
            JSONRPC_INVALID_PARAMS,
            "params.name is required",
        );
    };
    let Some((server_name, tool_name)) = full_name.split_once("__") else {
        return json_rpc_error(
            response_id,
            JSONRPC_INVALID_PARAMS,
            "tool name must be server__tool",
        );
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let Some((config, session_id, connected)) = get_call_target(state, server_name).await else {
        return json_rpc_error(
            response_id,
            JSONRPC_INVALID_PARAMS,
            format!("unknown upstream '{server_name}'"),
        );
    };

    if !connected {
        record_tool_call(
            state,
            server_name,
            tool_name,
            0.0,
            Some("upstream_disconnected"),
        )
        .await;
        return json_rpc_error(
            response_id,
            JSONRPC_UPSTREAM_ERROR,
            format!("upstream '{server_name}' is disconnected"),
        );
    }

    let started = Instant::now();
    let result = upstream_request(
        state,
        &config,
        session_id.as_deref(),
        response_id.clone(),
        "tools/call",
        json!({
            "name": tool_name,
            "arguments": arguments,
        }),
    )
    .await;
    let duration = started.elapsed().as_secs_f64();

    match result {
        Ok((_next_session_id, result)) => {
            let error_reason =
                upstream_tool_returned_error(&result).then_some("upstream_returned_error");
            record_tool_call(state, server_name, tool_name, duration, error_reason).await;
            mark_server_seen(state, server_name).await;
            json_rpc_result(response_id, result)
        }
        Err(err) => {
            let reason = categorize_tool_error(&err.to_string());
            record_tool_call(state, server_name, tool_name, duration, Some(reason)).await;
            json_rpc_error(response_id, JSONRPC_UPSTREAM_ERROR, err.to_string())
        }
    }
}

fn upstream_tool_returned_error(result: &Value) -> bool {
    result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn categorize_tool_error(error: &str) -> &'static str {
    let error = error.to_ascii_lowercase();
    if error.contains("timeout") || error.contains("timed out") {
        "upstream_timeout"
    } else if error.contains("http 401")
        || error.contains("http 403")
        || error.contains("unauthorized")
    {
        "upstream_auth_failed"
    } else if error.contains("json-rpc error")
        || error.contains("not valid json")
        || error.contains("sse")
        || error.contains("request failed")
        || error.contains("connection")
        || error.contains("http")
    {
        "upstream_protocol_error"
    } else {
        "internal"
    }
}

async fn get_call_target(
    state: &AppState,
    server_name: &str,
) -> Option<(ServerConfig, Option<String>, bool)> {
    let registry = state.registry.read().await;
    let server = registry.servers.get(server_name)?;
    Some((
        server.config.clone(),
        server.session_id.clone(),
        server.connected,
    ))
}

async fn mark_server_seen(state: &AppState, server_name: &str) {
    let mut registry = state.registry.write().await;
    if let Some(server) = registry.servers.get_mut(server_name) {
        server.last_seen = Some(Instant::now());
    }
}

async fn record_tool_call(
    state: &AppState,
    server_name: &str,
    tool_name: &str,
    duration: f64,
    error_reason: Option<&str>,
) {
    state
        .metrics
        .lock()
        .await
        .record_tool_call(server_name, tool_name, duration, error_reason);
}

fn value_has_method(value: &Value, method: &str) -> bool {
    if let Some(items) = value.as_array() {
        return items.iter().any(|item| value_has_method(item, method));
    }
    value.get("method").and_then(Value::as_str) == Some(method)
}

fn initialize_response_succeeded(value: &Value) -> bool {
    if let Some(items) = value.as_array() {
        return items.iter().any(initialize_response_succeeded);
    }
    value.get("error").is_none()
        && value.pointer("/result/serverInfo/name") == Some(&json!("mcpstead"))
}

fn downstream_response(
    headers: &HeaderMap,
    status: StatusCode,
    body: Option<Value>,
    session_id: Option<String>,
) -> Response {
    if body.is_none() {
        let mut builder = Response::builder().status(status);
        if let Some(session_id) = session_id {
            builder = builder.header(MCP_SESSION_ID, session_id);
        }
        return builder.body(Body::empty()).expect("response build failed");
    }

    let json = serde_json::to_string(&body.expect("body checked")).expect("JSON serialize failed");
    let wants_sse = wants_sse(headers);
    let (content_type, body) = if wants_sse {
        ("text/event-stream", format!("data: {json}\n\n"))
    } else {
        ("application/json", json)
    };

    let mut builder = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type);
    if let Some(session_id) = session_id {
        builder = builder.header(MCP_SESSION_ID, session_id);
    }
    builder
        .body(Body::from(body))
        .expect("response build failed")
}

fn wants_sse(headers: &HeaderMap) -> bool {
    let Some(accept) = headers.get(ACCEPT).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    accept.contains("text/event-stream") && !accept.contains("application/json")
}

fn json_rpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn json_rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        }
    })
}

pub(crate) async fn get_mcp(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let started = Instant::now();
    let auth = authorize_mcp(&headers, &state).await;
    record_auth_decision(&state, auth).await;
    if let AuthDecision::Deny(_) = auth {
        record_mcp_request(&state, "listen", "error", started.elapsed().as_secs_f64()).await;
        return unauthorized_response();
    }

    if let Some(session_id) = headers
        .get(MCP_SESSION_ID)
        .and_then(|value| value.to_str().ok())
    {
        touch_session(&state, session_id).await;
    }
    record_mcp_request(&state, "listen", "error", started.elapsed().as_secs_f64()).await;
    (
        StatusCode::METHOD_NOT_ALLOWED,
        "server notifications are not implemented",
    )
        .into_response()
}

pub(crate) async fn delete_mcp(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let started = Instant::now();
    let auth = authorize_mcp(&headers, &state).await;
    record_auth_decision(&state, auth).await;
    if let AuthDecision::Deny(_) = auth {
        record_mcp_request(
            &state,
            "terminate",
            "error",
            started.elapsed().as_secs_f64(),
        )
        .await;
        return unauthorized_response();
    }

    if let Some(session_id) = headers
        .get(MCP_SESSION_ID)
        .and_then(|value| value.to_str().ok())
    {
        terminate_session(&state, session_id, "client_delete").await;
    }
    record_mcp_request(
        &state,
        "terminate",
        "success",
        started.elapsed().as_secs_f64(),
    )
    .await;
    StatusCode::ACCEPTED.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::{Router, body::to_bytes, http::Request, routing::post};
    use tower::ServiceExt;

    use crate::config::{Config, McpAuthConfig, McpAuthMode, McpConfig};
    use crate::state::{build_state, stop_accepting_new_sessions};

    fn test_state(mode: McpAuthMode, token: Option<&str>) -> AppState {
        let config = Config {
            mcp: McpConfig {
                auth: McpAuthConfig {
                    mode,
                    bearer_token: None,
                },
                ..McpConfig::default()
            },
            ..Config::default()
        };
        build_state(&config, token.map(ToOwned::to_owned)).unwrap()
    }

    fn test_app(mode: McpAuthMode, token: Option<&str>) -> Router {
        Router::new()
            .route("/mcp", post(post_mcp).delete(delete_mcp))
            .with_state(test_state(mode, token))
    }

    async fn post_payload(
        app: &Router,
        auth: Option<&str>,
        payload: Value,
    ) -> (axum::http::response::Parts, Bytes) {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json");
        if let Some(auth) = auth {
            builder = builder.header("authorization", auth);
        }
        let request = builder.body(Body::from(payload.to_string())).unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        (parts, bytes)
    }

    async fn post_initialize(
        app: &Router,
        auth: Option<&str>,
    ) -> (axum::http::response::Parts, Bytes) {
        post_payload(
            app,
            auth,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2024-11-05" }
            }),
        )
        .await
    }

    #[tokio::test]
    async fn mode_none_allows_unauthenticated_request() {
        let app = test_app(McpAuthMode::None, None);
        let (parts, body) = post_initialize(&app, None).await;
        assert_eq!(parts.status, StatusCode::OK);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["id"], 1);
    }

    #[tokio::test]
    async fn bearer_auth_rejects_missing_header() {
        let app = test_app(McpAuthMode::Bearer, Some("secret"));
        let (parts, _body) = post_initialize(&app, None).await;
        assert_eq!(parts.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            parts
                .headers
                .get(WWW_AUTHENTICATE)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer"
        );
    }

    #[tokio::test]
    async fn bearer_auth_rejects_wrong_token() {
        let app = test_app(McpAuthMode::Bearer, Some("secret"));
        let (parts, _body) = post_initialize(&app, Some("Bearer wrong")).await;
        assert_eq!(parts.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            parts
                .headers
                .get(WWW_AUTHENTICATE)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer"
        );
    }

    #[tokio::test]
    async fn bearer_auth_accepts_right_token() {
        let app = test_app(McpAuthMode::Bearer, Some("secret"));
        let (parts, body) = post_initialize(&app, Some("Bearer secret")).await;
        assert_eq!(parts.status, StatusCode::OK);
        assert!(parts.headers.get(MCP_SESSION_ID).is_some());
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["id"], 1);
    }

    #[tokio::test]
    async fn shutdown_rejects_new_initialize_sessions() {
        let state = test_state(McpAuthMode::None, None);
        stop_accepting_new_sessions(&state);
        let app = Router::new()
            .route("/mcp", post(post_mcp).delete(delete_mcp))
            .with_state(state);

        let (parts, body) = post_initialize(&app, None).await;

        assert_eq!(parts.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(&body[..], b"gateway shutting down");
    }

    #[tokio::test]
    async fn initialize_notification_records_init_failed() {
        let state = test_state(McpAuthMode::None, None);
        let app = Router::new()
            .route("/mcp", post(post_mcp).delete(delete_mcp))
            .with_state(state.clone());

        let (parts, _body) = post_payload(
            &app,
            None,
            json!({
                "jsonrpc": "2.0",
                "method": "initialize",
                "params": {}
            }),
        )
        .await;

        assert_eq!(parts.status, StatusCode::ACCEPTED);
        assert!(parts.headers.get(MCP_SESSION_ID).is_none());
        assert!(state.sessions.read().await.is_empty());
        assert_eq!(
            state
                .metrics
                .lock()
                .await
                .downstream_session_terminations
                .get("init_failed"),
            Some(&1)
        );
    }

    #[test]
    fn synthetic_tool_error_uses_internal_reason() {
        assert_eq!(categorize_tool_error("synthetic"), "internal");
    }

    #[test]
    fn namespaces_tools() {
        let tool = json!({"name":"search","inputSchema":{"type":"object"}});
        let namespaced = namespaced_tool("example", &tool).unwrap();
        assert_eq!(namespaced["name"], "example__search");
    }
}
