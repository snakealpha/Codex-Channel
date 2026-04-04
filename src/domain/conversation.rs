use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::thread::ThreadRecord;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationState {
    pub current_thread: String,
    pub threads: HashMap<String, ThreadRecord>,
}

impl ConversationState {
    pub fn with_default_thread(default_working_directory: PathBuf) -> Self {
        let default_thread = ThreadRecord {
            alias: "main".to_owned(),
            codex_thread_id: None,
            working_directory: Some(default_working_directory),
            collaboration_mode: None,
        };

        let mut threads = HashMap::new();
        threads.insert(default_thread.alias.clone(), default_thread);

        Self {
            current_thread: "main".to_owned(),
            threads,
        }
    }

    pub fn current_thread_record(&self) -> Option<&ThreadRecord> {
        self.threads.get(&self.current_thread)
    }

    pub fn current_thread_record_mut(&mut self) -> Option<&mut ThreadRecord> {
        self.threads.get_mut(&self.current_thread)
    }
}

pub fn conversation_key(adapter: &str, conversation_id: &str) -> String {
    format!("{adapter}::{conversation_id}")
}
