use anyhow::Result;
use clap::Parser;
use perfweave_agent::{
    clock_sync::OffsetHandle,
    config::AgentConfig,
    grpc_client::{self, UploaderConfig},
    ring::EventRing,
    sampler::{synthetic::SyntheticSampler, Sampler},
};
use perfweave_common::intern::Interner;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    perfweave_common::logging::init("agent");
    let cfg = AgentConfig::parse();

    let ring = Arc::new(EventRing::new(cfg.ring_capacity));
    let offsets = OffsetHandle::default();
    let interner = Interner::new();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let hostname = cfg.hostname.clone().unwrap_or_else(|| {
        hostname_best_effort().unwrap_or_else(|| "unknown".to_string())
    });

    // Spawn samplers. NVML + DCGM are feature gated; synthetic is the fallback.
    let samplers: Vec<Box<dyn Sampler>> = if cfg.synthetic {
        vec![Box::new(SyntheticSampler {
            num_gpus: cfg.synthetic_gpus,
            metric_hz: cfg.metric_hz,
            node_id: cfg.node_id,
            seed: 0xC0FFEE_u64,
            burst: cfg.synthetic_burst,
        })]
    } else {
        build_real_samplers(&cfg)
    };

    let num_gpus = match cfg.synthetic {
        true => cfg.synthetic_gpus,
        false => detect_gpu_count(),
    };

    for s in samplers {
        let r = ring.clone();
        let sh = shutdown_rx.clone();
        tokio::spawn(async move {
            s.run(r, sh).await;
        });
    }

    // The address we advertise must be reachable by the server. If the
    // user left it at the 127.0.0.1 default, the server will only be able
    // to reach us when it runs on the same host; we log a hint.
    let agent_rpc_addr = if cfg.rpc_listen.is_empty() {
        String::new()
    } else {
        format!("http://{}", cfg.rpc_listen)
    };
    let uploader = grpc_client::run(
        UploaderConfig {
            collector_url: cfg.collector.clone(),
            hostname,
            node_id: cfg.node_id,
            num_gpus,
            max_batch_events: 20_000,
            flush_interval_ms: 250,
            agent_rpc_addr,
        },
        ring.clone(),
        offsets.clone(),
        interner.clone(),
        shutdown_rx.clone(),
    );

    // Spin up the on-demand replay-profile RPC server so the API server
    // can ask us to re-profile specific kernels.
    if !cfg.rpc_listen.is_empty() {
        let bind: std::net::SocketAddr = cfg.rpc_listen.parse()?;
        let synthetic = cfg.synthetic;
        tokio::spawn(async move {
            if let Err(e) = perfweave_agent::rpc_server::serve(bind, synthetic).await {
                tracing::error!(error=%e, "agent RPC server exited");
            }
        });
    }

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::select! {
        _ = uploader => {}
        _ = ctrl_c => {
            tracing::info!("ctrl-c received; shutting down");
            let _ = shutdown_tx.send(true);
        }
    }

    Ok(())
}

fn build_real_samplers(_cfg: &AgentConfig) -> Vec<Box<dyn Sampler>> {
    #[allow(unused_mut)]
    let mut out: Vec<Box<dyn Sampler>> = Vec::new();

    // Jetson/Tegra: NVML is crippled on these SoCs (no util, flaky power).
    // Auto-select the Tegra sysfs sampler when the kernel looks like
    // L4T. User can force with PERFWEAVE_FORCE_TEGRA=1.
    #[cfg(target_os = "linux")]
    {
        if perfweave_agent::sampler::tegra::is_tegra() {
            tracing::info!("detected Tegra/Jetson; using sysfs sampler instead of NVML");
            out.push(Box::new(perfweave_agent::sampler::tegra::TegraSampler {
                node_id: _cfg.node_id,
                metric_hz: _cfg.metric_hz,
            }));
            return out;
        }
    }

    #[cfg(feature = "nvml")]
    {
        out.push(Box::new(perfweave_agent::sampler::nvml::NvmlSampler {
            node_id: _cfg.node_id,
            metric_hz: _cfg.metric_hz,
        }));
    }
    #[cfg(feature = "dcgm")]
    {
        out.push(Box::new(perfweave_agent::sampler::dcgm::DcgmSampler {
            node_id: _cfg.node_id,
            metric_hz: _cfg.metric_hz,
        }));
    }
    #[cfg(unix)]
    {
        let path = std::env::var("PERFWEAVE_CUPTI_SOCK")
            .unwrap_or_else(|_| "/tmp/perfweave.cupti.sock".to_string());
        let _ = std::fs::remove_file(&path);
        out.push(Box::new(perfweave_agent::sampler::cupti::CuptiReceiver {
            socket_path: std::path::PathBuf::from(path),
        }));
    }
    if out.is_empty() {
        tracing::error!(
            "No GPU samplers enabled. Build with --features nvml (required) \
             and optionally dcgm/cupti. Or pass --synthetic for local dev."
        );
    }
    out
}

fn detect_gpu_count() -> u32 {
    #[cfg(feature = "nvml")]
    {
        if let Ok(n) = nvml_wrapper::Nvml::init() {
            return n.device_count().unwrap_or(0);
        }
    }
    0
}

fn hostname_best_effort() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
}
