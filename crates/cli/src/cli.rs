use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "nku-agent",
    version,
    about = "南开 Rust 编程助手命令行",
    disable_help_flag = true,
    disable_help_subcommand = true,
    disable_version_flag = true,
    override_usage = "nku-agent [选项] [命令]",
    help_template = "{about}\n\n用法：{usage}\n\n命令：\n{subcommands}\n选项：\n{options}"
)]
pub struct Cli {
    /// 可选的 TOML 配置文件。
    #[arg(short, long, global = true, value_name = "文件")]
    pub config: Option<PathBuf>,

    /// 显示帮助。
    #[arg(short = 'h', long = "help", action = ArgAction::Help)]
    pub help: Option<bool>,

    /// 显示版本。
    #[arg(short = 'V', long = "version", action = ArgAction::Version)]
    pub version: Option<bool>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Commands {
    /// 启动助手主循环。
    #[command(name = "运行", alias = "run")]
    Run,
    /// 打印合并配置文件和环境变量后的有效配置。
    #[command(name = "配置", alias = "config")]
    Config,
}
