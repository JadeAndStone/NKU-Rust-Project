use std::borrow::Cow;
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal::{self, ClearType},
};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::Helper;
use rustyline::{CompletionType, Config, Context as ReadlineContext, Editor};
use tracing::info;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::AppConfig;
use crate::repl::{MainLoop, MainLoopStatus, StartupSessionItem};

const HISTORY_FILE: &str = ".rust-codingagent-history";
const ROOT_COMMANDS: &[&str] = &[
    "/帮助",
    "/会话",
    "/会话列表",
    "/历史",
    "/模型",
    "/清空",
    "/计划",
    "/审批",
    "/回滚",
    "/工具",
    "退出",
];
const PLAN_SUBCOMMANDS: &[&str] = &["状态", "下一步", "执行", "清空", "帮助"];
const ROLLBACK_SUBCOMMANDS: &[&str] = &["列表", "预览", "恢复", "文件"];
const SESSION_SUBCOMMANDS: &[&str] = &["切换"];
const APPROVAL_SUBCOMMANDS: &[&str] = &["清空"];
const MAX_STARTUP_SESSIONS: usize = 8;
const RESET: &str = "\x1b[0m";
const MUTED: &str = "\x1b[90m";
const CYAN: &str = "\x1b[36m";
const BORDER: &str = "\x1b[2;37m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const WHITE: &str = "\x1b[37m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupChoice {
    Existing(usize),
    NewSession,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandSuggestion {
    command: String,
    description: String,
}

pub struct App {
    config: AppConfig,
}

impl App {
    pub fn new(config: AppConfig) -> Self {
        Self { config }
    }

    /// Interactive REPL with rustyline: cursor movement, history, tab-completion.
    pub fn run_stdio(&self) -> Result<()> {
        let config = Config::builder()
            .completion_type(CompletionType::List)
            .build();
        let mut editor = Editor::<PromptHelper, DefaultHistory>::with_config(config)?;
        editor.set_helper(Some(PromptHelper));

        let history_path = dirs_home().join(HISTORY_FILE);
        let _ = editor.load_history(&history_path);

        info!(
            profile = %self.config.profile,
            workspace = %self.config.workspace.display(),
            "starting interactive REPL"
        );

        let mut loop_runner = MainLoop::new(&self.config)?;
        select_startup_session(&mut loop_runner)?;
        loop_runner.print_banner(&mut io::stdout().lock())?;

        loop {
            let prompt = input_prompt();
            let readline = editor.readline(&prompt);
            match readline {
                Ok(line) => {
                    let input = line.trim();
                    if input.is_empty() {
                        continue;
                    }
                    let _ = editor.add_history_entry(input);

                    if is_exit_command(input) {
                        println!("已退出。");
                        break;
                    }

                    match loop_runner.handle_plain_intent(input, &mut io::stdout().lock()) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(e) => {
                            eprintln!("\x1b[31m错误：{e:#}\x1b[0m");
                            continue;
                        }
                    }

                    if input.starts_with('/') {
                        if let Err(e) = loop_runner.handle_command(input, &mut io::stdout().lock())
                        {
                            eprintln!("\x1b[31m错误：{e:#}\x1b[0m");
                        }
                        continue;
                    }

                    if let Err(e) = loop_runner.run_agent_turn(input, &mut io::stdout().lock()) {
                        eprintln!("\x1b[31m助手错误：{e:#}\x1b[0m");
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted) => {
                    println!("^C（输入“退出”结束）");
                }
                Err(rustyline::error::ReadlineError::Eof) => {
                    println!();
                    break;
                }
                Err(err) => {
                    eprintln!("读取输入失败：{err}");
                    break;
                }
            }
        }

        let _ = editor.save_history(&history_path);
        info!("agent REPL exited");
        Ok(())
    }

    /// Generic run (non-interactive, used in tests).
    pub fn run<R, W>(&self, reader: R, mut writer: W) -> Result<()>
    where
        R: BufRead,
        W: Write,
    {
        info!(
            profile = %self.config.profile,
            workspace = %self.config.workspace.display(),
            "starting agent main loop"
        );
        let mut loop_runner = MainLoop::new(&self.config)?;
        loop_runner.print_banner(&mut writer)?;
        let status = loop_runner.run_loop(reader, &mut writer)?;

        match status {
            MainLoopStatus::ExitedByCommand => info!("agent main loop exited by command"),
            MainLoopStatus::ExitedByEof => info!("agent main loop exited by EOF"),
        }
        Ok(())
    }
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn is_exit_command(input: &str) -> bool {
    let normalized = input.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "exit" | "quit" | "q" | "bye" | "exti" | "exi" | "eit"
    ) || matches!(input.trim(), "退出" | "/退出" | "结束" | "再见")
}

fn input_prompt() -> String {
    "\n╰─ 你> ".to_string()
}

fn select_startup_session(loop_runner: &mut MainLoop<'_>) -> Result<()> {
    let force_menu = env::var("RUST_CODINGAGENT_CONTEXT_MENU")
        .map(|value| matches!(value.as_str(), "1" | "true" | "always"))
        .unwrap_or(false);

    if !force_menu && !io::stdin().is_terminal() && !io::stdout().is_terminal() {
        return Ok(());
    }

    let items = loop_runner
        .startup_session_items(MAX_STARTUP_SESSIONS)?
        .into_iter()
        .filter(|item| item.message_count > 0)
        .collect::<Vec<_>>();

    if items.is_empty() {
        return Ok(());
    }

    let default_index = items.iter().position(|item| item.active).unwrap_or(0);
    let choice = prompt_startup_session_choice(&items, default_index);

    match choice {
        StartupChoice::Existing(index) => {
            if let Some(item) = items.get(index) {
                loop_runner.resume_session_by_id(&item.id)?;
            }
        }
        StartupChoice::NewSession => {
            loop_runner.create_fresh_session()?;
        }
    }

    Ok(())
}

fn prompt_startup_session_choice(
    items: &[StartupSessionItem],
    default_index: usize,
) -> StartupChoice {
    if terminal::enable_raw_mode().is_err() {
        return prompt_startup_session_choice_line(items, default_index);
    }

    drain_pending_terminal_events();

    let mut selected = StartupChoice::Existing(default_index);
    let mut rendered_lines = 0u16;

    loop {
        rendered_lines = render_startup_session_menu(items, selected, rendered_lines);

        match event::read() {
            Ok(Event::Key(key)) => match key.code {
                KeyCode::Up => selected = previous_startup_choice(selected, items.len()),
                KeyCode::Down => selected = next_startup_choice(selected, items.len()),
                KeyCode::Enter => {
                    let _ = terminal::disable_raw_mode();
                    clear_startup_session_menu(rendered_lines);
                    return selected;
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    let _ = terminal::disable_raw_mode();
                    clear_startup_session_menu(rendered_lines);
                    return StartupChoice::NewSession;
                }
                KeyCode::Char(ch) if ch.is_ascii_digit() => {
                    if let Some(index) = ch.to_digit(10).and_then(|n| n.checked_sub(1)) {
                        let index = index as usize;
                        if index < items.len() {
                            let _ = terminal::disable_raw_mode();
                            clear_startup_session_menu(rendered_lines);
                            return StartupChoice::Existing(index);
                        }
                    }
                }
                KeyCode::Esc => {
                    let _ = terminal::disable_raw_mode();
                    clear_startup_session_menu(rendered_lines);
                    return StartupChoice::Existing(default_index);
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => {
                let _ = terminal::disable_raw_mode();
                clear_startup_session_menu(rendered_lines);
                return prompt_startup_session_choice_line(items, default_index);
            }
        }
    }
}

fn drain_pending_terminal_events() {
    while event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let _ = event::read();
    }
}

fn prompt_startup_session_choice_line(
    items: &[StartupSessionItem],
    default_index: usize,
) -> StartupChoice {
    let width = card_width();
    let inner = width.saturating_sub(4);

    println!();
    println!("{BORDER}╭{}╮{RESET}", "─".repeat(width.saturating_sub(2)));
    print_card_line(inner, "▌ NKU·RS   南开 Rust 编程助手", BOLD_CYAN);
    print_card_line(inner, "  选择要继续的会话上下文", CYAN);
    print_card_line(inner, "  输入序号确认，n 新建。", MUTED);
    println!("{BORDER}├{}┤{RESET}", "─".repeat(width.saturating_sub(2)));
    for (index, item) in items.iter().enumerate() {
        let left = format!("  {}. {}", index + 1, item.preview);
        let right = format!(
            "消息 {} · 模型 {}{}",
            item.message_count,
            item.model,
            if item.active { " · 当前" } else { "" }
        );
        print_card_line(inner, &two_column_line(&left, &right, inner), WHITE);
    }
    print_card_line(inner, "  n. 新建会话", WHITE);
    println!("{BORDER}╰{}╯{RESET}", "─".repeat(width.saturating_sub(2)));
    print!("选择上下文 [回车={}]：", default_index + 1);
    let _ = io::stdout().flush();

    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return StartupChoice::Existing(default_index);
    }

    let trimmed = line.trim();
    if trimmed.eq_ignore_ascii_case("n") {
        return StartupChoice::NewSession;
    }
    if trimmed.is_empty() {
        return StartupChoice::Existing(default_index);
    }
    if let Ok(number) = trimmed.parse::<usize>() {
        if (1..=items.len()).contains(&number) {
            return StartupChoice::Existing(number - 1);
        }
    }
    StartupChoice::Existing(default_index)
}

fn render_startup_session_menu(
    items: &[StartupSessionItem],
    selected: StartupChoice,
    previous_lines: u16,
) -> u16 {
    if previous_lines > 0 {
        let _ = execute!(
            io::stdout(),
            cursor::MoveUp(previous_lines),
            terminal::Clear(ClearType::FromCursorDown)
        );
    }

    let width = card_width();
    let inner = width.saturating_sub(4);

    println!();
    println!("{BORDER}╭{}╮{RESET}", "─".repeat(width.saturating_sub(2)));
    print_card_line(inner, "▌ NKU·RS   南开 Rust 编程助手", BOLD_CYAN);
    print_card_line(inner, "  选择要继续的会话上下文", CYAN);
    print_card_line(inner, "  ↑/↓ 切换，回车确认，n 新建。", MUTED);
    println!("{BORDER}├{}┤{RESET}", "─".repeat(width.saturating_sub(2)));

    for (index, item) in items.iter().enumerate() {
        let is_selected = selected == StartupChoice::Existing(index);
        let marker = if is_selected { ">" } else { " " };
        let style = if is_selected {
            "\x1b[1;36m"
        } else {
            "\x1b[90m"
        };
        let left = format!("{marker} {}. {}", index + 1, item.preview);
        let right = format!(
            "消息 {} · 模型 {}{}",
            item.message_count,
            item.model,
            if item.active { " · 当前" } else { "" }
        );
        print_card_line(inner, &two_column_line(&left, &right, inner), style);
    }

    let is_new_selected = selected == StartupChoice::NewSession;
    let marker = if is_new_selected { ">" } else { " " };
    let style = if is_new_selected {
        "\x1b[1;36m"
    } else {
        "\x1b[90m"
    };
    print_card_line(inner, &format!("{marker} n. 新建会话"), style);
    println!("{BORDER}╰{}╯{RESET}", "─".repeat(width.saturating_sub(2)));
    let _ = io::stdout().flush();

    items.len() as u16 + 8
}

fn clear_startup_session_menu(lines: u16) {
    if lines == 0 {
        return;
    }
    let _ = execute!(
        io::stdout(),
        cursor::MoveUp(lines),
        terminal::Clear(ClearType::FromCursorDown)
    );
    let _ = io::stdout().flush();
}

fn next_startup_choice(choice: StartupChoice, item_count: usize) -> StartupChoice {
    match choice {
        StartupChoice::Existing(index) if index + 1 < item_count => {
            StartupChoice::Existing(index + 1)
        }
        StartupChoice::Existing(_) => StartupChoice::NewSession,
        StartupChoice::NewSession => StartupChoice::Existing(0),
    }
}

fn previous_startup_choice(choice: StartupChoice, item_count: usize) -> StartupChoice {
    match choice {
        StartupChoice::Existing(0) => StartupChoice::NewSession,
        StartupChoice::Existing(index) => StartupChoice::Existing(index - 1),
        StartupChoice::NewSession => StartupChoice::Existing(item_count.saturating_sub(1)),
    }
}

struct PromptHelper;

impl Helper for PromptHelper {}

impl Completer for PromptHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &ReadlineContext<'_>,
    ) -> std::result::Result<(usize, Vec<Pair>), ReadlineError> {
        let prefix = &line[..pos];
        if !prefix.starts_with('/') {
            return Ok((0, Vec::new()));
        }

        let token_start = prefix
            .rfind(char::is_whitespace)
            .map(|index| index + 1)
            .unwrap_or(0);
        let current = &prefix[token_start..];

        if token_start == 0 {
            return Ok((0, matching_pairs(ROOT_COMMANDS, current)));
        }

        let command = prefix[..token_start]
            .split_whitespace()
            .next()
            .unwrap_or("");
        let options = match command {
            "/plan" | "/计划" => PLAN_SUBCOMMANDS,
            "/rollback" | "/回滚" => ROLLBACK_SUBCOMMANDS,
            "/session" | "/会话" => SESSION_SUBCOMMANDS,
            "/approvals" | "/审批" => APPROVAL_SUBCOMMANDS,
            _ => &[],
        };

        Ok((token_start, matching_pairs(options, current)))
    }
}

