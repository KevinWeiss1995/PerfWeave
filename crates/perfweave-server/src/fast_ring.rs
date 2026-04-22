//! Fast in-memory metric ring for the live plane.
//!
//! Retention: 120s @ 1Hz tail + 10s @ 10Hz tail per series, keyed by
//! `(node_id, gpu_id, metric_id)`.
//!
//! Concurrency model:
//!   * Single writer task appends incoming samples into mutable ring buffers
//!     (one per series) under a cheap mutex.
//!   * Every ~100ms the writer publishes an immutable `FastRingSnapshot`
//!     into an `ArcSwap`. Readers (SSE clients, spike detector, UI polling)
//!     do a wait-free `ArcSwap::load()` and see a consistent view without
//!     ever contending with the writer on the hot path.
//!
//! This is strictly more efficient than an `RwLock<FastRing>` (no reader
//! coordination, no writer starvation) and strictly simpler than a
//! per-series lock-free ringbuffer (no ABA, no CAS loops).

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub const SLOW_HZ: u64 = 1;
pub const FAST_HZ: u64 = 10;
pub const SLOW_WINDOW_S: u64 = 120;
pub const FAST_WINDOW_S: u64 = 10;

pub const SLOW_CAPACITY: usize = (SLOW_WINDOW_S * SLOW_HZ) as usize;
pub const FAST_CAPACITY: usize = (FAST_WINDOW_S * FAST_HZ) as usize;

pub const SNAPSHOT_PERIOD: Duration = Duration::from_millis(100);

/// Key into the ring. Metric_ids are xxh3 hashes of the metric name so
/// they're already globally unique; we keep gpu/node in the key so the
/// UI can filter per-GPU without scanning.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct SeriesKey {
    pub node_id: u32,
    pub gpu_id: u32,
    pub metric_id: u64,
}

#[derive(Copy, Clone, Debug)]
pub struct Sample {
    pub ts_ns: u64,
    pub value: f64,
}

/// A simple fixed-capacity ring of `Sample`. We keep this un-generic for
/// clarity; a Vec<Sample> with manual head/len is both smaller than
/// crossbeam_queue and easier to snapshot cheaply.
#[derive(Clone, Debug)]
struct SeriesRing {
    /// `cap` entries; head advances on push; filled with zeros initially.
    slow: Vec<Sample>,
    slow_head: usize,
    slow_len: usize,

    fast: Vec<Sample>,
    fast_head: usize,
    fast_len: usize,

    /// Last sample time we accepted into the 1Hz slow ring (ns); used to
    /// decimate inputs that arrive faster than SLOW_HZ.
    last_slow_ns: u64,
}

impl SeriesRing {
    fn new() -> Self {
        Self {
            slow: vec![Sample { ts_ns: 0, value: 0.0 }; SLOW_CAPACITY],
            slow_head: 0,
            slow_len: 0,
            fast: vec![Sample { ts_ns: 0, value: 0.0 }; FAST_CAPACITY],
            fast_head: 0,
            fast_len: 0,
            last_slow_ns: 0,
        }
    }

    fn push(&mut self, s: Sample) {
        // Always record in the fast ring (it's our 10Hz live tail).
        self.fast[self.fast_head] = s;
        self.fast_head = (self.fast_head + 1) % FAST_CAPACITY;
        if self.fast_len < FAST_CAPACITY {
            self.fast_len += 1;
        }
        // Decimate to 1Hz for the slow ring. "one sample per 950ms" is
        // tolerant of slightly late samples.
        if s.ts_ns >= self.last_slow_ns + 950_000_000 {
            self.slow[self.slow_head] = s;
            self.slow_head = (self.slow_head + 1) % SLOW_CAPACITY;
            if self.slow_len < SLOW_CAPACITY {
                self.slow_len += 1;
            }
            self.last_slow_ns = s.ts_ns;
        }
    }

    fn snapshot(&self) -> SeriesSnapshot {
        let slow = read_ring(&self.slow, self.slow_head, self.slow_len);
        let fast = read_ring(&self.fast, self.fast_head, self.fast_len);
        SeriesSnapshot { slow, fast }
    }
}

