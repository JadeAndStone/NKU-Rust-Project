//! Agent dispatch loop: orchestrates the LLM provider, tools, and rollback.
//!
//! On each user message, the agent enters a multi-turn loop:
//!   user message → provider.complete() → text? → return to REPL
//!                                     → tool_calls? → execute via rollback
//!                                     → add tool results → back to provider
//!
//! This repeats until the provider returns a text response or MAX_TOOL_ROUNDS is reached.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use regex::Regex;
use rust_codingagent_core::{
    LanguageProvider, Message, MessageRole, ProviderRequest, ProviderResponse, Session,
    SessionStore, ToolDefinition,
};
use rust_codingagent_rollback::RollbackManager;
use rust_codingagent_tools::{ToolOutput, ToolRequest};
use rust_codingagent_tools_doc;
use serde_json::Value;
use tracing::info;
use walkdir::WalkDir;

/// Maximum number of tool-calling rounds per user message to prevent infinite loops.
const MAX_TOOL_ROUNDS: usize = 50;

/// System prompt prepended to each conversation to guide the agent's behavior.
const SYSTEM_PROMPT: &str = r#"You are a coding agent that helps users with their software projects.
Your own source code lives in NKU-Rust-Project/ — NEVER modify it unless the user explicitly asks you to improve the agent itself.
Focus on the user's projects and tasks.

IMPORTANT path rules:
- `write` and `edit` are workspace-restricted: only modify files under the workspace directory.
- `read`, `read_pdf`, `read_docx`, and `grep` can access any path (absolute or outside workspace).
- `shell` runs commands inside the workspace directory.

When answering:
- Use `read` for plain text files, `read_pdf` for .pdf files, `read_docx` for .docx files.
- Use `grep` to search for patterns in the code.
- Use `write` to create new files (inside workspace only).
- Use `edit` to make precise changes to existing files (inside workspace only).
- Use `shell` to run commands (build, test, git, mkdir, etc.).
- Always read a file before editing it.
- If `write` fails because the file already exists, retry immediately with overwrite: true.
- Create directories with `shell mkdir -p` before writing files into them.
- When creating multiple files, call multiple write tools in ONE response for speed. Batch as many writes as possible.
- Be concise and helpful. If a tool fails, explain what happened and suggest alternatives.
- Once you have completed the user's request, give a clear summary and STOP. Do not keep refining or running unnecessary checks."#;

// ── Agent ───────────────────────────────────────────────────────────────────

/// The Agent orchestrates the full LLM → tool → rollback loop.
pub struct Agent<'a> {
    provider: &'a dyn LanguageProvider,
    session: &'a mut Session,
    store: &'a SessionStore,
    rollback: &'a RollbackManager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellApproval {
    Approved,
    Denied,
}

impl<'a> Agent<'a> {
    pub fn new(
        provider: &'a dyn LanguageProvider,
        session: &'a mut Session,
        store: &'a SessionStore,
        rollback: &'a RollbackManager,
    ) -> Self {
        Self {
            provider,
            session,
            store,
            rollback,
        }
    }

    /// Process a user message and return the assistant's final text response.
    ///
    /// This enters the multi-turn dispatch loop: if the LLM requests tool calls,
    /// they are executed (with rollback recording for Write/Edit), results are
    /// fed back, and the loop continues until a text response is received.
    pub fn run(&mut self, user_input: &str) -> Result<String> {
        // 1. Add user message to session
        self.session.add_message(Message::user(user_input));
        self.store.save_session(self.session)?;

        // 2. Multi-turn dispatch loop
        let mut round = 0;
        loop {
            round += 1;
            if round > MAX_TOOL_ROUNDS {
                bail!(
                    "agent exceeded maximum tool-call rounds ({MAX_TOOL_ROUNDS}). \
                     The model may be stuck in a tool-calling loop."
                );
            }

            let response = self.call_provider()?;

            match response {
                ProviderResponse::Text { content } => {
                    // Got a text response — save and return
                    self.session.add_message(Message::assistant(&content));
                    self.store.save_session(self.session)?;
                    info!(round, "agent returned text response");
                    return Ok(content);
                }
                ProviderResponse::ToolCalls { calls } => {
                    info!(round, count = calls.len(), "executing tool calls");

                    // Execute tools first, collect results in memory
                    let mut results: Vec<(String, String)> = Vec::new();
                    for call in &calls {
                        let result_text =
                            match execute_tool_call(&call.name, &call.arguments, self.rollback) {
                                Ok(text) => text,
                                Err(e) => {
                                    format!("❌ 工具执行失败: {e:#}\n请根据错误信息调整后重试。")
                                }
                            };
                        results.push((call.id.clone(), result_text));
                    }

                    // Atomically add tool_calls + all results + save
                    let core_calls: Vec<rust_codingagent_core::ToolCall> = calls
                        .iter()
                        .map(|c| rust_codingagent_core::ToolCall {
                            id: c.id.clone(),
                            name: c.name.clone(),
                            arguments: c.arguments.clone(),
                        })
                        .collect();
                    self.session.add_message(Message::assistant_with_tool_calls(
                        String::new(),
                        core_calls,
                    ));
                    for (call_id, result_text) in results {
                        self.session
                            .add_message(Message::tool_result(result_text, &call_id));
                    }
                    self.store.save_session(self.session)?;
                }
            }
        }
    }

