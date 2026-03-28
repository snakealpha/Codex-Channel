mod console;
mod feishu;

use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::config::AdapterConfig;
use crate::model::{InboundMessage, OutboundMessage};

pub use console::ConsoleAdapter;
pub use feishu::FeishuAdapter;

#[async_trait]
pub trait ImAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run(&self, inbound: mpsc::Sender<InboundMessage>) -> Result<()>;
    async fn send(&self, message: OutboundMessage) -> Result<()>;
}

pub fn build_adapter(config: &AdapterConfig) -> Result<Arc<dyn ImAdapter>> {
    match config {
        AdapterConfig::Console(console) => Ok(Arc::new(ConsoleAdapter::new(console.clone()))),
        AdapterConfig::Telegram(telegram) => bail!(
            "telegram adapter is not implemented yet; bot token env key `{}` is configured, but you still need to add an adapter in src/im",
            telegram.bot_token_env
        ),
        AdapterConfig::Feishu(feishu) => Ok(Arc::new(FeishuAdapter::new(feishu.clone())?)),
    }
}
