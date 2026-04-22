//! Agent-side MPSC ring. Samplers (possibly many, one per source) push Events
//! into this buffer; a single uploader task drains it into batches and sends
//! them over gRPC.
//!
//! We use crossbeam's bounded channel so contention is low and we get clean
//! backpressure semantics: `try_send` fails fast when full, the sampler
//! increments a dropped counter, and we never block the CUDA target app.
//!
//! There are two lanes on the same ring:
//!   * `events`        — high-rate, per-event stream (kernel launches, API
//!                        calls, memcpy, markers, and any remaining Metric
//!                        events emitted by non-framed samplers).
//!   * `metric_frames` — aggregated per-(gpu, tick) metric samples. One
//!                        frame carries every NVML/Tegra metric for one GPU
//!                        at one timestamp so we pay per-frame overhead
//!                        instead of per-sample overhead on the gRPC path.

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use perfweave_proto::v1::{Event, MetricFrame};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub struct EventRing {
    tx: Sender<Event>,
    rx: Receiver<Event>,
    frame_tx: Sender<MetricFrame>,
    frame_rx: Receiver<MetricFrame>,
    dropped: Arc<AtomicU64>,
    dropped_frames: Arc<AtomicU64>,
}

impl EventRing {
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = bounded(capacity);
        // Frames are MUCH lower cardinality than Events (1 per gpu per tick).
        // A 1/32 capacity is plenty and keeps the agent's memory footprint
        // essentially unchanged.
        let (frame_tx, frame_rx) = bounded(capacity / 32 + 256);
        Self {
            tx,
            rx,
            frame_tx,
            frame_rx,
            dropped: Arc::new(AtomicU64::new(0)),
            dropped_frames: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Fast path for samplers. Never blocks.
    pub fn push(&self, e: Event) {
        if let Err(TrySendError::Full(_)) = self.tx.try_send(e) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Push an aggregated metric frame (preferred path for NVML/Tegra).
    pub fn push_frame(&self, f: MetricFrame) {
        if let Err(TrySendError::Full(_)) = self.frame_tx.try_send(f) {
            self.dropped_frames.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drain up to `max` events in one call.
    pub fn drain_up_to(&self, max: usize) -> Vec<Event> {
        let mut out = Vec::with_capacity(max);
        while out.len() < max {
            match self.rx.try_recv() {
                Ok(e) => out.push(e),
                Err(_) => break,
            }
        }
        out
    }

    /// Drain every metric frame currently queued. Frames are small and low
    /// rate; we always send them all in the next batch.
    pub fn drain_frames(&self) -> Vec<MetricFrame> {
        let mut out = Vec::new();
        while let Ok(f) = self.frame_rx.try_recv() {
            out.push(f);
        }
        out
    }

    /// Block until at least one event is available or the channel is empty.
    pub async fn wait_for_one(&self) {
        let rx = self.rx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = rx.recv();
        })
        .await
        .ok();
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }

    pub fn take_dropped_frames(&self) -> u64 {
        self.dropped_frames.swap(0, Ordering::Relaxed)
    }
}
