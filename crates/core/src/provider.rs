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

/// Description of a tool/function available to the LLM.
/// Follows the OpenAI function-calling schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    pub context: AgentContext,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

/// The response from a LanguageProvider, either a text reply or tool call requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderResponse {
    Text {
        content: String,
    },
    ToolCalls {
        calls: Vec<crate::message::ToolCall>,
    },
}

pub trait LanguageProvider {
    fn name(&self) -> &str;

    fn model(&self) -> &str;

    /// Non-streaming completion.
    fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse>;

    /// Streaming completion: calls `on_token` for each text delta.
    /// Returns the final response (text or tool calls).
    /// Default implementation falls back to non-streaming `complete`.
    fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<ProviderResponse> {
        let response = self.complete(request)?;
        if let ProviderResponse::Text { ref content } = response {
            on_token(content);
        }
        Ok(response)
    }
}
