use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::fs;
use tokio::sync::RwLock;

use crate::domain::conversation::ConversationState;
use crate::domain::thread::ThreadRecord;

pub struct SessionStore {
    path: PathBuf,
    sessions: RwLock<HashMap<String, ConversationState>>,
}

impl SessionStore {
    pub async fn load(path: PathBuf) -> Result<Self> {
        let sessions = if path.exists() {
            let raw = fs::read_to_string(&path)
                .await
                .with_context(|| format!("failed to read session store {}", path.display()))?;
            parse_session_snapshot(&raw)
                .with_context(|| format!("failed to parse session store {}", path.display()))?
        } else {
            HashMap::new()
        };

        Ok(Self {
            path,
            sessions: RwLock::new(sessions),
        })
    }

    pub async fn get(&self, key: &str) -> Option<ConversationState> {
        self.sessions.read().await.get(key).cloned()
    }

    pub async fn upsert(&self, key: String, record: ConversationState) -> Result<()> {
        let snapshot = {
            let mut sessions = self.sessions.write().await;
            sessions.insert(key, record);
            sessions.clone()
        };

        self.persist(&snapshot).await
    }

    async fn persist(&self, snapshot: &HashMap<String, ConversationState>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await.with_context(|| {
                format!(
                    "failed to create session store directory {}",
                    parent.display()
                )
            })?;
        }

        let serialized = serde_json::to_string_pretty(snapshot)?;
        fs::write(&self.path, serialized)
            .await
            .with_context(|| format!("failed to write session store {}", self.path.display()))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct LegacySessionRecord {
    codex_thread_id: String,
}

fn parse_session_snapshot(raw: &str) -> Result<HashMap<String, ConversationState>> {
    if let Ok(snapshot) = serde_json::from_str(raw) {
        return Ok(snapshot);
    }

    let legacy: HashMap<String, LegacySessionRecord> = serde_json::from_str(raw)?;
    Ok(legacy
        .into_iter()
        .map(|(key, value)| {
            let mut threads = HashMap::new();
            threads.insert(
                "main".to_owned(),
                ThreadRecord {
                    alias: "main".to_owned(),
                    codex_thread_id: Some(value.codex_thread_id),
                    working_directory: None,
                    collaboration_mode: None,
                },
            );

            (
                key,
                ConversationState {
                    current_thread: "main".to_owned(),
                    threads,
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::parse_session_snapshot;

    #[test]
    fn parses_legacy_session_snapshot_into_domain_conversation_state() {
        let raw = r#"{"console::default":{"codex_thread_id":"thread-123"}}"#;
        let snapshot = parse_session_snapshot(raw).expect("snapshot");
        let state = snapshot.get("console::default").expect("conversation");
        let thread = state.threads.get("main").expect("main thread");
        assert_eq!(thread.codex_thread_id.as_deref(), Some("thread-123"));
    }
}
