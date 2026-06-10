use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "rust-codingagent",
    version,
    about = "Rust Coding Agent CLI framework"
)]
pub struct Cli {
    /// Optional TOML configuration file.
    #[arg(short, long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Commands {
    /// Start the agent main loop.
    Run,
    /// Print the effective configuration after file and environment merging.
    Config,
}
