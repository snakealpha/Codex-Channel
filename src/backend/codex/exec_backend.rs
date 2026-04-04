use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::backend::traits::AgentBackend;
use crate::codex::CodexCli;
use crate::domain::interaction::{PendingInteractionAction, PendingInteractionSummary};
use crate::domain::message::{AgentRequest, AgentStreamEvent, AgentTurnSummary};

pub struct CodexExecBackend {
    inner: Arc<CodexCli>,
}

impl CodexExecBackend {
    pub fn new(inner: Arc<CodexCli>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl AgentBackend for CodexExecBackend {
    fn name(&self) -> &'static str {
        "codex-exec"
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
        _token: &str,
        _action: PendingInteractionAction,
    ) -> Result<()> {
        bail!("pending interactions are only available in app-server mode")
    }

    async fn list_pending_for_thread(
        &self,
        _thread_id: &str,
    ) -> Result<Vec<PendingInteractionSummary>> {
        Ok(Vec::new())
    }

    fn supports_collaboration_mode(&self) -> bool {
        false
    }
}