impl Hinter for PromptHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &ReadlineContext<'_>) -> Option<String> {
        if pos != line.len() {
            return None;
        }

        command_bar_hint(line)
    }
}

impl Validator for PromptHelper {}

impl Highlighter for PromptHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        if default {
            Cow::Owned(format!("\x1b[1;36m{prompt}\x1b[0m"))
        } else {
            Cow::Borrowed(prompt)
        }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("\x1b[2;37m{hint}\x1b[0m"))
    }
}

fn matching_pairs(options: &[&str], current: &str) -> Vec<Pair> {
    options
        .iter()
        .filter(|option| option.starts_with(current))
        .map(|option| Pair {
            display: option.to_string(),
            replacement: option.to_string(),
        })
        .collect()
}

fn command_bar_hint(line: &str) -> Option<String> {
    if !line.starts_with('/') {
        return None;
    }

    let suggestions = suggestions_for_line(line);
    if suggestions.is_empty() {
        return None;
    }

    let rendered = suggestions
        .into_iter()
        .map(|suggestion| format!("{} - {}", suggestion.command, suggestion.description))
        .collect::<Vec<_>>()
        .join("   ");
    Some(format!("\n  可选：{rendered}"))
}

fn suggestions_for_line(line: &str) -> Vec<CommandSuggestion> {
    let mut parts = line.split_whitespace();
    let command = parts.next().unwrap_or("");

    if !line.contains(char::is_whitespace) {
        return ROOT_COMMANDS
            .iter()
            .filter(|command| command.starts_with(line))
            .map(|command| command_suggestion(command, root_command_description(command)))
            .collect();
    }

    let current = line
        .rsplit_once(char::is_whitespace)
        .map(|(_, current)| current)
        .unwrap_or("");

    match command {
        "/plan" | "/计划" => {
            let mut suggestions: Vec<CommandSuggestion> = PLAN_SUBCOMMANDS
                .iter()
                .filter(|option| option.starts_with(current))
                .map(|option| {
                    command_suggestion(
                        &format!("{command} {option}"),
                        plan_subcommand_description(option),
                    )
                })
                .collect();
            if current.is_empty() || "<任务>".starts_with(current) {
                suggestions.push(command_suggestion(
                    &format!("{command} <任务>"),
                    "让模型生成执行计划",
                ));
            }
            suggestions
        }
        "/rollback" | "/回滚" => ROLLBACK_SUBCOMMANDS
            .iter()
            .filter(|option| option.starts_with(current))
            .map(|option| {
                command_suggestion(
                    &format!("{command} {option}"),
                    rollback_subcommand_description(option),
                )
            })
            .collect(),
        "/session" | "/会话" => SESSION_SUBCOMMANDS
            .iter()
            .filter(|option| option.starts_with(current))
            .map(|option| {
                command_suggestion(
                    &format!("{command} {option}"),
                    session_subcommand_description(option),
                )
            })
            .collect(),
        "/approvals" | "/审批" => APPROVAL_SUBCOMMANDS
            .iter()
            .filter(|option| option.starts_with(current))
            .map(|option| {
                command_suggestion(
                    &format!("{command} {option}"),
                    approval_subcommand_description(option),
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn command_suggestion(command: &str, description: &str) -> CommandSuggestion {
    CommandSuggestion {
        command: command.to_string(),
        description: description.to_string(),
    }
}

fn root_command_description(command: &str) -> &'static str {
    match command {
        "/帮助" | "/help" => "显示所有命令",
        "/会话" | "/session" => "查看或切换会话",
        "/会话列表" | "/sessions" => "列出保存的会话",
        "/历史" | "/history" => "查看最近消息",
        "/模型" | "/model" => "查看或切换模型",
        "/清空" | "/clear" => "开启一个新会话",
        "/计划" | "/plan" => "创建或执行工作计划",
        "/审批" | "/approvals" => "管理命令审批规则",
        "/回滚" | "/rollback" => "预览或恢复文件改动",
        "/工具" | "/tools" => "列出可用工具",
        "退出" | "exit" | "quit" => "退出交互界面",
        _ => "",
    }
}

fn plan_subcommand_description(option: &str) -> &'static str {
    match option {
        "状态" | "status" => "显示当前计划",
        "下一步" | "step" => "只执行下一步",
        "执行" | "run" => "执行所有剩余步骤",
        "清空" | "clear" => "清除当前计划",
        "帮助" | "help" => "显示计划命令用法",
        _ => "",
    }
}

fn rollback_subcommand_description(option: &str) -> &'static str {
    match option {
        "列表" | "list" => "显示回滚记录",
        "预览" | "preview" => "预览一条回滚记录",
        "恢复" | "apply" => "恢复该记录中的所有文件",
        "文件" | "file" => "只恢复该记录中的一个文件",
        _ => "",
    }
}

fn session_subcommand_description(option: &str) -> &'static str {
    match option {
        "切换" | "resume" => "切换到指定会话",
        _ => "",
    }
}

fn approval_subcommand_description(option: &str) -> &'static str {
    match option {
        "清空" | "clear" => "清除保存的审批规则",
        _ => "",
    }
}

