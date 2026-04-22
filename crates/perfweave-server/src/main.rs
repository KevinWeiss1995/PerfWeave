//! Unified PerfWeave server: ingest + api + live plane in one binary.
//!
//! Per `.cursorrules`: `perfweave start` must work with no flags. The
//! defaults are sourced from `perfweave_common::ports` so the CLI, the
//! server, and the agent upload target all agree without each file
//! hardcoding its own copy.

use clap::Parser;
use perfweave_common::ports;
use perfweave_server::{run, AppConfig};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "perfweave-server", version, about = "PerfWeave unified server")]
struct Cli {
    /// HTTP/GraphQL/SSE listen address.
    #[arg(long, default_value_t = format!("0.0.0.0:{}", ports::API_HTTP))]
    http: String,

    /// gRPC ingest listen address.
    #[arg(long, default_value_t = format!("0.0.0.0:{}", ports::COLLECTOR_GRPC))]
    grpc: String,

    /// Optional path to the frontend bundle (served as static assets).
    #[arg(long)]
    web_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let cfg = AppConfig {
        http_listen: cli.http,
        grpc_listen: cli.grpc,
        web_dir: cli.web_dir,
        ch: None,
    };
    run(cfg).await
}
