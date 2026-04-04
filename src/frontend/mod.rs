pub mod console;
pub mod feishu;
pub mod traits;

use std::sync::Arc;

use anyhow::{bail, Result};

use crate::config::AdapterConfig;

pub use traits::ChannelFrontend;

pub fn build_frontend(config: &AdapterConfig) -> Result<Arc<dyn ChannelFrontend>> {
    match config {
        AdapterConfig::Console(console) => Ok(Arc::new(console::ConsoleFrontend::new(
            console.clone(),
        ))),
        AdapterConfig::Telegram(telegram) => bail!(
            "telegram adapter is not implemented yet; bot token env key `{}` is configured, but you still need to add a frontend in src/frontend",
            telegram.bot_token_env
        ),
        AdapterConfig::Feishu(feishu) => Ok(Arc::new(feishu::FeishuFrontend::new(
            feishu.clone(),
        )?)),
    }
}
