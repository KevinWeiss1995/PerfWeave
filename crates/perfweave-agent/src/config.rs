use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "perfweave-agent")]
pub struct AgentConfig {
    /// Collector gRPC endpoint (e.g. http://127.0.0.1:7778).
    #[arg(long, env = "PERFWEAVE_COLLECTOR", default_value_t = format!("http://127.0.0.1:{}", perfweave_common::ports::COLLECTOR_GRPC))]
    pub collector: String,

    /// NVML/DCGM metric sampling rate in Hz.
    #[arg(long, default_value_t = 10)]
    pub metric_hz: u32,

    /// Emit synthetic data instead of real NVML/DCGM. Useful for UI dev on
    /// machines without NVIDIA GPUs. Implies metric_hz=1000 by default.
    #[arg(long)]
    pub synthetic: bool,

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
}
