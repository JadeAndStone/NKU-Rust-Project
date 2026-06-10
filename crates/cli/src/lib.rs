pub mod app;
pub mod cli;
pub mod config;
pub mod repl;
pub mod telemetry;

use anyhow::Result;
use clap::Parser;

use crate::app::App;
use crate::cli::{Cli, Commands};
use crate::config::AppConfig;

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = AppConfig::load(cli.config.as_deref())?;
    telemetry::init(&config.log_level)?;

    match cli.command.unwrap_or(Commands::Run) {
        Commands::Run => App::new(config).run_stdio(),
        Commands::Config => {
            println!("{}", config.to_pretty_toml()?);
            Ok(())
        }
    }
}
