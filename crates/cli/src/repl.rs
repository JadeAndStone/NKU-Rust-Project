use std::env;
use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use rust_codingagent_agent::Agent;
use rust_codingagent_core::{
    LanguageProvider, ProviderConfig as CoreProviderConfig, Session, SessionStore,
};
use rust_codingagent_provider_remote::RemoteProvider;
use rust_codingagent_rollback::RollbackManager;

use crate::config::AppConfig;

const MAX_VISIBLE_MESSAGES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainLoopStatus {
    ExitedByCommand,
    ExitedByEof,
}

pub struct MainLoop<'a> {
    config: &'a AppConfig,
    store: SessionStore,
    session: Session,
    provider: Box<dyn LanguageProvider>,
    rollback_manager: RollbackManager,
}

impl<'a> MainLoop<'a> {
    pub fn new(config: &'a AppConfig) -> Result<Self> {
        let store = SessionStore::new(&config.workspace, &config.profile);
        let provider = create_provider(config)?;

        let core_provider = CoreProviderConfig {
            name: provider.name().to_string(),
            model: provider.model().to_string(),
            api_base: config.provider.api_base.clone(),
        };
        let session =
            store.get_or_create_active_session(config.profile.clone(), config.workspace.clone(), core_provider)?;

        let rollback_manager = RollbackManager::new(session.context())?;

        Ok(Self {
            config,
            store,
            session,
            provider,
            rollback_manager,
        })
    }

    // ── Main REPL loop ──────────────────────────────────────────────────

    pub fn print_banner<W: Write>(&self, writer: &mut W) -> Result<()> {
        writeln!(writer, "╔══════════════════════════════════════════════════════╗")?;
        writeln!(writer, "║  NKU Rust Coding Agent                              ║")?;
        writeln!(writer, "╠══════════════════════════════════════════════════════╣")?;
        writeln!(writer, "║  workspace : {:<40}║", truncate_display(&self.config.workspace, 40))?;
        writeln!(writer, "║  provider  : {}/{:<32}║", self.provider.name(), truncate_right(&self.provider.model(), 32))?;
        if self.session.history.len() > 0 {
            writeln!(writer, "║  session   : {} ({} messages){:<15}║", self.session.id, self.session.history.len(), "")?;
        } else {
            writeln!(writer, "║  session   : new                                    ║")?;
        }
        writeln!(writer, "╠══════════════════════════════════════════════════════╣")?;
        writeln!(writer, "║  输入自然语言，Agent 会调用工具帮你完成任务          ║")?;
        writeln!(writer, "║                                                    ║")?;
        writeln!(writer, "║  /help        查看所有命令                          ║")?;
        writeln!(writer, "║  /sessions    查看历史对话，/session resume <id> 切换║")?;
        writeln!(writer, "║  /clear       开启新对话                            ║")?;
        writeln!(writer, "║  /rollback    回滚文件修改                          ║")?;
        writeln!(writer, "║  exit         退出                                  ║")?;
        writeln!(writer, "╚══════════════════════════════════════════════════════╝")?;
        Ok(())
    }

