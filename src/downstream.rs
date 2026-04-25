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
use crate::state::AppState;
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

fn is_mcp_authorized(headers: &HeaderMap, state: &AppState) -> bool {
    match state.mcp_auth_mode {
        McpAuthMode::None => true,
        McpAuthMode::Bearer => {
            let Some(token) = &state.mcp_bearer_token else {
                return false;
            };
            let Some(auth) = get_header(headers, "authorization") else {
                return false;
            };
            let expected = format!("Bearer {token}");
            bool::from(auth.as_bytes().ct_eq(expected.as_bytes()))
        }
    }
}

fn unauthorized_response() -> Response {
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
    if !is_mcp_authorized(&headers, &state) {
        return unauthorized_response();
    }

    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
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
    let session_id = saw_initialize.then(|| Uuid::new_v4().to_string());
    if let Some(session_id) = &session_id {
        state.sessions.write().await.insert(session_id.clone());
    }

    let response = if let Some(items) = value.as_array() {
        let mut responses = Vec::new();
        for item in items {
            if let Some(response) = handle_message(&state, item.clone()).await {
                responses.push(response);
            }
        }
        if responses.is_empty() {
            return downstream_response(&headers, StatusCode::ACCEPTED, None, session_id);
        }
        Value::Array(responses)
    } else {
        match handle_message(&state, value).await {
            Some(response) => response,
            None => return downstream_response(&headers, StatusCode::ACCEPTED, None, session_id),
        }
    };

    downstream_response(&headers, StatusCode::OK, Some(response), session_id)
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
            record_tool_call(state, server_name, tool_name, duration, false).await;
            mark_server_seen(state, server_name).await;
            json_rpc_result(response_id, result)
        }
        Err(err) => {
            record_tool_call(state, server_name, tool_name, duration, true).await;
            json_rpc_error(response_id, JSONRPC_UPSTREAM_ERROR, err.to_string())
        }
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
    error: bool,
) {
    let mut metrics = state.metrics.lock().await;
    let metric = metrics
        .tool_calls
        .entry((server_name.to_string(), tool_name.to_string()))
        .or_default();
    metric.count = metric.count.saturating_add(1);
    metric.duration_sum_seconds += duration;
    if error {
        metric.errors = metric.errors.saturating_add(1);
    }
}

fn value_has_method(value: &Value, method: &str) -> bool {
    if let Some(items) = value.as_array() {
        return items.iter().any(|item| value_has_method(item, method));
    }
    value.get("method").and_then(Value::as_str) == Some(method)
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

pub(crate) async fn get_mcp() -> impl IntoResponse {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        "server notifications are not implemented",
    )
}

pub(crate) async fn delete_mcp(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_mcp_authorized(&headers, &state) {
        return unauthorized_response();
    }

    if let Some(session_id) = headers
        .get(MCP_SESSION_ID)
        .and_then(|value| value.to_str().ok())
    {
        state.sessions.write().await.remove(session_id);
    }
    StatusCode::ACCEPTED.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::{Router, body::to_bytes, http::Request, routing::post};
    use tower::ServiceExt;

    use crate::config::{Config, McpAuthConfig, McpAuthMode, McpConfig};
    use crate::state::build_state;

    fn test_app(mode: McpAuthMode, token: Option<&str>) -> Router {
        let config = Config {
            mcp: McpConfig {
                auth: McpAuthConfig {
                    mode,
                    bearer_token: None,
                },
            },
            ..Config::default()
        };
        Router::new()
            .route("/mcp", post(post_mcp).delete(delete_mcp))
            .with_state(build_state(&config, token.map(ToOwned::to_owned)).unwrap())
    }

    async fn post_initialize(
        app: &Router,
        auth: Option<&str>,
    ) -> (axum::http::response::Parts, Bytes) {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json");
        if let Some(auth) = auth {
            builder = builder.header("authorization", auth);
        }
        let request = builder
            .body(Body::from(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocolVersion": "2024-11-05" }
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        (parts, bytes)
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

    #[test]
    fn namespaces_tools() {
        let tool = json!({"name":"search","inputSchema":{"type":"object"}});
        let namespaced = namespaced_tool("example", &tool).unwrap();
        assert_eq!(namespaced["name"], "example__search");
    }
}
