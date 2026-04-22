//! Per-(node, gpu) clock-skew table.
//!
//! The agent's own clock_sync loop fits an affine mapping `host_ns = a * gpu_ns + b`
//! and emits it on every batch as a `ClockOffset`. The agent already
//! applies this mapping to CUPTI events, but we re-apply on the server to
//! guard against agents that shipped before their first clock fit landed
//! (those events will have ts_ns in raw GPU time). This is cheap (a
//! HashMap lookup + one multiply-add per event) and idempotent.

use ahash::AHashMap;
use parking_lot::RwLock;
use perfweave_proto::v1::ClockOffset;

#[derive(Default)]
pub struct SkewTable {
    inner: RwLock<AHashMap<(u32, u32), Entry>>,
}

#[derive(Copy, Clone, Debug)]
struct Entry {
    offset_ns: i128,
    slope_num: i128,
    slope_den: i128,
    residual_max_ns: u64,
    updated_at_host_ns: u64,
}

impl SkewTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Absorb the latest ClockOffset for `(node, gpu)`. Called once per
    /// incoming batch (offsets are broadcast, not per-event).
    pub fn upsert(&self, c: &ClockOffset) {
        let den = (c.slope_den as i128).max(1);
        let entry = Entry {
            offset_ns: c.offset_ns as i128,
            slope_num: c.slope_num as i128,
            slope_den: den,
            residual_max_ns: c.residual_max_ns,
            updated_at_host_ns: perfweave_common::clock::host_realtime_ns(),
        };
        self.inner.write().insert((c.node_id, c.gpu_id), entry);
    }

    /// Map a raw source-clock timestamp to host-corrected ns. No-op
    /// (identity) when we have no fit yet for this (node, gpu) pair.
    #[inline]
    pub fn map(&self, node_id: u32, gpu_id: u32, src_ns: u64) -> u64 {
        let g = self.inner.read();
        if let Some(e) = g.get(&(node_id, gpu_id)) {
            let src = src_ns as i128;
            let host = e.offset_ns + (e.slope_num.saturating_mul(src)) / e.slope_den;
            host.max(0) as u64
        } else {
            src_ns
        }
    }

    pub fn latest_residual_ns(&self) -> Option<u64> {
        self.inner
            .read()
            .values()
            .map(|e| e.residual_max_ns)
            .max()
    }

    pub fn newest_update_host_ns(&self) -> Option<u64> {
        self.inner
            .read()
            .values()
            .map(|e| e.updated_at_host_ns)
            .max()
    }
}
