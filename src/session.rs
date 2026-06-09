use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ChannelBinding {
    pub platform: String,
    pub channel_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SubSessionKind {
    Agent,
    Acp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubSession {
    pub id: String,
    pub parent_session_id: String,
    pub kind: SubSessionKind,
    pub target: String,
    pub title: Option<String>,
    pub status: String,
    #[serde(default)]
    pub channel_binding: Option<ChannelBinding>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub channel_binding: ChannelBinding,
    pub root_agent_id: String,
    pub sub_sessions: Vec<SubSession>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionStore {
    sessions: HashMap<String, Session>,
    channel_index: HashMap<String, String>,
}

pub struct SessionRuntime {
    path: PathBuf,
    store: SessionStore,
}

impl SessionRuntime {
    pub fn load(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let path = data_dir.into().join("sessions.json");
        let store = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).context("parsing session store")?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => SessionStore::default(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, store })
    }

    pub fn resolve_channel(
        &mut self,
        platform: &str,
        channel_id: &str,
        root_agent_id: &str,
    ) -> Result<String> {
        let key = channel_key(platform, channel_id);
        if let Some(session_id) = self.store.channel_index.get(&key) {
            if let Some(session) = self.store.sessions.get_mut(session_id) {
                session.updated_at = Utc::now().to_rfc3339();
            }
            self.save()?;
            return Ok(session_id.clone());
        }

        let now = Utc::now().to_rfc3339();
        let session_id = Uuid::new_v4().to_string();
        let session = Session {
            id: session_id.clone(),
            channel_binding: ChannelBinding {
                platform: platform.to_string(),
                channel_id: channel_id.to_string(),
            },
            root_agent_id: root_agent_id.to_string(),
            sub_sessions: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.channel_index.insert(key, session_id.clone());
        self.store.sessions.insert(session_id.clone(), session);
        self.save()?;
        Ok(session_id)
    }

    pub fn channel_session_id(&self, platform: &str, channel_id: &str) -> Option<String> {
        self.store
            .channel_index
            .get(&channel_key(platform, channel_id))
            .cloned()
    }

    #[allow(dead_code)]
    pub fn list(&self) -> Vec<Session> {
        self.store.sessions.values().cloned().collect()
    }

    pub fn get(&self, session_id: &str) -> Option<Session> {
        self.store.sessions.get(session_id).cloned()
    }

    pub fn upsert_sub_session(
        &mut self,
        parent_session_id: &str,
        sub_session_id: &str,
        kind: SubSessionKind,
        target: &str,
        title: Option<String>,
        status: &str,
    ) -> Result<()> {
        let Some(session) = self.store.sessions.get_mut(parent_session_id) else {
            return Ok(());
        };
        let now = Utc::now().to_rfc3339();
        session.updated_at = now.clone();
        match session
            .sub_sessions
            .iter_mut()
            .find(|sub| sub.id == sub_session_id)
        {
            Some(sub) => {
                sub.kind = kind;
                sub.target = target.to_string();
                if title.is_some() {
                    sub.title = title;
                }
                sub.status = status.to_string();
                sub.updated_at = now;
            }
            None => {
                session.sub_sessions.push(SubSession {
                    id: sub_session_id.to_string(),
                    parent_session_id: parent_session_id.to_string(),
                    kind,
                    target: target.to_string(),
                    title,
                    status: status.to_string(),
                    channel_binding: None,
                    created_at: now.clone(),
                    updated_at: now,
                });
            }
        }
        self.save()
    }

    pub fn bind_sub_session_channel(
        &mut self,
        parent_session_id: &str,
        sub_session_id: &str,
        binding: ChannelBinding,
    ) -> Result<()> {
        let Some(session) = self.store.sessions.get_mut(parent_session_id) else {
            return Ok(());
        };
        let Some(sub_session) = session
            .sub_sessions
            .iter_mut()
            .find(|sub| sub.id == sub_session_id)
        else {
            return Ok(());
        };
        let now = Utc::now().to_rfc3339();
        sub_session.channel_binding = Some(binding);
        sub_session.updated_at = now.clone();
        session.updated_at = now;
        self.save()
    }

    pub fn sub_session_channel_binding(
        &self,
        parent_session_id: &str,
        sub_session_id: &str,
    ) -> Option<ChannelBinding> {
        self.store
            .sessions
            .get(parent_session_id)?
            .sub_sessions
            .iter()
            .find(|sub| sub.id == sub_session_id)?
            .channel_binding
            .clone()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(&self.store).context("serializing session store")?;
        std::fs::write(&self.path, raw).with_context(|| format!("writing {}", self.path.display()))
    }
}

fn channel_key(platform: &str, channel_id: &str) -> String {
    format!("{}:{}", platform.trim(), channel_id.trim())
}

#[cfg(test)]
mod tests {
    use super::{SessionRuntime, SubSessionKind};

    #[test]
    fn same_channel_reuses_session() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = SessionRuntime::load(&dir).unwrap();
        let first = runtime
            .resolve_channel("cli", "local-dev", "default")
            .unwrap();
        let second = runtime
            .resolve_channel("cli", "local-dev", "default")
            .unwrap();
        assert_eq!(first, second);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn different_channels_create_different_sessions() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = SessionRuntime::load(&dir).unwrap();
        let first = runtime.resolve_channel("cli", "a", "default").unwrap();
        let second = runtime.resolve_channel("cli", "b", "default").unwrap();
        assert_ne!(first, second);
        assert_eq!(runtime.list().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn channel_session_id_checks_without_creating_session() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = SessionRuntime::load(&dir).unwrap();
        assert_eq!(runtime.channel_session_id("cli", "local-dev"), None);
        assert!(runtime.list().is_empty());
        let created = runtime
            .resolve_channel("cli", "local-dev", "default")
            .unwrap();
        assert_eq!(
            runtime.channel_session_id("cli", "local-dev").as_deref(),
            Some(created.as_str())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn records_sub_session_under_parent() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = SessionRuntime::load(&dir).unwrap();
        let parent = runtime
            .resolve_channel("cli", "local-dev", "default")
            .unwrap();
        runtime
            .upsert_sub_session(
                &parent,
                "sub-1",
                SubSessionKind::Agent,
                "coder",
                Some("fix bug".to_string()),
                "running",
            )
            .unwrap();
        runtime
            .upsert_sub_session(
                &parent,
                "sub-1",
                SubSessionKind::Agent,
                "coder",
                None,
                "done",
            )
            .unwrap();
        let session = runtime.get(&parent).unwrap();
        assert_eq!(session.sub_sessions.len(), 1);
        assert_eq!(session.sub_sessions[0].status, "done");
        assert_eq!(session.sub_sessions[0].title.as_deref(), Some("fix bug"));
        assert_eq!(session.sub_sessions[0].channel_binding, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn records_acp_and_agent_sub_sessions_under_same_parent_model() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = SessionRuntime::load(&dir).unwrap();
        let parent = runtime.resolve_channel("cli", "mixed", "default").unwrap();
        runtime
            .upsert_sub_session(
                &parent,
                "acp-sub",
                SubSessionKind::Acp,
                "acp",
                Some("acp task".to_string()),
                "done",
            )
            .unwrap();
        runtime
            .upsert_sub_session(
                &parent,
                "agent-sub",
                SubSessionKind::Agent,
                "coder",
                Some("coding task".to_string()),
                "running",
            )
            .unwrap();

        let session = runtime.get(&parent).unwrap();
        assert_eq!(session.sub_sessions.len(), 2);
        assert!(session
            .sub_sessions
            .iter()
            .any(|sub| sub.id == "acp-sub" && sub.kind == SubSessionKind::Acp));
        assert!(session
            .sub_sessions
            .iter()
            .any(|sub| sub.id == "agent-sub" && sub.kind == SubSessionKind::Agent));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn binds_im_channel_to_agent_sub_session() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = SessionRuntime::load(&dir).unwrap();
        let parent = runtime
            .resolve_channel("feishu", "root-chat", "default")
            .unwrap();
        runtime
            .upsert_sub_session(
                &parent,
                "agent-sub",
                SubSessionKind::Agent,
                "coder",
                Some("coding task".to_string()),
                "running",
            )
            .unwrap();
        runtime
            .bind_sub_session_channel(
                &parent,
                "agent-sub",
                super::ChannelBinding {
                    platform: "feishu".to_string(),
                    channel_id: "sub-chat".to_string(),
                },
            )
            .unwrap();

        let binding = runtime
            .sub_session_channel_binding(&parent, "agent-sub")
            .unwrap();
        assert_eq!(binding.platform, "feishu");
        assert_eq!(binding.channel_id, "sub-chat");
        let _ = std::fs::remove_dir_all(dir);
    }
}
