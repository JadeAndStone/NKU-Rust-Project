use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{AgentContext, Message};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub model: String,
    pub api_base: Option<String>,
}

impl ProviderConfig {
    pub fn new(name: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            model: model.into(),
            api_base: None,
        }
    }

    pub fn with_api_base(mut self, api_base: Option<String>) -> Self {
        self.api_base = api_base;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    pub context: AgentContext,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResponse {
    pub message: Message,
}

pub trait LanguageProvider {
    fn name(&self) -> &str;

    fn model(&self) -> &str;

    fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse>;
}
