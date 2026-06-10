use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{ProviderConfig, Session, SessionId, SessionSummary};

const STATE_DIR: &str = ".rust-codingagent";

#[derive(Debug, Clone)]
pub struct SessionStore {
    state_root: PathBuf,
    session_dir: PathBuf,
    active_file: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct ActiveSession {
    session_id: SessionId,
}

impl SessionStore {
    pub fn new(workspace: impl AsRef<Path>, profile: impl AsRef<str>) -> Self {
        let state_root = workspace.as_ref().join(STATE_DIR);
        let safe_profile = sanitize_path_segment(profile.as_ref());
        let session_dir = state_root.join("sessions").join(&safe_profile);
        let active_file = state_root.join(format!("active-{safe_profile}.toml"));

        Self {
            state_root,
            session_dir,
            active_file,
        }
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn create_session(
        &self,
        profile: impl Into<String>,
        workspace: impl Into<PathBuf>,
        provider: ProviderConfig,
    ) -> Result<Session> {
        let session = Session::new(profile, workspace, provider);
        self.save_session(&session)?;
        self.set_active_session(&session.id)?;
        Ok(session)
    }

    pub fn get_or_create_active_session(
        &self,
        profile: impl Into<String>,
        workspace: impl Into<PathBuf>,
        provider: ProviderConfig,
    ) -> Result<Session> {
        let profile = profile.into();
        let workspace = workspace.into();

        if let Some(session) = self.load_active_session()? {
            return Ok(session);
        }

        self.create_session(profile, workspace, provider)
    }

    pub fn load_active_session(&self) -> Result<Option<Session>> {
        if !self.active_file.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&self.active_file).with_context(|| {
            format!(
                "failed to read active session file {}",
                self.active_file.display()
            )
        })?;
        let active: ActiveSession = toml::from_str(&content).with_context(|| {
            format!(
                "failed to parse active session file {}",
                self.active_file.display()
            )
        })?;

        match self.load_session(&active.session_id) {
            Ok(session) => Ok(Some(session)),
            Err(_) => Ok(None),
        }
    }

    pub fn set_active_session(&self, session_id: &str) -> Result<()> {
        fs::create_dir_all(&self.state_root).with_context(|| {
            format!(
                "failed to create state directory {}",
                self.state_root.display()
            )
        })?;

        let active = ActiveSession {
            session_id: session_id.to_string(),
        };
        let content =
            toml::to_string_pretty(&active).context("failed to serialize active session")?;
        fs::write(&self.active_file, content).with_context(|| {
            format!(
                "failed to write active session file {}",
                self.active_file.display()
            )
        })
    }

    pub fn save_session(&self, session: &Session) -> Result<()> {
        fs::create_dir_all(&self.session_dir).with_context(|| {
            format!(
                "failed to create session directory {}",
                self.session_dir.display()
            )
        })?;

        let content = toml::to_string_pretty(session)
            .with_context(|| format!("failed to serialize session {}", session.id))?;
        fs::write(self.session_path(&session.id), content)
            .with_context(|| format!("failed to write session {}", session.id))
    }

    pub fn load_session(&self, session_id: &str) -> Result<Session> {
        let path = self.session_path(session_id);
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read session {}", path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("failed to parse session {}", path.display()))
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        if !self.session_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.session_dir).with_context(|| {
            format!(
                "failed to read session directory {}",
                self.session_dir.display()
            )
        })? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
                continue;
            }

            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read session {}", path.display()))?;
            let session: Session = toml::from_str(&content)
                .with_context(|| format!("failed to parse session {}", path.display()))?;
            sessions.push(SessionSummary::from(&session));
        }

        sessions.sort_by(|left, right| right.updated_at_ms.cmp(&left.updated_at_ms));
        Ok(sessions)
    }

    fn session_path(&self, session_id: &str) -> PathBuf {
        self.session_dir.join(format!("{session_id}.toml"))
    }
}

fn sanitize_path_segment(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => ch,
            _ => '_',
        })
        .collect();

    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::Message;

    #[test]
    fn saves_and_restores_active_session() {
        let workspace = std::env::temp_dir().join(unique_name("core-session-store"));
        let store = SessionStore::new(&workspace, "test profile");
        let provider = ProviderConfig::new("local", "stub");

        let mut session = store
            .get_or_create_active_session("test profile", &workspace, provider)
            .unwrap();
        session.add_message(Message::user("hello"));
        store.save_session(&session).unwrap();

        let restored = store.load_active_session().unwrap().unwrap();
        assert_eq!(restored.id, session.id);
        assert_eq!(restored.history.len(), 1);
        assert_eq!(restored.context().turn_index, 1);

        fs::remove_dir_all(&workspace).unwrap();
    }

    #[test]
    fn lists_sessions_newest_first() {
        let workspace = std::env::temp_dir().join(unique_name("core-session-list"));
        let store = SessionStore::new(&workspace, "default");

        let first = store
            .create_session("default", &workspace, ProviderConfig::new("local", "one"))
            .unwrap();
        let mut second = store
            .create_session("default", &workspace, ProviderConfig::new("local", "two"))
            .unwrap();
        second.add_message(Message::assistant("ok"));
        store.save_session(&second).unwrap();

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, second.id);
        assert!(sessions.iter().any(|summary| summary.id == first.id));

        fs::remove_dir_all(&workspace).unwrap();
    }

    fn unique_name(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{nanos}")
    }
}
