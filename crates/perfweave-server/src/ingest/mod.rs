//! Agent ingest. gRPC server that receives `Batch` messages from agents
//! and routes them to the two planes:
//!   * Metric samples (and MetricFrames) → `crate::fast_ring`
//!   * Everything else → `crate::store::sink`
//!
//! Clock skew correction is applied on a per-(node_id, gpu_id) basis using
//! the latest `ClockOffset` provided by the agent in each batch. This
//! keeps CUPTI GPU-clock timestamps aligned with NVML/Tegra wall-clock
//! samples for cross-source joins.

pub mod grpc;
pub mod skew;

pub use grpc::CollectorSvc;
pub use skew::SkewTable;
