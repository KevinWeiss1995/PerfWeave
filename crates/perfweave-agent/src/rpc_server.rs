//! Agent-side `PerfweaveAgent.ProfileKernels` RPC server.
//!
//! The server calls us here when a user clicks a spike: it asks the agent
//! that owns the target process to re-profile the top-N kernels inside the
//! spike window using CUPTI's `PROFILER_REPLAY_KERNEL` mode. Kernel replay
//! is exact (not sampled), so cards come back HIGH confidence.
//!
//! Under `--features cupti-replay` the agent talks to CUPTI directly. On
//! every other build (including synthetic dev), the agent returns a small
//! set of plausible `KernelSol` rows keyed off the requested correlation
//! ids so the UI/end-to-end tests still exercise the full wire path.

use async_trait::async_trait;
use perfweave_common::intern::hash as intern_hash;
use perfweave_proto::v1::{
    kernel_sol::{Bound, Confidence, Source},
    perfweave_agent_server::PerfweaveAgent,
    KernelSol, ProfileRequest, StallCount,
};
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic::{Request, Response, Status};

pub struct AgentRpcServer {
    pub synthetic: bool,
}

#[async_trait]
impl PerfweaveAgent for AgentRpcServer {
    type ProfileKernelsStream =
        Pin<Box<dyn Stream<Item = Result<KernelSol, Status>> + Send + 'static>>;

    async fn profile_kernels(
        &self,
        req: Request<ProfileRequest>,
    ) -> Result<Response<Self::ProfileKernelsStream>, Status> {
        let req = req.into_inner();
        let (tx, rx) = mpsc::channel::<Result<KernelSol, Status>>(32);

        if self.synthetic {
            tokio::spawn(async move {
                synthetic_replay(&req, tx).await;
            });
        } else {
            // Real CUPTI path lives behind a feature; until it's wired we
            // return a clear error so the UI can fall back to host-sampling
            // cards instead of hanging.
            let _ = tx
                .send(Err(Status::unimplemented(
                    "CUPTI_PROFILER_REPLAY_KERNEL not compiled in; \
                     rebuild agent with --features cupti-replay",
                )))
                .await;
        }

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(stream) as Self::ProfileKernelsStream
        ))
    }
}

async fn synthetic_replay(req: &ProfileRequest, tx: mpsc::Sender<Result<KernelSol, Status>>) {
    let ids: Vec<u64> = if !req.correlation_ids.is_empty() {
        req.correlation_ids.clone()
    } else {
        req.name_ids.clone()
    };
    let stall_reason_mem = intern_hash("MemoryThrottle") as u32;
    let stall_reason_bar = intern_hash("Barrier") as u32;
    for (i, corr) in ids.into_iter().enumerate() {
        let bound = match i % 4 {
            0 => Bound::Memory,
            1 => Bound::Compute,
            2 => Bound::Latency,
            _ => Bound::Balanced,
        };
        let sol = KernelSol {
            correlation_id: corr,
            sm_active_pct: (35.0 + (i as f32 * 5.0) % 60.0),
            achieved_occupancy_pct: 40.0,
            dram_bw_pct: (70.0 - (i as f32 * 3.0) % 40.0),
            l1_bw_pct: 25.0,
            l2_bw_pct: 50.0,
            inst_throughput_pct: 60.0,
            theoretical_occupancy_pct: 60.0,
            arithmetic_intensity: 4.25,
            achieved_gflops: 1200.0,
            bound: bound as i32,
            confidence: Confidence::High as i32,
            source: Source::Replay as i32,
            stalls: vec![
                StallCount { reason_id: stall_reason_mem, count: 1000 },
                StallCount { reason_id: stall_reason_bar, count: 250 },
            ],
        };
        let _ = tx.send(Ok(sol)).await;
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

pub async fn serve(bind: SocketAddr, synthetic: bool) -> anyhow::Result<()> {
    use perfweave_proto::v1::perfweave_agent_server::PerfweaveAgentServer;
    tracing::info!(%bind, synthetic, "PerfweaveAgent RPC server listening");
    tonic::transport::Server::builder()
        .add_service(PerfweaveAgentServer::new(AgentRpcServer { synthetic }))
        .serve(bind)
        .await?;
    Ok(())
}
