//! Uploader task: drains the event ring, applies clock correction, batches
//! events into protobuf `Batch` messages, and streams them to the collector.
//!
//! On connection loss we exponentially back off and retry; events that land
//! in the ring during the outage are preserved up to `ring_capacity` before
//! back-pressure kicks in. The ring's dropped counter is surfaced every 5s
//! in a warning log so operators see data loss immediately.

use crate::clock_sync::OffsetHandle;
use crate::ring::EventRing;
use perfweave_common::intern::Interner;
use perfweave_proto::v1::{
    collector_client::CollectorClient, AgentHello, Batch, StringIntern,
};
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;

pub struct UploaderConfig {
    pub collector_url: String,
    pub hostname: String,
    pub node_id: u32,
    pub num_gpus: u32,
    pub max_batch_events: usize,
    pub flush_interval_ms: u64,
}

pub async fn run(
    cfg: UploaderConfig,
    ring: Arc<EventRing>,
    offsets: OffsetHandle,
    interner: Interner,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff_ms: u64 = 250;
    loop {
        if *shutdown.borrow() {
            break;
        }
        tracing::info!(collector=%cfg.collector_url, "connecting to collector");
        let client = match CollectorClient::connect(cfg.collector_url.clone()).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error=%e, backoff_ms, "collector connect failed");
                sleep_with_shutdown(backoff_ms, &mut shutdown).await;
                backoff_ms = (backoff_ms * 2).min(10_000);
                continue;
            }
        };
        backoff_ms = 250;

        if let Err(e) = run_session(&cfg, client, &ring, &offsets, &interner, shutdown.clone()).await {
            tracing::warn!(error=%e, "collector session ended");
            interner.reset();
            sleep_with_shutdown(500, &mut shutdown).await;
        } else {
            break;
        }
    }
}

async fn run_session(
    cfg: &UploaderConfig,
    mut client: CollectorClient<tonic::transport::Channel>,
    ring: &EventRing,
    offsets: &OffsetHandle,
    interner: &Interner,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let hello = AgentHello {
        node_id: cfg.node_id,
        hostname: cfg.hostname.clone(),
        agent_version: perfweave_common::PRODUCT_VERSION.to_string(),
        num_gpus: cfg.num_gpus,
    };
    let ack = client.register(hello).await?.into_inner();
    tracing::info!(assigned_node_id = ack.assigned_node_id, "registered with collector");

    let (tx, rx) = tokio::sync::mpsc::channel::<Batch>(16);
    let mut stream = client.ingest(ReceiverStream::new(rx)).await?.into_inner();

    let mut flush_tick = tokio::time::interval(Duration::from_millis(cfg.flush_interval_ms));
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut dropped_tick = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { break; },
            _ = flush_tick.tick() => {
                let events = ring.drain_up_to(cfg.max_batch_events);
                if events.is_empty() { continue; }
                let off = offsets.current();
                let mut strings: Vec<StringIntern> = Vec::new();
                let corrected: Vec<_> = events.into_iter().map(|mut e| {
                    // METRIC events come from NVML (already host-time) so off=identity.
                    // KERNEL/API_CALL events from CUPTI are GPU-time and get corrected here.
                    if e.category == perfweave_proto::v1::Category::Kernel as i32
                        || e.category == perfweave_proto::v1::Category::ApiCall as i32
                        || e.category == perfweave_proto::v1::Category::Memcpy as i32
                    {
                        e.ts_ns = off.map(e.ts_ns);
                    }
                    e
                }).collect();
                // Intern any metric_unit strings we have not announced yet.
                for e in &corrected {
                    if let Some(perfweave_proto::v1::event::Payload::Metric(m)) = &e.payload {
                        if !m.unit.is_empty() {
                            let (_, rec) = interner.record(&m.unit);
                            if let Some(r) = rec { strings.push(r); }
                        }
                    }
                }
                let batch = Batch { events: corrected, strings, offsets: Vec::new() };
                if let Err(e) = tx.send(batch).await {
                    return Err(anyhow::anyhow!("collector stream closed: {e}"));
                }
            }
            msg = stream.message() => {
                match msg? {
                    Some(ack) => tracing::debug!(accepted = ack.accepted, dropped = ack.dropped, "ack"),
                    None => return Err(anyhow::anyhow!("collector closed stream")),
                }
            }
            _ = dropped_tick.tick() => {
                let d = ring.take_dropped();
                if d > 0 {
                    tracing::warn!(dropped_events = d, "ring buffer overflowed; raise --ring-capacity or reduce sampling rate");
                }
            }
        }
    }
    Ok(())
}

async fn sleep_with_shutdown(ms: u64, shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
        _ = shutdown.changed() => {}
    }
}
