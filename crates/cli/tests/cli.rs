use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn help_command_is_available() {
    let output = Command::new(env!("CARGO_BIN_EXE_rust-codingagent"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("南开 Rust 编程助手命令行"));
}

#[test]
fn nku_agent_alias_is_available() {
    let output = Command::new(env!("CARGO_BIN_EXE_nku-agent"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("南开 Rust 编程助手命令行"));
}

#[test]
fn config_command_reads_toml_file() {
    let dir = std::env::temp_dir().join(unique_name("rust-codingagent-cli-test"));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("rust-codingagent.toml");
    std::fs::write(
        &config_path,
        r#"
profile = "integration"
workspace = "/tmp/rust-codingagent-integration"
log_level = "warn"

[provider]
name = "local-test"
model = "mock-model"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_rust-codingagent"))
        .env_remove("RUST_CODINGAGENT_PROVIDER")
        .env_remove("RUST_CODINGAGENT_MODEL")
        .env_remove("RUST_CODINGAGENT_API_KEY")
        .args(["--config", config_path.to_str().unwrap(), "config"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("profile = \"integration\""));
    assert!(stdout.contains("model = \"mock-model\""));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn run_command_enters_main_loop() {
    let workspace = std::env::temp_dir().join(unique_name("rust-codingagent-cli-run"));
    std::fs::create_dir_all(&workspace).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_rust-codingagent"))
        .env("RUST_CODINGAGENT_API_KEY", "sk-test")
        .env("RUST_CODINGAGENT_WORKSPACE", &workspace)
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all("退出\n".as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("南开 Rust 编程助手"));
    assert!(stdout.contains("已退出。"));

    let _ = std::fs::remove_dir_all(&workspace);
}

fn unique_name(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{nanos}")
}
