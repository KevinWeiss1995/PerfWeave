//! Merged PerfWeave server: ingestion (gRPC), storage (ClickHouse), live
//! plane (in-proc FastRing + SSE), spike detection, and the HTTP/GraphQL
//! API. Compiled as a single binary so high-rate metrics stay in-process
//! and never pay an IPC hop.
//!
//! Module layout mirrors the layering mandated by `.cursorrules`:
//!   - `ingest`  : receives agent batches over gRPC, normalizes, routes
//!   - `store`   : ClickHouse client + migrations + append-only sink
//!   - `fast_ring`: in-memory, wait-free, time-bounded metric ring
//!   - `spike_detect`: rolling median+MAD over the FastRing
//!   - `live`    : SSE streams (`/api/live`, `/api/live/kernels`)
//!   - `api`     : GraphQL + timeline (Arrow IPC) + imports
//!   - `replay`  : on-demand kernel-replay RPC back to the owning agent

pub mod api;
pub mod fast_ring;
pub mod ingest;
pub mod live;
pub mod replay;
pub mod spike_detect;
pub mod store;

pub use crate::api::app::{run, AppConfig, Services};
