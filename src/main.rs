use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{
        HeaderMap, StatusCode,
        header::{ACCEPT, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, RwLock};
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

const MCP_SESSION_ID: &str = "Mcp-Session-Id";
const DEFAULT_ACCEPT: &str = "application/json, text/event-stream";
const JSONRPC_PARSE_ERROR: i64 = -32700;
const JSONRPC_INVALID_REQUEST: i64 = -32600;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const JSONRPC_INVALID_PARAMS: i64 = -32602;
const JSONRPC_UPSTREAM_ERROR: i64 = -32000;

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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Config {
    host: String,
    port: u16,
    servers: Vec<ServerConfig>,
    metrics: MetricsConfig,
    logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8766,
            servers: Vec::new(),
            metrics: MetricsConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct ServerConfig {
    name: String,
    url: String,
    protocol: String,
    required: bool,
    auth: Option<AuthConfig>,
    headers: HashMap<String, String>,
    quirks: QuirksConfig,
    reconnect: ReconnectConfig,
    tools: ToolsConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: String::new(),
            protocol: "streamable".to_string(),
            required: false,
            auth: None,
            headers: HashMap::new(),
            quirks: QuirksConfig::default(),
            reconnect: ReconnectConfig::default(),
            tools: ToolsConfig::default(),
        }
    }
}

impl ServerConfig {
    fn accept_header(&self) -> &str {
        self.quirks
            .inject_accept_header
            .as_deref()
            .unwrap_or(DEFAULT_ACCEPT)
    }

