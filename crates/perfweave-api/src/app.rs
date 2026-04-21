//! HTTP router: GraphQL, timeline (Arrow IPC), imports, and static frontend.

use crate::{
    ch::{Ch, ChConfig},
    graphql::build_schema,
    imports,
    timeline,
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

#[derive(Clone)]
pub struct Services {
    pub ch: Ch,
    pub schema: crate::graphql::AppSchema,
    pub timeline: Arc<timeline::AppState>,
    pub web_dir: Option<std::path::PathBuf>,
}

pub fn make_router(services: Services) -> Router {
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
        .with_state(services.clone())
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    if let Some(dir) = services.web_dir.as_ref() {
        router = router.nest_service("/", tower_http::services::ServeDir::new(dir));
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
        let node_id: u32 = 0; // imports always use node_id=0 for now
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

pub async fn run(cfg: AppConfig) -> anyhow::Result<()> {
    let ch = Ch::new(ChConfig::default());
    let schema = build_schema(ch.clone());
    let timeline_state = Arc::new(timeline::AppState { ch: ch.clone() });

    // Spike recompute loop: every 10s, recompute the last 15 minutes of
    // spikes at the 262ms LOD. This is cheap (one INSERT ... SELECT with a
    // rolling window) and keeps the UI's spike badges live.
    let ch_bg = ch.clone();
    tokio::spawn(async move {
        let period = std::time::Duration::from_secs(10);
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let now_ns = perfweave_common::clock::host_realtime_ns();
            let start_ns = now_ns.saturating_sub(15 * 60 * 1_000_000_000);
            match crate::imports::recompute_spikes_window(&ch_bg, start_ns, now_ns).await {
                Ok(n) if n > 0 => tracing::info!(new_spikes = n, "spikes updated"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error=%e, "spike recompute failed"),
            }
        }
    });

    let services = Services {
        ch,
        schema,
        timeline: timeline_state,
        web_dir: cfg.web_dir.clone(),
    };
    let router = make_router(services);

    let addr: SocketAddr = cfg.listen.parse()?;
    tracing::info!(%addr, web_dir = ?cfg.web_dir, "api listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

pub struct AppConfig {
    pub listen: String,
    pub web_dir: Option<std::path::PathBuf>,
}
