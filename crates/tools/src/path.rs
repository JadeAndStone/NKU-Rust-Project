use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use rust_codingagent_core::AgentContext;

pub(crate) fn workspace_root(context: &AgentContext) -> Result<PathBuf> {
    dunce::canonicalize(&context.workspace).with_context(|| {
        format!(
            "failed to resolve workspace {}",
            context.workspace.display()
        )
    })
}

pub(crate) fn resolve_existing_path(context: &AgentContext, path: &Path) -> Result<PathBuf> {
    let root = workspace_root(context)?;
    let candidate = normalize_candidate(&root, path);
    let resolved = dunce::canonicalize(&candidate)
        .with_context(|| format!("failed to resolve path {}", candidate.display()))?;
    ensure_inside_workspace(&root, &resolved)?;
    Ok(resolved)
}

pub(crate) fn resolve_write_path(context: &AgentContext, path: &Path) -> Result<PathBuf> {
    let root = workspace_root(context)?;
    let candidate = normalize_candidate(&root, path);
    ensure_inside_workspace(&root, &candidate)?;

    let parent = candidate
        .parent()
        .with_context(|| format!("path has no parent directory: {}", candidate.display()))?;
    let existing_parent = nearest_existing_ancestor(parent)?;
    let resolved_parent = dunce::canonicalize(&existing_parent).with_context(|| {
        format!(
            "failed to resolve parent directory {}",
            existing_parent.display()
        )
    })?;
    ensure_inside_workspace(&root, &resolved_parent)?;

    Ok(candidate)
}

pub(crate) fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}

fn normalize_candidate(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(&root.join(path))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    while !current.exists() {
        if !current.pop() {
            bail!("no existing parent directory for {}", path.display());
        }
    }
    Ok(current)
}

fn ensure_inside_workspace(root: &Path, path: &Path) -> Result<()> {
    if path.starts_with(root) {
        Ok(())
    } else {
        bail!(
            "path {} is outside workspace {}",
            path.display(),
            root.display()
        );
    }
}
