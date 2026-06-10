use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use rust_codingagent_core::AgentContext;

use crate::path::{ensure_parent_dir, resolve_write_path};
use crate::tool::ToolOutput;

pub(crate) fn run(
    context: &AgentContext,
    path: PathBuf,
    content: String,
    overwrite: bool,
) -> Result<ToolOutput> {
    let path = resolve_write_path(context, &path)?;
    let existed = path.exists();

    if existed {
        if path.is_dir() {
            bail!("write target is a directory: {}", path.display());
        }
        if !overwrite {
            bail!("refusing to overwrite existing file {}", path.display());
        }
    }

    ensure_parent_dir(&path)?;
    fs::write(&path, content.as_bytes())
        .with_context(|| format!("failed to write file {}", path.display()))?;

    Ok(ToolOutput::Write {
        path,
        bytes: content.len(),
        created: !existed,
        overwritten: existed,
    })
}
