use std::io::{self, BufRead, Write};

use anyhow::Result;
use tracing::info;

use crate::config::AppConfig;
use crate::repl::{MainLoop, MainLoopStatus};

pub struct App {
    config: AppConfig,
}

impl App {
    pub fn new(config: AppConfig) -> Self {
        Self { config }
    }

    pub fn run_stdio(&self) -> Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        self.run(stdin.lock(), stdout.lock())
    }

    pub fn run<R, W>(&self, reader: R, writer: W) -> Result<()>
    where
        R: BufRead,
        W: Write,
    {
        info!(
            profile = %self.config.profile,
            workspace = %self.config.workspace.display(),
            "starting agent main loop"
        );
        let mut loop_runner = MainLoop::new(&self.config);
        let status = loop_runner.run(reader, writer)?;

        match status {
            MainLoopStatus::ExitedByCommand => info!("agent main loop exited by command"),
            MainLoopStatus::ExitedByEof => info!("agent main loop exited by EOF"),
        }

        Ok(())
    }
}
