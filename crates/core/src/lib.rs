//! Core session, message, state, and provider abstractions.
//!
//! The core crate owns stateful concepts shared by the CLI, tools, and
//! rollback layers. It intentionally avoids terminal IO and command parsing.

mod context;
mod message;
mod provider;
mod session;
mod store;
mod time;

pub use context::AgentContext;
pub use message::{ConversationHistory, Message, MessageRole};
pub use provider::{LanguageProvider, ProviderConfig, ProviderRequest, ProviderResponse};
pub use session::{Session, SessionId, SessionSummary};
pub use store::SessionStore;