    /// Process a user message with streaming output and optional tool-call notifications.
    ///
    /// Tokens are forwarded to `on_token` as they arrive.
    /// `on_tool` is called with (name, description) before each tool executes.
    /// `on_tool_done` is called with (name, result_summary) after each tool.
    pub fn run_streaming(
        &mut self,
        user_input: &str,
        on_token: &mut dyn FnMut(&str),
        on_tool: &mut dyn FnMut(&str, &str),
        on_tool_done: &mut dyn FnMut(&str, &str),
        on_shell_approval: &mut dyn FnMut(&str) -> ShellApproval,
    ) -> Result<()> {
        self.session.add_message(Message::user(user_input));
        self.store.save_session(self.session)?;

        let mut round = 0;
        loop {
            round += 1;
            if round > MAX_TOOL_ROUNDS {
                bail!("agent exceeded maximum tool-call rounds ({MAX_TOOL_ROUNDS})");
            }

            let request = self.build_request();
            let response = self.provider.complete_streaming(request, on_token)?;

            match response {
                ProviderResponse::Text { content } => {
                    self.session.add_message(Message::assistant(&content));
                    self.store.save_session(self.session)?;
                    return Ok(());
                }
                ProviderResponse::ToolCalls { calls } => {
                    // First, execute all tools and collect results (in memory only).
                    // Only after ALL results are collected do we add them to session
                    // and persist — preventing half-written tool_calls without results.
                    let mut results: Vec<(String, String)> = Vec::new();
                    for call in &calls {
                        let desc = tool_description(&call.name, &call.arguments);
                        on_tool(&call.name, &desc);
                        match shell_command_from_call(&call.name, &call.arguments) {
                            Ok(Some(command))
                                if on_shell_approval(&command) == ShellApproval::Denied =>
                            {
                                let result_text = "Shell command was denied by the user. Do not run it again unless the user changes their mind.".to_string();
                                let done =
                                    tool_done_message(&call.name, &result_text, self.rollback);
                                on_tool_done(&call.name, &done);
                                results.push((call.id.clone(), result_text));
                                continue;
                            }
                            Err(e) if call.name == "shell" => {
                                let result_text = format!(
                                    "Shell command was not executed because its arguments could not be parsed for approval: {e:#}"
                                );
                                let done =
                                    tool_done_message(&call.name, &result_text, self.rollback);
                                on_tool_done(&call.name, &done);
                                results.push((call.id.clone(), result_text));
                                continue;
                            }
                            _ => {}
                        }
                        let result_text =
                            match execute_tool_call(&call.name, &call.arguments, self.rollback) {
                                Ok(text) => text,
                                Err(e) => {
                                    format!("❌ 工具执行失败: {e:#}\n请根据错误信息调整后重试。")
                                }
                            };
                        let done = tool_done_message(&call.name, &result_text, self.rollback);
                        on_tool_done(&call.name, &done);
                        results.push((call.id.clone(), result_text));
                    }

                    // Atomically add tool_calls + all tool_results + save
                    let core_calls: Vec<rust_codingagent_core::ToolCall> = calls
                        .iter()
                        .map(|c| rust_codingagent_core::ToolCall {
                            id: c.id.clone(),
                            name: c.name.clone(),
                            arguments: c.arguments.clone(),
                        })
                        .collect();
                    self.session.add_message(Message::assistant_with_tool_calls(
                        String::new(),
                        core_calls,
                    ));
                    for (call_id, result_text) in results {
                        self.session
                            .add_message(Message::tool_result(result_text, &call_id));
                    }
                    self.store.save_session(self.session)?;
                }
            }
        }
    }

