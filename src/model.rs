#![allow(unused_imports)]

pub use crate::domain::command::{parse_pending_target, PendingTarget};
pub use crate::domain::conversation::{conversation_key, ConversationState};
pub use crate::domain::interaction::{
    PendingInteractionAction, PendingInteractionKind, PendingInteractionSummary,
};
pub use crate::domain::message::{
    AgentRequest, AgentStreamEvent, AgentTurnSummary, InboundMessage, OutboundMessage,
    OutboundMessageKind,
};
pub use crate::domain::mode::CollaborationModePreset;
pub use crate::domain::thread::ThreadRecord;

#[cfg(test)]
mod tests {
    use super::{CollaborationModePreset, PendingInteractionKind, PendingInteractionSummary};
    use serde_json::json;

    #[test]
    fn legacy_model_exports_still_expose_domain_types() {
        let summary: PendingInteractionSummary = PendingInteractionKind::UserInputRequest {
            questions: vec![json!({
                "id": "color_choice",
                "header": "Color",
                "question": "Pick a color",
                "options": [
                    { "label": "Blue" },
                    { "label": "Green" }
                ]
            })],
        }
        .summary("req-1".to_owned());

        assert!(summary.prompt.contains("```text"));
        assert!(summary.prompt.contains("Blue"));
        assert_eq!(CollaborationModePreset::Plan.display_name(), "plan");
    }
}
