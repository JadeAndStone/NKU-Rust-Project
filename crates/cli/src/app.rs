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

use crate::config::AppConfig;
use crate::repl::{MainLoop, MainLoopStatus, StartupSessionItem};

const HISTORY_FILE: &str = ".rust-codingagent-history";
const ROOT_COMMANDS: &[&str] = &[
    "/help",
    "/session",
    "/sessions",
    "/history",
    "/model",
    "/clear",
    "/plan",
    "/approvals",
    "/rollback",
    "/tools",
    "exit",
    "quit",
];
const PLAN_SUBCOMMANDS: &[&str] = &["status", "step", "run", "clear", "help"];
const ROLLBACK_SUBCOMMANDS: &[&str] = &["list", "preview", "apply", "file"];
const SESSION_SUBCOMMANDS: &[&str] = &["resume"];
const APPROVAL_SUBCOMMANDS: &[&str] = &["clear"];
const MAX_STARTUP_SESSIONS: usize = 8;

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
            let readline = editor.readline("你> ");
            match readline {
                Ok(line) => {
                    let input = line.trim();
                    if input.is_empty() {
                        continue;
                    }
                    let _ = editor.add_history_entry(input);

                    if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
                        println!("bye");
                        break;
                    }

                    if input.starts_with('/') {
                        if let Err(e) = loop_runner.handle_command(input, &mut io::stdout().lock())
                        {
                            eprintln!("\x1b[31merror: {e:#}\x1b[0m");
                        }
                        continue;
                    }

                    if let Err(e) = loop_runner.run_agent_turn(input, &mut io::stdout().lock()) {
                        eprintln!("\x1b[31magent error: {e:#}\x1b[0m");
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted) => {
                    println!("^C (type exit to quit)");
                }
                Err(rustyline::error::ReadlineError::Eof) => {
                    println!();
                    break;
                }
                Err(err) => {
                    eprintln!("readline error: {err}");
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
        println!("\x1b[90mNo previous conversations for this workspace/profile; starting new context.\x1b[0m");
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
    println!("\nRecent conversations:");
    for (index, item) in items.iter().enumerate() {
        println!(
            "  {}. {}  messages={}  model={}{}",
            index + 1,
            item.preview,
            item.message_count,
            item.model,
            if item.active { "  (active)" } else { "" }
        );
    }
    println!("  n. Start new conversation");
    print!("Choose context [Enter={}]: ", default_index + 1);
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

    println!("\n\x1b[1;36mChoose conversation context\x1b[0m");
    println!("  Use ↑/↓ then Enter. Press n for a fresh conversation.");
    for (index, item) in items.iter().enumerate() {
        let is_selected = selected == StartupChoice::Existing(index);
        let marker = if is_selected { ">" } else { " " };
        let style = if is_selected {
            "\x1b[1;36m"
        } else {
            "\x1b[90m"
        };
        println!(
            "  {style}{marker} {}. {:<72}\x1b[0m messages={} model={}{}",
            index + 1,
            item.preview,
            item.message_count,
            item.model,
            if item.active { " active" } else { "" }
        );
    }

    let is_new_selected = selected == StartupChoice::NewSession;
    let marker = if is_new_selected { ">" } else { " " };
    let style = if is_new_selected {
        "\x1b[1;36m"
    } else {
        "\x1b[90m"
    };
    println!("  {style}{marker} n. Start new conversation\x1b[0m");
    let _ = io::stdout().flush();

    items.len() as u16 + 4
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
            "/plan" => PLAN_SUBCOMMANDS,
            "/rollback" => ROLLBACK_SUBCOMMANDS,
            "/session" => SESSION_SUBCOMMANDS,
            "/approvals" => APPROVAL_SUBCOMMANDS,
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
    Some(format!("\n  next: {rendered}"))
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
        "/plan" => {
            let mut suggestions: Vec<CommandSuggestion> = PLAN_SUBCOMMANDS
                .iter()
                .filter(|option| option.starts_with(current))
                .map(|option| {
                    command_suggestion(
                        &format!("/plan {option}"),
                        plan_subcommand_description(option),
                    )
                })
                .collect();
            if current.is_empty() || "<task>".starts_with(current) {
                suggestions.push(command_suggestion(
                    "/plan <task>",
                    "ask the model to generate a workflow plan",
                ));
            }
            suggestions
        }
        "/rollback" => ROLLBACK_SUBCOMMANDS
            .iter()
            .filter(|option| option.starts_with(current))
            .map(|option| {
                command_suggestion(
                    &format!("/rollback {option}"),
                    rollback_subcommand_description(option),
                )
            })
            .collect(),
        "/session" => SESSION_SUBCOMMANDS
            .iter()
            .filter(|option| option.starts_with(current))
            .map(|option| {
                command_suggestion(
                    &format!("/session {option}"),
                    session_subcommand_description(option),
                )
            })
            .collect(),
        "/approvals" => APPROVAL_SUBCOMMANDS
            .iter()
            .filter(|option| option.starts_with(current))
            .map(|option| {
                command_suggestion(
                    &format!("/approvals {option}"),
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
        "/help" => "show all commands",
        "/session" => "show or switch conversation context",
        "/sessions" => "list saved conversations",
        "/history" => "show recent messages",
        "/model" => "show or switch model",
        "/clear" => "start a fresh conversation",
        "/plan" => "create or run an automated workflow",
        "/approvals" => "manage saved shell approvals",
        "/rollback" => "preview or restore file changes",
        "/tools" => "list available tools",
        "exit" => "leave the REPL",
        "quit" => "leave the REPL",
        _ => "",
    }
}

fn plan_subcommand_description(option: &str) -> &'static str {
    match option {
        "status" => "show the current workflow plan",
        "step" => "run only the next plan step",
        "run" => "run all remaining plan steps",
        "clear" => "discard the active plan",
        "help" => "show plan command usage",
        _ => "",
    }
}

fn rollback_subcommand_description(option: &str) -> &'static str {
    match option {
        "list" => "show rollback records",
        "preview" => "preview a rollback record",
        "apply" => "restore all files from a record",
        "file" => "restore one file from a record",
        _ => "",
    }
}

fn session_subcommand_description(option: &str) -> &'static str {
    match option {
        "resume" => "switch to a saved session id",
        _ => "",
    }
}

fn approval_subcommand_description(option: &str) -> &'static str {
    match option {
        "clear" => "forget all saved approvals",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_bar_shows_plan_options_after_space() {
        let hint = command_bar_hint("/plan ").unwrap();

        assert!(hint.starts_with("\n  next:"));
        assert!(hint.contains("/plan run"));
        assert!(hint.contains("run all remaining plan steps"));
        assert!(hint.contains("/plan step"));
        assert!(hint.contains("/plan <task>"));
    }

    #[test]
    fn command_bar_filters_plan_options() {
        let suggestions = suggestions_for_line("/plan r");

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].command, "/plan run");
        assert_eq!(suggestions[0].description, "run all remaining plan steps");
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