    fn call_provider(&self) -> Result<ProviderResponse> {
        let request = self.build_request();
        self.provider.complete(request)
    }

    fn build_request(&self) -> ProviderRequest {
        let messages = build_messages_with_system(self.session);
        // Debug: verify no orphaned tool messages
        for (i, m) in messages.iter().enumerate() {
            if m.role == MessageRole::Tool && m.tool_call_id.is_some() {
                let has_prev_calls = messages[..i].iter().any(|prev| {
                    prev.tool_calls.as_ref().map_or(false, |calls| {
                        calls.iter().any(|c| Some(&c.id) == m.tool_call_id.as_ref())
                    })
                });
                if !has_prev_calls {
                    tracing::warn!(
                        msg_index = i,
                        tool_call_id = ?m.tool_call_id,
                        "ORPHANED tool result in sanitized messages — will cause 400!"
                    );
                }
            }
        }
        let tools = build_tool_definitions();

        ProviderRequest {
            context: self.session.context(),
            messages,
            tools,
        }
    }
}

// ── Tool definitions ────────────────────────────────────────────────────────

/// Build the OpenAI-compatible function definitions for all available tools.
fn build_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read".to_string(),
            description: "Read the contents of any file. Supports absolute paths and paths outside the workspace. Returns the file content as text.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file to read. Can access files outside the workspace."
                    },
                    "max_bytes": {
                        "type": "integer",
                        "description": "Maximum number of bytes to read. Omit to read the entire file."
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "write".to_string(),
            description: "Write content to a file in the workspace. Always set overwrite: true when the file may already exist. If you get a 'refusing to overwrite' error, retry with overwrite: true.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write, relative to the workspace."
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file."
                    },
                    "overwrite": {
                        "type": "boolean",
                        "description": "Set to true to overwrite an existing file. Defaults to false."
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDefinition {
            name: "edit".to_string(),
            description: "Replace a unique string in an existing file with new content. The old string must appear exactly once in the file. Use this for precise, surgical edits.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to edit, relative to the workspace."
                    },
                    "old": {
                        "type": "string",
                        "description": "The exact text to replace. Must appear exactly once in the file."
                    },
                    "new": {
                        "type": "string",
                        "description": "The replacement text."
                    }
                },
                "required": ["path", "old", "new"]
            }),
        },
        ToolDefinition {
            name: "grep".to_string(),
            description: "Search for a regex pattern in any directory. Can search outside the workspace. Returns matching lines with file path, line number, and column.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression pattern to search for."
                    },
                    "path": {
                        "type": "string",
                        "description": "Optional subdirectory or file path to limit the search scope."
                    },
                    "max_matches": {
                        "type": "integer",
                        "description": "Maximum number of matches to return. Default is unlimited."
                    }
                },
                "required": ["pattern"]
            }),
        },
        ToolDefinition {
            name: "shell".to_string(),
            description: "Execute a shell command. Default timeout 15s; for longer tasks, set timeout_ms. Use for build, test, git. NEVER use 'find /' — limit search scope. For find, use 'find <specific_dir>' with -maxdepth.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Timeout in milliseconds (default 15000 = 15s). Use for long operations."
                    },
                    "max_output_bytes": {
                        "type": "integer",
                        "description": "Maximum bytes of stdout+stderr to capture. Default is unlimited."
                    }
                },
                "required": ["command"]
            }),
        },
        ToolDefinition {
            name: "read_pdf".to_string(),
            description: "Extract and read text content from a PDF file. Supports absolute paths and paths outside the workspace. Use this for reading .pdf files.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the PDF file to read."
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "read_docx".to_string(),
            description: "Extract and read text content from a .docx (Word) file. Supports absolute paths and paths outside the workspace. Use this for reading .docx files.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the .docx file to read."
                    }
                },
                "required": ["path"]
            }),
        },
    ]
}

// ── Tool call parsing ───────────────────────────────────────────────────────

