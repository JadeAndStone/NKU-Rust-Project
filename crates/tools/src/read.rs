use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use rust_codingagent_core::AgentContext;

use crate::path::resolve_existing_path;
use crate::tool::{truncate_to_bytes, Tool, ToolInput, ToolOutput};

#[derive(Debug, Default, Clone, Copy)]
pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn run(&self, context: &AgentContext, input: ToolInput) -> Result<ToolOutput> {
        let ToolInput::Read { path, max_bytes } = input else {
            bail!("read tool received non-read input");
        };
        run(context, path, max_bytes)
    }
}

pub(crate) fn run(
    context: &AgentContext,
    path: PathBuf,
    max_bytes: Option<usize>,
) -> Result<ToolOutput> {
    let path = resolve_existing_path(context, &path)?;
    if !path.is_file() {
        bail!("read target is not a file: {}", path.display());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read file {}", path.display()))?;
    let bytes = content.len();
    let (content, truncated) = truncate_to_bytes(content, max_bytes);

    Ok(ToolOutput::Read {
        path,
        content,
        bytes,
        truncated,
    })
}
