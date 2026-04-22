//! gRPC `Collector` service. Ingests `Batch` streams from agents and:
//!   * Splits `Event`s by category: METRIC → FastRing, all else → sink.
//!   * Applies clock-skew correction from agent-sent `ClockOffset`s.
//!   * Drops MetricFrames into the FastRing as aggregated points.
//!   * Emits `Ack` on every batch so agents can trim their ring.

use crate::fast_ring::{FastRing, Sample, SeriesKey};
use crate::ingest::skew::SkewTable;
use crate::live::{LiveBroadcaster, LiveKernelEvent};
use crate::replay::AgentRegistry;
use crate::store::sink::Sink;
use perfweave_proto::v1::{
    collector_server::{Collector, CollectorServer},
    event::Payload,
    Ack, AgentHello, Batch, Category, Event, RegisterAck,
};
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

pub struct CollectorSvc {
    sink: Arc<Sink>,
    fast: Arc<FastRing>,
    skew: Arc<SkewTable>,
    live: Arc<LiveBroadcaster>,
    agents: Arc<AgentRegistry>,
    next_node_id: Arc<AtomicU32>,
}

impl CollectorSvc {
    pub fn new(
        sink: Arc<Sink>,
        fast: Arc<FastRing>,
        skew: Arc<SkewTable>,
        live: Arc<LiveBroadcaster>,
        agents: Arc<AgentRegistry>,
    ) -> Self {
        Self {
            sink,
            fast,
            skew,
            live,
            agents,
            next_node_id: Arc::new(AtomicU32::new(1)),
        }
    }

    pub fn into_service(self) -> CollectorServer<Self> {
        CollectorServer::new(self)
    }
}

type AckStream = Pin<Box<dyn Stream<Item = Result<Ack, Status>> + Send>>;

#[tonic::async_trait]
impl Collector for CollectorSvc {
    async fn register(
        &self,
        req: Request<AgentHello>,
    ) -> Result<Response<RegisterAck>, Status> {
        let hello = req.into_inner();
        let assigned = if hello.node_id == 0 {
            self.next_node_id.fetch_add(1, Ordering::Relaxed)
        } else {
            hello.node_id
        };
        tracing::info!(
            hostname = hello.hostname,
            version = hello.agent_version,
            num_gpus = hello.num_gpus,
            assigned_node_id = assigned,
            agent_rpc = %hello.agent_rpc_addr,
            "agent registered"
        );
        // Remember the callback address (if any) for on-demand replay.
        self.agents.register(assigned, &hello);
        Ok(Response::new(RegisterAck {
            assigned_node_id: assigned,
            server_epoch_ns: perfweave_common::clock::host_realtime_ns(),
        }))
    }

    type IngestStream = AckStream;

    async fn ingest(
        &self,
        req: Request<Streaming<Batch>>,
    ) -> Result<Response<AckStream>, Status> {
        let mut inbound = req.into_inner();
        let sink = self.sink.clone();
        let fast = self.fast.clone();
        let skew = self.skew.clone();
        let live = self.live.clone();
        let (tx, rx) = mpsc::channel::<Result<Ack, Status>>(16);

        tokio::spawn(async move {
            let mut accepted: u64 = 0;
            let mut dropped: u64 = 0;
            while let Some(msg) = inbound.next().await {
                match msg {
                    Ok(batch) => {
                        let rows = batch.events.len() as u64;

                        // 1. Absorb any ClockOffsets so subsequent events
                        //    in *this* same batch benefit from the new
                        //    fit. Agents tend to ship their latest fit
                        //    ahead of events corrected with it.
                        for offset in &batch.offsets {
                            skew.upsert(offset);
                        }

                        // 2. Absorb MetricFrames into FastRing.
                        for frame in &batch.metric_frames {
                            let ts = skew.map(frame.node_id, frame.gpu_id, frame.ts_ns);
                            for p in &frame.points {
                                fast.push(
                                    SeriesKey {
                                        node_id: frame.node_id,
                                        gpu_id: frame.gpu_id,
                                        metric_id: p.metric_id,
                                    },
                                    Sample { ts_ns: ts, value: p.value },
                                );
                            }
                        }

                        // 3. Split events. Metric events → FastRing only.
                        //    Kernel/Memcpy/Api/Marker/etc. → preserve for
                        //    the sink (also broadcast KERNEL to /api/live/kernels).
                        let mut persisted: Vec<Event> = Vec::with_capacity(batch.events.len());
                        for mut e in batch.events {
                            // Apply skew correction as a safety net. Agents
                            // already do this for their own CUPTI events
                            // but a just-registered agent may have shipped
                            // raw timestamps before its first fit.
                            if e.category == Category::Kernel as i32
                                || e.category == Category::ApiCall as i32
                                || e.category == Category::Memcpy as i32
                            {
                                e.ts_ns = skew.map(e.node_id, e.gpu_id, e.ts_ns);
                            }

                            if e.category == Category::Metric as i32 {
                                if let Some(Payload::Metric(m)) = &e.payload {
                                    fast.push(
                                        SeriesKey {
                                            node_id: e.node_id,
                                            gpu_id: e.gpu_id,
                                            metric_id: m.metric_id,
                                        },
                                        Sample { ts_ns: e.ts_ns, value: m.value },
                                    );
                                }
                                continue;
                            }

                            if e.category == Category::Kernel as i32 {
                                live.publish_kernel(LiveKernelEvent {
                                    ts_ns: e.ts_ns,
                                    duration_ns: e.duration_ns,
                                    node_id: e.node_id,
                                    gpu_id: e.gpu_id,
                                    correlation_id: e.correlation_id,
                                    name_id: e.name_id,
                                });
                            }
                            persisted.push(e);
                        }

                        let batch_to_sink = Batch {
                            events: persisted,
                            strings: batch.strings,
                            offsets: batch.offsets,
                            metric_frames: Vec::new(),
                        };

                        match sink.try_push(batch_to_sink) {
                            Ok(()) => accepted += rows,
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                dropped += rows;
                                tracing::warn!(dropped, "sink backpressured; dropping batch");
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                tracing::error!("sink closed; terminating stream");
                                let _ = tx.send(Err(Status::unavailable("sink closed"))).await;
                                return;
                            }
                        }
                        if tx.send(Ok(Ack { accepted, dropped })).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error=%e, "agent stream error");
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
            tracing::info!(accepted, dropped, "agent stream closed cleanly");
        });

        let out: AckStream = Box::pin(ReceiverStream::new(rx));
        Ok(Response::new(out))
    }
}
