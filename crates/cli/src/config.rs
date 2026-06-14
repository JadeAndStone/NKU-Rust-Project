use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_CONFIG_FILE: &str = "rust-codingagent.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub profile: String,
    pub workspace: PathBuf,
    pub log_level: String,
    pub provider: ProviderConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub model: String,
    pub api_base: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialAppConfig {
    profile: Option<String>,
    workspace: Option<PathBuf>,
    log_level: Option<String>,
    provider: Option<PartialProviderConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialProviderConfig {
    name: Option<String>,
    model: Option<String>,
    api_base: Option<String>,
    api_key: Option<String>,
}

impl AppConfig {
    pub fn load(config_path: Option<&Path>) -> Result<Self> {
        let mut config = Self::default();

        if let Some(file_config) = load_file_config(config_path)? {
            config.merge_file(file_config);
        }

        config.merge_env();
        Ok(config)
    }

    pub fn to_pretty_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("failed to serialize effective configuration")
    }

    fn merge_file(&mut self, partial: PartialAppConfig) {
        if let Some(profile) = partial.profile {
            self.profile = profile;
        }
        if let Some(workspace) = partial.workspace {
            self.workspace = workspace;
        }
        if let Some(log_level) = partial.log_level {
            self.log_level = log_level;
        }
        if let Some(provider) = partial.provider {
            if let Some(name) = provider.name {
                self.provider.name = name;
            }
            if let Some(model) = provider.model {
                self.provider.model = model;
            }
            if provider.api_base.is_some() {
                self.provider.api_base = provider.api_base;
            }
            if provider.api_key.is_some() {
                self.provider.api_key = provider.api_key;
            }
        }
    }

    fn merge_env(&mut self) {
        if let Ok(profile) = env::var("RUST_CODINGAGENT_PROFILE") {
            self.profile = profile;
        }
        if let Ok(workspace) = env::var("RUST_CODINGAGENT_WORKSPACE") {
            self.workspace = PathBuf::from(workspace);
        }
        if let Ok(log_level) = env::var("RUST_CODINGAGENT_LOG_LEVEL") {
            self.log_level = log_level;
        }
        if let Ok(provider) = env::var("RUST_CODINGAGENT_PROVIDER") {
            self.provider.name = provider;
        }
        if let Ok(model) = env::var("RUST_CODINGAGENT_MODEL") {
            self.provider.model = model;
        }
        if let Ok(api_base) = env::var("RUST_CODINGAGENT_API_BASE") {
            self.provider.api_base = Some(api_base);
        }
        if let Ok(api_key) = env::var("RUST_CODINGAGENT_API_KEY") {
            self.provider.api_key = Some(api_key);
        }
    }
}

fn default_workspace() -> PathBuf {
    // Use ~/workspace if it exists (user project directory),
    // otherwise fall back to current directory.
    if let Ok(home) = env::var("HOME") {
        let ws = PathBuf::from(&home).join("workspace");
        if ws.exists() {
            return ws;
        }
    }
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            profile: "default".to_string(),
            workspace: default_workspace(),
            log_level: "warn".to_string(),
            provider: ProviderConfig {
                name: "local".to_string(),
                model: "stub".to_string(),
                api_base: None,
                api_key: None,
            },
        }
    }
}

fn load_file_config(config_path: Option<&Path>) -> Result<Option<PartialAppConfig>> {
    match config_path {
        Some(path) => read_config_file(path).map(Some),
        None => {
            let default_path = Path::new(DEFAULT_CONFIG_FILE);
            if default_path.exists() {
                read_config_file(default_path).map(Some)
            } else {
                Ok(None)
            }
        }
    }
}

fn read_config_file(path: &Path) -> Result<PartialAppConfig> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&content)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn loads_partial_toml_config() {
        // Save and clear env vars that might interfere
        let saved_provider = env::var("RUST_CODINGAGENT_PROVIDER").ok();
        let saved_model = env::var("RUST_CODINGAGENT_MODEL").ok();
        let saved_api_key = env::var("RUST_CODINGAGENT_API_KEY").ok();
        env::remove_var("RUST_CODINGAGENT_PROVIDER");
        env::remove_var("RUST_CODINGAGENT_MODEL");
        env::remove_var("RUST_CODINGAGENT_API_KEY");

        let dir = env::temp_dir().join(unique_name("rust-codingagent-config-test"));
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("agent.toml");
        fs::write(
            &config_path,
            r#"
profile = "demo"
workspace = "/tmp/rust-codingagent-demo"
log_level = "debug"

[provider]
name = "openai-compatible"
model = "demo-model"
"#,
        )
        .unwrap();

        let config = AppConfig::load(Some(&config_path)).unwrap();

        assert_eq!(config.profile, "demo");
        assert_eq!(
            config.workspace,
            PathBuf::from("/tmp/rust-codingagent-demo")
        );
        assert_eq!(config.log_level, "debug");
        assert_eq!(config.provider.name, "openai-compatible");
        assert_eq!(config.provider.model, "demo-model");

        fs::remove_dir_all(&dir).unwrap();

        // Restore env vars
        if let Some(v) = saved_provider {
            env::set_var("RUST_CODINGAGENT_PROVIDER", v);
        }
        if let Some(v) = saved_model {
            env::set_var("RUST_CODINGAGENT_MODEL", v);
        }
        if let Some(v) = saved_api_key {
            env::set_var("RUST_CODINGAGENT_API_KEY", v);
        }
    }

    fn unique_name(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{nanos}")
    }
}
