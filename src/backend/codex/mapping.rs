use crate::codex::{CodexRequest, CodexStreamEvent, CodexTurnSummary};
use crate::domain::message::{AgentRequest, AgentStreamEvent, AgentTurnSummary};

impl From<AgentRequest> for CodexRequest {
    fn from(value: AgentRequest) -> Self {
        Self {
            session_id: value.session_id,
            prompt: value.prompt,
            working_directory: value.working_directory,
            collaboration_mode: value.collaboration_mode,
        }
    }
}

impl From<CodexTurnSummary> for AgentTurnSummary {
    fn from(value: CodexTurnSummary) -> Self {
        Self {
            thread_id: value.thread_id,
        }
    }
}

impl From<CodexStreamEvent> for AgentStreamEvent {
    fn from(value: CodexStreamEvent) -> Self {
        match value {
            CodexStreamEvent::AgentMessage { text, is_partial } => {
                AgentStreamEvent::AgentMessage { text, is_partial }
            }
            CodexStreamEvent::Notice(message) => AgentStreamEvent::Notice(message),
            CodexStreamEvent::PendingInteraction(summary) => {
                AgentStreamEvent::PendingInteraction(summary)
            }
        }
    }
}
