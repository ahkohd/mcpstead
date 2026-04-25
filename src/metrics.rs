use axum::{
    Json,
    body::Body,
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub(crate) struct HealthResponse {
    status: String,
    uptime_seconds: f64,
    servers: Vec<HealthServer>,
}

#[derive(Serialize)]
pub(crate) struct HealthServer {
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

pub(crate) async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
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

pub(crate) async fn metrics(State(state): State<AppState>) -> Response {
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
    fn escapes_prometheus_labels() {
        assert_eq!(escape_label("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }
}