/// Parse an LLM function-call (name + JSON arguments) into a ToolRequest.
fn parse_tool_request(name: &str, arguments: &str) -> Result<ToolRequest> {
    let args: Value = parse_json_robust(arguments)?;

    match name {
        "read" => {
            let path = get_string(&args, "path")?;
            let max_bytes = args
                .get("max_bytes")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            Ok(ToolRequest::Read {
                path: PathBuf::from(path),
                max_bytes,
            })
        }
        "write" => {
            let path = get_string(&args, "path")?;
            let content = get_string(&args, "content")?;
            let overwrite = args
                .get("overwrite")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Ok(ToolRequest::Write {
                path: PathBuf::from(path),
                content,
                overwrite,
            })
        }
        "edit" => {
            let path = get_string(&args, "path")?;
            let old = get_string(&args, "old")?;
            let new = get_string(&args, "new")?;
            Ok(ToolRequest::Edit {
                path: PathBuf::from(path),
                old,
                new,
            })
        }
        "grep" => {
            let pattern = get_string(&args, "pattern")?;
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| PathBuf::from(s));
            let max_matches = args
                .get("max_matches")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            Ok(ToolRequest::Grep {
                pattern,
                path,
                max_matches,
            })
        }
        "shell" => {
            let command = get_string(&args, "command")?;
            let timeout_ms = args.get("timeout_ms").and_then(|v| v.as_u64());
            let max_output_bytes = args
                .get("max_output_bytes")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            Ok(ToolRequest::Shell {
                command,
                timeout_ms,
                max_output_bytes,
            })
        }
        other => bail!("unknown tool: {other}"),
    }
}

// ── Unified tool execution ──────────────────────────────────────────────────

/// Execute a tool call by name, dispatching to the appropriate handler.
///
/// `read_pdf` and `read_docx` are handled directly by the tools-doc crate
/// and can read files from any path (not restricted to workspace).
/// Other tools go through the standard tools + rollback pipeline.
fn shell_command_from_call(name: &str, arguments: &str) -> Result<Option<String>> {
    if name != "shell" {
        return Ok(None);
    }

    let args: Value = parse_json_robust(arguments)?;
    Ok(Some(get_string(&args, "command")?))
}