fn card_width() -> usize {
    terminal::size()
        .map(|(columns, _)| usize::from(columns).clamp(64, 88))
        .unwrap_or(78)
}

fn print_card_line(inner: usize, content: &str, style: &str) {
    let fitted = fit_text(content, inner);
    let padding = inner.saturating_sub(display_width(&fitted));
    println!("│ {style}{fitted}{RESET}{} │", " ".repeat(padding));
}

fn two_column_line(left: &str, right: &str, width: usize) -> String {
    let right_width = display_width(right);
    if right_width + 4 >= width {
        return fit_text(left, width);
    }

    let left_width = width - right_width - 2;
    let fitted_left = fit_text(left, left_width);
    let gap = width
        .saturating_sub(display_width(&fitted_left))
        .saturating_sub(right_width);
    format!("{fitted_left}{}{right}", " ".repeat(gap))
}

fn fit_text(input: &str, width: usize) -> String {
    if display_width(input) <= width {
        return input.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }

    let keep = width - 3;
    let mut output = String::new();
    let mut used = 0;
    for ch in input.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + char_width > keep {
            break;
        }
        output.push(ch);
        used += char_width;
    }
    output.push_str("...");
    output
}

fn display_width(input: &str) -> usize {
    UnicodeWidthStr::width(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_bar_shows_plan_options_after_space() {
        let hint = command_bar_hint("/计划 ").unwrap();

        assert!(hint.starts_with("\n  可选："));
        assert!(hint.contains("/计划 执行"));
        assert!(hint.contains("执行所有剩余步骤"));
        assert!(hint.contains("/计划 下一步"));
        assert!(hint.contains("/计划 <任务>"));
    }

    #[test]
    fn command_bar_filters_plan_options() {
        let suggestions = suggestions_for_line("/计划 执");

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].command, "/计划 执行");
        assert_eq!(suggestions[0].description, "执行所有剩余步骤");
    }

    #[test]
    fn command_bar_ignores_natural_language() {
        assert!(command_bar_hint("help me write code").is_none());
    }

    #[test]
    fn startup_choices_wrap_through_new_session() {
        assert_eq!(
            next_startup_choice(StartupChoice::Existing(1), 2),
            StartupChoice::NewSession
        );
        assert_eq!(
            next_startup_choice(StartupChoice::NewSession, 2),
            StartupChoice::Existing(0)
        );
        assert_eq!(
            previous_startup_choice(StartupChoice::Existing(0), 2),
            StartupChoice::NewSession
        );
    }
}
