#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::domain::message::{InboundMessage, OutboundMessage};

#[async_trait]
pub trait ChannelFrontend: Send + Sync {
    fn name(&self) -> &'static str;

    async fn run(&self, inbound: mpsc::Sender<InboundMessage>) -> Result<()>;

    async fn send(&self, message: OutboundMessage) -> Result<()>;
}
