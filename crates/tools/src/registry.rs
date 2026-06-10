use anyhow::Result;
use rust_codingagent_core::AgentContext;

use crate::edit::EditTool;
use crate::grep::GrepTool;
use crate::read::ReadTool;
use crate::shell::ShellTool;
use crate::tool::{Tool, ToolOutput, ToolRequest};
use crate::write::WriteTool;

#[derive(Debug, Default, Clone, Copy)]
pub struct ToolRegistry;

impl ToolRegistry {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&self, context: &AgentContext, request: ToolRequest) -> Result<ToolOutput> {
        match request {
            input @ ToolRequest::Read { .. } => ReadTool.run(context, input),
            input @ ToolRequest::Write { .. } => WriteTool.run(context, input),
            input @ ToolRequest::Edit { .. } => EditTool.run(context, input),
            input @ ToolRequest::Grep { .. } => GrepTool.run(context, input),
            input @ ToolRequest::Shell { .. } => ShellTool.run(context, input),
        }
    }
}

pub fn run_tool(context: &AgentContext, request: ToolRequest) -> Result<ToolOutput> {
    ToolRegistry::new().run(context, request)
}