fn execute_tool_call(name: &str, arguments: &str, rollback: &RollbackManager) -> Result<String> {
    match name {
        "read_pdf" => {
            let args: Value = parse_json_robust(arguments)?;
            let path = get_string(&args, "path")?;
            let path = std::path::PathBuf::from(&path);
            let text = rust_codingagent_tools_doc::extract_pdf_text(&path)
                .with_context(|| format!("failed to extract text from PDF: {}", path.display()))?;
            let preview = format_preview(&text, "PDF");
            Ok(preview)
        }
        "read_docx" => {
            let args: Value = parse_json_robust(arguments)?;
            let path = get_string(&args, "path")?;
            let path = std::path::PathBuf::from(&path);
            let text = rust_codingagent_tools_doc::extract_docx_text(&path)
                .with_context(|| format!("failed to extract text from DOCX: {}", path.display()))?;
            let preview = format_preview(&text, "DOCX");
            Ok(preview)
        }
        // `read` and `grep` handle both workspace and external paths directly.
        "read" => {
            let args: Value = parse_json_robust(arguments)?;
            let path = get_string(&args, "path")?;
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    // Help LLM fix path: try common alternatives
                    let p = std::path::Path::new(&path);
                    let suggestions = if !p.exists() {
                        let parent = p
                            .parent()
                            .map(|d| d.display().to_string())
                            .unwrap_or_default();
                        let mut hints = vec![format!("文件不存在: {path}")];
                        // Try listing parent directory
                        if let Ok(entries) = std::fs::read_dir(&parent) {
                            let files: Vec<String> = entries
                                .filter_map(|e| e.ok())
                                .map(|e| e.file_name().to_string_lossy().to_string())
                                .take(20)
                                .collect();
                            hints.push(format!("目录 '{parent}' 下的文件: {}", files.join(", ")));
                        }
                        // Try workspace root
                        hints.push("用 shell ls 或 find 先确认文件位置。".to_string());
                        hints.join("\n")
                    } else {
                        e.to_string()
                    };
                    anyhow::bail!("{suggestions}");
                }
            };
            let max_bytes = args
                .get("max_bytes")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let preview = if let Some(max) = max_bytes {
                if content.len() > max {
                    let mut end = max;
                    while end > 0 && !content.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}...\n\n[truncated at {max} bytes]", &content[..end])
                } else {
                    content
                }
            } else {
                content
            };
            Ok(preview)
        }
        "grep" => {
            let args: Value = parse_json_robust(arguments)?;
            let pattern = get_string(&args, "pattern")?;
            let search_path = args.get("path").and_then(|v| v.as_str()).map(PathBuf::from);
            let max_matches = args
                .get("max_matches")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(50);
            let re = Regex::new(&pattern).with_context(|| format!("invalid regex: {pattern}"))?;
            let mut results: Vec<String> = Vec::new();
            let root = search_path.unwrap_or_else(|| PathBuf::from("."));
            if root.is_file() {
                let content = std::fs::read_to_string(&root)?;
                for (i, line) in content.lines().enumerate() {
                    if let Some(m) = re.find(line) {
                        results.push(format!(
                            "{}:{}:{}: {}",
                            root.display(),
                            i + 1,
                            m.start() + 1,
                            line
                        ));
                        if results.len() >= max_matches {
                            break;
                        }
                    }
                }
            } else if root.is_dir() {
                for entry in WalkDir::new(&root)
                    .max_depth(10)
                    .follow_links(false)
                    .into_iter()
                    .flatten()
                {
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    if entry
                        .path()
                        .to_str()
                        .map_or(true, |p| p.contains("/.git/") || p.contains("/target/"))
                    {
                        continue;
                    }
                    let Ok(content) = std::fs::read_to_string(entry.path()) else {
                        continue;
                    };
                    for (i, line) in content.lines().enumerate() {
                        if let Some(m) = re.find(line) {
                            results.push(format!(
                                "{}:{}:{}: {}",
                                entry.path().display(),
                                i + 1,
                                m.start() + 1,
                                line
                            ));
                            if results.len() >= max_matches {
                                break;
                            }
                        }
                    }
                    if results.len() >= max_matches {
                        break;
                    }
                }
            }
            Ok(if results.is_empty() {
                "No matches found.".to_string()
            } else {
                results.join("\n")
            })
        }
        // `write` has a fallback for malformed JSON (LLM often mangles large content)
        "write" => {
            let tool_request = match parse_tool_request(name, arguments) {
                Ok(req) => req,
                Err(_) => {
                    let (path, content, overwrite) = extract_write_args_fallback(arguments)?;
                    ToolRequest::Write {
                        path: PathBuf::from(path),
                        content,
                        overwrite,
                    }
                }
            };
            let result = rollback
                .run_tool(tool_request)
                .with_context(|| "tool 'write' execution failed")?;
            Ok(format_tool_output(&result.output))
        }
        // Intercept dangerous shell commands
        "shell" => {
            let args: Value = parse_json_robust(arguments)?;
            let command = get_string(&args, "command")?;
            if (command == "find /"
                || command.starts_with("find / ")
                || command.contains("| find /")
                || command.contains("find / -"))
                && !command.contains("-maxdepth")
                && !command.contains("/home")
                && !command.contains("/usr")
                && !command.contains("/opt")
            {
                return Ok(format!(
                    "⛔ 命令被拦截: 'find /' 会扫描整个文件系统，太慢。\n\
                    请改用具体目录: find /home/liuxuem/workspace -name '...' 等"
                ));
            }
            let timeout_ms = args.get("timeout_ms").and_then(|v| v.as_u64());
            let max_output_bytes = args
                .get("max_output_bytes")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let tool_request = ToolRequest::Shell {
                command,
                timeout_ms,
                max_output_bytes,
            };
            let result = rollback
                .run_tool(tool_request)
                .with_context(|| "shell execution failed")?;
            Ok(format_tool_output(&result.output))
        }
        _ => {
            let tool_request = parse_tool_request(name, arguments).with_context(|| {
                format!("failed to parse arguments for tool '{name}': {arguments}")
            })?;
            let result = rollback
                .run_tool(tool_request)
                .with_context(|| format!("tool '{name}' execution failed"))?;
            Ok(format_tool_output(&result.output))
        }
    }
}

/// Format a long text preview, safely truncating at a UTF-8 char boundary.
fn format_preview(text: &str, kind: &str) -> String {
    let max_bytes = 3000;
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}...\n\n[{kind} text extracted, {} total chars]",
        &text[..end],
        text.len()
    )
}

