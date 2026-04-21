use anyhow::Result;
use clap::Parser;
use perfweave_api::app::{self, AppConfig};

#[derive(Parser, Debug)]
#[command(name = "perfweave-api")]
struct Args {
    #[arg(long, default_value_t = format!("0.0.0.0:{}", perfweave_common::ports::API_HTTP))]
    listen: String,

    /// Path to the pre-built frontend assets (web/dist). If unset, the UI is
    /// not served and only the API endpoints are available.
    #[arg(long, env = "PERFWEAVE_WEB_DIR")]
    web_dir: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    perfweave_common::logging::init("api");
    let args = Args::parse();
    app::run(AppConfig { listen: args.listen, web_dir: args.web_dir }).await
}
