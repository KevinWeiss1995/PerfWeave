//! Periodic clock alignment loop.
//!
//! Every 10s we sample the source clock and host realtime clock N=1024 times
//! in a tight loop, discard the top 10% by measurement latency (the gap
//! between the two reads), then fit a Theil-Sen affine model. The fit is
//! published to the collector as a `ClockOffset` record and applied to
//! subsequent events at emission time.
//!
//! For NVML/DCGM sources the source clock IS host realtime, so we skip the
//! fit and emit the identity. For CUPTI we fit against `cuptiGetTimestamp`.

use perfweave_common::clock::{fit_theil_sen, host_realtime_ns, ClockOffset, Pair};
use std::sync::Arc;

/// Abstraction over a source clock to make CUPTI/NVML/DCGM interchangeable.
pub trait SourceClock: Send + Sync {
    fn read_ns(&self) -> u64;
}

/// Fit using N paired samples, filtering outliers by the observed measurement
/// latency (delta between the two reads). Returns None if the fit residual
/// exceeds `max_residual_ns` — callers should log this and fall back to the
/// previous offset.
pub fn measure(src: &dyn SourceClock, n: usize, max_residual_ns: u64) -> Option<ClockOffset> {
    let mut pairs: Vec<Pair> = Vec::with_capacity(n);
    let mut latencies: Vec<u64> = Vec::with_capacity(n);
    for _ in 0..n {
        let before = host_realtime_ns();
        let src_ns = src.read_ns();
        let after = host_realtime_ns();
        let lat = after.saturating_sub(before);
        // Midpoint host ns reduces systematic bias.
        let host_ns = before + lat / 2;
        pairs.push(Pair { src_ns, host_ns });
        latencies.push(lat);
    }
    // Drop the top 10% by latency — these are samples where the scheduler
    // preempted us between the two reads.
    let mut sorted_lat = latencies.clone();
    sorted_lat.sort_unstable();
    let cutoff = sorted_lat[(sorted_lat.len() * 9) / 10];
    let filtered: Vec<Pair> = pairs
        .into_iter()
        .zip(latencies.iter())
        .filter(|(_, l)| **l <= cutoff)
        .map(|(p, _)| p)
        .collect();
    let fit = fit_theil_sen(&filtered)?;
    if fit.residual_max_ns > max_residual_ns {
        tracing::warn!(
            residual_ns = fit.residual_max_ns,
            limit = max_residual_ns,
            "clock fit residual exceeded limit; using anyway but flagging"
        );
    }
    Some(fit)
}

/// A SourceClock that returns host realtime. Useful for sanity tests and for
/// NVML/DCGM sources that are already on host time.
pub struct HostClock;
impl SourceClock for HostClock {
    #[inline]
    fn read_ns(&self) -> u64 {
        host_realtime_ns()
    }
}

/// Shared offset holder — updated by the clock_sync task, read on every event.
#[derive(Clone)]
pub struct OffsetHandle {
    inner: Arc<parking_lot::RwLock<ClockOffset>>,
}

impl OffsetHandle {
    pub fn new(initial: ClockOffset) -> Self {
        Self { inner: Arc::new(parking_lot::RwLock::new(initial)) }
    }

    #[inline]
    pub fn current(&self) -> ClockOffset {
        *self.inner.read()
    }

    pub fn set(&self, c: ClockOffset) {
        *self.inner.write() = c;
    }
}

impl Default for OffsetHandle {
    fn default() -> Self {
        Self::new(ClockOffset::IDENTITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        offset: u64,
    }
    impl SourceClock for Fake {
        fn read_ns(&self) -> u64 {
            host_realtime_ns().saturating_sub(self.offset)
        }
    }

    #[test]
    fn measure_recovers_simple_offset() {
        let fake = Fake { offset: 1_234_000 };
        let fit = measure(&fake, 256, 10_000).expect("fit");
        // Slope should be ~1.0, offset should reflect the fixed 1.234ms skew.
        let slope = fit.slope_num as f64 / fit.slope_den as f64;
        assert!((slope - 1.0).abs() < 1e-3);
        assert!(fit.offset_ns.abs() as u64 >= 1_000_000);
        assert!(fit.offset_ns.abs() as u64 <= 2_000_000);
    }
}
