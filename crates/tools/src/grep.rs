use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ignore::WalkBuilder;
use regex::Regex;
use rust_codingagent_core::AgentContext;

use crate::path::{resolve_existing_path, workspace_root};
use crate::tool::{GrepMatch, Tool, ToolInput, ToolOutput};

#[derive(Debug, Default, Clone, Copy)]
pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn run(&self, context: &AgentContext, input: ToolInput) -> Result<ToolOutput> {
        let ToolInput::Grep {
            pattern,
            path,
            max_matches,
        } = input
        else {
            bail!("grep tool received non-grep input");
        };
        run(context, pattern, path, max_matches)
    }
}

pub(crate) fn run(
    context: &AgentContext,
    pattern: String,
    path: Option<PathBuf>,
    max_matches: Option<usize>,
) -> Result<ToolOutput> {
    let regex = Regex::new(&pattern).with_context(|| format!("invalid regex '{pattern}'"))?;
    let root = workspace_root(context)?;
    let search_root = match path {
        Some(path) => resolve_existing_path(context, &path)?,
        None => root.clone(),
    };

    if !search_root.exists() {
        bail!("grep target does not exist: {}", search_root.display());
    }

    let mut matches = Vec::new();
    let truncated = collect_matches(&root, &search_root, &regex, max_matches, &mut matches)?;

    Ok(ToolOutput::Grep { matches, truncated })
}

fn collect_matches(
    workspace: &Path,
    search_root: &Path,
    regex: &Regex,
    max_matches: Option<usize>,
    matches: &mut Vec<GrepMatch>,
) -> Result<bool> {
    if max_matches == Some(0) {
        return Ok(true);
    }

    if search_root.is_file() {
        return grep_file(workspace, search_root, regex, max_matches, matches);
    }

    let mut builder = WalkBuilder::new(search_root);
    builder.standard_filters(true);

    for entry in builder.build() {
        let entry = entry.with_context(|| {
            format!("failed while walking grep target {}", search_root.display())
        })?;
        let path = entry.path();
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }

        if grep_file(workspace, path, regex, max_matches, matches)? {
            return Ok(true);
        }
    }

    Ok(false)
}

fn grep_file(
    workspace: &Path,
    path: &Path,
    regex: &Regex,
    max_matches: Option<usize>,
    matches: &mut Vec<GrepMatch>,
) -> Result<bool> {
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(false);
    };

    for (line_index, line) in content.lines().enumerate() {
        let Some(regex_match) = regex.find(line) else {
            continue;
        };

        matches.push(GrepMatch {
            path: path.strip_prefix(workspace).unwrap_or(path).to_path_buf(),
            line: line_index + 1,
            column: regex_match.start() + 1,
            line_text: line.to_string(),
        });

        if max_matches.is_some_and(|limit| matches.len() >= limit) {
            return Ok(true);
        }
    }

    Ok(false)
}
