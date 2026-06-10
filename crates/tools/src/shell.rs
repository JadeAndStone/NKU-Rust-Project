use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rust_codingagent_core::AgentContext;

use crate::path::workspace_root;
use crate::tool::{truncate_to_bytes, ToolOutput};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub(crate) fn run(
    context: &AgentContext,
    command: String,
    timeout_ms: Option<u64>,
    max_output_bytes: Option<usize>,
) -> Result<ToolOutput> {
    if command.trim().is_empty() {
        bail!("shell command must not be empty");
    }

    let workspace = workspace_root(context)?;
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let max_output_bytes = max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);

    let mut child = shell_command(&command)
        .current_dir(&workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start shell command: {command}"))?;

    let started_at = Instant::now();
    let timed_out = loop {
        if child.try_wait()?.is_some() {
            break false;
        }

        if started_at.elapsed() >= timeout {
            let _ = child.kill();
            break true;
        }

        thread::sleep(Duration::from_millis(10));
    };

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to collect shell command output: {command}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let (stdout, stdout_truncated) = truncate_to_bytes(stdout, Some(max_output_bytes));
    let (stderr, stderr_truncated) = truncate_to_bytes(stderr, Some(max_output_bytes));

    Ok(ToolOutput::Shell {
        status_code: output.status.code(),
        stdout,
        stderr,
        timed_out,
        stdout_truncated,
        stderr_truncated,
    })
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("cmd");
    shell.args(["/C", command]);
    shell
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("sh");
    shell.args(["-c", command]);
    shell
}
