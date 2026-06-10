use std::io::{BufRead, Write};

use anyhow::Result;
use rust_codingagent_core::{Message, ProviderConfig as CoreProviderConfig, Session, SessionStore};

use crate::config::AppConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainLoopStatus {
    ExitedByCommand,
    ExitedByEof,
}

pub struct MainLoop<'a> {
    config: &'a AppConfig,
    store: SessionStore,
    session: Session,
}

impl<'a> MainLoop<'a> {
    pub fn new(config: &'a AppConfig) -> Result<Self> {
        let store = SessionStore::new(&config.workspace, &config.profile);
        let provider = CoreProviderConfig {
            name: config.provider.name.clone(),
            model: config.provider.model.clone(),
            api_base: config.provider.api_base.clone(),
        };
        let session = store.get_or_create_active_session(
            config.profile.clone(),
            config.workspace.clone(),
            provider.clone(),
        )?;

        Ok(Self {
            config,
            store,
            session,
        })
    }

    pub fn run<R, W>(&mut self, mut reader: R, mut writer: W) -> Result<MainLoopStatus>
    where
        R: BufRead,
        W: Write,
    {
        writeln!(
            writer,
            "rust-codingagent started profile={} workspace={}",
            self.config.profile,
            self.config.workspace.display()
        )?;
        writeln!(
            writer,
            "session={} messages={} model={}/{}",
            self.session.id,
            self.session.history.len(),
            self.session.provider.name,
            self.session.provider.model
        )?;
        writeln!(writer, "type 'exit' or 'quit' to leave")?;

        loop {
            write!(writer, "rust-codingagent> ")?;
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

            if self.handle_command(input, &mut writer)? {
                continue;
            }

            self.session.add_message(Message::user(input));
            let response = format!("received: {input}");
            self.session.add_message(Message::assistant(&response));
            self.store.save_session(&self.session)?;

            writeln!(writer, "{response}")?;
        }
    }

    fn handle_command<W>(&mut self, input: &str, writer: &mut W) -> Result<bool>
    where
        W: Write,
    {
        let command = input.strip_prefix('/').unwrap_or(input);
        let mut parts = command.split_whitespace();
        let Some(name) = parts.next() else {
            return Ok(false);
        };

        match name {
            "session" => {
                let context = self.session.context();
                writeln!(
                    writer,
                    "session={} profile={} messages={} model={}/{} workspace={}",
                    context.session_id,
                    context.profile,
                    context.turn_index,
                    context.provider,
                    context.model,
                    context.workspace.display()
                )?;
                Ok(true)
            }
            "history" => {
                if self.session.history.is_empty() {
                    writeln!(writer, "history is empty")?;
                    return Ok(true);
                }

                for (index, message) in self.session.history.messages().iter().enumerate() {
                    writeln!(
                        writer,
                        "{} {:?}: {}",
                        index + 1,
                        message.role,
                        message.content
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
            _ => Ok(false),
        }
    }
}

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
            },
        };
        let mut output = Vec::new();
        let mut main_loop = MainLoop::new(&config).unwrap();

        let status = main_loop
            .run(Cursor::new("hello\nexit\n"), &mut output)
            .unwrap();
        let output = String::from_utf8(output).unwrap();

        assert_eq!(status, MainLoopStatus::ExitedByCommand);
        assert!(output.contains("rust-codingagent started profile=test"));
        assert!(output.contains("received: hello"));
        assert!(output.contains("bye"));

        std::fs::remove_dir_all(&workspace).unwrap();
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
            },
        };

        let mut first_output = Vec::new();
        MainLoop::new(&config)
            .unwrap()
            .run(
                Cursor::new("/model better-model\nhello\nexit\n"),
                &mut first_output,
            )
            .unwrap();

        let mut second_output = Vec::new();
        MainLoop::new(&config)
            .unwrap()
            .run(
                Cursor::new("/session\n/history\nexit\n"),
                &mut second_output,
            )
            .unwrap();
        let second_output = String::from_utf8(second_output).unwrap();

        assert!(second_output.contains("messages=2"));
        assert!(second_output.contains("User: hello"));
        assert!(second_output.contains("Assistant: received: hello"));

        std::fs::remove_dir_all(&workspace).unwrap();
    }

    fn unique_name(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{nanos}")
    }
}
