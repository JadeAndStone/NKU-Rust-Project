use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use rust_codingagent_core::AgentContext;

use crate::path::resolve_existing_path;
use crate::tool::{truncate_to_bytes, ToolOutput};

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
