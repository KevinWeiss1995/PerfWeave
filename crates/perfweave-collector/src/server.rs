//! gRPC server implementing the `Collector` service. Agents open a
//! bidirectional stream; the server drains batches into the ClickHouse sink
//! and periodically sends an `Ack` with accepted/dropped counts so the agent
//! can trim its ring buffer and surface dropped counts in `perfweave doctor`.

use crate::clickhouse_sink::Sink;
use perfweave_proto::v1::{
    collector_server::{Collector, CollectorServer},
    Ack, AgentHello, Batch, RegisterAck,
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
    next_node_id: Arc<AtomicU32>,
}

impl CollectorSvc {
    pub fn new(sink: Arc<Sink>) -> Self {
        Self { sink, next_node_id: Arc::new(AtomicU32::new(1)) }
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
            "agent registered"
        );
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
        let (tx, rx) = mpsc::channel::<Result<Ack, Status>>(16);

        tokio::spawn(async move {
            let mut accepted: u64 = 0;
            let mut dropped: u64 = 0;
            while let Some(msg) = inbound.next().await {
                match msg {
                    Ok(batch) => {
                        let rows = batch.events.len() as u64;
                        match sink.try_push(batch) {
                            Ok(()) => accepted += rows,
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                dropped += rows;
                                tracing::warn!(dropped, "sink backpressured; dropping batch");
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
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
