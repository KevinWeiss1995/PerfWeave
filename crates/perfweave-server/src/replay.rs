//! On-demand kernel-replay profiling.
//!
//! When a user clicks a spike the UI calls `/api/profile_kernels` with the
//! top-N correlation ids inside the spike window. The server fans out a
//! `PerfweaveAgent.ProfileKernels` RPC to the agent that owns the target
//! process, receives a stream of `KernelSol` messages, and persists them
//! to `kernel_sol` while echoing them back to the caller so the UI can
//! upgrade cards in place.
//!
//! The agent address is learned at `Register` time. If the agent didn't
//! advertise an RPC address (old build / disabled), we refuse the request
//! with a 409 so the UI can fall back to host-sampling cards.

use ahash::AHashMap;
use parking_lot::RwLock;
use perfweave_proto::v1::{
    perfweave_agent_client::PerfweaveAgentClient, AgentHello, KernelSol, ProfileRequest,
};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AgentEntry {
    pub node_id: u32,
    pub hostname: String,
    pub rpc_addr: Option<String>,
}

#[derive(Default)]
pub struct AgentRegistry {
    inner: RwLock<AHashMap<u32, AgentEntry>>,
}

impl AgentRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn register(&self, node_id: u32, hello: &AgentHello) {
        let rpc = if hello.agent_rpc_addr.is_empty() {
            None
        } else {
            Some(hello.agent_rpc_addr.clone())
        };
        self.inner.write().insert(
            node_id,
            AgentEntry {
                node_id,
                hostname: hello.hostname.clone(),
                rpc_addr: rpc,
            },
        );
    }

    pub fn get(&self, node_id: u32) -> Option<AgentEntry> {
        self.inner.read().get(&node_id).cloned()
    }
}

/// Connect to the agent for `node_id` and run the ProfileKernels RPC.
/// Returns a tonic `Streaming<KernelSol>` that the caller can drain.
pub async fn profile_kernels(
    registry: &AgentRegistry,
    node_id: u32,
    req: ProfileRequest,
) -> anyhow::Result<tonic::Streaming<KernelSol>> {
    let entry = registry
        .get(node_id)
        .ok_or_else(|| anyhow::anyhow!("node {node_id} not registered"))?;
    let addr = entry
        .rpc_addr
        .ok_or_else(|| anyhow::anyhow!("agent on node {node_id} did not advertise an RPC address; build with --features cupti-replay"))?;
    let mut client = PerfweaveAgentClient::connect(addr).await?;
    let resp = client.profile_kernels(req).await?;
    Ok(resp.into_inner())
}