    fn bearer_token(&self) -> Result<Option<String>> {
        match &self.auth {
            None => Ok(None),
            Some(AuthConfig::Mode(mode)) if mode == "none" => Ok(None),
            Some(AuthConfig::Mode(mode)) => bail!("unsupported auth mode '{mode}'"),
            Some(AuthConfig::Object(auth)) if auth.kind == "none" => Ok(None),
            Some(AuthConfig::Object(auth)) if auth.kind == "bearer" => {
                if let Some(token) = &auth.token {
                    return Ok(Some(token.clone()));
                }
                if let Some(name) = &auth.token_env {
                    return Ok(Some(env::var(name).with_context(|| {
                        format!("missing env var '{name}' for server '{}'", self.name)
                    })?));
                }
                bail!(
                    "bearer auth for server '{}' needs token or token_env",
                    self.name
                )
            }
            Some(AuthConfig::Object(auth)) => bail!("unsupported auth type '{}'", auth.kind),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum AuthConfig {
    Mode(String),
    Object(AuthObject),
}

#[derive(Debug, Clone, Deserialize)]
struct AuthObject {
    #[serde(rename = "type")]
    kind: String,
    token: Option<String>,
    token_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct QuirksConfig {
    normalize_sse_events: bool,
    inject_accept_header: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct ReconnectConfig {
    max_attempts: u32,
    backoff_base_ms: u64,
    backoff_max_ms: u64,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            max_attempts: 0,
            backoff_base_ms: 1_000,
            backoff_max_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct ToolsConfig {
    ttl_seconds: u64,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self { ttl_seconds: 300 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct MetricsConfig {
    enabled: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct LoggingConfig {
    level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

#[derive(Clone)]
struct AppState {
    registry: Arc<RwLock<Registry>>,
    metrics: Arc<Mutex<Metrics>>,
    sessions: Arc<RwLock<HashSet<String>>>,
    client: Client,
    start: Instant,
    metrics_enabled: bool,
}

#[derive(Default)]
struct Registry {
    servers: BTreeMap<String, ManagedServer>,
}

struct ManagedServer {
    config: ServerConfig,
    connected: bool,
    last_error: Option<String>,
    session_id: Option<String>,
    tools: Vec<Value>,
    last_seen: Option<Instant>,
    reconnects: u64,
}

impl ManagedServer {
    fn new(config: ServerConfig) -> Self {
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
struct Metrics {
    tool_calls: BTreeMap<(String, String), ToolCallMetric>,
}

#[derive(Default)]
struct ToolCallMetric {
    count: u64,
    errors: u64,
    duration_sum_seconds: f64,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    uptime_seconds: f64,
    servers: Vec<HealthServer>,
}

#[derive(Serialize)]
struct HealthServer {
    name: String,
    url: String,
    protocol: String,
    required: bool,
    connected: bool,
    tools_count: usize,
    last_seen_seconds_ago: Option<f64>,
    reconnects: u64,
    last_error: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = load_config(&args.config).await?;
    init_logging(&config.logging.level);
    validate_config(&config)?;

    let registry = Registry {
        servers: config
            .servers
            .iter()
            .cloned()
            .map(|server| (server.name.clone(), ManagedServer::new(server)))
            .collect(),
    };

    let state = AppState {
        registry: Arc::new(RwLock::new(registry)),
        metrics: Arc::new(Mutex::new(Metrics::default())),
        sessions: Arc::new(RwLock::new(HashSet::new())),
        client: Client::builder()
            .user_agent(format!("mcpstead/{}", env!("CARGO_PKG_VERSION")))
            .build()?,
        start: Instant::now(),
        metrics_enabled: config.metrics.enabled,
    };

    for name in server_names(&state).await {
        let state = state.clone();
        tokio::spawn(async move {
            manage_server(state, name).await;
        });
    }

    let app = Router::new()
        .route("/mcp", post(post_mcp).get(get_mcp).delete(delete_mcp))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

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

async fn load_config(path: &PathBuf) -> Result<Config> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read config '{}': failed", path.display()))?;
    serde_yaml::from_str(&text)
        .with_context(|| format!("parse config '{}': failed", path.display()))
}

fn init_logging(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}

fn validate_config(config: &Config) -> Result<()> {
    let mut names = HashSet::new();
    for server in &config.servers {
        if server.name.is_empty() {
            bail!("server name cannot be empty");
        }
        if server.name.contains("__") {
            bail!("server name '{}' cannot contain '__'", server.name);
        }
        if server.url.is_empty() {
            bail!("server '{}' url cannot be empty", server.name);
        }
        if !names.insert(server.name.clone()) {
            bail!("duplicate server name '{}'", server.name);
        }
    }
    Ok(())
}

async fn server_names(state: &AppState) -> Vec<String> {
    state
        .registry
        .read()
        .await
        .servers
        .keys()
        .cloned()
        .collect()
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        error!(%err, "failed to install ctrl-c handler");
    }
}

async fn manage_server(state: AppState, name: String) {
    let mut session_id: Option<String> = None;
    let mut failures = 0_u32;

    loop {
        let config = match get_server_config(&state, &name).await {
            Some(config) => config,
            None => return,
        };

        let result = if let Some(session) = session_id.as_deref() {
            load_tools(&state.client, &config, Some(session)).await
        } else {
            initialize_and_load_tools(&state.client, &config).await
        };

        match result {
            Ok((next_session_id, tools)) => {
                failures = 0;
                if next_session_id.is_some() {
                    session_id = next_session_id;
                }
                set_server_connected(&state, &name, session_id.clone(), tools).await;
                debug!(server = %name, "upstream healthy");
                tokio::time::sleep(Duration::from_secs(config.tools.ttl_seconds.max(1))).await;
            }
            Err(err) => {
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
                tokio::time::sleep(backoff_delay(&config.reconnect, failures)).await;
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

async fn initialize_and_load_tools(
    client: &Client,
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
    let (session_id, _) =
        upstream_request(client, config, None, json!(1), "initialize", params).await?;
    let _ = upstream_notification(
        client,
        config,
        session_id.as_deref(),
        "notifications/initialized",
        Value::Null,
    )
    .await;
    load_tools(client, config, session_id.as_deref()).await
}

async fn load_tools(
    client: &Client,
    config: &ServerConfig,
    session_id: Option<&str>,
) -> Result<(Option<String>, Vec<Value>)> {
    let (next_session_id, result) = upstream_request(
        client,
        config,
        session_id,
        json!(2),
        "tools/list",
        json!({}),
    )
    .await?;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "upstream '{}' tools/list returned no tools array",
                config.name
            )
        })?;
    Ok((
        next_session_id.or_else(|| session_id.map(ToOwned::to_owned)),
        tools,
    ))
}

async fn upstream_request(
    client: &Client,
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
    let (next_session_id, response) = send_upstream_body(client, config, session_id, body).await?;
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
    client: &Client,
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
    send_upstream_body(client, config, session_id, Value::Object(object)).await?;
    Ok(())
}

async fn send_upstream_body(
    client: &Client,
    config: &ServerConfig,
    session_id: Option<&str>,
    body: Value,
) -> Result<(Option<String>, Option<Value>)> {
    let mut request = client
        .post(&config.url)
        .header(CONTENT_TYPE.as_str(), "application/json")
        .header(ACCEPT.as_str(), config.accept_header())
        .json(&body);

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
    parse_upstream_response(config, response).await
}

async fn parse_upstream_response(
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

fn parse_sse_body(text: &str) -> Result<Value> {
    let mut events = Vec::new();
    let mut data_lines = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            push_sse_event(&mut events, &mut data_lines);
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_string());
        }
    }
    push_sse_event(&mut events, &mut data_lines);

    for event in events {
        let data = event.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        return serde_json::from_str(data).context("SSE data is not valid JSON");
    }

    bail!("SSE response had no data event")
}

fn push_sse_event(events: &mut Vec<String>, data_lines: &mut Vec<String>) {
    if !data_lines.is_empty() {
        events.push(data_lines.join("\n"));
        data_lines.clear();
    }
}

async fn post_mcp(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
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
        &state.client,
        &config,
        session_id.as_deref(),
        json!(3),
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

async fn get_mcp() -> impl IntoResponse {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        "server notifications are not implemented",
    )
}

async fn delete_mcp(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(session_id) = headers
        .get(MCP_SESSION_ID)
        .and_then(|value| value.to_str().ok())
    {
        state.sessions.write().await.remove(session_id);
    }
    StatusCode::ACCEPTED
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let registry = state.registry.read().await;
    let mut degraded = false;
    let mut servers = Vec::new();

    for (name, server) in &registry.servers {
        if !server.connected {
            degraded = true;
        }
        servers.push(HealthServer {
            name: name.clone(),
            url: server.config.url.clone(),
            protocol: server.config.protocol.clone(),
            required: server.config.required,
            connected: server.connected,
            tools_count: server.tools.len(),
            last_seen_seconds_ago: server.last_seen.map(|seen| seen.elapsed().as_secs_f64()),
            reconnects: server.reconnects,
            last_error: server.last_error.clone(),
        });
    }

    Json(HealthResponse {
        status: if degraded { "degraded" } else { "ok" }.to_string(),
        uptime_seconds: state.start.elapsed().as_secs_f64(),
        servers,
    })
}

async fn metrics(State(state): State<AppState>) -> Response {
    if !state.metrics_enabled {
        return (StatusCode::NOT_FOUND, "metrics disabled\n").into_response();
    }

    let registry = state.registry.read().await;
    let metrics = state.metrics.lock().await;
    let mut output = String::new();

    output.push_str("# HELP mcpstead_upstream_connected Upstream connection state.\n");
    output.push_str("# TYPE mcpstead_upstream_connected gauge\n");
    for (name, server) in &registry.servers {
        output.push_str(&format!(
            "mcpstead_upstream_connected{{server=\"{}\"}} {}\n",
            escape_label(name),
            u8::from(server.connected)
        ));
    }

    output.push_str("# HELP mcpstead_upstream_tools_count Cached upstream tool count.\n");
    output.push_str("# TYPE mcpstead_upstream_tools_count gauge\n");
    for (name, server) in &registry.servers {
        output.push_str(&format!(
            "mcpstead_upstream_tools_count{{server=\"{}\"}} {}\n",
            escape_label(name),
            server.tools.len()
        ));
    }

    output.push_str("# HELP mcpstead_upstream_reconnects_total Upstream reconnect attempts.\n");
    output.push_str("# TYPE mcpstead_upstream_reconnects_total counter\n");
    for (name, server) in &registry.servers {
        output.push_str(&format!(
            "mcpstead_upstream_reconnects_total{{server=\"{}\"}} {}\n",
            escape_label(name),
            server.reconnects
        ));
    }

    output.push_str("# HELP mcpstead_upstream_last_seen_seconds Seconds since upstream success.\n");
    output.push_str("# TYPE mcpstead_upstream_last_seen_seconds gauge\n");
    for (name, server) in &registry.servers {
        let seconds = server
            .last_seen
            .map(|seen| seen.elapsed().as_secs_f64())
            .unwrap_or(-1.0);
        output.push_str(&format!(
            "mcpstead_upstream_last_seen_seconds{{server=\"{}\"}} {:.6}\n",
            escape_label(name),
            seconds
        ));
    }

    output.push_str("# HELP mcpstead_downstream_sessions_active Active downstream sessions.\n");
    output.push_str("# TYPE mcpstead_downstream_sessions_active gauge\n");
    output.push_str(&format!(
        "mcpstead_downstream_sessions_active {}\n",
        state.sessions.read().await.len()
    ));

    output.push_str("# HELP mcpstead_tool_calls_total Tool calls routed upstream.\n");
    output.push_str("# TYPE mcpstead_tool_calls_total counter\n");
    for ((server, tool), metric) in &metrics.tool_calls {
        output.push_str(&format!(
            "mcpstead_tool_calls_total{{server=\"{}\",tool=\"{}\"}} {}\n",
            escape_label(server),
            escape_label(tool),
            metric.count
        ));
    }

    output.push_str("# HELP mcpstead_tool_call_errors_total Tool call errors.\n");
    output.push_str("# TYPE mcpstead_tool_call_errors_total counter\n");
    for ((server, tool), metric) in &metrics.tool_calls {
        output.push_str(&format!(
            "mcpstead_tool_call_errors_total{{server=\"{}\",tool=\"{}\"}} {}\n",
            escape_label(server),
            escape_label(tool),
            metric.errors
        ));
    }

    output.push_str("# HELP mcpstead_tool_call_duration_seconds_sum Total tool call duration.\n");
    output.push_str("# TYPE mcpstead_tool_call_duration_seconds_sum counter\n");
    for ((server, tool), metric) in &metrics.tool_calls {
        output.push_str(&format!(
            "mcpstead_tool_call_duration_seconds_sum{{server=\"{}\",tool=\"{}\"}} {:.6}\n",
            escape_label(server),
            escape_label(tool),
            metric.duration_sum_seconds
        ));
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(Body::from(output))
        .expect("response build failed")
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
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
    fn namespaces_tools() {
        let tool = json!({"name":"search","inputSchema":{"type":"object"}});
        let namespaced = namespaced_tool("writestead", &tool).unwrap();
        assert_eq!(namespaced["name"], "writestead__search");
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
