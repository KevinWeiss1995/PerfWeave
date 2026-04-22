use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "perfweave-agent")]
pub struct AgentConfig {
    /// Collector gRPC endpoint (e.g. http://127.0.0.1:7700).
    #[arg(long, env = "PERFWEAVE_COLLECTOR", default_value_t = format!("http://127.0.0.1:{}", perfweave_common::ports::COLLECTOR_GRPC))]
    pub collector: String,

    /// NVML/DCGM/synthetic metric sampling rate in Hz. Default is 1 Hz:
    /// the always-on metric plane is cheap, and spike detection only needs
    /// 1 Hz resolution. Bump to 10 if you want a denser live tail.
    #[arg(long, default_value_t = 1)]
    pub metric_hz: u32,

    /// Emit synthetic data instead of real NVML/DCGM. Useful for UI dev on
    /// machines without NVIDIA GPUs. Default profile is metrics-only at
    /// `metric_hz`; pass --synthetic-burst for kernel/API/memcpy traffic.
    #[arg(long)]
    pub synthetic: bool,

    /// When set together with --synthetic, also emit synthetic kernel/API
    /// /memcpy bursts (~4–32 kernels per gpu per tick). Useful for demos
    /// and load regression tests. Default off so --synthetic on its own
    /// doesn't pin a CPU pretending to be a busy GPU.
    #[arg(long, requires = "synthetic")]
    pub synthetic_burst: bool,

    /// Number of synthetic GPUs to emulate when --synthetic is set.
    #[arg(long, default_value_t = 1)]
    pub synthetic_gpus: u32,

    /// Node id suggested to the collector. 0 means "server assigns".
    #[arg(long, default_value_t = 0)]
    pub node_id: u32,

    /// Override the hostname sent to the collector.
    #[arg(long, env = "PERFWEAVE_HOSTNAME")]
    pub hostname: Option<String>,

    /// Maximum events pending in the ring buffer before we start dropping.
    /// Memory footprint ~ ring_capacity * 256B.
    #[arg(long, default_value_t = 1_048_576)]
    pub ring_capacity: usize,

    /// Bind address for the agent's `PerfweaveAgent` RPC server (used by
    /// the server for on-demand kernel-replay profiling). Set to empty
    /// string to disable. Default: 127.0.0.1:7780.
    #[arg(long, default_value = "127.0.0.1:7780")]
    pub rpc_listen: String,
}
