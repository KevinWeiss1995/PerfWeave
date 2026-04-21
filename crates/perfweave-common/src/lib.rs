//! Shared types, clock alignment, string interning, and structured logging
//! setup used across every PerfWeave process.
//!
//! The canonical event model lives in `perfweave-proto`; this crate provides
//! ergonomic Rust wrappers and the non-FFI logic that needs to be identical
//! on agents, collectors, and the API.

pub mod clock;
pub mod intern;
pub mod logging;

pub use perfweave_proto as proto;
pub use proto::v1::Category;

/// Short name used across logs; also the User-Agent and Docker image tag.
pub const PRODUCT_NAME: &str = "perfweave";
pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default TCP ports. Chosen to not collide with Prometheus (9090), Grafana
/// (3000), ClickHouse HTTP (8123), or Jaeger (16686).
pub mod ports {
    pub const API_HTTP: u16 = 7777;
    pub const COLLECTOR_GRPC: u16 = 7778;
    pub const CLICKHOUSE_NATIVE: u16 = 9000;
    pub const CLICKHOUSE_HTTP: u16 = 8123;
}

/// Tile pyramid resolutions used everywhere in the system. Widths are chosen
/// to be powers of 8 apart so LOD transitions are visually smooth (each level
/// is 3 bits of zoom).
pub const TILE_WIDTHS_NS: &[u64] = &[
    1_000,             // 1us
    8_000,             // 8us
    64_000,            // 64us
    512_000,           // 512us
    4_096_000,         // ~4ms
    32_768_000,        // ~32ms
    262_144_000,       // ~262ms
    2_097_152_000,     // ~2.1s
];
