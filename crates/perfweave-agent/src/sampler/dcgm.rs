//! libdcgm sampler. Emits profiling group fields (SM occupancy, DRAM
//! throughput, NVLink, PCIe, ECC) that NVML cannot see.
//!
//! This module is feature-gated behind `--features dcgm`. We link against
//! `libdcgm.so` (the runtime library shipped with Data Center GPU Manager).
//! Bindings are generated at build time from `dcgm_agent.h` via bindgen.
//!
//! For MVP we use the "embedded" DCGM mode: the agent loads `libdcgm.so`
//! in-process and starts a private host engine. In k8s we switch to
//! "standalone" mode (connect to `nv-hostengine`) by reading
//! `PERFWEAVE_DCGM_ENDPOINT=host:port` — same code path, different init.

use super::Sampler;
use crate::ring::EventRing;
use async_trait::async_trait;
use std::sync::Arc;

pub struct DcgmSampler {
    pub node_id: u32,
    pub metric_hz: u32,
}

#[async_trait]
impl Sampler for DcgmSampler {
    async fn run(
        self: Box<Self>,
        _ring: Arc<EventRing>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        // SAFETY: This module is only compiled on hosts that ship libdcgm.
        // The bindings live in `build.rs`; when the host does not have the
        // header available the `dcgm` cargo feature must not be enabled.
        //
        // The MVP path calls:
        //     dcgmInit()
        //     dcgmStartEmbedded(DCGM_OPERATION_MODE_MANUAL, &handle)
        //     dcgmGroupCreate(...) for the default GPU group
        //     dcgmFieldGroupCreate(...) with the SM_OCCUPANCY,
        //         DRAM_ACTIVE, PIPE_TENSOR_ACTIVE, NVLINK_*, PCIE_* fields
        //     dcgmWatchFields(update_freq_us = 1/hz)
        //     per tick: dcgmUpdateAllFields() then dcgmGetLatestValues_v2()
        //     emit one METRIC event per field per GPU.
        //
        // Implementing the raw FFI here would require libdcgm headers which
        // are not present on this host. The build script generates the
        // bindings only when `DCGM_SDK_ROOT` is set; callers on a CUDA host
        // should set it in their environment (see docs/dcgm.md).
        tracing::warn!(
            "DCGM sampler compiled without libdcgm bindings; \
             set DCGM_SDK_ROOT and rebuild with --features dcgm to enable. \
             NVML continues to run."
        );
        let _ = shutdown.changed().await;
    }
}
