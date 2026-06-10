use anyhow::Result;
use rust_codingagent_core::AgentContext;

use crate::tool::{ToolOutput, ToolRequest};
use crate::{edit, grep, read, shell, write};

#[derive(Debug, Default, Clone, Copy)]
pub struct ToolRegistry;

impl ToolRegistry {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&self, context: &AgentContext, request: ToolRequest) -> Result<ToolOutput> {
        match request {
            ToolRequest::Read { path, max_bytes } => read::run(context, path, max_bytes),
            ToolRequest::Write {
                path,
                content,
                overwrite,
            } => write::run(context, path, content, overwrite),
            ToolRequest::Edit { path, old, new } => edit::run(context, path, old, new),
            ToolRequest::Grep {
                pattern,
                path,
                max_matches,
            } => grep::run(context, pattern, path, max_matches),
            ToolRequest::Shell {
                command,
                timeout_ms,
                max_output_bytes,
            } => shell::run(context, command, timeout_ms, max_output_bytes),
        }
    }
}

pub fn run_tool(context: &AgentContext, request: ToolRequest) -> Result<ToolOutput> {
    ToolRegistry::new().run(context, request)
}
