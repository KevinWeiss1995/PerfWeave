//! Persistence layer. Owns the ClickHouse client, the append-only sink, and
//! the embedded migrations. High-rate metrics do NOT flow through here;
//! they live in `crate::fast_ring` and are only flushed into `tiles_metric`
//! every 2s at the coarsest LOD.

pub mod ch;
pub mod migrations;
pub mod sink;

pub use ch::{Ch, ChConfig};
pub use sink::{Sink, SinkConfig};
