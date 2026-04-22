//! Thin ClickHouse client wrapper. Keeps config consolidated and makes it
//! trivial to swap to a pooled client later without touching call sites.

use clickhouse::Client;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct ChConfig {
    pub url: String,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl Default for ChConfig {
    fn default() -> Self {
        Self {
            url: std::env::var("PERFWEAVE_CH_URL").unwrap_or_else(|_| "http://127.0.0.1:8123".into()),
            user: std::env::var("PERFWEAVE_CH_USER").unwrap_or_else(|_| "default".into()),
            password: std::env::var("PERFWEAVE_CH_PASSWORD").unwrap_or_default(),
            database: std::env::var("PERFWEAVE_CH_DATABASE").unwrap_or_else(|_| "default".into()),
        }
    }
}

#[derive(Clone)]
pub struct Ch {
    pub client: Arc<Client>,
}

impl Ch {
    pub fn new(cfg: ChConfig) -> Self {
        let client = Client::default()
            .with_url(&cfg.url)
            .with_user(&cfg.user)
            .with_password(&cfg.password)
            .with_database(&cfg.database);
        Self { client: Arc::new(client) }
    }
}
