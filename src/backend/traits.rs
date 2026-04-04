#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::domain::interaction::{PendingInteractionAction, PendingInteractionSummary};
use crate::domain::message::{AgentRequest, AgentStreamEvent, AgentTurnSummary};

#[async_trait]
pub trait AgentBackend: Send + Sync {
    fn name(&self) -> &'static str;

    async fn run_turn(
        &self,
        request: AgentRequest,
        event_tx: mpsc::UnboundedSender<AgentStreamEvent>,
    ) -> Result<AgentTurnSummary>;

    async fn respond_to_pending(
        &self,
        token: &str,
        action: PendingInteractionAction,
    ) -> Result<()>;

    async fn list_pending_for_thread(&self, thread_id: &str) -> Result<Vec<PendingInteractionSummary>>;

    fn supports_collaboration_mode(&self) -> bool;
}
