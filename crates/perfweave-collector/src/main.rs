use anyhow::Result;
use clap::Parser;
use clickhouse::Client;
use perfweave_collector::{clickhouse_sink::{Sink, SinkConfig}, migrations, server::CollectorSvc};
use std::{net::SocketAddr, sync::Arc};
use tonic::transport::Server;

#[derive(Parser, Debug)]
#[command(name = "perfweave-collector")]
struct Args {
    #[arg(long, default_value_t = format!("0.0.0.0:{}", perfweave_common::ports::COLLECTOR_GRPC))]
    listen: String,

    #[arg(long, env = "PERFWEAVE_CH_URL", default_value = "http://127.0.0.1:8123")]
    clickhouse_url: String,

    #[arg(long, env = "PERFWEAVE_CH_USER", default_value = "default")]
    clickhouse_user: String,

    #[arg(long, env = "PERFWEAVE_CH_PASSWORD", default_value = "")]
    clickhouse_password: String,

    #[arg(long, env = "PERFWEAVE_CH_DATABASE", default_value = "default")]
    clickhouse_database: String,

    /// Skip running migrations at startup. Use when multiple collectors share one DB.
    #[arg(long)]
    skip_migrations: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    perfweave_common::logging::init("collector");
    let args = Args::parse();

    if !args.skip_migrations {
        let client = Client::default()
            .with_url(&args.clickhouse_url)
            .with_user(&args.clickhouse_user)
            .with_password(&args.clickhouse_password)
            .with_database(&args.clickhouse_database);
        migrations::apply(&client).await?;
    }

    let sink = Sink::spawn(SinkConfig {
        url: args.clickhouse_url.clone(),
        user: args.clickhouse_user.clone(),
        password: args.clickhouse_password.clone(),
        database: args.clickhouse_database.clone(),
        ..Default::default()
    })?;

    let svc = CollectorSvc::new(Arc::new(sink)).into_service();
    let addr: SocketAddr = args.listen.parse()?;
    tracing::info!(%addr, "collector listening");

    Server::builder()
        .add_service(svc)
        .serve(addr)
        .await?;
    Ok(())
}
