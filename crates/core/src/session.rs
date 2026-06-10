use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::time::unix_millis;
use crate::{AgentContext, ConversationHistory, Message, ProviderConfig};

pub type SessionId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub profile: String,
    pub workspace: PathBuf,
    pub provider: ProviderConfig,
    pub history: ConversationHistory,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl Session {
    pub fn new(
        profile: impl Into<String>,
        workspace: impl Into<PathBuf>,
        provider: ProviderConfig,
    ) -> Self {
        let now = unix_millis();
        let pid = std::process::id();
        Self {
            id: format!("s-{now}-{pid}"),
            profile: profile.into(),
            workspace: workspace.into(),
            provider,
            history: ConversationHistory::new(),
            created_at_ms: now,
            updated_at_ms: now,
        }
    }

    pub fn add_message(&mut self, message: Message) {
        self.history.push(message);
        self.touch();
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        self.provider.model = model.into();
        self.touch();
    }

    pub fn set_provider(&mut self, provider: ProviderConfig) {
        self.provider = provider;
        self.touch();
    }

    pub fn context(&self) -> AgentContext {
        AgentContext {
            session_id: self.id.clone(),
            profile: self.profile.clone(),
            workspace: self.workspace.clone(),
            provider: self.provider.name.clone(),
            model: self.provider.model.clone(),
            turn_index: self.history.len(),
        }
    }

    fn touch(&mut self) {
        self.updated_at_ms = unix_millis();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: SessionId,
    pub profile: String,
    pub workspace: PathBuf,
    pub provider: String,
    pub model: String,
    pub message_count: usize,
    pub updated_at_ms: u64,
}

impl From<&Session> for SessionSummary {
    fn from(session: &Session) -> Self {
        Self {
            id: session.id.clone(),
            profile: session.profile.clone(),
            workspace: session.workspace.clone(),
            provider: session.provider.name.clone(),
            model: session.provider.model.clone(),
            message_count: session.history.len(),
            updated_at_ms: session.updated_at_ms,
        }
    }
}
