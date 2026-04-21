//! The CUPTI integration does not live here — CUPTI must be loaded into the
//! target CUDA process via `LD_PRELOAD`, not the agent. The real
//! `cupti_*` FFI lives in the `perfweave-cupti-inject` crate, which compiles
//! to `libperfweave_cupti_inject.so` and ships activity records into the
//! agent over a Unix domain socket.
//!
//! This module exists so the feature gate compiles and so the agent can
//! spawn a Unix-socket receiver that merges CUPTI events into the main ring.

use super::Sampler;
use crate::ring::EventRing;
use async_trait::async_trait;
use std::sync::Arc;

pub struct CuptiReceiver {
    pub socket_path: std::path::PathBuf,
}

#[async_trait]
impl Sampler for CuptiReceiver {
    async fn run(
        self: Box<Self>,
        ring: Arc<EventRing>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        tracing::info!(socket=?self.socket_path, "CUPTI receiver listening");
        // Wire format: length-prefixed protobuf `Batch` messages, one per
        // CUPTI flush. See `perfweave-cupti-inject/src/transport.rs`.
        let listener = match tokio::net::UnixListener::bind(&self.socket_path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error=%e, path=?self.socket_path, "failed to bind CUPTI socket");
                return;
            }
        };
        loop {
            tokio::select! {
                _ = shutdown.changed() => if *shutdown.borrow() { break; },
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            let ring = ring.clone();
                            tokio::spawn(handle_conn(stream, ring));
                        }
                        Err(e) => tracing::warn!(error=%e, "CUPTI accept failed"),
                    }
                }
            }
        }
    }
}

async fn handle_conn(mut stream: tokio::net::UnixStream, ring: Arc<EventRing>) {
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    let mut payload = Vec::<u8>::with_capacity(64 * 1024);
    loop {
        if stream.read_exact(&mut len_buf).await.is_err() {
            return;
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > 64 * 1024 * 1024 {
            tracing::warn!(len, "oversized CUPTI batch; dropping connection");
            return;
        }
        payload.resize(len, 0);
        if stream.read_exact(&mut payload).await.is_err() {
            return;
        }
        use prost::Message;
        match perfweave_proto::v1::Batch::decode(payload.as_slice()) {
            Ok(b) => {
                for e in b.events {
                    ring.push(e);
                }
            }
            Err(e) => {
                tracing::warn!(error=%e, "CUPTI batch decode failed");
            }
        }
    }
}
