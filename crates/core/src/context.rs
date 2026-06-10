use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentContext {
    pub session_id: String,
    pub profile: String,
    pub workspace: PathBuf,
    pub provider: String,
    pub model: String,
    pub turn_index: usize,
}
