#![allow(dead_code)]

use std::path::PathBuf;

use crate::domain::interaction::{PendingInteractionKind, PendingInteractionSummary};
use crate::domain::mode::CollaborationModePreset;

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
pub struct AgentRequest {
    pub session_id: Option<String>,
    pub prompt: String,
    pub working_directory: PathBuf,
    pub collaboration_mode: Option<CollaborationModePreset>,
}

#[derive(Debug, Clone)]
pub struct AgentTurnSummary {
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AgentStreamEvent {
    AgentMessage { text: String, is_partial: bool },
    Notice(String),
    PendingInteraction(PendingInteractionSummary),
}

#[allow(dead_code)]
pub(crate) fn summarize_pending_kind(kind: &PendingInteractionKind, token: String) -> String {
    kind.summary(token).prompt
}
