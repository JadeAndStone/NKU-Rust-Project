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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::AppConfig;

const MAX_VISIBLE_MESSAGES: usize = 50;
const MAX_PLAN_STEPS: usize = 8;
const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2;37m";
const MUTED: &str = "\x1b[90m";
const CYAN: &str = "\x1b[36m";
const BORDER: &str = "\x1b[2;37m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_WHITE: &str = "\x1b[1;37m";
const GREEN: &str = "\x1b[32m";
const BOLD_GREEN: &str = "\x1b[1;32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const WHITE: &str = "\x1b[37m";

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

#[derive(Debug)]
struct TurnUiState {
    answer_started: bool,
    answer_line_start: bool,
    tool_count: u32,
    loading_visible: bool,
    loading_frame: usize,
    tool_line_active: bool,
}

impl Default for TurnUiState {
    fn default() -> Self {
        Self {
            answer_started: false,
            answer_line_start: true,
            tool_count: 0,
            loading_visible: false,
            loading_frame: 0,
            tool_line_active: false,
        }
    }
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

impl PlanStepStatus {
    fn label(self) -> &'static str {
        match self {
            PlanStepStatus::Pending => "待执行",
            PlanStepStatus::Running => "执行中",
            PlanStepStatus::Done => "已完成",
            PlanStepStatus::Failed => "失败",
        }
    }
}

impl WorkflowPlan {
    fn from_instructions(goal: String, instructions: Vec<String>) -> Result<Self> {
        if instructions.is_empty() {
            bail!("模型没有返回计划步骤");
        }
        if instructions.len() > MAX_PLAN_STEPS {
            bail!(
                "模型返回了 {} 个计划步骤，最多只能有 {MAX_PLAN_STEPS} 个",
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
            .unwrap_or_else(|| "（空会话）".to_string());
        Ok(preview)
    }

    pub fn print_banner<W: Write>(&self, writer: &mut W) -> Result<()> {
        let width = terminal_content_width();
        let workspace = truncate_display(&self.config.workspace, width.saturating_sub(14));
        let provider = truncate_to_width(
            &format!("{}/{}", self.provider.name(), self.provider.model()),
            width.saturating_sub(18),
        );
        let session = if self.session.history.len() > 0 {
            format!("{}  {} 条消息", self.session.id, self.session.history.len())
        } else {
            "新会话".to_string()
        };
        let session = truncate_to_width(&session, width.saturating_sub(18));

        writeln!(writer)?;
        write_card_top(writer, width)?;
        write_card_line(writer, width, "  NKU·RS", CYAN)?;
        write_card_line(writer, width, "  南开 Rust 编程助手", BOLD_WHITE)?;
        write_card_line(
            writer,
            width,
            "  本地代码代理 · 文件读写 · 命令审批 · 可回滚",
            MUTED,
        )?;
        write_card_separator(writer, width)?;
        let inner = width.saturating_sub(4);
        write_card_line(
            writer,
            width,
            &two_column_line(
                &format!("模型  {provider}"),
                &format!("会话  {session}"),
                inner,
            ),
            WHITE,
        )?;
        write_card_line(writer, width, &format!("工作区  {workspace}"), WHITE)?;
        write_card_line(
            writer,
            width,
            "快捷命令  /帮助  /会话列表  /工具  /回滚",
            MUTED,
        )?;
        write_card_bottom(writer, width)?;
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
            write_input_prompt(&mut writer)?;
            writer.flush()?;
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                writeln!(writer)?;
                return Ok(MainLoopStatus::ExitedByEof);
            }
            let input = line.trim();
            if is_exit_command(input) {
                writeln!(writer, "已退出。")?;
                return Ok(MainLoopStatus::ExitedByCommand);
            }
            if input.is_empty() {
                continue;
            }
            if self.handle_plain_intent(input, &mut writer)? {
                continue;
            }
            if input.starts_with('/') {
                if let Err(e) = self.handle_command(input, &mut writer) {
                    writeln!(writer, "错误：{e:#}")?;
                }
                continue;
            }
            match self.run_agent_turn(input, &mut writer) {
                Ok(()) => {}
                Err(e) => {
                    writeln!(writer, "助手错误：{e:#}")?;
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

    pub fn handle_plain_intent<W: Write>(&mut self, input: &str, writer: &mut W) -> Result<bool> {
        if is_session_switch_request(input) {
            self.print_session_switch_help(writer)?;
            return Ok(true);
        }

        if is_plain_help_request(input) {
            writeln!(writer, "{}", HELP_TEXT)?;
            return Ok(true);
        }

        Ok(false)
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

        let w = RefCell::new(writer);
        let ui = RefCell::new(TurnUiState::default());
        let start_time = std::time::Instant::now();
        tick_loading_line(&mut *w.borrow_mut(), &mut ui.borrow_mut())?;

        let result = agent.run_streaming(
            input,
            &mut |token: &str| {
                let mut state = ui.borrow_mut();
                let _ = clear_transient_line(&mut *w.borrow_mut(), &mut state);
                if !state.answer_started {
                    state.answer_started = true;
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let _ = write!(
                        w.borrow_mut(),
                        "{DIM}╭─{RESET} {BOLD_CYAN}回答{RESET} {MUTED}{elapsed:.1}s{RESET}\n"
                    );
                }
                let _ =
                    write_answer_token(&mut *w.borrow_mut(), token, &mut state.answer_line_start);
                let _ = w.borrow_mut().flush();
            },
            &mut || {
                let _ = tick_loading_line(&mut *w.borrow_mut(), &mut ui.borrow_mut());
            },
            &mut |name: &str, desc: &str| {
                let mut state = ui.borrow_mut();
                let _ = clear_transient_line(&mut *w.borrow_mut(), &mut state);
                state.tool_count += 1;
                let color = match name {
                    "read" | "read_pdf" | "read_docx" => CYAN,
                    "write" => GREEN,
                    "edit" => YELLOW,
                    "grep" => BLUE,
                    "shell" => WHITE,
                    _ => WHITE,
                };
                let desc = compact_tool_description(name, desc);
                let tool_name = tool_display_name(name);
                let _ = write!(
                    w.borrow_mut(),
                    "{DIM}│{RESET} {color}{tool_name}{RESET} {MUTED}{desc}{RESET}"
                );
                state.tool_line_active = true;
                let _ = w.borrow_mut().flush();
            },
            &mut |name: &str, done: &str| {
                let mut state = ui.borrow_mut();
                let color = if name == "write" || name == "edit" {
                    GREEN
                } else {
                    MUTED
                };
                let _ = clear_transient_line(&mut *w.borrow_mut(), &mut state);
                let summary = compact_tool_done(name, done);
                let tool_name = tool_display_name(name);
                let _ = writeln!(
                    w.borrow_mut(),
                    "{DIM}│{RESET} {color}✓ {tool_name}{RESET} {MUTED}{summary}{RESET}"
                );
                let _ = w.borrow_mut().flush();
            },
            &mut |command: &str| {
                {
                    let mut state = ui.borrow_mut();
                    let _ = clear_transient_line(&mut *w.borrow_mut(), &mut state);
                }
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
        let mut state = ui.into_inner();
        clear_transient_line(writer, &mut state)?;

        match result {
            Ok(()) => {
                if state.answer_started {
                    if !state.answer_line_start {
                        writeln!(writer, "{RESET}")?;
                    } else {
                        write!(writer, "{RESET}")?;
                    }
                    let tool_note = if state.tool_count > 0 {
                        format!(" · 工具 {} 次", state.tool_count)
                    } else {
                        String::new()
                    };
                    if tool_note.is_empty() {
                        writeln!(writer, "{DIM}╰─{RESET} {BOLD_GREEN}✓ 完成{RESET}")?;
                    } else {
                        writeln!(
                            writer,
                            "{DIM}╰─{RESET} {BOLD_GREEN}✓ 完成{RESET} {MUTED}{tool_note}{RESET}"
                        )?;
                    }
                }
                if state.tool_count > 0 {
                    writeln!(
                        writer,
                        "{DIM}  ↳{RESET} {MUTED}可用 /回滚 查看或恢复本轮文件改动。{RESET}"
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
            None | Some("status") | Some("状态") => {
                self.print_plan_status(writer)?;
            }
            Some("clear") | Some("清空") => {
                clear_active_plan(&self.store, &self.config.profile)?;
                self.active_plan = None;
                writeln!(writer, "已清除当前计划。")?;
            }
            Some("run") | Some("执行") => {
                if self.active_plan.is_none() {
                    writeln!(writer, "当前没有计划。可以用 /计划 <任务> 创建。")?;
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
                    writeln!(writer, "计划已完成。")?;
                }
            }
            Some("step") | Some("下一步") => {
                if !self.run_next_plan_step(writer)? {
                    writeln!(writer, "没有可执行的计划步骤。")?;
                }
            }
            Some("help") | Some("帮助") => {
                writeln!(
                    writer,
                    "用法：/计划 <任务> | /计划 状态 | /计划 下一步 | /计划 执行 | /计划 清空"
                )?;
            }
            Some(_) => {
                let goal = args.join(" ");
                writeln!(writer, "正在让模型生成计划...")?;
                let plan = self.generate_plan_with_model(&goal)?;
                save_active_plan(&self.store, &self.config.profile, &plan)?;
                self.active_plan = Some(plan);
                writeln!(writer, "已创建当前计划。")?;
                self.print_plan_status(writer)?;
            }
        }

        Ok(true)
    }

    fn print_plan_status<W: Write>(&self, writer: &mut W) -> Result<()> {
        let Some(plan) = &self.active_plan else {
            writeln!(writer, "当前没有计划。可以用 /计划 <任务> 创建。")?;
            return Ok(());
        };

        writeln!(writer, "计划：{}", plan.goal)?;
        for step in &plan.steps {
            writeln!(
                writer,
                "  {}. [{}] {}",
                step.index,
                step.status.label(),
                step.instruction
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
                .with_context(|| format!("无法解析模型返回的计划：{content}")),
            ProviderResponse::ToolCalls { .. } => {
                bail!("模型在计划模式下返回了工具调用，期望的是 TOML 文本")
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

        writeln!(writer, "\n[计划] 正在执行第 {step_index} 步：{instruction}")?;

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
        writeln!(writer, "[计划] 第 {step_index} 步已完成。")?;
        Ok(true)
    }

    fn print_session_switch_help<W: Write>(&self, writer: &mut W) -> Result<()> {
        self.print_session_overview(writer)
    }

    fn print_session_overview<W: Write>(&self, writer: &mut W) -> Result<()> {
        let ctx = self.session.context();
        let sessions = self.startup_session_items(12)?;
        let others = sessions
            .iter()
            .filter(|item| item.id != self.session.id)
            .collect::<Vec<_>>();

        writeln!(
            writer,
            "\n{DIM}╭─{RESET} {BOLD_CYAN}会话{RESET} {MUTED}当前上下文{RESET}"
        )?;
        writeln!(
            writer,
            "{DIM}│{RESET} {MUTED}当前{RESET}  {CYAN}{}{RESET}  {MUTED}{} 条消息 · {}/{} · {}{RESET}",
            ctx.session_id,
            ctx.turn_index,
            ctx.provider,
            ctx.model,
            format_age(self.session.updated_at_ms)
        )?;
        writeln!(
            writer,
            "{DIM}│{RESET} {MUTED}工作区{RESET} {}",
            ctx.workspace.display()
        )?;

        if others.is_empty() {
            writeln!(writer, "{DIM}│{RESET} {MUTED}没有其他可切换会话。{RESET}")?;
        } else {
            writeln!(
                writer,
                "{DIM}│{RESET} {MUTED}可切换会话（复制 ID 到命令里）：{RESET}"
            )?;
            for (index, item) in others.into_iter().take(6).enumerate() {
                write_session_item(writer, index + 1, item)?;
            }
        }

        writeln!(
            writer,
            "{DIM}╰─{RESET} {MUTED}切换：{RESET}{BOLD_WHITE}/会话 切换 <会话ID>{RESET}  {MUTED}新建：{RESET}{BOLD_WHITE}/清空{RESET}\n"
        )?;
        Ok(())
    }

    fn print_session_list<W: Write>(&self, writer: &mut W) -> Result<()> {
        let sessions = self.startup_session_items(20)?;
        if sessions.is_empty() {
            writeln!(writer, "没有已保存的会话。")?;
            return Ok(());
        }

        writeln!(
            writer,
            "\n{DIM}╭─{RESET} {BOLD_CYAN}会话列表{RESET} {MUTED}共 {} 个{RESET}",
            sessions.len()
        )?;
        for (index, item) in sessions.iter().enumerate() {
            write_session_item(writer, index + 1, item)?;
        }
        writeln!(
            writer,
            "{DIM}╰─{RESET} {MUTED}切换：{RESET}{BOLD_WHITE}/会话 切换 <会话ID>{RESET}  {MUTED}新建：{RESET}{BOLD_WHITE}/清空{RESET}\n"
        )?;
        Ok(())
    }

    pub fn handle_command<W: Write>(&mut self, input: &str, writer: &mut W) -> Result<bool> {
        let command = input.strip_prefix('/').unwrap_or(input);
        let mut parts = command.split_whitespace();
        let Some(name) = parts.next() else {
            return Ok(false);
        };

        match name {
            "help" | "帮助" => {
                writeln!(writer, "{}", HELP_TEXT)?;
                Ok(true)
            }
            "session" | "会话" => {
                let sub = parts.next();
                match sub {
                    Some("resume") | Some("切换") => {
                        let id = parts.next().context("用法：/会话 切换 <会话ID>")?;
                        match self.store.load_session(id) {
                            Ok(s) => {
                                self.store.set_active_session(&s.id)?;
                                self.rollback_manager = RollbackManager::new(s.context())?;
                                writeln!(
                                    writer,
                                    "已切换到会话 {}（{} 条消息，模型={}/{}）",
                                    s.id,
                                    s.history.len(),
                                    s.provider.name,
                                    s.provider.model
                                )?;
                                self.session = s;
                            }
                            Err(e) => {
                                writeln!(writer, "找不到会话：{id}（{e:#}）")?;
                                let sessions = self.store.list_sessions()?;
                                if sessions.is_empty() {
                                    writeln!(writer, "没有已保存的会话。")?;
                                } else {
                                    writeln!(writer, "可用会话：")?;
                                    for s in &sessions {
                                        writeln!(
                                            writer,
                                            "  {}  消息={}  更新时间={}",
                                            s.id, s.message_count, s.updated_at_ms
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        self.print_session_overview(writer)?;
                    }
                }
                Ok(true)
            }
            "sessions" | "会话列表" => {
                self.print_session_list(writer)?;
                Ok(true)
            }
            "history" | "历史" => {
                let messages = self.session.history.messages();
                if messages.is_empty() {
                    writeln!(writer, "历史消息为空。")?;
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
                        "共有 {total} 条消息，下面显示最近 {MAX_VISIBLE_MESSAGES} 条。"
                    )?;
                }
                for (i, msg) in messages.iter().enumerate().skip(start) {
                    let tc_info = match &msg.tool_calls {
                        Some(calls) if !calls.is_empty() => {
                            let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                            format!(" [工具调用：{}]", names.join(", "))
                        }
                        _ => String::new(),
                    };
                    let tool_id = match &msg.tool_call_id {
                        Some(id) => format!(" [工具调用ID：{id}]"),
                        None => String::new(),
                    };
                    writeln!(
                        writer,
                        "{} {}：{}{}{}",
                        i + 1,
                        message_role_label(&msg.role),
                        truncate_str(&msg.content, 200),
                        tc_info,
                        tool_id
                    )?;
                }
                Ok(true)
            }
            "model" | "模型" => {
                if let Some(model) = parts.next() {
                    self.session.set_model(model);
                    self.store.save_session(&self.session)?;
                    writeln!(
                        writer,
                        "模型已切换为 {}/{}",
                        self.session.provider.name, self.session.provider.model
                    )?;
                } else {
                    writeln!(
                        writer,
                        "当前模型：{}/{}",
                        self.session.provider.name, self.session.provider.model
                    )?;
                }
                Ok(true)
            }
            "clear" | "清空" => {
                let msg_count = self.session.history.len();
                self.session = Session::new(
                    self.session.profile.clone(),
                    self.session.workspace.clone(),
                    self.session.provider.clone(),
                );
                self.store.save_session(&self.session)?;
                self.store.set_active_session(&self.session.id)?;
                writeln!(
                    writer,
                    "已清除 {msg_count} 条消息，新会话={}",
                    self.session.id
                )?;
                Ok(true)
            }
            "plan" | "计划" => {
                let args: Vec<&str> = parts.collect();
                self.handle_plan_command(&args, writer)
            }
            "approvals" | "审批" => {
                match parts.next() {
                    Some("clear") | Some("清空") => {
                        self.command_approval_rules.clear();
                        save_command_approval_rules(
                            &self.store,
                            &self.config.profile,
                            &self.command_approval_rules,
                        )?;
                        writeln!(writer, "已清除命令审批规则。")?;
                    }
                    _ => {
                        if self.command_approval_rules.is_empty() {
                            writeln!(writer, "没有保存的命令审批规则。")?;
                        } else {
                            writeln!(
                                writer,
                                "{} 条命令审批规则：",
                                self.command_approval_rules.len()
                            )?;
                            for rule in &self.command_approval_rules {
                                writeln!(writer, "  允许以 `{}` 开头的命令", rule.prefix)?;
                            }
                            writeln!(writer, "使用 /审批 清空 删除全部规则。")?;
                        }
                    }
                }
                Ok(true)
            }
            "rollback" | "回滚" => {
                let sub = parts.next().unwrap_or("list");
                match sub {
                    "list" | "列表" => {
                        let records = self.rollback_manager.list_records()?;
                        if records.is_empty() {
                            writeln!(writer, "没有回滚记录。")?;
                        } else {
                            writeln!(writer, "{} 条回滚记录：", records.len())?;
                            for r in &records {
                                let files: Vec<String> = r
                                    .changed_files
                                    .iter()
                                    .map(|p| p.display().to_string())
                                    .collect();
                                writeln!(
                                    writer,
                                    "  {}  轮次={}  工具={}  文件=[{}]",
                                    r.id,
                                    r.turn_index,
                                    r.tool_name,
                                    files.join(", ")
                                )?;
                            }
                        }
                    }
                    "preview" | "预览" => {
                        let id = parts.next().context("用法：/回滚 预览 <记录ID>")?;
                        let preview = self.rollback_manager.preview(id)?;
                        writeln!(writer, "回滚预览 {}：", preview.record_id)?;
                        for f in &preview.files {
                            writeln!(writer, "  {}  动作={:?}", f.path.display(), f.action)?;
                            if !f.diff.is_empty() && f.diff != "(no changes)\n" {
                                for line in f.diff.lines() {
                                    writeln!(writer, "    {line}")?;
                                }
                            }
                        }
                    }
                    "apply" | "恢复" => {
                        let id = parts.next().context("用法：/回滚 恢复 <记录ID>")?;
                        let report = self.rollback_manager.restore(id)?;
                        writeln!(writer, "已应用回滚 {}：", report.record_id)?;
                        for f in &report.files {
                            writeln!(writer, "  {} → {:?}", f.path.display(), f.action)?;
                        }
                    }
                    "file" | "文件" => {
                        let id = parts.next().context("用法：/回滚 文件 <记录ID> <路径>")?;
                        let path = parts.next().context("用法：/回滚 文件 <记录ID> <路径>")?;
                        let report = self.rollback_manager.restore_file(id, path)?;
                        writeln!(writer, "已将回滚 {} 应用到指定文件：", report.record_id)?;
                        for f in &report.files {
                            writeln!(writer, "  {} → {:?}", f.path.display(), f.action)?;
                        }
                    }
                    other => {
                        writeln!(writer, "未知回滚子命令：{other}")?;
                        writeln!(
                            writer,
                            "用法：/回滚 [列表|预览 <记录ID>|恢复 <记录ID>|文件 <记录ID> <路径>]"
                        )?;
                    }
                }
                Ok(true)
            }
            "tools" | "工具" => {
                writeln!(writer, "可用工具：")?;
                writeln!(writer, "  read    — 读取文件内容")?;
                writeln!(writer, "  write   — 写入文件内容")?;
                writeln!(writer, "  edit    — 替换文件中的文本")?;
                writeln!(writer, "  grep    — 用正则搜索代码")?;
                writeln!(writer, "  shell   — 执行命令行命令")?;
                Ok(true)
            }
            _ => {
                writeln!(writer, "未知命令：{name}")?;
                writeln!(writer, "输入 /帮助 查看可用命令。")?;
                Ok(true)
            }
        }
    }
}

// ── Provider factory ────────────────────────────────────────────────────────

fn is_exit_command(input: &str) -> bool {
    let normalized = input.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "exit" | "quit" | "q" | "bye" | "exti" | "exi" | "eit"
    ) || matches!(input.trim(), "退出" | "/退出" | "结束" | "再见")
}

fn is_plain_help_request(input: &str) -> bool {
    matches!(input.trim(), "帮助" | "查看帮助" | "命令" | "可用命令")
}

fn is_session_switch_request(input: &str) -> bool {
    let compact = input
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    let lower = compact.to_ascii_lowercase();

    if ["实现", "开发", "功能", "代码", "怎么", "如何", "为什么"]
        .iter()
        .any(|word| compact.contains(word))
    {
        return false;
    }

    matches!(
        lower.as_str(),
        "session" | "sessions" | "switchsession" | "changesession"
    ) || (compact.chars().count() <= 12
        && compact.contains("会话")
        && (compact.contains("切换") || compact.contains("换") || compact.contains("选择")))
        || matches!(
            compact.as_str(),
            "我想切换会话" | "切换上下文" | "换上下文" | "选择会话"
        )
}

fn write_session_item<W: Write>(
    writer: &mut W,
    index: usize,
    item: &StartupSessionItem,
) -> Result<()> {
    let label = if item.active {
        "当前".to_string()
    } else {
        format!("{index}.")
    };
    let preview = truncate_to_width(&item.preview.replace('\n', " "), 80);
    writeln!(
        writer,
        "{DIM}│{RESET} {BOLD_WHITE}{}{RESET} {CYAN}{}{RESET}  {MUTED}{} 条消息 · {} · {}{RESET}",
        pad_to_width(&label, 5),
        item.id,
        item.message_count,
        item.model,
        format_age(item.updated_at_ms)
    )?;
    if !preview.is_empty() {
        writeln!(
            writer,
            "{DIM}│{RESET}       {MUTED}预览{RESET}  {}",
            preview
        )?;
    }
    Ok(())
}

fn format_age(updated_at_ms: u64) -> String {
    let now = unix_millis();
    if updated_at_ms > now {
        return "刚刚".to_string();
    }

    let seconds = (now - updated_at_ms) / 1000;
    match seconds {
        0..=59 => "刚刚".to_string(),
        60..=3599 => format!("{} 分钟前", seconds / 60),
        3600..=86_399 => format!("{} 小时前", seconds / 3600),
        86_400..=2_592_000 => format!("{} 天前", seconds / 86_400),
        _ => "较早".to_string(),
    }
}

fn message_role_label(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::System => "系统",
        MessageRole::User => "用户",
        MessageRole::Assistant => "助手",
        MessageRole::Tool => "工具",
    }
}

fn tool_display_name(name: &str) -> &'static str {
    match name {
        "read" => "读取",
        "read_pdf" => "读取PDF",
        "read_docx" => "读取DOCX",
        "write" => "写入",
        "edit" => "编辑",
        "grep" => "搜索",
        "shell" => "命令",
        _ => "工具",
    }
}

fn tick_loading_line<W: Write>(writer: &mut W, state: &mut TurnUiState) -> io::Result<()> {
    if state.answer_started || state.tool_line_active {
        return Ok(());
    }

    const FRAMES: &[&str] = &["◐", "◓", "◑", "◒"];
    let frame = FRAMES[state.loading_frame % FRAMES.len()];
    state.loading_frame = state.loading_frame.wrapping_add(1);
    state.loading_visible = true;

    write!(
        writer,
        "\r\x1b[K{DIM}╭─{RESET} {CYAN}{frame} 思考中{RESET} {MUTED}正在分析上下文，等待模型响应{RESET}"
    )?;
    writer.flush()
}

fn clear_transient_line<W: Write>(writer: &mut W, state: &mut TurnUiState) -> io::Result<()> {
    if state.loading_visible || state.tool_line_active {
        write!(writer, "\r\x1b[K")?;
        state.loading_visible = false;
        state.tool_line_active = false;
    }
    Ok(())
}

fn compact_tool_description(name: &str, desc: &str) -> String {
    let summary = match name {
        "shell" => "执行命令".to_string(),
        "read" | "read_pdf" | "read_docx" | "write" | "edit" | "grep" => desc.to_string(),
        _ => "调用工具".to_string(),
    };
    truncate_to_width(&summary, 72)
}

fn compact_tool_done(name: &str, done: &str) -> String {
    let done = localize_tool_done(done);

    if done.starts_with("命令已被用户拒绝") {
        return "已拒绝".to_string();
    }
    if done.starts_with("命令参数无法解析") {
        return "未执行，参数无法解析".to_string();
    }
    if done.contains("Exit code: 0") {
        return "已完成".to_string();
    }
    if let Some(code) = done
        .split_once("Exit code:")
        .map(|(_, rest)| rest.lines().next().unwrap_or("").trim())
        .filter(|code| !code.is_empty())
    {
        return format!("退出码 {code}");
    }

    match name {
        "write" => compact_file_tool_done(&done, "已写入"),
        "edit" => compact_file_tool_done(&done, "已编辑"),
        "read" | "read_pdf" | "read_docx" => "已读取".to_string(),
        "grep" if done.starts_with("No matches found") => "没有匹配结果".to_string(),
        "grep" => "已完成搜索".to_string(),
        _ => truncate_to_width(&done, 72),
    }
}

fn compact_file_tool_done(done: &str, action: &str) -> String {
    if let Some((_, rest)) = done.split_once(':') {
        let path = rest
            .split_once(" (")
            .map(|(path, _)| path.trim())
            .unwrap_or_else(|| rest.trim());
        if !path.is_empty() {
            return truncate_to_width(&format!("{action} {path}"), 72);
        }
    }
    action.to_string()
}

fn localize_tool_done(done: &str) -> String {
    if done.starts_with("Shell command was denied by the user") {
        "命令已被用户拒绝。".to_string()
    } else if done.starts_with("Shell command was not executed") {
        format!(
            "命令参数无法解析，未执行：{}",
            done.split_once(':')
                .map(|(_, detail)| detail.trim())
                .unwrap_or(done)
        )
    } else {
        done.to_string()
    }
}

fn build_plan_prompt(goal: &str, workspace: &Path, workspace_outline: &str) -> String {
    format!(
        "User task:\n{goal}\n\nWorkspace:\n{}\n\nWorkspace outline:\n{workspace_outline}\n\nReturn only TOML following the schema.",
        workspace.display()
    )
}

fn parse_model_plan(goal: &str, raw: &str) -> Result<WorkflowPlan> {
    let candidate = extract_toml_plan(raw)?;
    let parsed: ModelGeneratedPlan =
        toml::from_str(&candidate).context("模型计划不是合法的 TOML")?;

    let instructions: Vec<String> = parsed
        .steps
        .into_iter()
        .map(|step| step.instruction.trim().to_string())
        .filter(|instruction| !instruction.is_empty())
        .collect();

    if instructions.len() < 2 {
        bail!("模型计划至少需要包含 2 个非空步骤");
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

    bail!("模型回复中没有找到 [[steps]] TOML")
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
        "（工作区为空或无法读取）".to_string()
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
    if rules
        .borrow()
        .iter()
        .find(|rule| command_matches_rule(command, rule))
        .is_some()
    {
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
                let _ = writeln!(writer.borrow_mut(), "警告：保存审批规则失败：{e:#}");
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
        "\n{DIM}╭─{RESET} {YELLOW}! 命令确认{RESET}\n{DIM}│{RESET} {MUTED}意图{RESET}  {summary}\n{DIM}│{RESET} {MUTED}命令{RESET}  {command}\n{DIM}│{RESET} {MUTED}规则{RESET}  {suggested_rule}\n{DIM}╰─{RESET} {MUTED}回车/y 本次允许 · a 总是允许类似命令 · n 拒绝{RESET}"
    );
    let _ = write!(writer.borrow_mut(), "{BOLD_CYAN}选择>{RESET} ");
    let _ = writer.borrow_mut().flush();

    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        let _ = writeln!(writer.borrow_mut(), "读取审批输入失败，已拒绝该命令。");
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
        (ApprovalChoice::AllowOnce, "本次允许", "只运行当前这条命令"),
        (
            ApprovalChoice::AlwaysAllowSimilar,
            "总是允许类似命令",
            "保存此前缀为可信规则",
        ),
        (ApprovalChoice::Deny, "拒绝", "不运行这条命令"),
    ];

    let command_display = truncate_str(command, 96);
    let _ = writeln!(
        writer.borrow_mut(),
        "{DIM}╭─{RESET} {YELLOW}! 命令确认{RESET}"
    );
    let _ = writeln!(
        writer.borrow_mut(),
        "{DIM}│{RESET} {MUTED}意图{RESET}  {summary}"
    );
    let _ = writeln!(
        writer.borrow_mut(),
        "{DIM}│{RESET} {MUTED}命令{RESET}  {command_display}"
    );
    let _ = writeln!(
        writer.borrow_mut(),
        "{DIM}│{RESET} {MUTED}规则{RESET}  {suggested_rule}"
    );
    let _ = writeln!(
        writer.borrow_mut(),
        "{DIM}│{RESET} {MUTED}↑/↓ 切换，回车确认。快捷键：y 本次，a 总是，n 拒绝{RESET}"
    );

    for (choice, label, help) in options {
        let marker = if choice == selected { ">" } else { " " };
        let style = if choice == selected {
            "\x1b[1;36m"
        } else {
            "\x1b[90m"
        };
        let label = pad_to_width(&format!("{marker} {label}"), 22);
        let _ = writeln!(
            writer.borrow_mut(),
            "{DIM}│{RESET} {style}{label}{RESET} {MUTED}{help}{RESET}"
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
        return "执行命令行命令".to_string();
    };

    match first {
        "cargo" => match words.get(1).copied() {
            Some("test") => "运行 Rust 测试验证项目".to_string(),
            Some("check") => "检查 Rust 项目类型和编译错误".to_string(),
            Some("fmt") => "格式化 Rust 源码".to_string(),
            Some("run") => "运行 Rust 应用".to_string(),
            Some("build") => "构建 Rust 项目".to_string(),
            _ => "运行 Cargo 命令".to_string(),
        },
        "git" => match words.get(1).copied() {
            Some("status") => "查看仓库状态".to_string(),
            Some("diff") => "查看未提交的代码改动".to_string(),
            Some("log") => "查看提交历史".to_string(),
            Some("add") => "暂存准备提交的文件".to_string(),
            Some("commit") => "创建 Git 提交".to_string(),
            Some("push") => "推送本地提交到远端仓库".to_string(),
            Some("pull") => "拉取远端改动到本地仓库".to_string(),
            _ => "运行 Git 命令".to_string(),
        },
        "npm" | "pnpm" | "yarn" => match words.get(1).copied() {
            Some("test") => "运行 JavaScript 项目测试".to_string(),
            Some("run") => "运行包脚本".to_string(),
            Some("install") | Some("add") => "安装包依赖".to_string(),
            _ => "运行 JavaScript 包管理命令".to_string(),
        },
        "python" | "python3" => "运行 Python 脚本或内联命令".to_string(),
        "mkdir" => "创建目录".to_string(),
        "dir" | "ls" => "列出目录中的文件".to_string(),
        "type" | "cat" => "打印文件内容".to_string(),
        "del" | "rm" => "删除文件或目录".to_string(),
        "copy" | "cp" => "复制文件或目录".to_string(),
        "move" | "mv" => "移动或重命名文件或目录".to_string(),
        _ => "执行模型请求的命令行命令".to_string(),
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
        .with_context(|| format!("读取审批规则文件失败：{}", path.display()))?;
    let rules: CommandApprovalRules = toml::from_str(&content)
        .with_context(|| format!("解析审批规则文件失败：{}", path.display()))?;
    Ok(rules.rules)
}

fn save_command_approval_rules(
    store: &SessionStore,
    profile: &str,
    rules: &[CommandApprovalRule],
) -> Result<()> {
    fs::create_dir_all(store.state_root())
        .with_context(|| format!("创建状态目录失败：{}", store.state_root().display()))?;
    let path = command_approval_rules_path(store, profile);
    let content = toml::to_string_pretty(&CommandApprovalRules {
        rules: rules.to_vec(),
    })
    .context("序列化审批规则失败")?;
    fs::write(&path, content).with_context(|| format!("写入审批规则文件失败：{}", path.display()))
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
        .with_context(|| format!("读取当前计划文件失败：{}", path.display()))?;
    let plan = toml::from_str(&content)
        .with_context(|| format!("解析当前计划文件失败：{}", path.display()))?;
    Ok(Some(plan))
}

fn save_active_plan(store: &SessionStore, profile: &str, plan: &WorkflowPlan) -> Result<()> {
    fs::create_dir_all(store.state_root())
        .with_context(|| format!("创建状态目录失败：{}", store.state_root().display()))?;
    let path = plan_path(store, profile);
    let content = toml::to_string_pretty(plan).context("序列化当前计划失败")?;
    fs::write(&path, content).with_context(|| format!("写入当前计划文件失败：{}", path.display()))
}

fn clear_active_plan(store: &SessionStore, profile: &str) -> Result<()> {
    let path = plan_path(store, profile);
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("删除当前计划文件失败：{}", path.display()))?;
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
            "没有配置 API Key。请设置 RUST_CODINGAGENT_API_KEY 环境变量，或在配置文件中添加 api_key",
        )?;

    let name = config.provider.name.clone();
    let model = config.provider.model.clone();
    let api_base = config
        .provider
        .api_base
        .clone()
        .unwrap_or_else(|| "https://api.deepseek.com/v1".to_string());

    let provider = RemoteProvider::new(&name, &model, &api_base, &api_key)
        .context("创建远程模型提供方失败")?;

    Ok(Box::new(provider))
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn write_card_top<W: Write>(writer: &mut W, width: usize) -> Result<()> {
    writeln!(
        writer,
        "{BORDER}╭{}╮{RESET}",
        "─".repeat(width.saturating_sub(2))
    )?;
    Ok(())
}

fn write_card_separator<W: Write>(writer: &mut W, width: usize) -> Result<()> {
    writeln!(
        writer,
        "{BORDER}├{}┤{RESET}",
        "─".repeat(width.saturating_sub(2))
    )?;
    Ok(())
}

fn write_card_bottom<W: Write>(writer: &mut W, width: usize) -> Result<()> {
    writeln!(
        writer,
        "{BORDER}╰{}╯{RESET}",
        "─".repeat(width.saturating_sub(2))
    )?;
    Ok(())
}

fn write_card_line<W: Write>(
    writer: &mut W,
    width: usize,
    content: &str,
    style: &str,
) -> Result<()> {
    let inner = width.saturating_sub(4);
    let content = truncate_to_width(content, inner);
    let padding = inner.saturating_sub(display_width(&content));
    writeln!(writer, "│ {style}{content}{RESET}{} │", " ".repeat(padding))?;
    Ok(())
}

fn two_column_line(left: &str, right: &str, width: usize) -> String {
    let right_width = display_width(right);
    if right_width + 4 >= width {
        return truncate_to_width(left, width);
    }

    let left_width = width - right_width - 2;
    let left = truncate_to_width(left, left_width);
    let gap = width
        .saturating_sub(display_width(&left))
        .saturating_sub(right_width);
    format!("{left}{}{right}", " ".repeat(gap))
}

fn write_input_prompt<W: Write>(writer: &mut W) -> Result<()> {
    write!(writer, "\n{DIM}╰─{RESET} {BOLD_CYAN}你>{RESET} ")?;
    Ok(())
}

fn write_answer_token<W: Write>(
    writer: &mut W,
    token: &str,
    at_line_start: &mut bool,
) -> io::Result<()> {
    for ch in token.chars() {
        if *at_line_start && ch != '\n' {
            write!(writer, "{DIM}│{RESET} {BOLD_WHITE}")?;
            *at_line_start = false;
        }

        write!(writer, "{ch}")?;
        if ch == '\n' {
            *at_line_start = true;
        }
    }
    Ok(())
}

fn pad_to_width(input: &str, width: usize) -> String {
    let input = truncate_to_width(input, width);
    let padding = width.saturating_sub(display_width(&input));
    format!("{input}{}", " ".repeat(padding))
}

fn truncate_display(path: &std::path::Path, max: usize) -> String {
    let s = path.display().to_string();
    truncate_left(&s, max)
}

fn truncate_right(s: &str, max: usize) -> String {
    truncate_to_width(s, max)
}

fn truncate_left(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }

    if max <= 3 {
        return ".".repeat(max);
    }

    let keep = max - 3;
    let mut suffix = String::new();
    let mut used = 0;
    for ch in s.chars().rev() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > keep {
            break;
        }
        suffix.insert(0, ch);
        used += width;
    }
    format!("...{suffix}")
}

fn truncate_str(s: &str, max_len: usize) -> String {
    truncate_right(s, max_len)
}

fn terminal_content_width() -> usize {
    terminal::size()
        .map(|(columns, _)| usize::from(columns).clamp(58, 88))
        .unwrap_or(74)
}

fn truncate_to_width(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }

    if max <= 3 {
        return ".".repeat(max);
    }

    let keep = max - 3;
    let mut output = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > keep {
            break;
        }
        output.push(ch);
        used += width;
    }
    output.push_str("...");
    output
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

// ── Help text ───────────────────────────────────────────────────────────────

const HELP_TEXT: &str = r#"可用命令：
  /帮助                     显示这份帮助
  /会话                     查看当前会话和可切换会话
  /会话 切换 <会话ID>       切换到已保存会话
  /会话列表                 列出所有保存的会话
  /历史                     查看最近 50 条消息
  /模型 [模型名]            查看或切换模型
  /清空                     清空消息历史并开启新会话
  /计划 <任务>              让模型生成执行计划
  /计划 状态                查看当前计划
  /计划 下一步              执行下一步
  /计划 执行                执行所有剩余步骤
  /计划 清空                清除当前计划
  /审批                     查看保存的命令审批规则
  /审批 清空                清除保存的命令审批规则
  /回滚 列表                列出所有回滚记录
  /回滚 预览 <记录ID>       预览一次回滚会修改什么
  /回滚 恢复 <记录ID>       应用一次回滚
  /回滚 文件 <记录ID> <路径> 只恢复某个文件
  /工具                     列出可用工具
  退出                      退出交互界面"#;

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

        match MainLoop::new(&config) {
            Ok(mut main_loop) => {
                let status = main_loop
                    .run(Cursor::new("hello\n退出\n"), &mut output)
                    .unwrap();
                let output = String::from_utf8(output).unwrap();
                assert_eq!(status, MainLoopStatus::ExitedByCommand);
                assert!(output.contains("南开 Rust 编程助手"));
                assert!(output.contains("已退出。"));
            }
            Err(_) => {
                // Provider creation may fail without network in tests.
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

        let mut first_output = Vec::new();
        match MainLoop::new(&config) {
            Ok(mut main_loop) => {
                main_loop
                    .run(
                        Cursor::new("/模型 better-model\n/会话\n/历史\n退出\n"),
                        &mut first_output,
                    )
                    .unwrap();
                let first_output = String::from_utf8(first_output).unwrap();
                assert!(first_output.contains("模型已切换"));
            }
            Err(_) => {
                // Provider creation may fail in CI.
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
        assert!(err.to_string().contains("至少需要包含 2 个"));
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
            "运行 Rust 测试验证项目"
        );
        assert_eq!(
            describe_shell_command("git push origin main"),
            "推送本地提交到远端仓库"
        );
        assert_eq!(describe_shell_command("mkdir dist"), "创建目录");
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
        assert_eq!(truncate_right("你好，Rust Coding Agent", 8), "你好...");
        assert_eq!(truncate_str("中文历史消息不会panic", 6), "中...");
    }

    #[test]
    fn exit_command_accepts_common_typos() {
        assert!(is_exit_command("exit"));
        assert!(is_exit_command("exti"));
        assert!(is_exit_command("q"));
        assert!(is_exit_command("退出"));
    }

    #[test]
    fn plain_session_switch_intent_is_handled_without_hijacking_coding_tasks() {
        assert!(is_session_switch_request("我想切换会话"));
        assert!(is_session_switch_request("换会话"));
        assert!(!is_session_switch_request("帮我实现切换会话功能"));
        assert!(!is_session_switch_request("如何开发会话切换代码"));
    }

    fn unique_name(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{nanos}")
    }
}