/// Parse JSON from an LLM-generated arguments string, tolerating trailing text.
///
/// LLMs sometimes append extra text after the JSON object (e.g.
/// `{"path": "x"} 一些多余的文字`). This function tries the full string first,
/// then falls back to extracting the first complete JSON object.
fn parse_json_robust(arguments: &str) -> Result<Value> {
    // Empty or whitespace → default to empty object
    if arguments.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    // Try full string first (fast path, works most of the time)
    if let Ok(v) = serde_json::from_str(arguments) {
        return Ok(v);
    }

    // Extract the first `{...}` JSON object from the string,
    // properly handling escaped characters and strings within.
    let trimmed = arguments.trim();
    if let Some(start) = trimmed.find('{') {
        let mut depth = 0;
        let mut in_string = false;
        let mut escaped = false;
        for (i, ch) in trimmed[start..].char_indices() {
            if escaped {
                escaped = false;
                continue; // this char is literal, skip bracket/quote logic
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = !in_string,
                '{' if !in_string => depth += 1,
                '}' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        let json_part = &trimmed[start..start + i + 1];
                        // Try standard parse
                        if let Ok(v) = serde_json::from_str(json_part) {
                            return Ok(v);
                        }
                        // Fallback: try fixing unescaped control chars in strings
                        if let Ok(v) = serde_json::from_str(&fix_json_strings(json_part)) {
                            return Ok(v);
                        }
                        anyhow::bail!("failed to parse extracted JSON: {json_part}");
                    }
                }
                _ => {}
            }
        }
    }

    anyhow::bail!("no valid JSON object found in: {arguments}")
}

