//! In-process spike detector over the FastRing.
//!
//! Two-pass design to keep steady-state cost negligible:
//!   1. Per series we maintain a Welford-style running (mean, variance)
//!      updated every tick in O(1). If |z| = |x - mean| / sigma > 3.0 we
//!      escalate to pass 2.
//!   2. Pass 2 recomputes robust statistics (median + MAD) on the last
//!      60s of the slow tail. A spike fires when |x - median| / MAD > 3.5.
//!
//! Rationale: gaussian z-score is cheap but blind to long-tailed noise.
//! MAD is robust but O(n log n). Running MAD on every tick for every
//! series would cost more than the downstream work. The hybrid gives us
//! MAD-quality detections at Welford cost.

use crate::fast_ring::{FastRing, SeriesKey, SeriesSnapshot};
use crate::live::{LiveBroadcaster, LiveFrame};
use crate::store::sink::{Sink, SpikeRow};
use ahash::AHashMap;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

#[derive(Copy, Clone, Debug, Default)]
struct Welford {
    n: u64,
    mean: f64,
    m2: f64,
}

impl Welford {
    fn update(&mut self, x: f64) {
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        let delta2 = x - self.mean;
        self.m2 += delta * delta2;
    }
    fn sigma(&self) -> f64 {
        if self.n < 2 { return 0.0; }
        (self.m2 / (self.n - 1) as f64).sqrt()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SpikeEvent {
    pub bucket_start_ns: u64,
    pub bucket_width_ns: u64,
    pub node_id: u32,
    pub gpu_id: u32,
    pub metric_id: u64,
    pub value: f64,
    pub median: f64,
    pub mad: f64,
    pub z_mad: f64,
}

const WELFORD_Z_THRESHOLD: f64 = 3.0;
const MAD_Z_THRESHOLD: f64 = 3.5;
const DETECT_PERIOD: Duration = Duration::from_millis(1000);

pub struct SpikeDetector {
    ring: Arc<FastRing>,
    sink: Arc<Sink>,
    live: Arc<LiveBroadcaster>,
}

impl SpikeDetector {
    pub fn new(ring: Arc<FastRing>, sink: Arc<Sink>, live: Arc<LiveBroadcaster>) -> Self {
        Self { ring, sink, live }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut welford: AHashMap<SeriesKey, Welford> = AHashMap::new();
        let mut last_fired_ts: AHashMap<SeriesKey, u64> = AHashMap::new();
        let mut ticker = tokio::time::interval(DETECT_PERIOD);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.changed() => if *shutdown.borrow() { break; },
                _ = ticker.tick() => {
                    self.tick_once(&mut welford, &mut last_fired_ts).await;
                }
            }
        }
    }

    async fn tick_once(
        &self,
        welford: &mut AHashMap<SeriesKey, Welford>,
        last_fired_ts: &mut AHashMap<SeriesKey, u64>,
    ) {
        // Gather (key, value, snapshot) triples up front so we don't hold
        // the ArcSwap guard across await.
        let mut work: Vec<(SeriesKey, f64, SeriesSnapshot)> = Vec::new();
        self.ring.for_each_series(|k, snap| {
            if let Some(latest) = snap.fast.last().copied() {
                work.push((*k, latest.value, snap.clone()));
            }
        });

        for (k, x, snap) in work {
            let w = welford.entry(k).or_default();
            w.update(x);
            let sigma = w.sigma();
            let z = if sigma > 0.0 { (x - w.mean).abs() / sigma } else { 0.0 };
            if z <= WELFORD_Z_THRESHOLD || w.n < 16 {
                continue;
            }
            // Escalate to MAD on the slow tail.
            let (median, mad) = median_and_mad(&snap);
            if mad <= 0.0 {
                continue;
            }
            let z_mad = (x - median).abs() / mad;
            if z_mad < MAD_Z_THRESHOLD {
                continue;
            }
            // Rate-limit: one spike per series per 10s window so a single
            // long excursion doesn't fan out into a flood.
            let latest_ts = snap.fast.last().map(|s| s.ts_ns).unwrap_or(0);
            if let Some(prev) = last_fired_ts.get(&k) {
                if latest_ts.saturating_sub(*prev) < 10_000_000_000 {
                    continue;
                }
            }
            last_fired_ts.insert(k, latest_ts);

            let ev = SpikeEvent {
                bucket_start_ns: latest_ts,
                bucket_width_ns: 1_000_000_000, // 1s "live" bucket
                node_id: k.node_id,
                gpu_id: k.gpu_id,
                metric_id: k.metric_id,
                value: x,
                median,
                mad,
                z_mad,
            };
            tracing::info!(
                node = k.node_id,
                gpu = k.gpu_id,
                metric = k.metric_id,
                value = x,
                z_mad = z_mad,
                "spike detected"
            );
            // Persist + broadcast.
            let row = SpikeRow {
                bucket_start_ns: ev.bucket_start_ns,
                bucket_width_ns: ev.bucket_width_ns,
                node_id: ev.node_id,
                gpu_id: ev.gpu_id as u8,
                metric_id: ev.metric_id,
                value: ev.value,
                median: ev.median,
                mad: ev.mad,
                z_mad: ev.z_mad,
            };
            if let Err(e) = self.sink.push_spike(row).await {
                tracing::warn!(error=%e, "failed to persist spike");
            }
            self.live.publish_frame(LiveFrame::Spike(ev));
        }
    }
}

fn median_and_mad(snap: &SeriesSnapshot) -> (f64, f64) {
    if snap.slow.is_empty() {
        return (0.0, 0.0);
    }
    let mut values: Vec<f64> = snap.slow.iter().map(|s| s.value).collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = values[values.len() / 2];
    let mut dev: Vec<f64> = values.iter().map(|v| (v - median).abs()).collect();
    dev.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mad = dev[dev.len() / 2];
    // 1.4826 converts MAD to an estimator of σ under gaussian assumption.
    (median, mad * 1.4826)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fast_ring::{Sample, SeriesSnapshot};

    #[test]
    fn welford_z_spots_outlier() {
        let mut w = Welford::default();
        for _ in 0..32 {
            w.update(10.0);
        }
        w.update(10.01); // mostly clean
        let z = (100.0_f64 - w.mean).abs() / w.sigma().max(1e-9);
        assert!(z > 10.0);
    }

    #[test]
    fn mad_ignores_big_single_point() {
        let mut snap = SeriesSnapshot::default();
        for i in 0..120 {
            snap.slow.push(Sample { ts_ns: (i as u64) * 1_000_000_000, value: 10.0 });
        }
        // Inject a single 9999 deep in the slow buffer.
        snap.slow[42].value = 9999.0;
        let (median, mad) = median_and_mad(&snap);
        assert!((median - 10.0).abs() < 1e-6);
        // Single outlier can't move the median and shouldn't dominate MAD.
        assert!(mad.abs() < 1e-6, "mad {mad}");
    }
}
