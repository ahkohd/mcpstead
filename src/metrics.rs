use std::{fs, sync::atomic::Ordering};

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::state::{AppState, DURATION_BUCKETS, HistogramMetric, SESSION_DURATION_BUCKETS};

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
    if !state.metrics_enabled.load(Ordering::Relaxed) {
        return (StatusCode::NOT_FOUND, "metrics disabled\n").into_response();
    }

    let registry = state.registry.read().await;
    let session_count = state.sessions.read().await.len();
    let metrics = state.metrics.lock().await;
    let mut output = String::new();

    output.push_str("# HELP mcpstead_build_info Build and version info.\n");
    output.push_str("# TYPE mcpstead_build_info gauge\n");
    output.push_str(&format!(
        "mcpstead_build_info{{version=\"{}\",rust_version=\"{}\",git_sha=\"{}\"}} 1\n",
        escape_label(env!("CARGO_PKG_VERSION")),
        escape_label(option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("unknown")),
        escape_label(option_env!("GITHUB_SHA").unwrap_or("unknown"))
    ));

    output.push_str("# HELP mcpstead_start_time_seconds Unix timestamp for process start.\n");
    output.push_str("# TYPE mcpstead_start_time_seconds gauge\n");
    output.push_str(&format!(
        "mcpstead_start_time_seconds {:.6}\n",
        state.start_unix_seconds
    ));

    output.push_str("# HELP mcpstead_uptime_seconds Process uptime in seconds.\n");
    output.push_str("# TYPE mcpstead_uptime_seconds gauge\n");
    output.push_str(&format!(
        "mcpstead_uptime_seconds {:.6}\n",
        state.start.elapsed().as_secs_f64()
    ));

    append_process_metrics(&mut output);

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

    output.push_str("# HELP mcpstead_upstream_reconnects_total Upstream reconnect cycles.\n");
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
        session_count
    ));

    output.push_str("# HELP mcpstead_downstream_sessions_total Downstream sessions opened.\n");
    output.push_str("# TYPE mcpstead_downstream_sessions_total counter\n");
    output.push_str(&format!(
        "mcpstead_downstream_sessions_total {}\n",
        metrics.downstream_sessions_total
    ));

    write_histogram(
        &mut output,
        "mcpstead_downstream_session_duration_seconds",
        "Downstream session duration.",
        &[],
        &metrics.downstream_session_duration,
        SESSION_DURATION_BUCKETS,
    );

    output.push_str(
        "# HELP mcpstead_downstream_session_terminations_total Downstream session terminations.\n",
    );
    output.push_str("# TYPE mcpstead_downstream_session_terminations_total counter\n");
    for (reason, value) in &metrics.downstream_session_terminations {
        output.push_str(&format!(
            "mcpstead_downstream_session_terminations_total{{reason=\"{}\"}} {}\n",
            escape_label(reason),
            value
        ));
    }

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

    output.push_str("# HELP mcpstead_tool_call_errors_total Tool call errors by reason.\n");
    output.push_str("# TYPE mcpstead_tool_call_errors_total counter\n");
    for ((server, tool), metric) in &metrics.tool_calls {
        for (reason, value) in &metric.errors_by_reason {
            output.push_str(&format!(
                "mcpstead_tool_call_errors_total{{server=\"{}\",tool=\"{}\",reason=\"{}\"}} {}\n",
                escape_label(server),
                escape_label(tool),
                escape_label(reason),
                value
            ));
        }
    }

    for ((server, tool), metric) in &metrics.tool_calls {
        write_histogram(
            &mut output,
            "mcpstead_tool_call_duration_seconds",
            "Tool call duration.",
            &[("server", server), ("tool", tool)],
            &metric.duration,
            DURATION_BUCKETS,
        );
    }

    output.push_str("# HELP mcpstead_mcp_requests_total MCP requests by method and result.\n");
    output.push_str("# TYPE mcpstead_mcp_requests_total counter\n");
    for ((method, result), value) in &metrics.mcp_requests {
        output.push_str(&format!(
            "mcpstead_mcp_requests_total{{method=\"{}\",result=\"{}\"}} {}\n",
            escape_label(method),
            escape_label(result),
            value
        ));
    }

    for (method, metric) in &metrics.mcp_request_duration {
        write_histogram(
            &mut output,
            "mcpstead_mcp_request_duration_seconds",
            "MCP request duration.",
            &[("method", method)],
            metric,
            DURATION_BUCKETS,
        );
    }

    output.push_str("# HELP mcpstead_upstream_initialize_total Upstream initialize attempts.\n");
    output.push_str("# TYPE mcpstead_upstream_initialize_total counter\n");
    for ((server, result), value) in &metrics.upstream_initialize {
        output.push_str(&format!(
            "mcpstead_upstream_initialize_total{{server=\"{}\",result=\"{}\"}} {}\n",
            escape_label(server),
            escape_label(result),
            value
        ));
    }

    for (server, metric) in &metrics.upstream_initialize_duration {
        write_histogram(
            &mut output,
            "mcpstead_upstream_initialize_duration_seconds",
            "Upstream initialize duration.",
            &[("server", server)],
            metric,
            DURATION_BUCKETS,
        );
    }

    output
        .push_str("# HELP mcpstead_upstream_health_checks_total Upstream health check outcomes.\n");
    output.push_str("# TYPE mcpstead_upstream_health_checks_total counter\n");
    for ((server, result), value) in &metrics.upstream_health_checks {
        output.push_str(&format!(
            "mcpstead_upstream_health_checks_total{{server=\"{}\",result=\"{}\"}} {}\n",
            escape_label(server),
            escape_label(result),
            value
        ));
    }

    output.push_str("# HELP mcpstead_upstream_reconnect_attempts_total Upstream reconnect attempts inside reconnect cycles.\n");
    output.push_str("# TYPE mcpstead_upstream_reconnect_attempts_total counter\n");
    for ((server, result), value) in &metrics.upstream_reconnect_attempts {
        output.push_str(&format!(
            "mcpstead_upstream_reconnect_attempts_total{{server=\"{}\",result=\"{}\"}} {}\n",
            escape_label(server),
            escape_label(result),
            value
        ));
    }

    output.push_str("# HELP mcpstead_upstream_backoff_seconds_total Seconds spent in upstream reconnect backoff.\n");
    output.push_str("# TYPE mcpstead_upstream_backoff_seconds_total counter\n");
    for (server, value) in &metrics.upstream_backoff_seconds_total {
        output.push_str(&format!(
            "mcpstead_upstream_backoff_seconds_total{{server=\"{}\"}} {:.6}\n",
            escape_label(server),
            value
        ));
    }

    output.push_str(
        "# HELP mcpstead_upstream_in_backoff Whether upstream is currently in reconnect backoff.\n",
    );
    output.push_str("# TYPE mcpstead_upstream_in_backoff gauge\n");
    for (server, value) in &metrics.upstream_in_backoff {
        output.push_str(&format!(
            "mcpstead_upstream_in_backoff{{server=\"{}\"}} {}\n",
            escape_label(server),
            value
        ));
    }

    output.push_str(
        "# HELP mcpstead_upstream_current_backoff_seconds Current upstream backoff delay.\n",
    );
    output.push_str("# TYPE mcpstead_upstream_current_backoff_seconds gauge\n");
    for (server, value) in &metrics.upstream_current_backoff_seconds {
        output.push_str(&format!(
            "mcpstead_upstream_current_backoff_seconds{{server=\"{}\"}} {:.6}\n",
            escape_label(server),
            value
        ));
    }

    output.push_str("# HELP mcpstead_upstream_session_resets_total Upstream session resets after session invalidation.\n");
    output.push_str("# TYPE mcpstead_upstream_session_resets_total counter\n");
    for ((server, reason), value) in &metrics.upstream_session_resets {
        output.push_str(&format!(
            "mcpstead_upstream_session_resets_total{{server=\"{}\",reason=\"{}\"}} {}\n",
            escape_label(server),
            escape_label(reason),
            value
        ));
    }

    output.push_str("# HELP mcpstead_upstream_tools_refresh_total Upstream tools/list cache refresh outcomes.\n");
    output.push_str("# TYPE mcpstead_upstream_tools_refresh_total counter\n");
    for ((server, result), value) in &metrics.upstream_tools_refresh {
        output.push_str(&format!(
            "mcpstead_upstream_tools_refresh_total{{server=\"{}\",result=\"{}\"}} {}\n",
            escape_label(server),
            escape_label(result),
            value
        ));
    }

    for (server, metric) in &metrics.upstream_tools_refresh_duration {
        write_histogram(
            &mut output,
            "mcpstead_upstream_tools_refresh_duration_seconds",
            "Upstream tools/list cache refresh duration.",
            &[("server", server)],
            metric,
            DURATION_BUCKETS,
        );
    }

    output.push_str("# HELP mcpstead_upstream_tools_last_refresh_timestamp_seconds Last successful tool cache refresh timestamp.\n");
    output.push_str("# TYPE mcpstead_upstream_tools_last_refresh_timestamp_seconds gauge\n");
    for (server, value) in &metrics.upstream_tools_last_refresh_timestamp_seconds {
        output.push_str(&format!(
            "mcpstead_upstream_tools_last_refresh_timestamp_seconds{{server=\"{}\"}} {:.6}\n",
            escape_label(server),
            value
        ));
    }

    output.push_str("# HELP mcpstead_mcp_auth_attempts_total MCP auth attempts.\n");
    output.push_str("# TYPE mcpstead_mcp_auth_attempts_total counter\n");
    for (result, value) in &metrics.mcp_auth_attempts {
        output.push_str(&format!(
            "mcpstead_mcp_auth_attempts_total{{result=\"{}\"}} {}\n",
            escape_label(result),
            value
        ));
    }

    output.push_str("# HELP mcpstead_mcp_auth_failures_total MCP auth failures by reason.\n");
    output.push_str("# TYPE mcpstead_mcp_auth_failures_total counter\n");
    for (reason, value) in &metrics.mcp_auth_failures {
        output.push_str(&format!(
            "mcpstead_mcp_auth_failures_total{{reason=\"{}\"}} {}\n",
            escape_label(reason),
            value
        ));
    }

    output.push_str("# HELP mcpstead_config_reloads_total Config reload attempts.\n");
    output.push_str("# TYPE mcpstead_config_reloads_total counter\n");
    for (result, value) in &metrics.config_reloads {
        output.push_str(&format!(
            "mcpstead_config_reloads_total{{result=\"{}\"}} {}\n",
            escape_label(result),
            value
        ));
    }

    output.push_str("# HELP mcpstead_config_last_reload_timestamp_seconds Last successful config reload timestamp.\n");
    output.push_str("# TYPE mcpstead_config_last_reload_timestamp_seconds gauge\n");
    if let Some(value) = metrics.config_last_reload_timestamp_seconds {
        output.push_str(&format!(
            "mcpstead_config_last_reload_timestamp_seconds {:.6}\n",
            value
        ));
    }

    output.push_str("# HELP mcpstead_upstream_bytes_total Bytes exchanged with upstreams.\n");
    output.push_str("# TYPE mcpstead_upstream_bytes_total counter\n");
    for ((server, direction), value) in &metrics.upstream_bytes {
        output.push_str(&format!(
            "mcpstead_upstream_bytes_total{{server=\"{}\",direction=\"{}\"}} {}\n",
            escape_label(server),
            escape_label(direction),
            value
        ));
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(Body::from(output))
        .expect("response build failed")
}

