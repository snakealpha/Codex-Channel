use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::mode::CollaborationModePreset;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadRecord {
    pub alias: String,
    pub codex_thread_id: Option<String>,
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub collaboration_mode: Option<CollaborationModePreset>,
}
