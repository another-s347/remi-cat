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
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
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
            title: None,
            metadata: serde_json::Map::new(),
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

    pub fn metadata_bool(&self, session_id: &str, key: &str) -> bool {
        self.store
            .sessions
            .get(session_id)
            .and_then(|session| session.metadata.get(key))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }

    pub fn metadata_string(&self, session_id: &str, key: &str) -> Option<String> {
        self.store
            .sessions
            .get(session_id)
            .and_then(|session| session.metadata.get(key))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    }

    pub fn metadata_value(&self, session_id: &str, key: &str) -> Option<serde_json::Value> {
        self.store
            .sessions
            .get(session_id)
            .and_then(|session| session.metadata.get(key))
            .cloned()
    }

    pub fn set_metadata_bool(&mut self, session_id: &str, key: &str, value: bool) -> Result<bool> {
        let Some(session) = self.store.sessions.get_mut(session_id) else {
            return Ok(false);
        };
        session
            .metadata
            .insert(key.to_string(), serde_json::Value::Bool(value));
        session.updated_at = Utc::now().to_rfc3339();
        self.save()?;
        Ok(true)
    }

    pub fn set_metadata_string(
        &mut self,
        session_id: &str,
        key: &str,
        value: &str,
    ) -> Result<bool> {
        let Some(session) = self.store.sessions.get_mut(session_id) else {
            return Ok(false);
        };
        session.metadata.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
        session.updated_at = Utc::now().to_rfc3339();
        self.save()?;
        Ok(true)
    }

    pub fn set_metadata_value(
        &mut self,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<bool> {
        let Some(session) = self.store.sessions.get_mut(session_id) else {
            return Ok(false);
        };
        session.metadata.insert(key.to_string(), value);
        session.updated_at = Utc::now().to_rfc3339();
        self.save()?;
        Ok(true)
    }

    pub fn remove_metadata(&mut self, session_id: &str, key: &str) -> Result<bool> {
        let Some(session) = self.store.sessions.get_mut(session_id) else {
            return Ok(false);
        };
        let removed = session.metadata.remove(key).is_some();
        if removed {
            session.updated_at = Utc::now().to_rfc3339();
            self.save()?;
        }
        Ok(removed)
    }

    pub fn create_channel(
        &mut self,
        platform: &str,
        channel_id: &str,
        root_agent_id: &str,
        title: Option<String>,
    ) -> Result<Session> {
        let session_id = self.resolve_channel(platform, channel_id, root_agent_id)?;
        if let Some(title) = normalize_title(title.as_deref()) {
            if let Some(session) = self.store.sessions.get_mut(&session_id) {
                session.title = Some(title);
                session.updated_at = Utc::now().to_rfc3339();
            }
            self.save()?;
        }
        self.get(&session_id)
            .context("newly-created session missing from store")
    }

    pub fn fork_session(
        &mut self,
        source_session_id: &str,
        platform: &str,
        channel_id: &str,
        title: Option<String>,
    ) -> Result<Option<Session>> {
        let Some(source) = self.store.sessions.get(source_session_id).cloned() else {
            return Ok(None);
        };
        let key = channel_key(platform, channel_id);
        if let Some(existing_id) = self.store.channel_index.get(&key) {
            return self
                .get(existing_id)
                .map(Some)
                .context("indexed session missing");
        }
        let now = Utc::now().to_rfc3339();
        let session_id = Uuid::new_v4().to_string();
        let mut metadata = source.metadata.clone();
        metadata.insert(
            "forked_from_session_id".to_string(),
            serde_json::Value::String(source.id.clone()),
        );
        metadata.insert(
            "forked_at".to_string(),
            serde_json::Value::String(now.clone()),
        );
        let fork_title = normalize_title(title.as_deref()).or_else(|| {
            let base = source.title.as_deref().unwrap_or("新对话");
            normalize_title(Some(&format!("{base} (fork)")))
        });
        let session = Session {
            id: session_id.clone(),
            channel_binding: ChannelBinding {
                platform: platform.to_string(),
                channel_id: channel_id.to_string(),
            },
            root_agent_id: source.root_agent_id,
            title: fork_title,
            metadata,
            sub_sessions: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.channel_index.insert(key, session_id.clone());
        self.store
            .sessions
            .insert(session_id.clone(), session.clone());
        self.save()?;
        Ok(Some(session))
    }

    pub fn set_channel_binding(
        &mut self,
        session_id: &str,
        platform: &str,
        channel_id: &str,
    ) -> Result<Option<Session>> {
        let Some(session) = self.store.sessions.get_mut(session_id) else {
            return Ok(None);
        };
        self.store
            .channel_index
            .retain(|_, indexed_session_id| indexed_session_id != session_id);
        session.channel_binding = ChannelBinding {
            platform: platform.to_string(),
            channel_id: channel_id.to_string(),
        };
        session.updated_at = Utc::now().to_rfc3339();
        let updated = session.clone();
        self.store
            .channel_index
            .insert(channel_key(platform, channel_id), session_id.to_string());
        self.save()?;
        Ok(Some(updated))
    }

    pub fn rename(&mut self, session_id: &str, title: &str) -> Result<Option<Session>> {
        let Some(session) = self.store.sessions.get_mut(session_id) else {
            return Ok(None);
        };
        session.title = normalize_title(Some(title));
        session.updated_at = Utc::now().to_rfc3339();
        let updated = session.clone();
        self.save()?;
        Ok(Some(updated))
    }

    pub fn set_title_if_empty(&mut self, session_id: &str, message: &str) -> Result<()> {
        let Some(session) = self.store.sessions.get_mut(session_id) else {
            return Ok(());
        };
        if session.title.is_none() {
            session.title = auto_title(message);
        }
        session.updated_at = Utc::now().to_rfc3339();
        self.save()?;
        Ok(())
    }

    pub fn delete(&mut self, session_id: &str) -> Result<Option<Session>> {
        let Some(session) = self.store.sessions.remove(session_id) else {
            return Ok(None);
        };
        self.store
            .channel_index
            .retain(|_, indexed_session_id| indexed_session_id != session_id);
        self.save()?;
        Ok(Some(session))
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

fn normalize_title(title: Option<&str>) -> Option<String> {
    let title = title?.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        None
    } else {
        Some(title.chars().take(80).collect())
    }
}

fn auto_title(message: &str) -> Option<String> {
    normalize_title(Some(message)).map(|title| title.chars().take(48).collect())
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
    fn creates_renames_titles_and_deletes_web_session() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = SessionRuntime::load(&dir).unwrap();
        let session = runtime
            .create_channel("web", "browser-tab", "default", None)
            .unwrap();
        assert_eq!(session.title, None);

        runtime
            .set_title_if_empty(
                &session.id,
                "  A   deliberately long first message for title generation  ",
            )
            .unwrap();
        let titled = runtime.get(&session.id).unwrap();
        assert_eq!(
            titled.title.as_deref(),
            Some("A deliberately long first message for title gene")
        );

        let renamed = runtime
            .rename(&session.id, "  Project   Aurora  ")
            .unwrap()
            .unwrap();
        assert_eq!(renamed.title.as_deref(), Some("Project Aurora"));

        runtime.delete(&session.id).unwrap();
        assert!(runtime.get(&session.id).is_none());
        assert!(runtime.channel_session_id("web", "browser-tab").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn loads_sessions_written_before_title_and_metadata_fields() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("sessions.json"),
            r#"{
              "sessions": {
                "legacy": {
                  "id": "legacy",
                  "channel_binding": {"platform": "cli", "channel_id": "old"},
                  "root_agent_id": "default",
                  "sub_sessions": [],
                  "created_at": "2026-01-01T00:00:00Z",
                  "updated_at": "2026-01-01T00:00:00Z"
                }
              },
              "channel_index": {"cli:old": "legacy"}
            }"#,
        )
        .unwrap();
        let runtime = SessionRuntime::load(&dir).unwrap();
        let session = runtime.get("legacy").unwrap();
        assert_eq!(session.title, None);
        assert!(session.metadata.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn session_metadata_bool_round_trips() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = SessionRuntime::load(&dir).unwrap();
        let session_id = runtime
            .resolve_channel("feishu", "chat", "default")
            .unwrap();

        assert!(!runtime.metadata_bool(&session_id, "debug"));
        assert!(runtime
            .set_metadata_bool(&session_id, "debug", true)
            .unwrap());
        assert!(runtime.metadata_bool(&session_id, "debug"));

        let runtime = SessionRuntime::load(&dir).unwrap();
        assert!(runtime.metadata_bool(&session_id, "debug"));
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

    #[test]
    fn stores_and_removes_string_metadata() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = SessionRuntime::load(&dir).unwrap();
        let session_id = runtime
            .resolve_channel("cli", "local-dev", "default")
            .unwrap();

        assert_eq!(
            runtime.metadata_string(&session_id, "model_profile_id"),
            None
        );
        assert!(runtime
            .set_metadata_string(&session_id, "model_profile_id", "deepseek-v4-flash")
            .unwrap());
        assert_eq!(
            runtime.metadata_string(&session_id, "model_profile_id"),
            Some("deepseek-v4-flash".to_string())
        );
        assert!(runtime
            .remove_metadata(&session_id, "model_profile_id")
            .unwrap());
        assert_eq!(
            runtime.metadata_string(&session_id, "model_profile_id"),
            None
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stores_json_metadata() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = SessionRuntime::load(&dir).unwrap();
        let session_id = runtime
            .resolve_channel("web", "local-dev", "default")
            .unwrap();

        let value = serde_json::json!(["first", "second"]);
        assert!(runtime
            .set_metadata_value(&session_id, "input_history", value.clone())
            .unwrap());
        assert_eq!(
            runtime.metadata_value(&session_id, "input_history"),
            Some(value)
        );

        let runtime = SessionRuntime::load(&dir).unwrap();
        assert_eq!(
            runtime.metadata_value(&session_id, "input_history"),
            Some(serde_json::json!(["first", "second"]))
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forks_session_metadata_without_sub_sessions() {
        let dir = std::env::temp_dir().join(format!("remi-session-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = SessionRuntime::load(&dir).unwrap();
        let parent = runtime
            .create_channel("web", "source", "default", Some("Project".to_string()))
            .unwrap();
        runtime
            .set_metadata_string(&parent.id, "model_profile_id", "fast")
            .unwrap();
        runtime
            .upsert_sub_session(
                &parent.id,
                "agent-sub",
                SubSessionKind::Agent,
                "coder",
                Some("fix".to_string()),
                "done",
            )
            .unwrap();

        let fork = runtime
            .fork_session(&parent.id, "web", "fork-channel", None)
            .unwrap()
            .unwrap();

        assert_ne!(fork.id, parent.id);
        assert_eq!(fork.title.as_deref(), Some("Project (fork)"));
        assert_eq!(fork.sub_sessions.len(), 0);
        assert_eq!(
            fork.metadata
                .get("model_profile_id")
                .and_then(serde_json::Value::as_str),
            Some("fast")
        );
        assert_eq!(
            fork.metadata
                .get("forked_from_session_id")
                .and_then(serde_json::Value::as_str),
            Some(parent.id.as_str())
        );
        assert!(fork
            .metadata
            .get("forked_at")
            .and_then(serde_json::Value::as_str)
            .is_some());

        let rebound = runtime
            .set_channel_binding(&fork.id, "feishu", "chat-fork")
            .unwrap()
            .unwrap();
        assert_eq!(rebound.channel_binding.platform, "feishu");
        assert_eq!(
            runtime.channel_session_id("web", "fork-channel").as_deref(),
            None
        );
        assert_eq!(
            runtime.channel_session_id("feishu", "chat-fork").as_deref(),
            Some(fork.id.as_str())
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