fn read_ring(buf: &[Sample], head: usize, len: usize) -> Vec<Sample> {
    if len == 0 {
        return Vec::new();
    }
    let cap = buf.len();
    let start = (head + cap - len) % cap;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(buf[(start + i) % cap]);
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct SeriesSnapshot {
    /// 120s tail at ~1Hz, oldest first.
    pub slow: Vec<Sample>,
    /// 10s tail at ~10Hz, oldest first.
    pub fast: Vec<Sample>,
}

/// Immutable point-in-time view of every series. Cheap to clone (Arc).
#[derive(Debug, Default)]
pub struct FastRingSnapshot {
    pub series: HashMap<SeriesKey, SeriesSnapshot>,
    pub published_at_ns: u64,
}

/// The writer side. Only one writer task owns this; everybody else reads
/// via the ArcSwap. Cheaper than RwLock<HashMap<..>> at the same cost
/// profile.
pub struct FastRing {
    writer: Mutex<HashMap<SeriesKey, SeriesRing>>,
    snapshot: ArcSwap<FastRingSnapshot>,
}

impl FastRing {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            writer: Mutex::new(HashMap::new()),
            snapshot: ArcSwap::from_pointee(FastRingSnapshot::default()),
        })
    }

    /// Append a sample to a series. Called from ingest on every incoming
    /// MetricFrame / MetricSample. O(1) amortized.
    pub fn push(&self, key: SeriesKey, s: Sample) {
        let mut w = self.writer.lock();
        w.entry(key).or_insert_with(SeriesRing::new).push(s);
    }

    /// Called by the publisher task every SNAPSHOT_PERIOD.
    pub fn publish(&self) {
        let snap = {
            let w = self.writer.lock();
            let mut out = HashMap::with_capacity(w.len());
            for (k, ring) in w.iter() {
                out.insert(*k, ring.snapshot());
            }
            out
        };
        self.snapshot.store(Arc::new(FastRingSnapshot {
            series: snap,
            published_at_ns: perfweave_common::clock::host_realtime_ns(),
        }));
    }

    /// Wait-free read. Returns an Arc pointing at the most recently
    /// published immutable snapshot. Do NOT hold this across long async
    /// awaits (it pins memory); clone the HashMap entry you need and drop.
    pub fn snapshot(&self) -> Arc<FastRingSnapshot> {
        self.snapshot.load_full()
    }

    /// Iterate every (key, slow_tail, fast_tail) in the current snapshot.
    /// Intended for spike detection and SSE fan-out.
    pub fn for_each_series<F: FnMut(&SeriesKey, &SeriesSnapshot)>(&self, mut f: F) {
        let snap = self.snapshot.load();
        for (k, v) in snap.series.iter() {
            f(k, v);
        }
    }
}

/// Publisher task. Spawn once per process.
pub async fn spawn_publisher(ring: Arc<FastRing>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(SNAPSHOT_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { break; },
            _ = ticker.tick() => ring.publish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_push_and_snapshot() {
        let r = FastRing::new();
        let k = SeriesKey { node_id: 0, gpu_id: 0, metric_id: 42 };
        for i in 0..5u64 {
            r.push(k, Sample { ts_ns: i * 1_000_000_000, value: i as f64 });
        }
        r.publish();
        let snap = r.snapshot();
        let s = snap.series.get(&k).expect("series present");
        // 5 samples at 1s intervals — all of them survive in both rings
        // because 5 < fast capacity (100) and 5 < slow capacity (120).
        assert_eq!(s.fast.len(), 5);
        assert_eq!(s.slow.len(), 5);
        assert_eq!(s.fast.first().unwrap().value, 0.0);
        assert_eq!(s.fast.last().unwrap().value, 4.0);
    }

    #[test]
    fn slow_ring_decimates_high_rate_inputs() {
        let r = FastRing::new();
        let k = SeriesKey { node_id: 0, gpu_id: 0, metric_id: 1 };
        // 1 kHz input for 2 seconds — 2000 samples.
        for i in 0..2_000u64 {
            r.push(k, Sample { ts_ns: i * 1_000_000, value: i as f64 });
        }
        r.publish();
        let snap = r.snapshot();
        let s = snap.series.get(&k).unwrap();
        // Slow ring should have picked up only ~2 of those.
        assert!(s.slow.len() >= 2 && s.slow.len() <= 3, "got {}", s.slow.len());
        // Fast ring should be at capacity.
        assert_eq!(s.fast.len(), FAST_CAPACITY);
    }
}