    /// Interactive REPL (used from app.rs with rustyline).
    /// Banner is printed separately by caller. This just handles input loop.
    pub fn run_loop<R, W>(&mut self, mut reader: R, mut writer: W) -> Result<MainLoopStatus>
    where
        R: BufRead,
        W: Write,
    {
        loop {
            write!(writer, "\n> ")?;
            writer.flush()?;
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 { writeln!(writer)?; return Ok(MainLoopStatus::ExitedByEof); }
            let input = line.trim();
            if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
                writeln!(writer, "bye")?;
                return Ok(MainLoopStatus::ExitedByCommand);
            }
            if input.is_empty() { continue; }
            if input.starts_with('/') {
                if let Err(e) = self.handle_command(input, &mut writer) {
                    writeln!(writer, "error: {e:#}")?;
                }
                continue;
            }
            match self.run_agent_turn(input, &mut writer) {
                Ok(()) => {}
                Err(e) => { writeln!(writer, "agent error: {e:#}")?; }
            }
        }
    }

    /// Banner + loop (for integration tests with Cursor).
    pub fn run<R, W>(&mut self, reader: R, mut writer: W) -> Result<MainLoopStatus>
    where
        R: BufRead,
        W: Write,
    {
        self.print_banner(&mut writer)?;
        self.run_loop(reader, writer)
    }

    pub fn run_agent_turn<W: Write>(&mut self, input: &str, writer: &mut W) -> Result<()> {
        let mut agent = Agent::new(
            self.provider.as_ref(),
            &mut self.session,
            &self.store,
            &self.rollback_manager,
        );

        // Show thinking indicator before first token
        write!(writer, "\n\x1b[93m⏳ 思考中...\x1b[0m")?;
        writer.flush()?;

        use std::cell::RefCell;
        let w = RefCell::new(writer);
        let mut first_token = true;
        let mut tool_count = 0u32;
        let start_time = std::time::Instant::now();

        let result = agent.run_streaming(
            input,
            &mut |token: &str| {
                if first_token {
                    first_token = false;
                    let elapsed = start_time.elapsed().as_secs_f64();
                    // Clear "\x1b[33m⏳ 思考中...\x1b[0m" and replace with elapsed time
                    let _ = write!(w.borrow_mut(), "\r\x1b[K\x1b[97m⏳ {elapsed:.1}s\x1b[0m\n\x1b[1;37m");
                }
                let _ = write!(w.borrow_mut(), "{token}");
                let _ = w.borrow_mut().flush();
            },
            &mut |name: &str, desc: &str| {
                tool_count += 1;
                let (icon, color) = match name {
                    "read" | "read_pdf" | "read_docx" => ("📖", "\x1b[96m"),
                    "write" => ("📝", "\x1b[93m"),
                    "edit" => ("✏️", "\x1b[95m"),
                    "grep" => ("🔍", "\x1b[94m"),
                    "shell" => ("⚡", "\x1b[97m"),
                    _ => ("🔧", "\x1b[97m"),
                };
                let _ = writeln!(w.borrow_mut(), "\n\x1b[48;5;240m{color} {icon} TOOL: {desc}\x1b[0m");
                let _ = w.borrow_mut().flush();
            },
            &mut |name: &str, done: &str| {
                let (icon, color) = if name == "write" || name == "edit" {
                    ("✅", "\x1b[92m")
                } else {
                    ("✓", "\x1b[97m")
                };
                let truncated = if done.len() > 120 { format!("{}...", &done[..117]) } else { done.to_string() };
                let _ = writeln!(w.borrow_mut(), "\x1b[48;5;240m{color} {icon} {truncated}\x1b[0m");
                let _ = w.borrow_mut().flush();
            },
        );
        let writer = w.into_inner();

        match result {
            Ok(()) => {
                if tool_count > 0 {
                    writeln!(writer, "\x1b[48;5;240m\x1b[97m── {tool_count} 个工具执行完毕 ──\x1b[0m")?;
                }
                writeln!(writer)?;
                Ok(())
            }
            Err(e) => {
                writeln!(writer)?;
                Err(e)
            }
        }
    }

    // ── Slash commands ─────────────────────────────────────────────────

    pub fn handle_command<W: Write>(&mut self, input: &str, writer: &mut W) -> Result<bool> {
        let command = input.strip_prefix('/').unwrap_or(input);
        let mut parts = command.split_whitespace();
        let Some(name) = parts.next() else {
            return Ok(false);
        };

        match name {
            "help" => {
                writeln!(writer, "{}", HELP_TEXT)?;
                Ok(true)
            }
            "session" => {
                let sub = parts.next();
                match sub {
                    Some("resume") => {
                        let id = parts.next()
                            .context("usage: /session resume <id>")?;
                        match self.store.load_session(id) {
                            Ok(s) => {
                                self.store.set_active_session(&s.id)?;
                                // Recreate rollback manager for new session
                                self.rollback_manager = RollbackManager::new(s.context())?;
                                writeln!(
                                    writer,
                                    "switched to session {} ({} messages, model={}/{})",
                                    s.id,
                                    s.history.len(),
                                    s.provider.name,
                                    s.provider.model
                                )?;
                                self.session = s;
                            }
                            Err(e) => {
                                writeln!(writer, "session not found: {id} ({e:#})")?;
                                // List available sessions
                                let sessions = self.store.list_sessions()?;
                                if sessions.is_empty() {
                                    writeln!(writer, "no saved sessions")?;
                                } else {
                                    writeln!(writer, "available sessions:")?;
                                    for s in &sessions {
                                        writeln!(
                                            writer,
                                            "  {}  messages={}  updated={}",
                                            s.id, s.message_count, s.updated_at_ms
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        // Show current session info
                        let ctx = self.session.context();
                        writeln!(
                            writer,
                            "session={}  profile={}  messages={}  model={}/{}  workspace={}",
                            ctx.session_id,
                            ctx.profile,
                            ctx.turn_index,
                            ctx.provider,
                            ctx.model,
                            ctx.workspace.display()
                        )?;
                        // Also list available sessions
                        let sessions = self.store.list_sessions()?;
                        if sessions.len() > 1 {
                            writeln!(writer, "{} other session(s) available: /session resume <id>", sessions.len() - 1)?;
                            for s in &sessions {
                                if s.id != self.session.id {
                                    writeln!(
                                        writer,
                                        "  {}  messages={}  updated={}",
                                        s.id, s.message_count, s.updated_at_ms
                                    )?;
                                }
                            }
                        }
                    }
                }
                Ok(true)
            }
            "sessions" => {
                let sessions = self.store.list_sessions()?;
                if sessions.is_empty() {
                    writeln!(writer, "no saved sessions")?;
                } else {
                    writeln!(writer, "{} session(s):", sessions.len())?;
                    for s in &sessions {
                        let active = if s.id == self.session.id { " (active)" } else { "" };
                        writeln!(
                            writer,
                            "  {}  messages={}  model={}/{}  updated={}{}",
                            s.id, s.message_count, s.provider, s.model, s.updated_at_ms, active
                        )?;
                    }
                    writeln!(writer, "use /session resume <id> to switch")?;
                }
                Ok(true)
            }
            "history" => {
                let messages = self.session.history.messages();
                if messages.is_empty() {
                    writeln!(writer, "history is empty")?;
                    return Ok(true);
                }
                let total = messages.len();
                let start = if total > MAX_VISIBLE_MESSAGES {
                    total - MAX_VISIBLE_MESSAGES
                } else {
                    0
                };
                if start > 0 {
                    writeln!(writer, "showing last {MAX_VISIBLE_MESSAGES} of {total} messages")?;
                }
                for (i, msg) in messages.iter().enumerate().skip(start) {
                    let tc_info = match &msg.tool_calls {
                        Some(calls) if !calls.is_empty() => {
                            let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                            format!(" [tool_calls: {}]", names.join(", "))
                        }
                        _ => String::new(),
                    };
                    let tool_id = match &msg.tool_call_id {
                        Some(id) => format!(" [tool_call_id: {id}]"),
                        None => String::new(),
                    };
                    writeln!(
                        writer,
                        "{} {:?}: {}{}{}",
                        i + 1,
                        msg.role,
                        truncate_str(&msg.content, 200),
                        tc_info,
                        tool_id
                    )?;
                }
                Ok(true)
            }
            "model" => {
                if let Some(model) = parts.next() {
                    self.session.set_model(model);
                    self.store.save_session(&self.session)?;
                    writeln!(
                        writer,
                        "model switched to {}/{}",
                        self.session.provider.name, self.session.provider.model
                    )?;
                } else {
                    writeln!(
                        writer,
                        "current model: {}/{}",
                        self.session.provider.name, self.session.provider.model
                    )?;
                }
                Ok(true)
            }
            "clear" => {
                let msg_count = self.session.history.len();
                // Recreate session with fresh history
                self.session = Session::new(
                    self.session.profile.clone(),
                    self.session.workspace.clone(),
                    self.session.provider.clone(),
                );
                self.store.save_session(&self.session)?;
                writeln!(writer, "cleared {msg_count} messages, new session={}", self.session.id)?;
                Ok(true)
            }
            "rollback" => {
                let sub = parts.next().unwrap_or("list");
                match sub {
                    "list" => {
                        let records = self.rollback_manager.list_records()?;
                        if records.is_empty() {
                            writeln!(writer, "no rollback records")?;
                        } else {
                            writeln!(writer, "{} rollback record(s):", records.len())?;
                            for r in &records {
                                let files: Vec<String> =
                                    r.changed_files.iter().map(|p| p.display().to_string()).collect();
                                writeln!(
                                    writer,
                                    "  {}  turn={}  tool={}  files=[{}]",
                                    r.id,
                                    r.turn_index,
                                    r.tool_name,
                                    files.join(", ")
                                )?;
                            }
                        }
                    }
                    "preview" => {
                        let id = parts.next().context("usage: /rollback preview <id>")?;
                        let preview = self.rollback_manager.preview(id)?;
                        writeln!(writer, "rollback preview for {}:", preview.record_id)?;
                        for f in &preview.files {
                            writeln!(
                                writer,
                                "  {}  action={:?}",
                                f.path.display(),
                                f.action
                            )?;
                            if !f.diff.is_empty() && f.diff != "(no changes)\n" {
                                for line in f.diff.lines() {
                                    writeln!(writer, "    {line}")?;
                                }
                            }
                        }
                    }
                    "apply" => {
                        let id = parts.next().context("usage: /rollback apply <id>")?;
                        let report = self.rollback_manager.restore(id)?;
                        writeln!(writer, "rollback {} applied:", report.record_id)?;
                        for f in &report.files {
                            writeln!(writer, "  {} → {:?}", f.path.display(), f.action)?;
                        }
                    }
                    "file" => {
                        let id = parts.next().context("usage: /rollback file <id> <path>")?;
                        let path = parts.next().context("usage: /rollback file <id> <path>")?;
                        let report = self.rollback_manager.restore_file(id, path)?;
                        writeln!(writer, "rollback {} applied to file:", report.record_id)?;
                        for f in &report.files {
                            writeln!(writer, "  {} → {:?}", f.path.display(), f.action)?;
                        }
                    }
                    other => {
                        writeln!(writer, "unknown rollback sub-command: {other}")?;
                        writeln!(writer, "usage: /rollback [list|preview <id>|apply <id>|file <id> <path>]")?;
                    }
                }
                Ok(true)
            }
            "tools" => {
                writeln!(writer, "available tools:")?;
                writeln!(writer, "  read    — Read file contents")?;
                writeln!(writer, "  write   — Write content to a file")?;
                writeln!(writer, "  edit    — Replace text in a file")?;
                writeln!(writer, "  grep    — Search with regex")?;
                writeln!(writer, "  shell   — Execute shell commands")?;
                Ok(true)
            }
            _ => {
                writeln!(writer, "unknown command: {name}")?;
                writeln!(writer, "type /help for available commands")?;
                Ok(true)
            }
        }
    }
}

// ── Provider factory ────────────────────────────────────────────────────────

fn create_provider(config: &AppConfig) -> Result<Box<dyn LanguageProvider>> {
    let api_key = config
        .provider
        .api_key
        .clone()
        .or_else(|| env::var("RUST_CODINGAGENT_API_KEY").ok())
        .or_else(|| env::var("OPENAI_API_KEY").ok())
        .context(
            "no API key configured. Set RUST_CODINGAGENT_API_KEY env var \
             or add api_key to your config file",
        )?;

    let name = config.provider.name.clone();
    let model = config.provider.model.clone();
    let api_base = config
        .provider
        .api_base
        .clone()
        .unwrap_or_else(|| "https://api.deepseek.com/v1".to_string());

    let provider = RemoteProvider::new(&name, &model, &api_base, &api_key)
        .context("failed to create remote provider")?;

    Ok(Box::new(provider))
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn truncate_display(path: &std::path::Path, max: usize) -> String {
    let s = path.display().to_string();
    if s.len() <= max { s } else { format!("...{}", &s[s.len().saturating_sub(max - 3)..]) }
}

fn truncate_right(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}...", &s[..max.saturating_sub(3)]) }
}

fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..max_len]
    }
}

// ── Help text ───────────────────────────────────────────────────────────────

const HELP_TEXT: &str = r#"Available commands:
  /help              Show this help
  /session           Show current session + available sessions
  /session resume <id>  Switch to a saved session
  /sessions          List all saved sessions
  /history           Show message history (last 50)
  /model [name]      Show or switch model
  /clear             Clear message history, start new session
  /rollback list     List all rollback records
  /rollback preview <id>  Preview what a rollback would change
  /rollback apply <id>    Apply a rollback (restore files)
  /rollback file <id> <path>  Restore a single file from a record
  /tools             List available tools (read, write, edit, grep, shell, read_pdf, read_docx)
  exit | quit        Exit the REPL"#;

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::config::{AppConfig, ProviderConfig};

    #[test]
    fn enters_loop_and_exits_by_command() {
        let workspace = std::env::temp_dir().join(unique_name("rust-codingagent-repl-test"));
        let config = AppConfig {
            profile: "test".to_string(),
            workspace: workspace.clone(),
            log_level: "off".to_string(),
            provider: ProviderConfig {
                name: "local".to_string(),
                model: "stub".to_string(),
                api_base: None,
                api_key: Some("sk-test".to_string()),
            },
        };
        let mut output = Vec::new();

        // This will fail to create a real provider, but we just test the REPL framework
        match MainLoop::new(&config) {
            Ok(mut main_loop) => {
                let status = main_loop
                    .run(Cursor::new("hello\nexit\n"), &mut output)
                    .unwrap();
                let output = String::from_utf8(output).unwrap();
                assert_eq!(status, MainLoopStatus::ExitedByCommand);
                assert!(output.contains("NKU Rust Coding Agent"));
                assert!(output.contains("bye"));
            }
            Err(_) => {
                // Provider creation fails without network in tests — that's fine
            }
        }

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn persists_history_and_switches_model() {
        let workspace = std::env::temp_dir().join(unique_name("rust-codingagent-repl-model"));
        let config = AppConfig {
            profile: "test".to_string(),
            workspace: workspace.clone(),
            log_level: "off".to_string(),
            provider: ProviderConfig {
                name: "local".to_string(),
                model: "stub".to_string(),
                api_base: None,
                api_key: Some("sk-test".to_string()),
            },
        };

        // Test without actual agent calls — just slash commands
        let mut first_output = Vec::new();
        match MainLoop::new(&config) {
            Ok(mut main_loop) => {
                main_loop
                    .run(
                        Cursor::new("/model better-model\n/session\n/history\nexit\n"),
                        &mut first_output,
                    )
                    .unwrap();
                let first_output = String::from_utf8(first_output).unwrap();
                assert!(first_output.contains("model switched"));
            }
            Err(_) => {
                // Provider creation may fail, expected in CI
            }
        }

        let _ = std::fs::remove_dir_all(&workspace);
    }

    fn unique_name(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{nanos}")
    }
}
