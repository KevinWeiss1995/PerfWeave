//! In-process agent launched by `perfweave start` in the zero-config case.
//!
//! This is a thin convenience wrapper: it builds samplers, spins the
//! uploader against the local collector, and shares a shutdown channel
//! with the rest of the process. It stays feature-parity with the
//! standalone `perfweave-agent` binary by reusing the same building blocks
//! from the `perfweave-agent` library crate.

use std::sync::Arc;

use perfweave_agent::{
    clock_sync::OffsetHandle,
    grpc_client::{self, UploaderConfig},
    ring::EventRing,
    sampler::{synthetic::SyntheticSampler, Sampler},
};
use perfweave_common::intern::Interner;

pub struct Opts {
    pub collector_url: String,
    pub synthetic: bool,
    pub synthetic_gpus: u32,
}

pub async fn run(opts: Opts) {
    let ring = Arc::new(EventRing::new(1_048_576));
    let offsets = OffsetHandle::default();
    let interner = Interner::new();
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let hostname = hostname_best_effort().unwrap_or_else(|| "local".into());

    let samplers: Vec<Box<dyn Sampler>> = if opts.synthetic || !has_real_gpu() {
        if !opts.synthetic {
            tracing::warn!(
                "no real GPU detected; local agent falling back to synthetic mode. \
                 Pass --no-local-agent and run `perfweave-agent` on a CUDA host for live data."
            );
        }
        vec![Box::new(SyntheticSampler {
            num_gpus: opts.synthetic_gpus.max(1),
            metric_hz: 1000,
            node_id: 0,
            seed: 0xC0FFEE,
        })]
    } else {
        build_real_samplers()
    };

    let num_gpus = if opts.synthetic || !has_real_gpu() {
        opts.synthetic_gpus.max(1)
    } else {
        gpu_count()
    };

    for s in samplers {
        let r = ring.clone();
        let sh = shutdown_rx.clone();
        tokio::spawn(async move { s.run(r, sh).await });
    }

    let _ = grpc_client::run(
        UploaderConfig {
            collector_url: opts.collector_url,
            hostname,
            node_id: 0,
            num_gpus,
            max_batch_events: 20_000,
            flush_interval_ms: 250,
        },
        ring,
        offsets,
        interner,
        shutdown_rx,
    )
    .await;
}

#[allow(unused_mut)]
fn build_real_samplers() -> Vec<Box<dyn Sampler>> {
    let mut out: Vec<Box<dyn Sampler>> = Vec::new();
    #[cfg(feature = "nvml")]
    {
        out.push(Box::new(perfweave_agent::sampler::nvml::NvmlSampler {
            node_id: 0,
            metric_hz: 10,
        }));
    }
    out
}

fn has_real_gpu() -> bool {
    #[cfg(feature = "nvml")]
    {
        nvml_wrapper::Nvml::init()
            .and_then(|n| n.device_count())
            .map(|c| c > 0)
            .unwrap_or(false)
    }
    #[cfg(not(feature = "nvml"))]
    {
        false
    }
}

fn gpu_count() -> u32 {
    #[cfg(feature = "nvml")]
    {
        nvml_wrapper::Nvml::init()
            .and_then(|n| n.device_count())
            .unwrap_or(0)
    }
    #[cfg(not(feature = "nvml"))]
    {
        0
    }
}

fn hostname_best_effort() -> Option<String> {
    std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()).or_else(|| {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}
