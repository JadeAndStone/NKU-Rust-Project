use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

pub fn init(log_level: &str) -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(log_level))
        .with_context(|| format!("invalid log level '{log_level}'"))?;

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init()
        .ok();

    Ok(())
}
