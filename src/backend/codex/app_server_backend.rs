use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::backend::traits::AgentBackend;
use crate::codex::CodexCli;
use crate::domain::interaction::{PendingInteractionAction, PendingInteractionSummary};
use crate::domain::message::{AgentRequest, AgentStreamEvent, AgentTurnSummary};

pub struct CodexAppServerBackend {
    inner: Arc<CodexCli>,
}

impl CodexAppServerBackend {
    pub fn new(inner: Arc<CodexCli>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl AgentBackend for CodexAppServerBackend {
    fn name(&self) -> &'static str {
        "codex-app-server"
    }

    async fn run_turn(
        &self,
        request: AgentRequest,
        event_tx: mpsc::UnboundedSender<AgentStreamEvent>,
    ) -> Result<AgentTurnSummary> {
        let summary = self
            .inner
            .run_turn(request.into(), move |event| {
                let event_tx = event_tx.clone();
                async move {
                    event_tx
                        .send(event.into())
                        .map_err(|_| anyhow::anyhow!("agent event receiver dropped"))?;
                    Ok(())
                }
            })
            .await?;
        Ok(summary.into())
    }

    async fn respond_to_pending(
        &self,
        token: &str,
        action: PendingInteractionAction,
    ) -> Result<()> {
        self.inner.respond_to_pending(token, action).await
    }

    async fn list_pending_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingInteractionSummary>> {
        self.inner.list_pending_for_thread(thread_id).await
    }

    fn supports_collaboration_mode(&self) -> bool {
        self.inner.supports_collaboration_mode()
    }
}
