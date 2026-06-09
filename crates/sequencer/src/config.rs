use alloy::primitives::Address;
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub l1: L1Config,
    pub database: DbConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub port: u16,
    pub host: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct L1Config {
    pub enabled: bool,
    pub rpc_url: String,
    pub bridge_address: Address,
    pub private_key: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DbConfig {
    pub state_tree_path: String,
    pub queues_path: String,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv().context("failed to load .env file");

        let s = config::Config::builder()
            .add_source(config::File::with_name("config/default"))
            .add_source(config::Environment::with_prefix("SEQ").separator("__"))
            .build()
            .context("failed to build config")?;

        // println!("parsed config {:?}", s);

        let config: AppConfig = s
            .try_deserialize()
            .context("failed to deserialize config")?;
        Ok(config)
    }
}
