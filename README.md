# MCPstead

MCP gateway for Moonstead tools.

## MVP

- One downstream endpoint: `POST /mcp`
- Health: `GET /health`
- Prometheus metrics: `GET /metrics`
- Upstream init, reconnect, and tool cache
- Tool namespace: `server__tool`
- Supported JSON-RPC methods:
  - `initialize`
  - `notifications/initialized`
  - `tools/list`
  - `tools/call`

## Run

```bash
cargo run -- --config config/mcpstead.yaml
```

## Test

```bash
cargo test
```

## Config

See `config/mcpstead.yaml`.

Each upstream can set bearer auth via `token_env`:

```yaml
auth:
  type: bearer
  token_env: RAINDROP_TOKEN
```

Avoid raw tokens in committed config.
