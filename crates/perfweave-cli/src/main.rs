//! The `perfweave` user-facing CLI.
//!
//! Zero-config goal: `perfweave start` auto-detects GPUs, brings up the
//! collector + API against a local ClickHouse (or errors with the exact
//! fix to run), starts an agent, and opens the browser at
//! http://localhost:7777.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod doctor;
mod local_agent;
mod validate_clocks;

#[derive(Parser, Debug)]
#[command(name = "perfweave", version, about = "Unified GPU observability")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Start the collector + API and open the UI. Zero-config.
    Start(StartArgs),

    /// Run a CUDA program under the CUPTI injector with perfweave attached.
    Run(RunArgs),

    /// Diagnose the environment: NVML/DCGM/CUPTI presence, driver versions,
    /// ClickHouse reachability. Every failure includes the fix.
    Doctor(DoctorArgs),

    /// Measure clock alignment. Exits non-zero if residual > 2us.
    ValidateClocks(ValidateClocksArgs),

    /// Import a .nsys-rep file.
    Import(ImportArgs),
}

#[derive(Parser, Debug)]
struct StartArgs {
    #[arg(long, default_value_t = format!("0.0.0.0:{}", perfweave_common::ports::API_HTTP))]
    api_listen: String,
    #[arg(long, default_value_t = format!("0.0.0.0:{}", perfweave_common::ports::COLLECTOR_GRPC))]
    collector_listen: String,
    #[arg(long, env = "PERFWEAVE_WEB_DIR")]
    web_dir: Option<std::path::PathBuf>,
    #[arg(long)]
    no_open: bool,
    #[arg(long, env = "PERFWEAVE_CH_URL", default_value = "http://127.0.0.1:8123")]
    clickhouse_url: String,
    /// Don't spawn an in-process local agent. Useful when a separate agent
    /// is feeding this collector (typical on multi-node deployments).
    #[arg(long)]
    no_local_agent: bool,
    /// Force the local agent into synthetic mode (no real GPU access).
    /// Useful for demos and UI development.
    #[arg(long)]
    synthetic: bool,
    /// Synthetic mode GPU count when --synthetic is set.
    #[arg(long, default_value_t = 2)]
    synthetic_gpus: u32,
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Arguments passed to the target program. Use `--` to separate.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    argv: Vec<String>,
}

#[derive(Parser, Debug)]
struct DoctorArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
struct ValidateClocksArgs {
    #[arg(long, default_value_t = 1024)]
    samples: usize,
    #[arg(long, default_value_t = 2000)]
    max_residual_ns: u64,
}

#[derive(Parser, Debug)]
struct ImportArgs {
    path: std::path::PathBuf,
    #[arg(long, default_value_t = 0)]
    node_id: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse before init so --help / --version exit without a log line.
    let cli = Cli::parse();
    perfweave_common::logging::init("cli");
    match cli.cmd {
        Cmd::Start(a) => cmd_start(a).await,
        Cmd::Run(a) => cmd_run(a).await,
        Cmd::Doctor(a) => doctor::run(a.json).await,
        Cmd::ValidateClocks(a) => validate_clocks::run(a.samples, a.max_residual_ns).await,
        Cmd::Import(a) => cmd_import(a).await,
    }
}

async fn cmd_start(args: StartArgs) -> Result<()> {
    use perfweave_server::{run, store::ch::ChConfig, AppConfig};

    // Unified server: ingest (gRPC) + api (HTTP/GraphQL/SSE) + FastRing +
    // spike detector in one process. No IPC hop for metrics.
    let cfg = AppConfig {
        http_listen: args.api_listen.clone(),
        grpc_listen: args.collector_listen.clone(),
        web_dir: args.web_dir.clone(),
        ch: Some(ChConfig {
            url: args.clickhouse_url.clone(),
            ..Default::default()
        }),
    };
    let server = tokio::spawn(async move {
        if let Err(e) = run(cfg).await {
            tracing::error!(error=%e, "perfweave-server crashed");
        }
    });

    // Local agent (the zero-config case). For multi-node deployments, pass
    // --no-local-agent and run `perfweave-agent` on every node.
    if !args.no_local_agent {
        let collector_url = format!(
            "http://{}",
            args.collector_listen.replace("0.0.0.0", "127.0.0.1"),
        );
        tokio::spawn(local_agent::run(local_agent::Opts {
            collector_url,
            synthetic: args.synthetic,
            synthetic_gpus: args.synthetic_gpus,
        }));
    }

    // Open browser unless asked not to.
    if !args.no_open {
        let url = format!("http://{}/", args.api_listen.replace("0.0.0.0", "localhost"));
        if let Err(e) = open_browser(&url) {
            tracing::info!(url = %url, reason = %e, "PerfWeave UI ready; open this URL manually");
        }
    }

    server.await?;
    Ok(())
}

async fn cmd_run(args: RunArgs) -> Result<()> {
    if args.argv.is_empty() {
        anyhow::bail!("usage: perfweave run -- <program> [args...]");
    }
    let inject = std::env::var("PERFWEAVE_CUPTI_INJECT_SO").unwrap_or_else(|_| {
        "/usr/local/lib/libperfweave_cupti_inject.so".to_string()
    });
    let mut child = tokio::process::Command::new(&args.argv[0])
        .args(&args.argv[1..])
        .env("CUDA_INJECTION64_PATH", &inject)
        .env("PERFWEAVE_CUPTI_SOCK", "/tmp/perfweave.cupti.sock")
        .spawn()
        .with_context(|| format!("failed to spawn {}", args.argv[0]))?;
    let status = child.wait().await?;
    std::process::exit(status.code().unwrap_or(1));
}

async fn cmd_import(args: ImportArgs) -> Result<()> {
    use perfweave_server::store::ch::{Ch, ChConfig};
    let ch = Ch::new(ChConfig::default());
    let r = perfweave_server::api::imports::import_nsys(&ch, &args.path, args.node_id).await?;
    println!("imported {} events ({}ns .. {}ns)", r.event_count, r.ts_min_ns, r.ts_max_ns);
    Ok(())
}

fn open_browser(url: &str) -> Result<()> {
    // On headless Linux (no DISPLAY / WAYLAND_DISPLAY) don't even try —
    // xdg-open will print "Error: no DISPLAY..." to stderr which looks
    // like a real failure in the logs. Caller has already printed the URL.
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("DISPLAY").is_none()
            && std::env::var_os("WAYLAND_DISPLAY").is_none()
        {
            anyhow::bail!("headless (no DISPLAY); not opening a browser");
        }
    }

    #[cfg(target_os = "linux")]
    let prog = "xdg-open";
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let prog = "cmd";

    std::process::Command::new(prog)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}
