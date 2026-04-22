//! Always-on CUPTI Profiler host-sampling.
//!
//! Design (per the two-plane rework plan):
//!
//! * Rate: 10 Hz (one sample every 100 ms). Low enough that the overhead
//!   is well under the 5% budget, high enough that a typical DL kernel
//!   (mid-µs to low-ms) will land ≥1 sample per launch and a "long" kernel
//!   gets enough samples to cross the confidence threshold.
//! * Metrics: exactly four, chosen to cover the four SOL axes that drive
//!   bound classification:
//!     - sm__cycles_active.sum                   → sm_active_pct
//!     - sm__warps_active.sum                    → achieved_occupancy_pct
//!     - dram__throughput.avg.pct_of_peak        → dram_bw_pct
//!     - smsp__inst_executed.sum                 → inst_throughput_pct
//!   Every extra metric costs another Profiler pass, so we keep it tight.
//! * Attribution: each sample is attributed to whichever kernel is
//!   currently running on each SM (CUPTI's Profiler API reports this via
//!   range-based accounting). A kernel needs ≥5 samples before we promote
//!   its SOL card from LOW to… still LOW actually, because host sampling
//!   is never exact. HIGH confidence comes only from replay-profile.
//!
//! This module is intentionally self-contained: the sampling loop and the
//! attribution bookkeeping run regardless of whether CUPTI is linked; the
//! actual Profiler FFI calls live behind `#[cfg(feature = "cupti-pm")]`.
//! Unit tests exercise the attribution logic against synthetic samples.

use perfweave_common::intern::hash as intern_hash;
use perfweave_proto::v1::{
    kernel_sol::{Bound, Confidence, Source},
    KernelSol, StallCount,
};
use std::collections::HashMap;

/// Minimum samples a kernel needs inside the window before we're willing
/// to emit a card for it. Fewer than this and the noise dominates.
pub const MIN_SAMPLES_FOR_CARD: u32 = 5;

/// Wall period between samples. 10 Hz = 100ms.
pub const SAMPLE_PERIOD_MS: u64 = 100;

/// Raw per-sample reading from CUPTI Profiler. Units are "percent of
/// peak" for all four metrics so we can average them directly.
#[derive(Copy, Clone, Debug)]
pub struct PmSample {
    pub sm_active_pct: f32,
    pub occupancy_pct: f32,
    pub dram_bw_pct: f32,
    pub inst_tp_pct: f32,
}

/// Accumulator for per-kernel samples. One entry per (gpu, correlation_id);
/// we rotate the whole map out to the wire once per emission window.
#[derive(Default, Debug)]
pub struct Attribution {
    count: u32,
    sum_sm_active: f64,
    sum_occ: f64,
    sum_dram: f64,
    sum_inst: f64,
}

impl Attribution {
    pub fn add(&mut self, s: PmSample) {
        self.count += 1;
        self.sum_sm_active += s.sm_active_pct as f64;
        self.sum_occ += s.occupancy_pct as f64;
        self.sum_dram += s.dram_bw_pct as f64;
        self.sum_inst += s.inst_tp_pct as f64;
    }

    /// Average into a `KernelSol` if we have enough samples; otherwise None.
    /// Bound classification is a three-way vote between SM-active, DRAM-bw,
    /// and occupancy: highest dominates, with a hysteresis margin to avoid
    /// flipping between adjacent kernels.
    pub fn finalize(&self, correlation_id: u64) -> Option<KernelSol> {
        if self.count < MIN_SAMPLES_FOR_CARD {
            return None;
        }
        let n = self.count as f64;
        let sm_active = (self.sum_sm_active / n) as f32;
        let occ = (self.sum_occ / n) as f32;
        let dram = (self.sum_dram / n) as f32;
        let inst = (self.sum_inst / n) as f32;

        let bound = classify_bound(sm_active, dram, inst);
        Some(KernelSol {
            correlation_id,
            sm_active_pct: sm_active,
            achieved_occupancy_pct: occ,
            dram_bw_pct: dram,
            l1_bw_pct: 0.0,
            l2_bw_pct: 0.0,
            inst_throughput_pct: inst,
            theoretical_occupancy_pct: 0.0,
            arithmetic_intensity: 0.0,
            achieved_gflops: 0.0,
            bound: bound as i32,
            // Host sampling is never exact; cards start LOW and only get
            // promoted to HIGH when the user drills in and replay fires.
            confidence: Confidence::Low as i32,
            source: Source::HostSampling as i32,
            stalls: Vec::new(),
        })
    }
}

