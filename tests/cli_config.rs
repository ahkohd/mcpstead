use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn temp_config(name: &str, body: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "mcpstead-{name}-{}-{stamp}.yaml",
        std::process::id()
    ));
    fs::write(&path, body).expect("write config");
    path
}

fn invalid_bind_config() -> &'static str {
    r#"
host: "not a host"
port: 8766
servers: []
"#
}

#[test]
fn missing_config_errors_clearly() {
    let output = Command::new(env!("CARGO_BIN_EXE_mcpstead"))
        .env_remove("MCPSTEAD_CONFIG")
        .output()
        .expect("run mcpstead");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mcpstead requires --config <path> or MCPSTEAD_CONFIG=<path>"),
        "{stderr}"
    );
}

#[test]
fn flag_config_loads() {
    let config = temp_config("flag", invalid_bind_config());
    let output = Command::new(env!("CARGO_BIN_EXE_mcpstead"))
        .env_remove("MCPSTEAD_CONFIG")
        .arg("--config")
        .arg(&config)
        .output()
        .expect("run mcpstead");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid bind address 'not a host:8766'"),
        "{stderr}"
    );
}

#[test]
fn env_config_loads() {
    let config = temp_config("env", invalid_bind_config());
    let output = Command::new(env!("CARGO_BIN_EXE_mcpstead"))
        .env("MCPSTEAD_CONFIG", &config)
        .output()
        .expect("run mcpstead");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid bind address 'not a host:8766'"),
        "{stderr}"
    );
}

#[test]
fn flag_wins_over_env_config() {
    let flag_config = temp_config("flag-wins", invalid_bind_config());
    let output = Command::new(env!("CARGO_BIN_EXE_mcpstead"))
        .env("MCPSTEAD_CONFIG", "/tmp/mcpstead-env-should-not-load.yaml")
        .arg("--config")
        .arg(&flag_config)
        .output()
        .expect("run mcpstead");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid bind address 'not a host:8766'"),
        "{stderr}"
    );
    assert!(!stderr.contains("mcpstead-env-should-not-load"), "{stderr}");
}
