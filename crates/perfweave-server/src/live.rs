//! Live plane: SSE endpoints and broadcast channels for the UI.
//!
//! Two streams:
//!   * `/api/live` — 1 Hz metric rollup per GPU + new-spike notifications.
//!   * `/api/live/kernels` — each KERNEL event, one message per row.
//!
//! Frame schema is JSON for MVP but designed to be binary-friendly (fixed
//! field layout, integer ids) so a WebSocket upgrade to a packed binary
//! frame is a drop-in.

use crate::fast_ring::FastRing;
use crate::spike_detect::SpikeEvent;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::IntoResponse;
use futures::stream::{Stream, StreamExt};
use serde::Serialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Clone, Debug, Serialize)]
pub struct LiveKernelEvent {
    pub ts_ns: u64,
    pub duration_ns: u64,
    pub node_id: u32,
    pub gpu_id: u32,
    pub correlation_id: u64,
    pub name_id: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum LiveFrame {
    Metric(MetricFrame),
    Spike(SpikeEvent),
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricFrame {
    pub ts_ns: u64,
    pub gpus: Vec<GpuFrame>,
}

#[derive(Clone, Debug, Serialize)]
pub struct GpuFrame {
    pub node_id: u32,
    pub gpu_id: u32,
    pub metrics: Vec<MetricPoint>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricPoint {
    pub metric_id: u64,
    pub latest: f64,
}

pub struct LiveBroadcaster {
    pub metrics: broadcast::Sender<LiveFrame>,
    pub kernels: broadcast::Sender<LiveKernelEvent>,
}

impl LiveBroadcaster {
    pub fn new() -> Arc<Self> {
        let (metrics, _) = broadcast::channel(1024);
        let (kernels, _) = broadcast::channel(8192);
        Arc::new(Self { metrics, kernels })
    }

    pub fn publish_frame(&self, f: LiveFrame) {
        let _ = self.metrics.send(f);
    }

    pub fn publish_kernel(&self, k: LiveKernelEvent) {
        let _ = self.kernels.send(k);
    }

    pub fn subscribe_metrics(&self) -> broadcast::Receiver<LiveFrame> {
        self.metrics.subscribe()
    }

    pub fn subscribe_kernels(&self) -> broadcast::Receiver<LiveKernelEvent> {
        self.kernels.subscribe()
    }
}

/// Background task that polls the FastRing every second and publishes a
/// compact MetricFrame to the broadcast channel. Running on the server
/// side (not per-connection) means N SSE clients cost N mpsc hops, not N
/// FastRing scans.
pub async fn spawn_metric_ticker(
    ring: Arc<FastRing>,
    bc: Arc<LiveBroadcaster>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(1000));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { break; },
            _ = ticker.tick() => {
                let frame = build_metric_frame(&ring);
                bc.publish_frame(LiveFrame::Metric(frame));
            }
        }
    }
}

fn build_metric_frame(ring: &FastRing) -> MetricFrame {
    // Group by (node, gpu); within each group emit one MetricPoint per
    // series with the latest value.
    use std::collections::HashMap;
    let snap = ring.snapshot();
    let mut by_gpu: HashMap<(u32, u32), Vec<MetricPoint>> = HashMap::new();
    for (k, v) in snap.series.iter() {
        let latest = v
            .fast
            .last()
            .copied()
            .or_else(|| v.slow.last().copied())
            .map(|s| s.value)
            .unwrap_or(0.0);
        by_gpu
            .entry((k.node_id, k.gpu_id))
            .or_default()
            .push(MetricPoint {
                metric_id: k.metric_id,
                latest,
            });
    }
    let mut gpus: Vec<GpuFrame> = by_gpu
        .into_iter()
        .map(|((node_id, gpu_id), metrics)| GpuFrame {
            node_id,
            gpu_id,
            metrics,
        })
        .collect();
    gpus.sort_by_key(|g| (g.node_id, g.gpu_id));
    MetricFrame {
        ts_ns: snap.published_at_ns,
        gpus,
    }
}

// ---------------------------------------------------------------------------
// Axum handlers.
// ---------------------------------------------------------------------------

pub async fn sse_live(
    axum::extract::State(bc): axum::extract::State<Arc<LiveBroadcaster>>,
) -> impl IntoResponse {
    let stream = BroadcastStream::new(bc.subscribe_metrics()).filter_map(|r| async move {
        match r {
            Ok(frame) => Some(frame_to_sse(&frame)),
            Err(_) => None, // lagged; drop this tick, keep connection
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

pub async fn sse_live_kernels(
    axum::extract::State(bc): axum::extract::State<Arc<LiveBroadcaster>>,
) -> impl IntoResponse {
    let stream = BroadcastStream::new(bc.subscribe_kernels()).filter_map(|r| async move {
        match r {
            Ok(k) => {
                let json = serde_json::to_string(&k).unwrap_or_default();
                Some(Ok::<_, Infallible>(SseEvent::default().event("kernel").data(json)))
            }
            Err(_) => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn frame_to_sse(frame: &LiveFrame) -> Result<SseEvent, Infallible> {
    let (event, data) = match frame {
        LiveFrame::Metric(m) => ("metric", serde_json::to_string(m).unwrap_or_default()),
        LiveFrame::Spike(s) => ("spike", serde_json::to_string(s).unwrap_or_default()),
    };
    Ok(SseEvent::default().event(event).data(data))
}

/// Helper that assembles the router fragment so `api::app` doesn't need to
/// know our internal topology.
pub fn router(bc: Arc<LiveBroadcaster>) -> axum::Router {
    use axum::routing::get;
    axum::Router::new()
        .route("/api/live", get(sse_live))
        .route("/api/live/kernels", get(sse_live_kernels))
        .with_state(bc)
}

/// Stream adapter for the compiler: broadcast receiver → axum SSE stream.
#[allow(dead_code)]
fn _assert_stream<T: Stream<Item = Result<SseEvent, Infallible>> + Send + 'static>(_t: T) {}
