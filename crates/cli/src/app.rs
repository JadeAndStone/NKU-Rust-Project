use std::borrow::Cow;
use std::io::{self, BufRead, Write};

use anyhow::Result;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::Helper;
use rustyline::{CompletionType, Config, Editor};
use tracing::info;

use crate::config::AppConfig;
use crate::repl::{MainLoop, MainLoopStatus};

const HISTORY_FILE: &str = ".rust-codingagent-history";

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
            .completion_type(CompletionType::Circular)
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

struct PromptHelper;

impl Helper for PromptHelper {}

impl Completer for PromptHelper {
    type Candidate = Pair;
}

impl Hinter for PromptHelper {
    type Hint = String;
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
}
