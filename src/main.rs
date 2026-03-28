mod codex;
mod config;
mod gateway;
mod im;
mod model;
mod session_store;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use config::GatewayConfig;
use gateway::Gateway;
use im::build_adapter;
use session_store::SessionStore;
use tracing_subscriber::EnvFilter;

struct Cli {
    config: PathBuf,
}

impl Cli {
    fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut config = PathBuf::from("config/console.example.toml");

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("expected a path after --config"))?;
                    config = PathBuf::from(value);
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                "--version" | "-V" => {
                    println!("{}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown argument `{other}`")),
            }
        }

        Ok(Self { config })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse()?;
    let config = GatewayConfig::load(&cli.config).await?;
    let default_working_directory = config.codex.working_directory.clone();
    let adapter = build_adapter(&config.adapter)?;
    let codex = Arc::new(codex::CodexCli::new(config.codex.clone()));
    let session_store = Arc::new(SessionStore::load(config.state_file.clone()).await?);

    Gateway::new(adapter, codex, session_store, default_working_directory)
        .run()
        .await
}

fn print_help() {
    println!("codex-channel {}", env!("CARGO_PKG_VERSION"));
    println!("Usage: codex-channel [--config <FILE>]");
}
