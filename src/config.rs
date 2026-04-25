use std::{
    collections::{HashMap, HashSet},
    env,
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, de};
use tracing::warn;

const DEFAULT_ACCEPT: &str = "application/json, text/event-stream";

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct Config {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) mcp: McpConfig,
    pub(crate) servers: Vec<ServerConfig>,
    pub(crate) metrics: MetricsConfig,
    pub(crate) logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8766,
            mcp: McpConfig::default(),
            servers: Vec::new(),
            metrics: MetricsConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(default)]
pub(crate) struct McpConfig {
    pub(crate) auth: McpAuthConfig,
    pub(crate) session: McpSessionConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct McpAuthConfig {
    pub(crate) mode: McpAuthMode,
    pub(crate) bearer_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct McpSessionConfig {
    pub(crate) idle_ttl_seconds: u64,
    pub(crate) gc_interval_seconds: u64,
    pub(crate) shutdown_grace_seconds: u64,
}

impl Default for McpSessionConfig {
    fn default() -> Self {
        Self {
            idle_ttl_seconds: 3600,
            gc_interval_seconds: 60,
            shutdown_grace_seconds: 5,
        }
    }
}

impl Default for McpAuthConfig {
    fn default() -> Self {
        Self {
            mode: McpAuthMode::None,
            bearer_token: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum McpAuthMode {
    None,
    Bearer,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct ServerConfig {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) protocol: String,
    pub(crate) required: bool,
    pub(crate) tls_skip_verify: bool,
    auth: Option<AuthConfig>,
    pub(crate) headers: HashMap<String, String>,
    quirks: QuirksConfig,
    pub(crate) reconnect: ReconnectConfig,
    pub(crate) tools: ToolsConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: String::new(),
            protocol: "streamable".to_string(),
            required: false,
            tls_skip_verify: false,
            auth: None,
            headers: HashMap::new(),
            quirks: QuirksConfig::default(),
            reconnect: ReconnectConfig::default(),
            tools: ToolsConfig::default(),
        }
    }
}

impl ServerConfig {
    pub(crate) fn accept_header(&self) -> &str {
        self.quirks
            .inject_accept_header
            .as_deref()
            .unwrap_or(DEFAULT_ACCEPT)
    }

    pub(crate) fn requires_fresh_upstream_session(&self, other: &Self) -> bool {
        self.url != other.url
            || self.protocol != other.protocol
            || self.tls_skip_verify != other.tls_skip_verify
            || self.auth != other.auth
            || self.headers != other.headers
    }

    pub(crate) fn validate_auth_config(&self) -> Result<()> {
        match &self.auth {
            None => Ok(()),
            Some(AuthConfig::Mode(mode)) if mode == "none" => Ok(()),
            Some(AuthConfig::Mode(mode)) => bail!("unsupported auth mode '{mode}'"),
            Some(AuthConfig::Object(auth)) if auth.kind == "none" => Ok(()),
            Some(AuthConfig::Object(auth)) if auth.kind == "bearer" => {
                if auth.token_env.is_some() {
                    Ok(())
                } else {
                    bail!("bearer auth for server '{}' requires token_env", self.name)
                }
            }
            Some(AuthConfig::Object(auth)) => bail!("unsupported auth type '{}'", auth.kind),
        }
    }

    pub(crate) fn bearer_token(&self) -> Result<Option<String>> {
        match &self.auth {
            None => Ok(None),
            Some(AuthConfig::Mode(mode)) if mode == "none" => Ok(None),
            Some(AuthConfig::Mode(mode)) => bail!("unsupported auth mode '{mode}'"),
            Some(AuthConfig::Object(auth)) if auth.kind == "none" => Ok(None),
            Some(AuthConfig::Object(auth)) if auth.kind == "bearer" => {
                let Some(name) = &auth.token_env else {
                    bail!("bearer auth for server '{}' requires token_env", self.name);
                };
                Ok(Some(env::var(name).with_context(|| {
                    format!("missing env var '{name}' for server '{}'", self.name)
                })?))
            }
            Some(AuthConfig::Object(auth)) => bail!("unsupported auth type '{}'", auth.kind),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum AuthConfig {
    Mode(String),
    Object(AuthObject),
}

impl<'de> Deserialize<'de> for AuthConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        // Config is YAML-only today, so use serde_yaml::Value to reject raw token early.
        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::String(mode) => Ok(Self::Mode(mode)),
            serde_yaml::Value::Mapping(mapping) => {
                if mapping.contains_key(serde_yaml::Value::String("token".to_string())) {
                    return Err(de::Error::custom(
                        "upstream auth token field is not allowed; use token_env",
                    ));
                }
                let auth = serde_yaml::from_value(serde_yaml::Value::Mapping(mapping))
                    .map_err(de::Error::custom)?;
                Ok(Self::Object(auth))
            }
            _ => Err(de::Error::custom(
                "upstream auth must be 'none' or an object with type",
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct AuthObject {
    #[serde(rename = "type")]
    kind: String,
    token_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(default)]
struct QuirksConfig {
    normalize_sse_events: bool,
    inject_accept_header: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct ReconnectConfig {
    pub(crate) max_attempts: u32,
    pub(crate) backoff_base_ms: u64,
    pub(crate) backoff_max_ms: u64,
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct ToolsConfig {
    pub(crate) ttl_seconds: u64,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self { ttl_seconds: 300 }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct MetricsConfig {
    pub(crate) enabled: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct LoggingConfig {
    pub(crate) level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

pub(crate) async fn load_config(path: &PathBuf) -> Result<Config> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read config '{}': failed", path.display()))?;
    serde_yaml::from_str(&text)
        .with_context(|| format!("parse config '{}': failed", path.display()))
}

pub(crate) fn validate_config(config: &Config) -> Result<()> {
    if config.mcp.auth.bearer_token.is_some() {
        bail!("mcp.auth.bearer_token is not allowed; use MCPSTEAD_BEARER_TOKEN");
    }
    if config.mcp.session.idle_ttl_seconds == 0 {
        bail!("mcp.session.idle_ttl_seconds must be greater than 0");
    }
    if config.mcp.session.gc_interval_seconds == 0 {
        bail!("mcp.session.gc_interval_seconds must be greater than 0");
    }
    if config.mcp.session.shutdown_grace_seconds == 0 {
        bail!("mcp.session.shutdown_grace_seconds must be greater than 0");
    }
    if config.mcp.session.gc_interval_seconds > config.mcp.session.idle_ttl_seconds {
        warn!(
            idle_ttl_seconds = config.mcp.session.idle_ttl_seconds,
            gc_interval_seconds = config.mcp.session.gc_interval_seconds,
            "mcp.session.gc_interval_seconds is greater than idle_ttl_seconds; idle sessions may live past ttl"
        );
    }

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
        server.validate_auth_config()?;
    }
    Ok(())
}

pub(crate) fn load_mcp_bearer_token(config: &Config) -> Result<Option<String>> {
    match config.mcp.auth.mode {
        McpAuthMode::None => Ok(None),
        McpAuthMode::Bearer => Ok(Some(
            env::var("MCPSTEAD_BEARER_TOKEN")
                .context("MCPSTEAD_BEARER_TOKEN is required when mcp.auth.mode=bearer")?,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bearer_token_in_config_file() {
        let config: Config = serde_yaml::from_str(
            r#"
            mcp:
              auth:
                mode: bearer
                bearer_token: secret
            "#,
        )
        .unwrap();
        let err = validate_config(&config).unwrap_err();
        assert!(
            err.to_string()
                .contains("mcp.auth.bearer_token is not allowed")
        );
    }

    #[test]
    fn session_config_defaults() {
        let config: Config = serde_yaml::from_str("{}").unwrap();
        assert_eq!(config.mcp.session.idle_ttl_seconds, 3600);
        assert_eq!(config.mcp.session.gc_interval_seconds, 60);
        assert_eq!(config.mcp.session.shutdown_grace_seconds, 5);
    }

    #[test]
    fn rejects_zero_session_durations() {
        let config: Config = serde_yaml::from_str(
            r#"
            mcp:
              session:
                idle_ttl_seconds: 0
            "#,
        )
        .unwrap();
        let err = validate_config(&config).unwrap_err();
        assert!(
            err.to_string()
                .contains("mcp.session.idle_ttl_seconds must be greater than 0")
        );
    }

    #[test]
    fn rejects_upstream_bearer_without_token_env() {
        let config: Config = serde_yaml::from_str(
            r#"
            servers:
              - name: example
                url: http://127.0.0.1:3000/mcp
                auth:
                  type: bearer
            "#,
        )
        .unwrap();
        let err = validate_config(&config).unwrap_err();
        assert!(
            err.to_string()
                .contains("bearer auth for server 'example' requires token_env")
        );
    }

    #[test]
    fn resolves_upstream_bearer_token_from_env() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let env_name = format!(
            "MCPSTEAD_TEST_UPSTREAM_TOKEN_{}_{}",
            std::process::id(),
            stamp
        );
        let config: Config = serde_yaml::from_str(&format!(
            r#"
            servers:
              - name: example
                url: http://127.0.0.1:3000/mcp
                auth:
                  type: bearer
                  token_env: {env_name}
            "#,
        ))
        .unwrap();
        validate_config(&config).unwrap();
        unsafe {
            env::set_var(&env_name, "secret");
        }
        let token = config.servers[0].bearer_token().unwrap();
        unsafe {
            env::remove_var(&env_name);
        }
        assert_eq!(token.as_deref(), Some("secret"));
    }

    #[test]
    fn rejects_raw_upstream_bearer_token_field() {
        let err = serde_yaml::from_str::<Config>(
            r#"
            servers:
              - name: example
                url: http://127.0.0.1:3000/mcp
                auth:
                  type: bearer
                  token: raw-secret
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("upstream auth token field is not allowed; use token_env"),
            "{err}"
        );
    }
}
