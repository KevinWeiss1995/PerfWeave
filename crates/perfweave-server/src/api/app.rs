//! HTTP router + top-level `run()` that wires up the whole server:
//!  - ingest (gRPC) on 7700
//!  - HTTP/GraphQL/Arrow-IPC/SSE on 7777 (default)
//!  - FastRing publisher task
//!  - SSE metric ticker
//!  - In-process spike detector
//!
//! One binary, one process, one FastRing. No IPC between collector and
//! API — they're the same program now.

use crate::{
    fast_ring::{spawn_publisher, FastRing},
    ingest::{grpc::CollectorSvc, skew::SkewTable},
    live::{spawn_metric_ticker, sse_live, sse_live_kernels, LiveBroadcaster},
    replay::AgentRegistry,
    spike_detect::SpikeDetector,
    store::{
        ch::{Ch, ChConfig},
        migrations,
        sink::{Sink, SinkConfig},
    },
    api::{graphql::build_schema, imports, timeline},
};
use async_graphql::http::{playground_source, GraphQLPlaygroundConfig};
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::{
    extract::{DefaultBodyLimit, Multipart, State},
    http::{HeaderValue, Method, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use std::{net::SocketAddr, sync::Arc};
use tower_http::{cors::CorsLayer, trace::TraceLayer};

/// Top-level per-request state for the HTTP router.
#[derive(Clone)]
pub struct Services {
    pub ch: Ch,
    pub schema: crate::api::graphql::AppSchema,
    pub timeline: Arc<timeline::AppState>,
    pub web_dir: Option<std::path::PathBuf>,
}

pub fn make_router(services: Services, live: Arc<LiveBroadcaster>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin("*".parse::<HeaderValue>().unwrap())
        .allow_methods([Method::GET, Method::POST])
        .allow_headers(tower_http::cors::Any);

    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/graphql", post(graphql_handler))
        .route("/graphql", get(graphql_playground))
        .route(
            "/api/timeline",
            get({
                let tl = services.timeline.clone();
                move |q| timeline_handler(tl.clone(), q)
            }),
        )
        .route(
            "/api/import/nsys",
            post(nsys_import_handler).layer(DefaultBodyLimit::max(8 * 1024 * 1024 * 1024)),
        )
        .with_state(services.clone());

    // Nest the live SSE routes with their own state (Arc<LiveBroadcaster>).
    let live_router = Router::new()
        .route("/api/live", get(sse_live))
        .route("/api/live/kernels", get(sse_live_kernels))
        .with_state(live);
    router = router.merge(live_router);

    router = router.layer(cors).layer(TraceLayer::new_for_http());

    if let Some(dir) = services.web_dir.as_ref() {
        // axum 0.8 forbids `nest_service("/", ...)`. `fallback_service` is
        // the documented replacement: any route not matched above falls
        // through to ServeDir, which resolves `/` to `index.html`.
        router = router.fallback_service(
            tower_http::services::ServeDir::new(dir)
                .append_index_html_on_directories(true),
        );
    }
    router
}

async fn healthz() -> Json<HealthResp> {
    Json(HealthResp { status: "ok".into(), version: perfweave_common::PRODUCT_VERSION.into() })
}

#[derive(Serialize)]
struct HealthResp { status: String, version: String }

async fn graphql_handler(State(s): State<Services>, req: GraphQLRequest) -> GraphQLResponse {
    s.schema.execute(req.into_inner()).await.into()
}

async fn graphql_playground() -> impl IntoResponse {
    Html(playground_source(GraphQLPlaygroundConfig::new("/graphql")))
}

async fn timeline_handler(
    state: Arc<timeline::AppState>,
    q: axum::extract::Query<timeline::TimelineQuery>,
) -> impl IntoResponse {
    timeline::handler(axum::extract::State(state), q).await
}

async fn nsys_import_handler(
    State(s): State<Services>,
    mut mp: Multipart,
) -> Result<Json<ImportAck>, (StatusCode, String)> {
    while let Some(field) = mp.next_field().await.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))? {
        let filename = field.file_name().map(|s| s.to_string()).unwrap_or_else(|| "upload.nsys-rep".into());
        let data = field.bytes().await.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        let tmp = std::env::temp_dir().join(format!("{}-{filename}", uuid_like()));
        std::fs::write(&tmp, &data).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let ch = s.ch.clone();
        let node_id: u32 = 0;
        tokio::spawn(async move {
            if let Err(e) = imports::import_nsys(&ch, &tmp, node_id).await {
                tracing::error!(error=%e, file=?tmp, "nsys import failed");
            }
            let _ = std::fs::remove_file(&tmp);
        });
        return Ok(Json(ImportAck { queued: true }));
    }
    Err((StatusCode::BAD_REQUEST, "expected a multipart file upload".into()))
}

