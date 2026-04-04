pub mod console;
pub mod feishu;

use std::sync::Arc;

use anyhow::Result;

use crate::config::AdapterConfig;
use crate::frontend;
pub use crate::frontend::traits::ChannelFrontend as ImAdapter;

pub use console::ConsoleAdapter;
pub use feishu::FeishuAdapter;

pub fn build_adapter(config: &AdapterConfig) -> Result<Arc<dyn ImAdapter>> {
    frontend::build_frontend(config)
}