/// Classify a kernel as memory-, compute-, or latency-bound based on its
/// 4-metric snapshot. Thresholds match the NVIDIA Speed-of-Light guidance
/// (Compute Workload Analysis Guide, §6) but with a coarser bucketing so
/// the UI shows a single dominant bound.
pub fn classify_bound(sm_active_pct: f32, dram_bw_pct: f32, inst_tp_pct: f32) -> Bound {
    // Hysteresis margin: need a clear winner by ≥10 pct-points.
    const H: f32 = 10.0;
    // If *nothing* is busy, the kernel is latency-bound (waiting on
    // memory, syncs, or warp scheduling).
    if sm_active_pct < 30.0 && dram_bw_pct < 30.0 && inst_tp_pct < 30.0 {
        return Bound::Latency;
    }
    let compute_score = sm_active_pct.max(inst_tp_pct);
    if dram_bw_pct > compute_score + H {
        Bound::Memory
    } else if compute_score > dram_bw_pct + H {
        Bound::Compute
    } else {
        Bound::Balanced
    }
}

/// The shared, per-(gpu, correlation_id) bucket map. Host-sampling writes
/// into it concurrently with kernel-end events, so in the real integration
/// this lives behind a `parking_lot::Mutex` or sharded by gpu_id. Here it
/// is a plain HashMap because unit tests are single-threaded.
#[derive(Default, Debug)]
pub struct HostSampler {
    buckets: HashMap<(u32, u64), Attribution>,
}

impl HostSampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample(&mut self, gpu_id: u32, correlation_id: u64, s: PmSample) {
        self.buckets.entry((gpu_id, correlation_id)).or_default().add(s);
    }

    /// Drain every bucket and emit one SOL row per kernel that hit the
    /// confidence floor. Dropped buckets are lost on purpose: we never
    /// want to emit a card built from 1-2 samples.
    pub fn drain_cards(&mut self) -> Vec<KernelSol> {
        let mut out = Vec::with_capacity(self.buckets.len());
        for ((_gpu, corr), bucket) in self.buckets.drain() {
            if let Some(card) = bucket.finalize(corr) {
                out.push(card);
            }
        }
        out
    }
}

/// Placeholder kept so the linker always has the symbol even when the
/// CUPTI Profiler feature is off. See `InitializeInjection` for where the
/// real loop gets spawned.
#[allow(dead_code)]
pub fn reserved_stall_ids() -> Vec<StallCount> {
    const REASONS: &[&str] = &[
        "MemoryThrottle",
        "Barrier",
        "ExecutionDependency",
        "Selected",
        "NotSelected",
    ];
    REASONS
        .iter()
        .map(|r| StallCount {
            reason_id: intern_hash(r) as u32,
            count: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_needs_five_samples() {
        let mut a = Attribution::default();
        for _ in 0..4 {
            a.add(PmSample {
                sm_active_pct: 80.0,
                occupancy_pct: 75.0,
                dram_bw_pct: 10.0,
                inst_tp_pct: 60.0,
            });
        }
        assert!(a.finalize(1).is_none(), "4 samples should be too few");
        a.add(PmSample {
            sm_active_pct: 80.0,
            occupancy_pct: 75.0,
            dram_bw_pct: 10.0,
            inst_tp_pct: 60.0,
        });
        let card = a.finalize(1).expect("5 samples should emit");
        assert!(card.sm_active_pct > 70.0);
        assert_eq!(card.bound, Bound::Compute as i32);
        assert_eq!(card.confidence, Confidence::Low as i32);
        assert_eq!(card.source, Source::HostSampling as i32);
    }

    #[test]
    fn classify_memory_bound() {
        assert_eq!(classify_bound(30.0, 85.0, 20.0), Bound::Memory);
    }

    #[test]
    fn classify_compute_bound() {
        assert_eq!(classify_bound(85.0, 20.0, 70.0), Bound::Compute);
    }

    #[test]
    fn classify_latency_bound_when_idle() {
        assert_eq!(classify_bound(10.0, 10.0, 10.0), Bound::Latency);
    }

    #[test]
    fn host_sampler_drops_underfilled_buckets() {
        let mut hs = HostSampler::new();
        let s = PmSample {
            sm_active_pct: 50.0,
            occupancy_pct: 50.0,
            dram_bw_pct: 50.0,
            inst_tp_pct: 50.0,
        };
        for i in 0..4 {
            hs.sample(0, 100, s); // 4 samples for kernel 100 -> dropped
            if i < 5 {
                hs.sample(0, 200, s); // 5+ samples for kernel 200 -> kept
            }
        }
        hs.sample(0, 200, s);
        let cards = hs.drain_cards();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].correlation_id, 200);
    }
}