#[derive(Serialize)]
struct ImportAck { queued: bool }

fn uuid_like() -> String {
    let ns = perfweave_common::clock::host_realtime_ns();
    format!("{:x}", ns)
}

pub struct AppConfig {
    /// HTTP/GraphQL/SSE listen address. Default 0.0.0.0:7777.
    pub http_listen: String,
    /// gRPC ingest listen address. Default 0.0.0.0:7700.
    pub grpc_listen: String,
    /// Optional web bundle dir to serve as static assets.
    pub web_dir: Option<std::path::PathBuf>,
    /// ClickHouse connection. If `None`, takes it from env/defaults.
    pub ch: Option<ChConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            http_listen: "0.0.0.0:7777".into(),
            grpc_listen: "0.0.0.0:7700".into(),
            web_dir: None,
            ch: None,
        }
    }
}

/// Start the merged ingest + API server.
///
/// Fires up, in order:
///   1. ClickHouse + migrations (fails fast if unreachable).
///   2. Sink task (async batched writer).
///   3. FastRing + publisher task.
///   4. Live broadcaster + metric ticker.
///   5. Spike detector.
///   6. gRPC ingest on `grpc_listen`.
///   7. HTTP/GraphQL/SSE on `http_listen`.
pub async fn run(cfg: AppConfig) -> anyhow::Result<()> {
    let ch = Ch::new(cfg.ch.clone().unwrap_or_default());
    migrations::apply_all(&ch).await?;

    let sink = Arc::new(Sink::spawn(SinkConfig::default())?);
    let fast = FastRing::new();
    let skew = Arc::new(SkewTable::default());
    let live = LiveBroadcaster::new();
    let agents = AgentRegistry::new();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Background tasks.
    tokio::spawn({
        let fast = fast.clone();
        let rx = shutdown_rx.clone();
        async move { spawn_publisher(fast, rx).await }
    });
    tokio::spawn({
        let fast = fast.clone();
        let live = live.clone();
        let rx = shutdown_rx.clone();
        async move { spawn_metric_ticker(fast, live, rx).await }
    });
    tokio::spawn({
        let det = SpikeDetector::new(fast.clone(), sink.clone(), live.clone());
        let rx = shutdown_rx.clone();
        async move { det.run(rx).await }
    });

    // gRPC ingest.
    let grpc_addr: SocketAddr = cfg.grpc_listen.parse()?;
    tokio::spawn({
        let sink = sink.clone();
        let fast = fast.clone();
        let skew = skew.clone();
        let live = live.clone();
        let agents = agents.clone();
        async move {
            let svc = CollectorSvc::new(sink, fast, skew, live, agents).into_service();
            tracing::info!(%grpc_addr, "grpc ingest listening");
            if let Err(e) = tonic::transport::Server::builder()
                .add_service(svc)
                .serve(grpc_addr)
                .await
            {
                tracing::error!(error=%e, "grpc server exited");
            }
        }
    });

    // HTTP + GraphQL + SSE.
    let schema = build_schema(ch.clone(), sink.clone(), agents.clone());
    let timeline_state = Arc::new(timeline::AppState { ch: ch.clone() });
    let services = Services { ch, schema, timeline: timeline_state, web_dir: cfg.web_dir.clone() };
    let router = make_router(services, live.clone());

    let http_addr: SocketAddr = cfg.http_listen.parse()?;
    tracing::info!(%http_addr, web_dir = ?cfg.web_dir, "http listening");
    let listener = tokio::net::TcpListener::bind(http_addr).await?;

    // Handle SIGTERM / Ctrl+C.
    let server = async move {
        axum::serve(listener, router).await
    };
    let sig = async {
        tokio::signal::ctrl_c().await.ok();
    };
    tokio::select! {
        r = server => { r?; }
        _ = sig => {
            tracing::info!("shutdown signal received");
            let _ = shutdown_tx.send(true);
        }
    }
    Ok(())
}
