use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::io::{self, AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::config::ConsoleAdapterConfig;
use crate::im::ImAdapter;
use crate::model::{InboundMessage, OutboundMessage, OutboundMessageKind};

pub struct ConsoleAdapter {
    config: ConsoleAdapterConfig,
}

impl ConsoleAdapter {
    pub fn new(config: ConsoleAdapterConfig) -> Self {
        Self { config }
    }

    fn parse_line(&self, line: &str) -> Option<(String, String)> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        if let Some((conversation_id, text)) = trimmed.split_once('>') {
            let conversation_id = conversation_id.trim();
            let text = text.trim();
            if !conversation_id.is_empty() && !text.is_empty() {
                return Some((conversation_id.to_owned(), text.to_owned()));
            }
        }

        Some((
            self.config.default_conversation_id.clone(),
            trimmed.to_owned(),
        ))
    }

    fn print_help(&self) {
        println!("{}", self.config.banner);
    }
}

#[async_trait]
impl ImAdapter for ConsoleAdapter {
    fn name(&self) -> &'static str {
        "console"
    }

    async fn run(&self, inbound: mpsc::Sender<InboundMessage>) -> Result<()> {
        println!("{}", self.config.banner);

        let stdin = io::stdin();
        let mut lines = BufReader::new(stdin).lines();

        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.eq_ignore_ascii_case("/quit") {
                break;
            }
            if trimmed.eq_ignore_ascii_case("/help") {
                self.print_help();
                continue;
            }

            let Some((conversation_id, text)) = self.parse_line(trimmed) else {
                continue;
            };

            inbound
                .send(InboundMessage {
                    adapter: self.name().to_owned(),
                    conversation_id,
                    sender_id: None,
                    text,
                })
                .await
                .map_err(|_| {
                    anyhow!("gateway stopped before console message could be delivered")
                })?;
        }

        Ok(())
    }

    async fn send(&self, message: OutboundMessage) -> Result<()> {
        let label = if message.is_partial {
            "codex-partial"
        } else {
            match message.kind {
                OutboundMessageKind::Status => "codex-status",
                OutboundMessageKind::Agent => "codex",
                OutboundMessageKind::Notice => "codex-notice",
                OutboundMessageKind::CommandResult => "codex-command",
            }
        };
        println!(
            "[{}:{}] {}",
            message.adapter, message.conversation_id, label
        );
        println!("{}", message.text);
        Ok(())
    }
}
