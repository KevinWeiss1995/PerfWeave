//! Agent-side MPSC ring. Samplers (possibly many, one per source) push Events
//! into this buffer; a single uploader task drains it into batches and sends
//! them over gRPC.
//!
//! We use crossbeam's bounded channel so contention is low and we get clean
//! backpressure semantics: `try_send` fails fast when full, the sampler
//! increments a dropped counter, and we never block the CUDA target app.

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use perfweave_proto::v1::Event;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub struct EventRing {
    tx: Sender<Event>,
    rx: Receiver<Event>,
    dropped: Arc<AtomicU64>,
}

impl EventRing {
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = bounded(capacity);
        Self { tx, rx, dropped: Arc::new(AtomicU64::new(0)) }
    }

    /// Fast path for samplers. Never blocks.
    pub fn push(&self, e: Event) {
        if let Err(TrySendError::Full(_)) = self.tx.try_send(e) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
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
}
