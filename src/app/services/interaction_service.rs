use crate::domain::message::{OutboundMessage, OutboundMessageKind};

pub struct InteractionService;

impl InteractionService {
    pub fn build_reply_other_notice(
        adapter: impl Into<String>,
        conversation_id: impl Into<String>,
        example: impl Into<String>,
    ) -> OutboundMessage {
        OutboundMessage {
            adapter: adapter.into(),
            conversation_id: conversation_id.into(),
            text: format!(
                "Send your custom answer like this:\n\n```text\n{}\n```",
                example.into()
            ),
            is_partial: false,
            kind: OutboundMessageKind::Notice,
            pending_interaction: None,
            dismiss_pending_token: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::InteractionService;
    use crate::domain::message::OutboundMessageKind;

    #[test]
    fn reply_other_notice_is_generated_as_notice_message() {
        let message =
            InteractionService::build_reply_other_notice("feishu", "test", "/reply your answer");
        assert!(matches!(message.kind, OutboundMessageKind::Notice));
    }
}
