use std::io::{BufRead, Write};

use anyhow::Result;

use crate::config::AppConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainLoopStatus {
    ExitedByCommand,
    ExitedByEof,
}

pub struct MainLoop<'a> {
    config: &'a AppConfig,
}

impl<'a> MainLoop<'a> {
    pub fn new(config: &'a AppConfig) -> Self {
        Self { config }
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

            writeln!(writer, "received: {input}")?;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::config::{AppConfig, ProviderConfig};

    #[test]
    fn enters_loop_and_exits_by_command() {
        let config = AppConfig {
            profile: "test".to_string(),
            workspace: "/tmp/rust-codingagent-test".into(),
            log_level: "off".to_string(),
            provider: ProviderConfig {
                name: "local".to_string(),
                model: "stub".to_string(),
                api_base: None,
            },
        };
        let mut output = Vec::new();
        let mut main_loop = MainLoop::new(&config);

        let status = main_loop
            .run(Cursor::new("hello\nexit\n"), &mut output)
            .unwrap();
        let output = String::from_utf8(output).unwrap();

        assert_eq!(status, MainLoopStatus::ExitedByCommand);
        assert!(output.contains("rust-codingagent started profile=test"));
        assert!(output.contains("received: hello"));
        assert!(output.contains("bye"));
    }
}
