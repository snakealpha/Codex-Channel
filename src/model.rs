use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub adapter: String,
    pub conversation_id: String,
    pub sender_id: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct OutboundMessage {
    pub adapter: String,
    pub conversation_id: String,
    pub text: String,
    pub is_partial: bool,
    pub kind: OutboundMessageKind,
    pub pending_interaction: Option<PendingInteractionSummary>,
    pub dismiss_pending_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundMessageKind {
    Status,
    Agent,
    Notice,
    CommandResult,
    PendingInteraction,
}

#[derive(Debug, Clone)]
pub enum PendingInteractionKind {
    CommandApproval {
        command: Option<String>,
        cwd: Option<String>,
        reason: Option<String>,
    },
    FileChangeApproval,
    PermissionsApproval {
        permissions: Value,
        reason: Option<String>,
    },
    UserInputRequest {
        questions: Vec<Value>,
    },
}

#[derive(Debug, Clone)]
pub struct PendingInteractionSummary {
    pub token: String,
    pub kind: PendingInteractionKind,
    pub prompt: String,
}

impl PendingInteractionKind {
    pub fn summary(&self, token: String) -> PendingInteractionSummary {
        let prompt = match self {
            PendingInteractionKind::CommandApproval { command, cwd, reason } => format!(
                "Codex requests command approval (`{token}`).\nCommand: `{}`\nWorking directory: `{}`\nReason: {}\nIf this is the only pending request, reply with `/approve`.\nOtherwise run `/pending` and use `/approve 1`, `/approve-session 1`, `/deny 1`, or `/cancel 1`.\nYou can still use the token directly if needed.",
                command.as_deref().unwrap_or("(not provided)"),
                cwd.as_deref().unwrap_or("(not provided)"),
                reason.as_deref().unwrap_or("(not provided)")
            ),
            PendingInteractionKind::FileChangeApproval => format!(
                "Codex requests file-change approval (`{token}`).\nIf this is the only pending request, reply with `/approve`.\nOtherwise run `/pending` and use `/approve 1`, `/approve-session 1`, `/deny 1`, or `/cancel 1`.\nYou can still use the token directly if needed."
            ),
            PendingInteractionKind::PermissionsApproval { permissions, reason } => format!(
                "Codex requests additional permissions (`{token}`).\nRequested permissions: `{}`\nReason: {}\nIf this is the only pending request, reply with `/approve`.\nOtherwise run `/pending` and use `/approve 1`, `/approve-session 1`, `/deny 1`, or `/cancel 1`.\nYou can still use the token directly if needed.",
                permissions,
                reason.as_deref().unwrap_or("(not provided)")
            ),
            PendingInteractionKind::UserInputRequest { questions } => {
                let mut lines = vec![format!("Codex requests user input (`{token}`).")];
                for question in questions {
                    let header = question
                        .get("header")
                        .and_then(Value::as_str)
                        .unwrap_or("question");
                    let body = question
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or("(missing question text)");
                    lines.push(format!("{header}: {body}"));

                    if let Some(options) = question.get("options").and_then(Value::as_array) {
                        let options_text = options
                            .iter()
                            .filter_map(|option| option.get("label").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join(", ");
                        if !options_text.is_empty() {
                            lines.push(format!("Options: {options_text}"));
                        }
                    }
                }
                if questions.len() == 1 {
                    lines.push(
                        "If this is the only pending request, reply with `/reply <answer>`."
                            .to_owned(),
                    );
                    lines.push(
                        "If there are multiple pending requests, run `/pending` and use `/reply 1 <answer>`."
                            .to_owned(),
                    );
                } else {
                    lines.push(
                        "If this is the only pending request, reply with `/reply {\"question_id\": [\"answer\"]}`."
                            .to_owned(),
                    );
                    lines.push(
                        "If there are multiple pending requests, run `/pending` and use `/reply 1 {\"question_id\": [\"answer\"]}`."
                            .to_owned(),
                    );
                }
                lines.push("You can still use the token directly if needed.".to_owned());
                lines.join("\n")
            }
        };

        PendingInteractionSummary {
            token,
            kind: self.clone(),
            prompt,
        }
    }
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadRecord {
    pub alias: String,
    pub codex_thread_id: Option<String>,
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub collaboration_mode: Option<CollaborationModePreset>,
}

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
