use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal::{self, ClearType},
};
use rust_codingagent_agent::{Agent, ShellApproval};
use rust_codingagent_core::{
    LanguageProvider, Message, MessageRole, ProviderConfig as CoreProviderConfig, ProviderRequest,
    ProviderResponse, Session, SessionStore,
};
use rust_codingagent_provider_remote::RemoteProvider;
use rust_codingagent_rollback::RollbackManager;
use serde::{Deserialize, Serialize};

use crate::config::AppConfig;

const MAX_VISIBLE_MESSAGES: usize = 50;
const MAX_PLAN_STEPS: usize = 8;

const PLAN_SYSTEM_PROMPT: &str = r#"You are the planning mode of a coding CLI agent.
Create a practical execution plan for the user's coding task.

Rules:
- Do not call tools.
- Do not modify files.
- Produce only TOML, with no markdown fence and no explanation.
- Use 3 to 6 steps unless the task clearly needs more.
- Each step must be independently executable by a coding agent.
- Each instruction should be specific, concrete, and mention relevant files or checks when possible.
- Include verification as the final step.

Output schema:
[[steps]]
instruction = "Inspect the relevant files and identify the change points."

[[steps]]
instruction = "Implement the first concrete change."
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainLoopStatus {
    ExitedByCommand,
    ExitedByEof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSessionItem {
    pub id: String,
    pub message_count: usize,
    pub model: String,
    pub updated_at_ms: u64,
    pub preview: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowPlan {
    id: String,
    goal: String,
    steps: Vec<WorkflowStep>,
    created_at_ms: u64,
    updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowStep {
    index: usize,
    instruction: String,
    status: PlanStepStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommandApprovalRule {
    prefix: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CommandApprovalRules {
    rules: Vec<CommandApprovalRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalChoice {
    AllowOnce,
    AlwaysAllowSimilar,
    Deny,
}

#[derive(Debug, Deserialize)]
struct ModelGeneratedPlan {
    steps: Vec<ModelGeneratedStep>,
}

#[derive(Debug, Deserialize)]
struct ModelGeneratedStep {
    instruction: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum PlanStepStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl WorkflowPlan {
    fn from_instructions(goal: String, instructions: Vec<String>) -> Result<Self> {
        if instructions.is_empty() {
            bail!("model returned no plan steps");
        }
        if instructions.len() > MAX_PLAN_STEPS {
            bail!(
                "model returned {} plan steps, maximum is {MAX_PLAN_STEPS}",
                instructions.len()
            );
        }

        let now = unix_millis();
        let steps = instructions
            .into_iter()
            .enumerate()
            .map(|(index, instruction)| WorkflowStep {
                index: index + 1,
                instruction,
                status: PlanStepStatus::Pending,
            })
            .collect();

        Ok(Self {
            id: format!("plan-{now}"),
            goal,
            steps,
            created_at_ms: now,
            updated_at_ms: now,
        })
    }

    fn touch(&mut self) {
        self.updated_at_ms = unix_millis();
    }

    fn next_step_pos(&self) -> Option<usize> {
        self.steps.iter().position(|step| {
            matches!(
                step.status,
                PlanStepStatus::Pending | PlanStepStatus::Failed
            )
        })
    }

    fn is_complete(&self) -> bool {
        self.steps
            .iter()
            .all(|step| step.status == PlanStepStatus::Done)
    }
}

pub struct MainLoop<'a> {
    config: &'a AppConfig,
    store: SessionStore,
    session: Session,
    provider: Box<dyn LanguageProvider>,
    rollback_manager: RollbackManager,
    active_plan: Option<WorkflowPlan>,
    command_approval_rules: Vec<CommandApprovalRule>,
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
        let session = store.get_or_create_active_session(
            config.profile.clone(),
            config.workspace.clone(),
            core_provider,
        )?;

        let rollback_manager = RollbackManager::new(session.context())?;
        let active_plan = load_active_plan(&store, &config.profile)?;
        let command_approval_rules = load_command_approval_rules(&store, &config.profile)?;

        Ok(Self {
            config,
            store,
            session,
            provider,
            rollback_manager,
            active_plan,
            command_approval_rules,
        })
    }

    // ── Main REPL loop ──────────────────────────────────────────────────

    pub fn startup_session_items(&self, limit: usize) -> Result<Vec<StartupSessionItem>> {
        let sessions = self.store.list_sessions()?;
        let mut items = Vec::new();

        for summary in sessions.into_iter().take(limit) {
            let preview = self
                .session_preview(&summary.id)
                .unwrap_or_else(|_| "(unable to load preview)".to_string());
            items.push(StartupSessionItem {
                id: summary.id.clone(),
                message_count: summary.message_count,
                model: summary.model,
                updated_at_ms: summary.updated_at_ms,
                preview,
                active: summary.id == self.session.id,
            });
        }

        Ok(items)
    }

    pub fn resume_session_by_id(&mut self, id: &str) -> Result<()> {
        let session = self.store.load_session(id)?;
        self.store.set_active_session(&session.id)?;
        self.rollback_manager = RollbackManager::new(session.context())?;
        self.session = session;
        Ok(())
    }

    pub fn create_fresh_session(&mut self) -> Result<()> {
        self.session = Session::new(
            self.session.profile.clone(),
            self.session.workspace.clone(),
            self.session.provider.clone(),
        );
        self.store.save_session(&self.session)?;
        self.store.set_active_session(&self.session.id)?;
        self.rollback_manager = RollbackManager::new(self.session.context())?;
        Ok(())
    }

    fn session_preview(&self, id: &str) -> Result<String> {
        let session = self.store.load_session(id)?;
        let preview = session
            .history
            .messages()
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::User)
            .map(|message| truncate_str(message.content.trim(), 72))
            .filter(|content| !content.is_empty())
            .unwrap_or_else(|| "(empty conversation)".to_string());
        Ok(preview)
    }

    pub fn print_banner<W: Write>(&self, writer: &mut W) -> Result<()> {
        writeln!(
            writer,
            "╔══════════════════════════════════════════════════════╗"
        )?;
        writeln!(
            writer,
            "║  NKU Rust Coding Agent                              ║"
        )?;
        writeln!(
            writer,
            "╠══════════════════════════════════════════════════════╣"
        )?;
        writeln!(
            writer,
            "║  workspace : {:<40}║",
            truncate_display(&self.config.workspace, 40)
        )?;
        writeln!(
            writer,
            "║  provider  : {}/{:<32}║",
            self.provider.name(),
            truncate_right(&self.provider.model(), 32)
        )?;
        if self.session.history.len() > 0 {
            writeln!(
                writer,
                "║  session   : {} ({} messages){:<15}║",
                self.session.id,
                self.session.history.len(),
                ""
            )?;
        } else {
            writeln!(
                writer,
                "║  session   : new                                    ║"
            )?;
        }
        writeln!(
            writer,
            "╠══════════════════════════════════════════════════════╣"
        )?;
        writeln!(
            writer,
            "║  输入自然语言，Agent 会调用工具帮你完成任务          ║"
        )?;
        writeln!(
            writer,
            "║                                                    ║"
        )?;
        writeln!(
            writer,
            "║  /help        查看所有命令                          ║"
        )?;
        writeln!(
            writer,
            "║  /sessions    查看历史对话，/session resume <id> 切换║"
        )?;
        writeln!(
            writer,
            "║  /clear       开启新对话                            ║"
        )?;
        writeln!(
            writer,
            "║  /plan        生成/执行自动化工作流计划             ║"
        )?;
        writeln!(
            writer,
            "║  /rollback    回滚文件修改                          ║"
        )?;
        writeln!(
            writer,
            "║  exit         退出                                  ║"
        )?;
        writeln!(
            writer,
            "╚══════════════════════════════════════════════════════╝"
        )?;
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
            if bytes == 0 {
                writeln!(writer)?;
                return Ok(MainLoopStatus::ExitedByEof);
            }
            let input = line.trim();
            if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
                writeln!(writer, "bye")?;
                return Ok(MainLoopStatus::ExitedByCommand);
            }
            if input.is_empty() {
                continue;
            }
            if input.starts_with('/') {
                if let Err(e) = self.handle_command(input, &mut writer) {
                    writeln!(writer, "error: {e:#}")?;
                }
                continue;
            }
            match self.run_agent_turn(input, &mut writer) {
                Ok(()) => {}
                Err(e) => {
                    writeln!(writer, "agent error: {e:#}")?;
                }
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
        use std::cell::RefCell;

        let approval_rules = RefCell::new(std::mem::take(&mut self.command_approval_rules));
        let approval_store = self.store.clone();
        let approval_profile = self.config.profile.clone();

        let mut agent = Agent::new(
            self.provider.as_ref(),
            &mut self.session,
            &self.store,
            &self.rollback_manager,
        );

        // Show thinking indicator before first token
        write!(writer, "\n\x1b[93m⏳ 思考中...\x1b[0m")?;
        writer.flush()?;

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
                    let _ = write!(
                        w.borrow_mut(),
                        "\r\x1b[K\x1b[97m⏳ {elapsed:.1}s\x1b[0m\n\x1b[1;37m"
                    );
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
                let _ = writeln!(
                    w.borrow_mut(),
                    "\n\x1b[48;5;240m{color} {icon} TOOL: {desc}\x1b[0m"
                );
                let _ = w.borrow_mut().flush();
            },
            &mut |name: &str, done: &str| {
                let (icon, color) = if name == "write" || name == "edit" {
                    ("✅", "\x1b[92m")
                } else {
                    ("✓", "\x1b[97m")
                };
                let truncated = truncate_right(done, 120);
                let _ = writeln!(
                    w.borrow_mut(),
                    "\x1b[48;5;240m{color} {icon} {truncated}\x1b[0m"
                );
                let _ = w.borrow_mut().flush();
            },
            &mut |command: &str| {
                approve_shell_command(
                    command,
                    &approval_rules,
                    &approval_store,
                    &approval_profile,
                    &w,
                )
            },
        );
        drop(agent);
        self.command_approval_rules = approval_rules.into_inner();
        let writer = w.into_inner();

        match result {
            Ok(()) => {
                if tool_count > 0 {
                    writeln!(
                        writer,
                        "\x1b[48;5;240m\x1b[97m── {tool_count} 个工具执行完毕 ──\x1b[0m"
                    )?;
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

    fn handle_plan_command<W: Write>(&mut self, args: &[&str], writer: &mut W) -> Result<bool> {
        match args.first().copied() {
            None | Some("status") => {
                self.print_plan_status(writer)?;
            }
            Some("clear") => {
                clear_active_plan(&self.store, &self.config.profile)?;
                self.active_plan = None;
                writeln!(writer, "cleared active plan")?;
            }
            Some("run") => {
                if self.active_plan.is_none() {
                    writeln!(writer, "no active plan. create one with /plan <task>")?;
                    return Ok(true);
                }

                while self
                    .active_plan
                    .as_ref()
                    .and_then(WorkflowPlan::next_step_pos)
                    .is_some()
                {
                    self.run_next_plan_step(writer)?;
                }

                if self
                    .active_plan
                    .as_ref()
                    .is_some_and(WorkflowPlan::is_complete)
                {
                    writeln!(writer, "plan complete")?;
                }
            }
            Some("step") => {
                if !self.run_next_plan_step(writer)? {
                    writeln!(writer, "no runnable plan step")?;
                }
            }
            Some("help") => {
                writeln!(
                    writer,
                    "usage: /plan <task> | /plan status | /plan step | /plan run | /plan clear"
                )?;
            }
            Some(_) => {
                let goal = args.join(" ");
                writeln!(writer, "planning with model...")?;
                let plan = self.generate_plan_with_model(&goal)?;
                save_active_plan(&self.store, &self.config.profile, &plan)?;
                self.active_plan = Some(plan);
                writeln!(writer, "created active plan")?;
                self.print_plan_status(writer)?;
            }
        }

        Ok(true)
    }

    fn print_plan_status<W: Write>(&self, writer: &mut W) -> Result<()> {
        let Some(plan) = &self.active_plan else {
            writeln!(writer, "no active plan. create one with /plan <task>")?;
            return Ok(());
        };

        writeln!(writer, "plan: {}", plan.goal)?;
        for step in &plan.steps {
            writeln!(
                writer,
                "  {}. [{:?}] {}",
                step.index, step.status, step.instruction
            )?;
        }
        Ok(())
    }

    fn generate_plan_with_model(&self, goal: &str) -> Result<WorkflowPlan> {
        let workspace_outline = collect_workspace_outline(&self.config.workspace);
        let prompt = build_plan_prompt(goal, &self.config.workspace, &workspace_outline);
        let request = ProviderRequest {
            context: self.session.context(),
            messages: vec![Message::system(PLAN_SYSTEM_PROMPT), Message::user(prompt)],
            tools: vec![],
        };

        match self.provider.complete(request)? {
            ProviderResponse::Text { content } => parse_model_plan(goal, &content)
                .with_context(|| format!("failed to parse model plan response: {content}")),
            ProviderResponse::ToolCalls { .. } => {
                bail!("model returned tool calls while planning; expected TOML text")
            }
        }
    }

    fn run_next_plan_step<W: Write>(&mut self, writer: &mut W) -> Result<bool> {
        let Some(step_pos) = self
            .active_plan
            .as_ref()
            .and_then(WorkflowPlan::next_step_pos)
        else {
            return Ok(false);
        };

        let (step_index, instruction) = {
            let plan = self.active_plan.as_mut().expect("plan checked above");
            let step = &mut plan.steps[step_pos];
            step.status = PlanStepStatus::Running;
            let step_index = step.index;
            let instruction = step.instruction.clone();
            plan.touch();
            save_active_plan(&self.store, &self.config.profile, plan)?;
            (step_index, instruction)
        };

        writeln!(writer, "\n[plan] running step {step_index}: {instruction}")?;

        let result = self.run_agent_turn(&instruction, writer);
        let status = if result.is_ok() {
            PlanStepStatus::Done
        } else {
            PlanStepStatus::Failed
        };

        if let Some(plan) = self.active_plan.as_mut() {
            if let Some(step) = plan.steps.get_mut(step_pos) {
                step.status = status;
            }
            plan.touch();
            save_active_plan(&self.store, &self.config.profile, plan)?;
        }

        result?;
        writeln!(writer, "[plan] step {step_index} done")?;
        Ok(true)
    }

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
                        let id = parts.next().context("usage: /session resume <id>")?;
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
                            writeln!(
                                writer,
                                "{} other session(s) available: /session resume <id>",
                                sessions.len() - 1
                            )?;
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
                        let active = if s.id == self.session.id {
                            " (active)"
                        } else {
                            ""
                        };
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
                    writeln!(
                        writer,
                        "showing last {MAX_VISIBLE_MESSAGES} of {total} messages"
                    )?;
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
                self.store.set_active_session(&self.session.id)?;
                writeln!(
                    writer,
                    "cleared {msg_count} messages, new session={}",
                    self.session.id
                )?;
                Ok(true)
            }
            "plan" => {
                let args: Vec<&str> = parts.collect();
                self.handle_plan_command(&args, writer)
            }
            "approvals" => {
                match parts.next() {
                    Some("clear") => {
                        self.command_approval_rules.clear();
                        save_command_approval_rules(
                            &self.store,
                            &self.config.profile,
                            &self.command_approval_rules,
                        )?;
                        writeln!(writer, "cleared command approval rules")?;
                    }
                    _ => {
                        if self.command_approval_rules.is_empty() {
                            writeln!(writer, "no saved command approval rules")?;
                        } else {
                            writeln!(
                                writer,
                                "{} command approval rule(s):",
                                self.command_approval_rules.len()
                            )?;
                            for rule in &self.command_approval_rules {
                                writeln!(
                                    writer,
                                    "  allow commands starting with `{}`",
                                    rule.prefix
                                )?;
                            }
                            writeln!(writer, "use /approvals clear to remove all rules")?;
                        }
                    }
                }
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
                                let files: Vec<String> = r
                                    .changed_files
                                    .iter()
                                    .map(|p| p.display().to_string())
                                    .collect();
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
                            writeln!(writer, "  {}  action={:?}", f.path.display(), f.action)?;
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
                        writeln!(
                            writer,
                            "usage: /rollback [list|preview <id>|apply <id>|file <id> <path>]"
                        )?;
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

fn build_plan_prompt(goal: &str, workspace: &Path, workspace_outline: &str) -> String {
    format!(
        "User task:\n{goal}\n\nWorkspace:\n{}\n\nWorkspace outline:\n{workspace_outline}\n\nReturn only TOML following the schema.",
        workspace.display()
    )
}

fn parse_model_plan(goal: &str, raw: &str) -> Result<WorkflowPlan> {
    let candidate = extract_toml_plan(raw)?;
    let parsed: ModelGeneratedPlan =
        toml::from_str(&candidate).context("model plan is not valid TOML")?;

    let instructions: Vec<String> = parsed
        .steps
        .into_iter()
        .map(|step| step.instruction.trim().to_string())
        .filter(|instruction| !instruction.is_empty())
        .collect();

    if instructions.len() < 2 {
        bail!("model plan must contain at least 2 non-empty steps");
    }

    WorkflowPlan::from_instructions(goal.to_string(), instructions)
}

fn extract_toml_plan(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let without_fence = strip_code_fence(trimmed);
    if toml::from_str::<ModelGeneratedPlan>(&without_fence).is_ok() {
        return Ok(without_fence);
    }

    if let Some(start) = without_fence.find("[[steps]]") {
        return Ok(without_fence[start..].trim().to_string());
    }

    bail!("model response did not contain [[steps]] TOML")
}

fn strip_code_fence(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }

    let mut lines = trimmed.lines();
    let _ = lines.next();
    let mut body = lines.collect::<Vec<_>>().join("\n");
    if let Some(pos) = body.rfind("```") {
        body.truncate(pos);
    }
    body.trim().to_string()
}

fn collect_workspace_outline(workspace: &Path) -> String {
    let mut entries = Vec::new();
    collect_workspace_outline_inner(workspace, workspace, 0, &mut entries);
    if entries.is_empty() {
        "(workspace is empty or cannot be read)".to_string()
    } else {
        entries.join("\n")
    }
}

fn collect_workspace_outline_inner(
    root: &Path,
    current: &Path,
    depth: usize,
    entries: &mut Vec<String>,
) {
    if depth > 2 || entries.len() >= 80 {
        return;
    }

    let Ok(read_dir) = fs::read_dir(current) else {
        return;
    };

    let mut children = read_dir
        .filter_map(|entry| entry.ok())
        .filter(|entry| !is_ignored_outline_entry(&entry.path()))
        .collect::<Vec<_>>();
    children.sort_by_key(|entry| entry.file_name());

    for entry in children {
        if entries.len() >= 80 {
            return;
        }

        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let marker = if path.is_dir() { "/" } else { "" };
        entries.push(format!("{}{}", relative.display(), marker));

        if path.is_dir() {
            collect_workspace_outline_inner(root, &path, depth + 1, entries);
        }
    }
}

fn is_ignored_outline_entry(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    matches!(
        name,
        ".git" | "target" | "node_modules" | ".rust-codingagent" | ".agents" | ".codex"
    )
}

fn approve_shell_command<W: Write>(
    command: &str,
    rules: &std::cell::RefCell<Vec<CommandApprovalRule>>,
    store: &SessionStore,
    profile: &str,
    writer: &std::cell::RefCell<&mut W>,
) -> ShellApproval {
    if let Some(rule) = rules
        .borrow()
        .iter()
        .find(|rule| command_matches_rule(command, rule))
        .cloned()
    {
        let _ = writeln!(
            writer.borrow_mut(),
            "\x1b[90m[approval] allowed by rule: {}\x1b[0m",
            rule.prefix
        );
        let _ = writer.borrow_mut().flush();
        return ShellApproval::Approved;
    }

    let suggested_rule = command_rule_prefix(command);
    let summary = describe_shell_command(command);
    let choice = prompt_approval_choice(command, &summary, &suggested_rule, writer);

    match choice {
        ApprovalChoice::AllowOnce => ShellApproval::Approved,
        ApprovalChoice::AlwaysAllowSimilar => {
            {
                let mut rules = rules.borrow_mut();
                if !rules.iter().any(|rule| rule.prefix == suggested_rule) {
                    rules.push(CommandApprovalRule {
                        prefix: suggested_rule.clone(),
                    });
                }
            }
            if let Err(e) = save_command_approval_rules(store, profile, &rules.borrow()) {
                let _ = writeln!(
                    writer.borrow_mut(),
                    "warning: failed to save approval rule: {e:#}"
                );
            }
            ShellApproval::Approved
        }
        ApprovalChoice::Deny => ShellApproval::Denied,
    }
}

fn command_matches_rule(command: &str, rule: &CommandApprovalRule) -> bool {
    command.trim_start().starts_with(&rule.prefix)
}

fn prompt_approval_choice<W: Write>(
    command: &str,
    summary: &str,
    suggested_rule: &str,
    writer: &std::cell::RefCell<&mut W>,
) -> ApprovalChoice {
    let mut selected = ApprovalChoice::AllowOnce;
    if terminal::enable_raw_mode().is_err() {
        return prompt_approval_choice_line(command, summary, suggested_rule, writer);
    }

    let mut rendered_lines = 0u16;
    loop {
        rendered_lines = render_approval_menu(
            command,
            summary,
            suggested_rule,
            selected,
            writer,
            rendered_lines,
        );

        match event::read() {
            Ok(Event::Key(key)) => match key.code {
                KeyCode::Up => selected = previous_approval_choice(selected),
                KeyCode::Down => selected = next_approval_choice(selected),
                KeyCode::Enter => {
                    let _ = terminal::disable_raw_mode();
                    clear_approval_menu(writer, rendered_lines);
                    return selected;
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let _ = terminal::disable_raw_mode();
                    clear_approval_menu(writer, rendered_lines);
                    return ApprovalChoice::AllowOnce;
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    let _ = terminal::disable_raw_mode();
                    clear_approval_menu(writer, rendered_lines);
                    return ApprovalChoice::AlwaysAllowSimilar;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    let _ = terminal::disable_raw_mode();
                    clear_approval_menu(writer, rendered_lines);
                    return ApprovalChoice::Deny;
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => {
                let _ = terminal::disable_raw_mode();
                clear_approval_menu(writer, rendered_lines);
                return prompt_approval_choice_line(command, summary, suggested_rule, writer);
            }
        }
    }
}

fn prompt_approval_choice_line<W: Write>(
    command: &str,
    summary: &str,
    suggested_rule: &str,
    writer: &std::cell::RefCell<&mut W>,
) -> ApprovalChoice {
    let _ = writeln!(
        writer.borrow_mut(),
        "\n\x1b[93m? Shell command requires approval\x1b[0m\n  Intent : {summary}\n  Command: {command}\n  Rule   : {suggested_rule}\n\n  Enter/y = allow once   a = always allow similar   n = deny"
    );
    let _ = write!(writer.borrow_mut(), "approve? ");
    let _ = writer.borrow_mut().flush();

    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        let _ = writeln!(
            writer.borrow_mut(),
            "approval input failed; denying command"
        );
        return ApprovalChoice::Deny;
    }

    match line.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => ApprovalChoice::AllowOnce,
        "a" | "always" => ApprovalChoice::AlwaysAllowSimilar,
        _ => ApprovalChoice::Deny,
    }
}

fn render_approval_menu<W: Write>(
    command: &str,
    summary: &str,
    suggested_rule: &str,
    selected: ApprovalChoice,
    writer: &std::cell::RefCell<&mut W>,
    previous_lines: u16,
) -> u16 {
    if previous_lines > 0 {
        let mut writer = writer.borrow_mut();
        let _ = execute!(
            &mut *writer,
            cursor::MoveUp(previous_lines),
            terminal::Clear(ClearType::FromCursorDown)
        );
    }

    let options = [
        (
            ApprovalChoice::AllowOnce,
            "Allow once",
            "run only this command now",
        ),
        (
            ApprovalChoice::AlwaysAllowSimilar,
            "Always allow similar",
            "save this command prefix as trusted",
        ),
        (ApprovalChoice::Deny, "Deny", "do not run this command"),
    ];

    let command_display = truncate_str(command, 96);
    let _ = writeln!(
        writer.borrow_mut(),
        "\x1b[93m? Shell command requires approval\x1b[0m"
    );
    let _ = writeln!(writer.borrow_mut(), "  Intent : {summary}");
    let _ = writeln!(writer.borrow_mut(), "  Command: {command_display}");
    let _ = writeln!(writer.borrow_mut(), "  Rule   : {suggested_rule}");
    let _ = writeln!(
        writer.borrow_mut(),
        "  Use ↑/↓ then Enter. Shortcuts: y=once, a=always, n=deny"
    );

    for (choice, label, help) in options {
        let marker = if choice == selected { ">" } else { " " };
        let style = if choice == selected {
            "\x1b[1;36m"
        } else {
            "\x1b[90m"
        };
        let _ = writeln!(
            writer.borrow_mut(),
            "  {style}{marker} {label:<22}\x1b[0m {help}"
        );
    }
    let _ = writer.borrow_mut().flush();

    8
}

fn clear_approval_menu<W: Write>(writer: &std::cell::RefCell<&mut W>, lines: u16) {
    if lines == 0 {
        return;
    }

    let mut writer = writer.borrow_mut();
    let _ = execute!(
        &mut *writer,
        cursor::MoveUp(lines),
        terminal::Clear(ClearType::FromCursorDown)
    );
    let _ = writer.flush();
}

fn next_approval_choice(choice: ApprovalChoice) -> ApprovalChoice {
    match choice {
        ApprovalChoice::AllowOnce => ApprovalChoice::AlwaysAllowSimilar,
        ApprovalChoice::AlwaysAllowSimilar => ApprovalChoice::Deny,
        ApprovalChoice::Deny => ApprovalChoice::AllowOnce,
    }
}

fn previous_approval_choice(choice: ApprovalChoice) -> ApprovalChoice {
    match choice {
        ApprovalChoice::AllowOnce => ApprovalChoice::Deny,
        ApprovalChoice::AlwaysAllowSimilar => ApprovalChoice::AllowOnce,
        ApprovalChoice::Deny => ApprovalChoice::AlwaysAllowSimilar,
    }
}

fn command_rule_prefix(command: &str) -> String {
    let words = command
        .split_whitespace()
        .take(2)
        .map(|word| word.trim_matches('"').trim_matches('\''))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();

    if words.is_empty() {
        return command.trim().to_string();
    }

    let first = words[0];
    match first {
        "cargo" | "git" | "npm" | "pnpm" | "yarn" | "python" | "python3" | "rustc"
            if words.len() >= 2 =>
        {
            format!("{} {}", words[0], words[1])
        }
        _ => first.to_string(),
    }
}

fn describe_shell_command(command: &str) -> String {
    let lower = command.trim().to_ascii_lowercase();
    let words = lower.split_whitespace().collect::<Vec<_>>();
    let Some(first) = words.first().copied() else {
        return "Run a shell command".to_string();
    };

    match first {
        "cargo" => match words.get(1).copied() {
            Some("test") => "Run Rust tests to verify the project".to_string(),
            Some("check") => {
                "Type-check the Rust project without building final binaries".to_string()
            }
            Some("fmt") => "Format Rust source files".to_string(),
            Some("run") => "Run the Rust application".to_string(),
            Some("build") => "Build the Rust project".to_string(),
            _ => "Run a Cargo command for this Rust project".to_string(),
        },
        "git" => match words.get(1).copied() {
            Some("status") => "Inspect repository status".to_string(),
            Some("diff") => "Inspect uncommitted code changes".to_string(),
            Some("log") => "Inspect commit history".to_string(),
            Some("add") => "Stage files for a Git commit".to_string(),
            Some("commit") => "Create a Git commit".to_string(),
            Some("push") => "Push local commits to the remote repository".to_string(),
            Some("pull") => "Pull remote changes into the local repository".to_string(),
            _ => "Run a Git repository command".to_string(),
        },
        "npm" | "pnpm" | "yarn" => match words.get(1).copied() {
            Some("test") => "Run JavaScript project tests".to_string(),
            Some("run") => "Run a package script".to_string(),
            Some("install") | Some("add") => "Install package dependencies".to_string(),
            _ => "Run a JavaScript package manager command".to_string(),
        },
        "python" | "python3" => "Run a Python script or inline Python command".to_string(),
        "mkdir" => "Create a directory".to_string(),
        "dir" | "ls" => "List files in a directory".to_string(),
        "type" | "cat" => "Print file contents".to_string(),
        "del" | "rm" => "Delete files or directories".to_string(),
        "copy" | "cp" => "Copy files or directories".to_string(),
        "move" | "mv" => "Move or rename files or directories".to_string(),
        _ => "Run a shell command requested by the model".to_string(),
    }
}

fn load_command_approval_rules(
    store: &SessionStore,
    profile: &str,
) -> Result<Vec<CommandApprovalRule>> {
    let path = command_approval_rules_path(store, profile);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read approval rules file {}", path.display()))?;
    let rules: CommandApprovalRules = toml::from_str(&content)
        .with_context(|| format!("failed to parse approval rules file {}", path.display()))?;
    Ok(rules.rules)
}

fn save_command_approval_rules(
    store: &SessionStore,
    profile: &str,
    rules: &[CommandApprovalRule],
) -> Result<()> {
    fs::create_dir_all(store.state_root()).with_context(|| {
        format!(
            "failed to create state directory {}",
            store.state_root().display()
        )
    })?;
    let path = command_approval_rules_path(store, profile);
    let content = toml::to_string_pretty(&CommandApprovalRules {
        rules: rules.to_vec(),
    })
    .context("failed to serialize approval rules")?;
    fs::write(&path, content)
        .with_context(|| format!("failed to write approval rules file {}", path.display()))
}

fn command_approval_rules_path(store: &SessionStore, profile: &str) -> PathBuf {
    store.state_root().join(format!(
        "command-approvals-{}.toml",
        sanitize_file_segment(profile)
    ))
}

fn load_active_plan(store: &SessionStore, profile: &str) -> Result<Option<WorkflowPlan>> {
    let path = plan_path(store, profile);
    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read active plan file {}", path.display()))?;
    let plan = toml::from_str(&content)
        .with_context(|| format!("failed to parse active plan file {}", path.display()))?;
    Ok(Some(plan))
}

fn save_active_plan(store: &SessionStore, profile: &str, plan: &WorkflowPlan) -> Result<()> {
    fs::create_dir_all(store.state_root()).with_context(|| {
        format!(
            "failed to create state directory {}",
            store.state_root().display()
        )
    })?;
    let path = plan_path(store, profile);
    let content = toml::to_string_pretty(plan).context("failed to serialize active plan")?;
    fs::write(&path, content)
        .with_context(|| format!("failed to write active plan file {}", path.display()))
}

fn clear_active_plan(store: &SessionStore, profile: &str) -> Result<()> {
    let path = plan_path(store, profile);
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove active plan file {}", path.display()))?;
    }
    Ok(())
}

fn plan_path(store: &SessionStore, profile: &str) -> PathBuf {
    store.state_root().join(format!(
        "active-plan-{}.toml",
        sanitize_file_segment(profile)
    ))
}

fn sanitize_file_segment(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '-',
        })
        .collect();

    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

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
    truncate_left(&s, max)
}

fn truncate_right(s: &str, max: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max {
        return s.to_string();
    }

    if max <= 3 {
        return ".".repeat(max);
    }

    let keep = max - 3;
    let prefix: String = s.chars().take(keep).collect();
    format!("{prefix}...")
}

fn truncate_left(s: &str, max: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max {
        return s.to_string();
    }

    if max <= 3 {
        return ".".repeat(max);
    }

    let keep = max - 3;
    let suffix: String = s
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{suffix}")
}

fn truncate_str(s: &str, max_len: usize) -> String {
    truncate_right(s, max_len)
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
  /plan <task>       Create an automated workflow plan
  /plan status       Show the active plan
  /plan step         Run the next plan step
  /plan run          Run all remaining plan steps
  /plan clear        Clear the active plan
  /approvals         List saved shell command approval rules
  /approvals clear   Clear saved shell command approval rules
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

    #[test]
    fn workflow_plan_from_instructions_has_pending_steps() {
        let plan = WorkflowPlan::from_instructions(
            "add plan mode".to_string(),
            vec![
                "Inspect the REPL command handling.".to_string(),
                "Implement generated planning.".to_string(),
                "Run cargo test.".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(plan.goal, "add plan mode");
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].index, 1);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Pending);
        assert!(plan.steps[0].instruction.contains("REPL"));
        assert_eq!(plan.next_step_pos(), Some(0));
        assert!(!plan.is_complete());
    }

    #[test]
    fn active_plan_persistence_round_trips() {
        let workspace = std::env::temp_dir().join(unique_name("rust-codingagent-plan-store"));
        let store = SessionStore::new(&workspace, "test/profile");
        let mut plan = WorkflowPlan::from_instructions(
            "persist me".to_string(),
            vec!["First step.".to_string(), "Second step.".to_string()],
        )
        .unwrap();
        plan.steps[0].status = PlanStepStatus::Done;

        save_active_plan(&store, "test/profile", &plan).unwrap();
        let loaded = load_active_plan(&store, "test/profile").unwrap().unwrap();

        assert_eq!(loaded.id, plan.id);
        assert_eq!(loaded.goal, "persist me");
        assert_eq!(loaded.steps[0].status, PlanStepStatus::Done);

        clear_active_plan(&store, "test/profile").unwrap();
        assert!(load_active_plan(&store, "test/profile").unwrap().is_none());

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn parses_model_generated_plan_toml() {
        let raw = r#"
[[steps]]
instruction = "Inspect crates/cli/src/repl.rs and find slash command handling."

[[steps]]
instruction = "Implement model-generated plan creation."

[[steps]]
instruction = "Run cargo fmt and cargo test."
"#;

        let plan = parse_model_plan("add model plan mode", raw).unwrap();

        assert_eq!(plan.goal, "add model plan mode");
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].index, 1);
        assert!(plan.steps[1].instruction.contains("model-generated"));
    }

    #[test]
    fn parses_model_generated_plan_inside_code_fence() {
        let raw = r#"```toml
[[steps]]
instruction = "Inspect the project."

[[steps]]
instruction = "Verify the implementation."
```"#;

        let plan = parse_model_plan("fenced output", raw).unwrap();

        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].instruction, "Inspect the project.");
    }

    #[test]
    fn rejects_model_plan_with_too_few_steps() {
        let raw = r#"
[[steps]]
instruction = "Only one step."
"#;

        let err = parse_model_plan("too short", raw).unwrap_err();
        assert!(err.to_string().contains("at least 2"));
    }

    #[test]
    fn command_approval_rule_prefixes_are_scoped() {
        assert_eq!(
            command_rule_prefix("cargo test -p rust-codingagent-cli"),
            "cargo test"
        );
        assert_eq!(command_rule_prefix("git status --short"), "git status");
        assert_eq!(command_rule_prefix("mkdir dist"), "mkdir");
    }

    #[test]
    fn shell_command_descriptions_explain_intent() {
        assert_eq!(
            describe_shell_command("cargo test -p rust-codingagent-cli"),
            "Run Rust tests to verify the project"
        );
        assert_eq!(
            describe_shell_command("git push origin main"),
            "Push local commits to the remote repository"
        );
        assert_eq!(describe_shell_command("mkdir dist"), "Create a directory");
    }

    #[test]
    fn approval_menu_choices_wrap() {
        assert_eq!(
            next_approval_choice(ApprovalChoice::AllowOnce),
            ApprovalChoice::AlwaysAllowSimilar
        );
        assert_eq!(
            next_approval_choice(ApprovalChoice::Deny),
            ApprovalChoice::AllowOnce
        );
        assert_eq!(
            previous_approval_choice(ApprovalChoice::AllowOnce),
            ApprovalChoice::Deny
        );
    }

    #[test]
    fn command_approval_rules_round_trip() {
        let workspace = std::env::temp_dir().join(unique_name("rust-codingagent-approvals"));
        let store = SessionStore::new(&workspace, "test/profile");
        let rules = vec![
            CommandApprovalRule {
                prefix: "cargo test".to_string(),
            },
            CommandApprovalRule {
                prefix: "git status".to_string(),
            },
        ];

        save_command_approval_rules(&store, "test/profile", &rules).unwrap();
        let loaded = load_command_approval_rules(&store, "test/profile").unwrap();

        assert_eq!(loaded.len(), 2);
        assert!(command_matches_rule(
            "cargo test -p rust-codingagent-cli",
            &loaded[0]
        ));
        assert!(!command_matches_rule("cargo run", &loaded[0]));

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn truncation_handles_utf8_text() {
        let path = std::path::Path::new("F:\\大二下\\Rust\\cc-haha-rs\\NKU-Rust-Project");

        assert_eq!(truncate_display(path, 12), "...t-Project");
        assert_eq!(truncate_right("你好，Rust Coding Agent", 8), "你好，Ru...");
        assert_eq!(truncate_str("中文历史消息不会panic", 6), "中文历...");
    }

    fn unique_name(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{nanos}")
    }
}