fn write_histogram(
    output: &mut String,
    name: &str,
    help: &str,
    labels: &[(&str, &str)],
    metric: &HistogramMetric,
    buckets: &[f64],
) {
    output.push_str(&format!("# HELP {name} {help}\n"));
    output.push_str(&format!("# TYPE {name} histogram\n"));
    for (index, bucket) in buckets.iter().enumerate() {
        let count = metric.buckets.get(index).copied().unwrap_or(0);
        let le = format_bucket(*bucket);
        output.push_str(&format!(
            "{name}_bucket{} {}\n",
            format_labels(labels, Some(("le", &le))),
            count
        ));
    }
    let count = metric.buckets.get(buckets.len()).copied().unwrap_or(0);
    output.push_str(&format!(
        "{name}_bucket{} {}\n",
        format_labels(labels, Some(("le", "+Inf"))),
        count
    ));
    output.push_str(&format!(
        "{name}_sum{} {:.6}\n",
        format_labels(labels, None),
        metric.sum
    ));
    output.push_str(&format!(
        "{name}_count{} {}\n",
        format_labels(labels, None),
        metric.count
    ));
}

fn format_bucket(value: f64) -> String {
    let mut text = format!("{value:.6}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

fn format_labels(labels: &[(&str, &str)], extra: Option<(&str, &str)>) -> String {
    if labels.is_empty() && extra.is_none() {
        return String::new();
    }

    let mut parts = Vec::new();
    for (key, value) in labels {
        parts.push(format!("{key}=\"{}\"", escape_label(value)));
    }
    if let Some((key, value)) = extra {
        parts.push(format!("{key}=\"{}\"", escape_label(value)));
    }
    format!("{{{}}}", parts.join(","))
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn append_process_metrics(output: &mut String) {
    let Some(stats) = process_stats() else {
        return;
    };

    output
        .push_str("# HELP mcpstead_process_resident_memory_bytes Resident memory size in bytes.\n");
    output.push_str("# TYPE mcpstead_process_resident_memory_bytes gauge\n");
    output.push_str(&format!(
        "mcpstead_process_resident_memory_bytes {}\n",
        stats.resident_memory_bytes
    ));

    output.push_str("# HELP mcpstead_process_virtual_memory_bytes Virtual memory size in bytes.\n");
    output.push_str("# TYPE mcpstead_process_virtual_memory_bytes gauge\n");
    output.push_str(&format!(
        "mcpstead_process_virtual_memory_bytes {}\n",
        stats.virtual_memory_bytes
    ));

    output.push_str("# HELP mcpstead_process_cpu_seconds_total Process CPU seconds.\n");
    output.push_str("# TYPE mcpstead_process_cpu_seconds_total counter\n");
    output.push_str(&format!(
        "mcpstead_process_cpu_seconds_total {:.6}\n",
        stats.cpu_seconds_total
    ));

    output.push_str("# HELP mcpstead_process_open_fds Open file descriptors.\n");
    output.push_str("# TYPE mcpstead_process_open_fds gauge\n");
    output.push_str(&format!("mcpstead_process_open_fds {}\n", stats.open_fds));

    output.push_str("# HELP mcpstead_process_max_fds Maximum file descriptors.\n");
    output.push_str("# TYPE mcpstead_process_max_fds gauge\n");
    output.push_str(&format!("mcpstead_process_max_fds {}\n", stats.max_fds));

    output.push_str("# HELP mcpstead_process_threads Process thread count.\n");
    output.push_str("# TYPE mcpstead_process_threads gauge\n");
    output.push_str(&format!("mcpstead_process_threads {}\n", stats.threads));
}

struct ProcessStats {
    resident_memory_bytes: u64,
    virtual_memory_bytes: u64,
    cpu_seconds_total: f64,
    open_fds: u64,
    max_fds: u64,
    threads: u64,
}

#[cfg(target_os = "linux")]
fn process_stats() -> Option<ProcessStats> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime_ticks = fields.get(11)?.parse::<u64>().ok()?;
    let stime_ticks = fields.get(12)?.parse::<u64>().ok()?;
    let threads = fields.get(17)?.parse::<u64>().ok()?;
    let virtual_memory_bytes = fields.get(20)?.parse::<u64>().ok()?;
    let resident_pages = fields.get(21)?.parse::<i64>().ok()?.max(0) as u64;
    let resident_memory_bytes = resident_pages.saturating_mul(4096);
    let cpu_seconds_total = (utime_ticks + stime_ticks) as f64 / 100.0;
    let open_fds = fs::read_dir("/proc/self/fd")
        .map(|fds| fds.count() as u64)
        .unwrap_or(0);
    let max_fds = read_max_fds().unwrap_or(0);

    Some(ProcessStats {
        resident_memory_bytes,
        virtual_memory_bytes,
        cpu_seconds_total,
        open_fds,
        max_fds,
        threads,
    })
}

#[cfg(not(target_os = "linux"))]
fn process_stats() -> Option<ProcessStats> {
    None
}

#[cfg(target_os = "linux")]
fn read_max_fds() -> Option<u64> {
    for line in fs::read_to_string("/proc/self/limits").ok()?.lines() {
        if line.starts_with("Max open files") {
            return line
                .split_whitespace()
                .nth(3)
                .and_then(|value| value.parse::<u64>().ok());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_prometheus_labels() {
        assert_eq!(escape_label("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }

    #[test]
    fn formats_histogram_labels() {
        assert_eq!(
            format_labels(&[("server", "s1")], Some(("le", "1"))),
            "{server=\"s1\",le=\"1\"}"
        );
    }
}
