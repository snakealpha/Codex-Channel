use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    #[serde(default = "default_state_file")]
    pub state_file: PathBuf,
    pub adapter: AdapterConfig,
    pub codex: CodexConfig,
}

impl GatewayConfig {
    pub async fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut config: GatewayConfig =
            toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))?;

        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        config.resolve_relative_paths(base_dir);
        Ok(config)
    }

    fn resolve_relative_paths(&mut self, base_dir: &Path) {
        if self.state_file.is_relative() {
            self.state_file = base_dir.join(&self.state_file);
        }
        self.codex.resolve_relative_paths(base_dir);
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdapterConfig {
    Console(ConsoleAdapterConfig),
    Telegram(TelegramAdapterConfig),
    Feishu(FeishuAdapterConfig),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConsoleAdapterConfig {
    #[serde(default = "default_conversation_id")]
    pub default_conversation_id: String,
    #[serde(default = "default_console_banner")]
    pub banner: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramAdapterConfig {
    pub bot_token_env: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeishuAdapterConfig {
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub app_secret: Option<String>,
    #[serde(default)]
    pub app_id_env: Option<String>,
    #[serde(default)]
    pub app_secret_env: Option<String>,
    #[serde(default = "default_feishu_base_url")]
    pub base_url: String,
    #[serde(default = "default_feishu_reconnect_seconds")]
    pub reconnect_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexConfig {
    #[serde(default = "default_launcher")]
    pub launcher: Vec<String>,
    pub working_directory: PathBuf,
    #[serde(default)]
    pub http_proxy: Option<String>,
    #[serde(default)]
    pub https_proxy: Option<String>,
    #[serde(default)]
    pub all_proxy: Option<String>,
    #[serde(default)]
    pub no_proxy: Option<String>,
    #[serde(default)]
    pub additional_writable_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub full_auto: bool,
    #[serde(default)]
    pub skip_git_repo_check: bool,
    #[serde(default = "default_turn_timeout_secs")]
    pub turn_timeout_secs: u64,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

impl CodexConfig {
    fn resolve_relative_paths(&mut self, base_dir: &Path) {
        if self.working_directory.is_relative() {
            self.working_directory = base_dir.join(&self.working_directory);
        }

        for dir in &mut self.additional_writable_dirs {
            if dir.is_relative() {
                *dir = base_dir.join(&*dir);
            }
        }
    }
}

fn default_state_file() -> PathBuf {
    PathBuf::from(".codex-channel/sessions.json")
}

fn default_conversation_id() -> String {
    "default".to_owned()
}

fn default_feishu_base_url() -> String {
    "https://open.feishu.cn".to_owned()
}

fn default_feishu_reconnect_seconds() -> u64 {
    5
}

fn default_console_banner() -> String {
    [
        "codex-channel console adapter is running.",
        "Type a message and press Enter.",
        "Use `thread-id > your message` to simulate multiple IM conversations.",
        "Use `/quit` to stop.",
    ]
    .join("\n")
}

fn default_turn_timeout_secs() -> u64 {
    90
}

#[cfg(target_os = "windows")]
fn default_launcher() -> Vec<String> {
    vec!["cmd".to_owned(), "/c".to_owned(), "codex".to_owned()]
}

#[cfg(not(target_os = "windows"))]
fn default_launcher() -> Vec<String> {
    vec!["codex".to_owned()]
}
