use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationModePreset {
    Plan,
    Default,
}

impl CollaborationModePreset {
    pub fn as_codex_mode(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Default => "default",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Default => "default",
        }
    }
}
