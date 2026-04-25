# mcpstead

[![CI](https://github.com/ahkohd/mcpstead/actions/workflows/ci.yml/badge.svg)](https://github.com/ahkohd/mcpstead/actions/workflows/ci.yml) [![npm version](https://img.shields.io/npm/v/@ahkohd/mcpstead.svg)](https://www.npmjs.com/package/@ahkohd/mcpstead) [![Crates.io](https://img.shields.io/crates/v/mcpstead.svg)](https://crates.io/crates/mcpstead) [![License](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

MCP Gateway

One downstream `/mcp` endpoint that fronts many upstream MCP servers. Persistent upstream connections with reconnect, tool registry with qualified names, JSON or SSE responses based on client `Accept`, per-upstream auth, Prometheus metrics.

## Install

```bash
# npm (macOS, Linux, WSL)
npm i -g @ahkohd/mcpstead

# homebrew (macOS, Linux)
brew install ahkohd/tap/mcpstead

# cargo
cargo install mcpstead --locked --force

# verify
mcpstead --version
```

## Quick start

```bash
# 1. write a config
mkdir -p ~/.config/mcpstead
cat > ~/.config/mcpstead/config.yaml <<'EOF'
host: 0.0.0.0
port: 8766

mcp:
  auth:
    mode: none

servers:
  - name: example
    url: http://127.0.0.1:3000/mcp
    protocol: streamable
    auth: none
EOF

# 2. run
mcpstead --config ~/.config/mcpstead/config.yaml
```

Then point any MCP client at `http://127.0.0.1:8766/mcp`.

## Docker

```bash
docker build -t mcpstead .
docker run --rm \
  -p 8766:8766 \
  -v "$PWD/config:/etc/mcpstead:ro" \
  mcpstead
```

The Dockerfile installs from crates.io.

## HTTP API

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/mcp` | MCP over HTTP JSON-RPC |
| `GET` | `/mcp` | returns 405 |
| `DELETE` | `/mcp` | terminate downstream session |
| `GET` | `/health` | upstream status, tool counts, last-seen, reconnects |
| `GET` | `/metrics` | Prometheus text format |

## MCP client setup

Local, no-auth:

```yaml
mcpstead:
  url: http://127.0.0.1:8766/mcp
  tools:
    resources: false
    prompts: false
```

Bearer auth:

```yaml
mcpstead:
  url: http://127.0.0.1:8766/mcp
  headers:
    Authorization: Bearer ${MCPSTEAD_BEARER_TOKEN}
  tools:
    resources: false
    prompts: false
```

Tools appear with qualified names - `<server>__<tool>` - so multiple upstreams can ship overlapping tool names without collision.

## Auth

### Downstream (clients to mcpstead)

Default is no auth:

```yaml
mcp:
  auth:
    mode: none
```

Bearer auth:

```yaml
mcp:
  auth:
    mode: bearer
```

Token comes from env:

```bash
export MCPSTEAD_BEARER_TOKEN='replace-with-strong-secret'
mcpstead --config ~/.config/mcpstead/config.yaml
```

Clients send `Authorization: Bearer <token>`. Missing or wrong token returns 401. `mcp.auth.bearer_token` in config is rejected at startup.

`/health` and `/metrics` stay open regardless of auth mode - for monitoring without exposing the token.

### Upstream (mcpstead to MCP servers)

Per-server in the `servers` list. Three modes:

```yaml
servers:
  - name: local
    url: http://127.0.0.1:3000/mcp
    auth: none

  - name: workflow
    url: https://workflow.example/mcp-server/http
    auth:
      type: bearer
      token_env: WORKFLOW_TOKEN

  - name: custom
    url: https://api.example/mcp
    headers:
      X-API-Key: '${EXAMPLE_KEY}'
```

`token_env` resolves the named env var at startup. Avoid raw upstream tokens in committed config.

## Config

Default path: `~/.config/mcpstead/config.yaml` (or `--config <path>`).

```yaml
host: 0.0.0.0
port: 8766

mcp:
  auth:
    mode: none           # none | bearer

servers:
  - name: local
    url: http://127.0.0.1:3000/mcp
    protocol: streamable # streamable | sse | auto
    required: false      # if true, gateway won't start without this upstream
    auth: none
    reconnect:
      max_attempts: 0    # 0 = infinite
      backoff_base_ms: 1000
      backoff_max_ms: 30000
    tools:
      ttl_seconds: 300
    tls_skip_verify: false
    quirks:
      normalize_sse_events: true
      inject_accept_header: 'application/json, text/event-stream'

metrics:
  enabled: true

logging:
  level: info
```

### Per-upstream config keys

- `name` - required, used as tool prefix
- `url` - required, MCP endpoint
- `protocol` - `streamable | sse | auto` (default `auto`)
- `required` - block startup if upstream fails to initialize (default `false`)
- `auth` - `none`, `bearer` (with `token` or `token_env`), or `headers` map
- `reconnect.max_attempts` - `0` = infinite (default)
- `reconnect.backoff_base_ms` / `backoff_max_ms` - exponential backoff bounds
- `tools.ttl_seconds` - refresh cached `tools/list` after this interval
- `tls_skip_verify` - disable TLS cert checks for this upstream (default `false`, use only for trusted local nets)
- `quirks.normalize_sse_events` - strip `event:` lines from upstream SSE responses
- `quirks.inject_accept_header` - override the Accept header sent upstream

## Observability

### Metrics

`/metrics` exposes Prometheus-format counters and gauges:

```
mcpstead_upstream_connected{server="..."}
mcpstead_upstream_tools_count{server="..."}
mcpstead_upstream_reconnects_total{server="..."}
mcpstead_upstream_last_seen_seconds{server="..."}
mcpstead_downstream_sessions_active
mcpstead_tool_calls_total{server="...",tool="..."}
mcpstead_tool_call_errors_total{server="...",tool="..."}
mcpstead_tool_call_duration_seconds_sum{server="...",tool="..."}
```

### Scrape config

```yaml
- job_name: mcpstead
  metrics_path: /metrics
  static_configs:
    - targets: ['mcpstead:8766']
```

### Health check

```bash
curl http://127.0.0.1:8766/health
```

Returns JSON: per-upstream connection state, tool counts, last successful contact, reconnect counts, last error.

## Troubleshooting

- **All tools list empty** - at least one upstream failed to `initialize`. Check `/health` for per-server status; check upstream URL, auth, and reachability.
- **Sporadic `SSE parse failed`** - upstream sends an SSE dialect mcpstead doesn't recognize. Try `quirks.normalize_sse_events: true` for that server, or set `quirks.inject_accept_header: 'application/json'` to force JSON.
- **`tools/call` returns auth error** - upstream rejected the bearer token. Confirm `token_env` resolves to the right value at startup; check upstream's expected header name.
- **TLS handshake errors against a self-signed upstream** - set `tls_skip_verify: true` on that server. Only safe on trusted local networks.
- **401 from `/mcp` with mode bearer** - client missing or sending wrong `Authorization: Bearer <token>`. Verify `MCPSTEAD_BEARER_TOKEN` matches what the client sends.
- **Upstream goes red in `/health` repeatedly** - check `mcpstead_upstream_reconnects_total` and `mcpstead_upstream_last_seen_seconds`. Tune `reconnect.backoff_max_ms` if the upstream needs longer recovery.
