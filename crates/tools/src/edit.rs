use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use rust_codingagent_core::AgentContext;

use crate::path::resolve_existing_path;
use crate::tool::ToolOutput;

pub(crate) fn run(
    context: &AgentContext,
    path: PathBuf,
    old: String,
    new: String,
) -> Result<ToolOutput> {
    if old.is_empty() {
        bail!("edit old text must not be empty");
    }

    let path = resolve_existing_path(context, &path)?;
    if !path.is_file() {
        bail!("edit target is not a file: {}", path.display());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read file {}", path.display()))?;
    let matches = content.match_indices(&old).count();
    match matches {
        0 => bail!("old text was not found in {}", path.display()),
        1 => {}
        _ => bail!(
            "old text appears {matches} times in {}; edit requires a unique match",
            path.display()
        ),
    }

    let bytes_before = content.len();
    let updated = content.replacen(&old, &new, 1);
    let bytes_after = updated.len();
    fs::write(&path, updated.as_bytes())
        .with_context(|| format!("failed to write edited file {}", path.display()))?;

    Ok(ToolOutput::Edit {
        path,
        replacements: 1,
        bytes_before,
        bytes_after,
    })
}