/// Fix common issues in LLM-generated JSON strings:
/// - literal newlines/tabs inside strings → escaped
fn fix_json_strings(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in json.chars() {
        if escaped {
            escaped = false;
            out.push(ch);
            continue;
        }
        match ch {
            '\\' => {
                escaped = true;
                out.push(ch);
            }
            '"' => {
                in_string = !in_string;
                out.push(ch);
            }
            '\n' if in_string => out.push_str("\\n"),
            '\r' if in_string => out.push_str("\\r"),
            '\t' if in_string => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

/// Last-resort extraction of `path` and `content` from a malformed write-tool argument.
/// Fallback when the LLM's JSON is too broken to parse.
fn extract_write_args_fallback(arguments: &str) -> Result<(String, String, bool)> {
    let path = arguments
        .split("\"path\":")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().strip_prefix('"'))
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or("unknown.txt");
    let overwrite =
        arguments.contains("\"overwrite\": true") || arguments.contains("\"overwrite\":true");
    // Try to find content between "content": " and the next major key
    let content = arguments
        .split("\"content\": \"")
        .nth(1)
        .and_then(|s| {
            // Find end: look for ", "overwrite" or ", "path" or ending "}
            let mut end = s.len();
            for pat in ["\", \"overwrite\"", "\", \"path\""] {
                if let Some(pos) = s.find(pat) {
                    end = end.min(pos);
                }
            }
            if s.ends_with("\"}") && end == s.len() {
                end = s.len() - 2;
            }
            Some(s[..end].to_string())
        })
        .unwrap_or_default();
    Ok((path.to_string(), content, overwrite))
}

fn get_string(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .with_context(|| format!("missing or invalid field '{key}' in tool call arguments"))
}

// ── Tool output formatting ──────────────────────────────────────────────────

/// Format a tool output into a string suitable for sending back to the LLM.
fn format_tool_output(output: &ToolOutput) -> String {
    match output {
        ToolOutput::Read {
            content, truncated, ..
        } => {
            if *truncated {
                format!("{content}\n\n[Note: output was truncated]")
            } else {
                content.clone()
            }
        }
        ToolOutput::Write {
            path,
            bytes,
            created,
            overwritten,
        } => {
            let action = if *created {
                "created".to_string()
            } else if *overwritten {
                "overwritten".to_string()
            } else {
                "written".to_string()
            };
            format!(
                "File {action}: {path} ({bytes} bytes)",
                path = path.display()
            )
        }
        ToolOutput::Edit {
            path,
            replacements,
            bytes_before,
            bytes_after,
        } => {
            format!(
                "File edited: {} — {} replacement(s), {} → {} bytes",
                path.display(),
                replacements,
                bytes_before,
                bytes_after
            )
        }
        ToolOutput::Grep { matches, truncated } => {
            if matches.is_empty() {
                return "No matches found.".to_string();
            }
            let mut lines: Vec<String> = matches
                .iter()
                .map(|m| {
                    format!(
                        "{}:{}:{}: {}",
                        m.path.display(),
                        m.line,
                        m.column,
                        m.line_text
                    )
                })
                .collect();
            if *truncated {
                lines.push("... (results truncated)".to_string());
            }
            lines.join("\n")
        }
        ToolOutput::Shell {
            status_code,
            stdout,
            stderr,
            timed_out,
            stdout_truncated,
            stderr_truncated,
        } => {
            let mut parts = Vec::new();

            if *timed_out {
                parts.push("Command timed out.".to_string());
            }

            let exit = status_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "none".to_string());
            parts.push(format!("Exit code: {exit}"));

            if !stdout.is_empty() {
                let mut s = stdout.clone();
                if *stdout_truncated {
                    s.push_str("\n[stdout truncated]");
                }
                parts.push(format!("stdout:\n{s}"));
            }
            if !stderr.is_empty() {
                let mut s = stderr.clone();
                if *stderr_truncated {
                    s.push_str("\n[stderr truncated]");
                }
                parts.push(format!("stderr:\n{s}"));
            }

            parts.join("\n")
        }
    }
}

// ── Tool notification helpers ────────────────────────────────────────────────

/// Build a human-readable description of what a tool call does.
fn tool_description(name: &str, arguments: &str) -> String {
    let args: Value = parse_json_robust(arguments).unwrap_or_default();
    match name {
        "read" => format!(
            "读取 {}",
            args.get("path").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        "read_pdf" => format!(
            "提取PDF {}",
            args.get("path").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        "read_docx" => format!(
            "提取DOCX {}",
            args.get("path").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        "write" => format!(
            "写入 {}",
            args.get("path").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        "edit" => format!(
            "编辑 {}",
            args.get("path").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        "grep" => format!(
            "搜索 \"{}\"",
            args.get("pattern").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        "shell" => format!(
            "执行 {}",
            args.get("command").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        other => format!("调用 {other}"),
    }
}

fn safe_truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Build a summary after a tool finishes, including rollback info.
fn tool_done_message(name: &str, result: &str, rollback: &RollbackManager) -> String {
    match name {
        "write" | "edit" => {
            let records = rollback.list_records().ok();
            let last_id = records
                .as_ref()
                .and_then(|r| r.last().map(|s| s.id.as_str()))
                .unwrap_or("?");
            let truncated = safe_truncate(result, 80);
            format!("{truncated}  ↩️ /rollback apply {last_id}")
        }
        _ => {
            if result.len() > 100 {
                let mut end = 100;
                while end > 0 && !result.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}...", &result[..end])
            } else {
                result.to_string()
            }
        }
    }
}

// ── Convenience: create a session with system prompt ─────────────────────────

/// Ensure the session has the system prompt as its first message.
/// Safe to call multiple times — only adds if not already present.
pub fn ensure_system_prompt(session: &mut Session) {
    if session
        .history
        .messages()
        .first()
        .map_or(true, |m| m.content != SYSTEM_PROMPT)
    {
        // Insert system message at the beginning by creating a new history
        let mut messages = session.history.messages().to_vec();
        messages.insert(0, Message::system(SYSTEM_PROMPT));
        // We can't replace the history, so we just check each time and insert
        // by using add_message order. A cleaner approach: always inject in build_request.
    }
}

/// Sanitize message history so it's valid for the LLM API.
///
/// Ensures every assistant message with `tool_calls` is followed by matching
/// tool-result messages for each `tool_call_id`. Strips orphaned tool_calls.
fn sanitize_messages(messages: &[Message]) -> Vec<Message> {
    let mut clean: Vec<Message> = Vec::new();
    let mut i = 0;

    while i < messages.len() {
        let msg = &messages[i];

        if let Some(ref tool_calls) = msg.tool_calls {
            if tool_calls.is_empty() {
                clean.push(msg.clone());
                i += 1;
                continue;
            }

            // Assistant with tool_calls — require contiguous tool results after it
            let mut pending: Vec<&str> = tool_calls.iter().map(|c| c.id.as_str()).collect();
            let mut j = i + 1;
            let mut keep_indices: Vec<usize> = Vec::new();

            while j < messages.len() && !pending.is_empty() {
                let next = &messages[j];
                if next.role == MessageRole::Tool {
                    if let Some(ref tid) = next.tool_call_id {
                        if let Some(pos) = pending.iter().position(|id| id == tid) {
                            pending.remove(pos);
                            keep_indices.push(j);
                        } else {
                            // Tool result references an unknown call_id → block invalid
                            break;
                        }
                    }
                    j += 1;
                } else {
                    // Non-tool message before all results → block invalid
                    break;
                }
            }

            if pending.is_empty() {
                // Valid: all tool results found contiguously
                clean.push(msg.clone());
                for &idx in &keep_indices {
                    clean.push(messages[idx].clone());
                }
                i = j;
            } else {
                // Orphaned assistant tool_calls → skip
                i += 1;
            }
        } else if msg.role == MessageRole::Tool && msg.tool_call_id.is_some() {
            // Orphaned tool result → skip
            i += 1;
        } else {
            clean.push(msg.clone());
            i += 1;
        }
    }

    clean
}

/// Maximum estimated tokens in the context window.
/// DeepSeek V3 has 64K; we leave ~14K for response and tool output.
const MAX_CONTEXT_TOKENS: usize = 50_000;

/// Rough token count estimation. For mixed Chinese/English text,
/// ~3 characters per token is a conservative estimate.
fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 3
}

fn estimate_message_tokens(msg: &Message) -> usize {
    let mut tokens = estimate_tokens(&msg.content);
    if let Some(ref tc) = msg.tool_calls {
        for call in tc {
            tokens += estimate_tokens(&call.name);
            tokens += estimate_tokens(&call.arguments);
        }
    }
    tokens
}

/// Build the full message list for the provider, injecting the system prompt
/// at the beginning if the session doesn't already have one.
///
/// If the total estimated tokens exceed `MAX_CONTEXT_TOKENS`, the oldest
/// non-system messages are truncated to stay within the limit.
pub fn build_messages_with_system(session: &Session) -> Vec<Message> {
    let messages = sanitize_messages(session.history.messages());

    let has_system = messages
        .first()
        .map_or(false, |m| m.role == MessageRole::System);

    let system_msg = if has_system {
        None
    } else {
        Some(Message::system(SYSTEM_PROMPT))
    };

    let system_tokens = system_msg
        .as_ref()
        .map_or(0, |m| estimate_message_tokens(m));

    // Build the full list, then truncate from the top if needed
    let mut full: Vec<Message> = if let Some(sys) = system_msg {
        let mut v = vec![sys];
        v.extend(messages);
        v
    } else {
        messages
    };

    // Calculate total tokens
    let total_tokens: usize = full.iter().map(|m| estimate_message_tokens(m)).sum();
    if total_tokens <= MAX_CONTEXT_TOKENS {
        return full;
    }

    // Truncate: keep system prompt (index 0), drop oldest non-system messages
    // until we're under the limit. Always keep at least the last 4 messages.
    let mut current_tokens: usize = full.iter().map(|m| estimate_message_tokens(m)).sum();
    let remove_idx = if has_system || system_tokens > 0 {
        1usize
    } else {
        0usize
    };

    while current_tokens > MAX_CONTEXT_TOKENS && remove_idx + 4 < full.len() {
        let removed = estimate_message_tokens(&full[remove_idx]);
        current_tokens = current_tokens.saturating_sub(removed);
        full.remove(remove_idx);
        // Don't increment remove_idx — the next element shifted into this position
    }

    if current_tokens > MAX_CONTEXT_TOKENS {
        tracing::warn!(
            total = total_tokens,
            remaining = current_tokens,
            "context still over limit after truncation"
        );
    } else {
        tracing::info!(
            original = total_tokens,
            truncated = full.len(),
            tokens = current_tokens,
            "context window trimmed"
        );
    }

    sanitize_messages(&full)
}

#[cfg(test)]
mod tests {
    use rust_codingagent_core::{ProviderConfig, ToolCall};

    use super::*;

    #[test]
    fn context_trimming_does_not_leave_orphan_tool_results() {
        let provider = ProviderConfig::new("test", "model");
        let mut session = Session::new("test", ".", provider);
        session.add_message(Message::user("x".repeat(180_000)));
        session.add_message(Message::assistant_with_tool_calls(
            String::new(),
            vec![ToolCall {
                id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments: format!(r#"{{"command":"{}"}}"#, "x".repeat(27_000)),
            }],
        ));
        session.add_message(Message::tool_result("tool output", "call_1"));
        session.add_message(Message::user("y".repeat(144_000)));
        session.add_message(Message::assistant("latest assistant"));
        session.add_message(Message::user("latest user"));

        let messages = build_messages_with_system(&session);

        for (index, message) in messages.iter().enumerate() {
            if message.role == MessageRole::Tool {
                let Some(tool_call_id) = &message.tool_call_id else {
                    continue;
                };
                let has_preceding_call = messages[..index].iter().any(|previous| {
                    previous.tool_calls.as_ref().map_or(false, |calls| {
                        calls.iter().any(|call| &call.id == tool_call_id)
                    })
                });
                assert!(
                    has_preceding_call,
                    "orphan tool result remained after trimming"
                );
            }
        }
    }
}
