//! The timeline data endpoint. Returns Arrow IPC.
//!
//! Query params:
//!   start_ns:   u64, inclusive
//!   end_ns:     u64, exclusive
//!   gpus:       optional comma list of gpu_ids
//!   categories: optional comma list of "KERNEL,MEMCPY,..."
//!   pixels:     u32, timeline width in CSS pixels (picks the LOD)
//!   kind:       "activity" | "metric" | "raw"  (default picks by pixels)

use crate::api::{
    arrow_tile::{
        activity_to_arrow, metric_to_arrow, raw_to_arrow,
        ActivityTileRows, MetricTileRows, RawEventRows,
    },
    lod::{choose_tile, should_serve_raw},
};
use crate::store::Ch;
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub ch: Ch,
}

#[derive(Deserialize, Debug)]
pub struct TimelineQuery {
    pub start_ns: u64,
    pub end_ns: u64,
    #[serde(default)]
    pub gpus: Option<String>,
    #[serde(default)]
    pub categories: Option<String>,
    #[serde(default = "default_pixels")]
    pub pixels: u32,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub metric_ids: Option<String>,
}

fn default_pixels() -> u32 { 2048 }

#[axum::debug_handler]
pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TimelineQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let range = q.end_ns.saturating_sub(q.start_ns);
    if range == 0 {
        return Err((StatusCode::BAD_REQUEST, "end_ns must be > start_ns".into()));
    }

    let kind = q.kind.as_deref().unwrap_or_else(|| {
        if should_serve_raw(range, q.pixels) { "raw" } else { "activity" }
    });

    let bytes = match kind {
        "raw" => serve_raw(&state, &q).await.map_err(ise)?,
        "metric" => serve_metric(&state, &q).await.map_err(ise)?,
        _ => serve_activity(&state, &q).await.map_err(ise)?,
    };

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/vnd.apache.arrow.stream".parse().unwrap());
    headers.insert("X-Perfweave-Kind", kind.parse().unwrap());
    Ok((headers, bytes))
}

fn ise(e: anyhow::Error) -> (StatusCode, String) {
    tracing::error!(error = %e, "timeline handler failed");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

// --------------------------------------------------------------------------
// Query shapes.
// --------------------------------------------------------------------------

#[derive(Row, Deserialize, Serialize, Debug)]
struct ActivityTileRow {
    bucket_start_ns: u64,
    bucket_width_ns: u64,
    gpu_id: u8,
    // ClickHouse returns the enum as its numeric underlying value via the
    // clickhouse crate only when declared as the integer type.
    category: u8,
    count: u64,
    sum_duration_ns: u64,
    max_duration_ns: u64,
    top_name_id: u64,
}

async fn serve_activity(state: &AppState, q: &TimelineQuery) -> anyhow::Result<Bytes> {
    let width = choose_tile(q.end_ns - q.start_ns, q.pixels).unwrap_or(4_096_000);

    let gpu_filter = gpu_filter_sql(q.gpus.as_deref());
    let cat_filter = category_filter_sql(q.categories.as_deref(), "category");
    let sql = format!(
        r#"
        SELECT
            bucket_start_ns,
            bucket_width_ns,
            gpu_id,
            toUInt8(category) AS category,
            countMerge(count) AS count,
            sumMerge(sum_duration_ns) AS sum_duration_ns,
            maxMerge(max_duration_ns) AS max_duration_ns,
            arrayElement(topKMerge(16)(top_names), 1) AS top_name_id
        FROM tiles_activity
        WHERE bucket_width_ns = {width}
          AND bucket_start_ns >= {start}
          AND bucket_start_ns <  {end}
          {gpu_filter}
          {cat_filter}
        GROUP BY bucket_start_ns, bucket_width_ns, gpu_id, category
        ORDER BY bucket_start_ns, gpu_id, category
        "#,
        width = width,
        start = q.start_ns,
        end = q.end_ns,
        gpu_filter = gpu_filter,
        cat_filter = cat_filter,
    );

    let rows: Vec<ActivityTileRow> = state.ch.client.query(&sql).fetch_all().await?;

    let mut out = ActivityTileRows::default();
    for r in rows {
        out.bucket_start_ns.push(r.bucket_start_ns);
        out.bucket_width_ns.push(r.bucket_width_ns);
        out.gpu_id.push(r.gpu_id);
        out.category.push(r.category);
        out.count.push(r.count);
        out.sum_duration_ns.push(r.sum_duration_ns);
        out.max_duration_ns.push(r.max_duration_ns);
        out.top_name_id.push(r.top_name_id);
    }
    activity_to_arrow(&out)
}

#[derive(Row, Deserialize, Serialize, Debug)]
struct MetricTileRow {
    bucket_start_ns: u64,
    bucket_width_ns: u64,
    gpu_id: u8,
    metric_id: u64,
    p50: f64,
    p99: f64,
    min: f64,
    max: f64,
}

async fn serve_metric(state: &AppState, q: &TimelineQuery) -> anyhow::Result<Bytes> {
    let width = choose_tile(q.end_ns - q.start_ns, q.pixels).unwrap_or(4_096_000);

    let gpu_filter = gpu_filter_sql(q.gpus.as_deref());
    let metric_filter = match q.metric_ids.as_deref() {
        Some(v) if !v.is_empty() => format!(" AND metric_id IN ({})", sanitize_u64_list(v)),
        _ => String::new(),
    };
    let sql = format!(
        r#"
        SELECT
            bucket_start_ns,
            bucket_width_ns,
            gpu_id,
            metric_id,
            quantileTDigestMerge(0.5)(p50_value) AS p50,
            quantileTDigestMerge(0.99)(p99_value) AS p99,
            minMerge(min_value) AS min,
            maxMerge(max_value) AS max
        FROM tiles_metric
        WHERE bucket_width_ns = {width}
          AND bucket_start_ns >= {start}
          AND bucket_start_ns <  {end}
          {gpu_filter}
          {metric_filter}
        GROUP BY bucket_start_ns, bucket_width_ns, gpu_id, metric_id
        ORDER BY bucket_start_ns, gpu_id, metric_id
        "#,
        width = width,
        start = q.start_ns,
        end = q.end_ns,
        gpu_filter = gpu_filter,
        metric_filter = metric_filter,
    );
    let rows: Vec<MetricTileRow> = state.ch.client.query(&sql).fetch_all().await?;

    let mut out = MetricTileRows::default();
    for r in rows {
        out.bucket_start_ns.push(r.bucket_start_ns);
        out.bucket_width_ns.push(r.bucket_width_ns);
        out.gpu_id.push(r.gpu_id);
        out.metric_id.push(r.metric_id);
        out.p50.push(r.p50);
        out.p99.push(r.p99);
        out.min.push(r.min);
        out.max.push(r.max);
    }
    metric_to_arrow(&out)
}

#[derive(Row, Deserialize, Serialize, Debug)]
struct RawEventDbRow {
    ts_ns: u64,
    duration_ns: u64,
    gpu_id: u8,
    category: u8,
    name_id: u64,
    correlation_id: u64,
    pid: u32,
    stream_id: u32,
}

async fn serve_raw(state: &AppState, q: &TimelineQuery) -> anyhow::Result<Bytes> {
    let cat_filter = category_filter_sql(q.categories.as_deref(), "category");
    let gpu_filter = gpu_filter_sql(q.gpus.as_deref());
    let sql = format!(
        r#"
        SELECT ts_ns, duration_ns, gpu_id, toUInt8(category) AS category,
               name_id, correlation_id, pid, stream_id
        FROM events
        WHERE ts_ns >= {start}
          AND ts_ns <  {end}
          {gpu_filter}
          {cat_filter}
          AND category != 'METRIC'
        ORDER BY ts_ns
        LIMIT 200000
        "#,
        start = q.start_ns,
        end = q.end_ns,
        gpu_filter = gpu_filter,
        cat_filter = cat_filter,
    );
    let rows: Vec<RawEventDbRow> = state.ch.client.query(&sql).fetch_all().await?;
    let mut out = RawEventRows::default();
    for r in rows {
        out.ts_ns.push(r.ts_ns);
        out.duration_ns.push(r.duration_ns);
        out.gpu_id.push(r.gpu_id);
        out.category.push(r.category);
        out.name_id.push(r.name_id);
        out.correlation_id.push(r.correlation_id);
        out.pid.push(r.pid);
        out.stream_id.push(r.stream_id);
    }
    raw_to_arrow(&out)
}

fn gpu_filter_sql(gpus: Option<&str>) -> String {
    match gpus {
        Some(v) if !v.is_empty() => format!(" AND gpu_id IN ({})", sanitize_u32_list(v)),
        _ => String::new(),
    }
}

fn category_filter_sql(cats: Option<&str>, col: &str) -> String {
    match cats {
        Some(v) if !v.is_empty() => {
            let allowed = v
                .split(',')
                .map(|s| s.trim().to_ascii_uppercase())
                .filter(|s| {
                    matches!(
                        s.as_str(),
                        "METRIC" | "API_CALL" | "KERNEL" | "MEMCPY" | "MEMSET" | "SYNC"
                            | "OVERHEAD" | "MARKER"
                    )
                })
                .map(|s| format!("'{}'", s))
                .collect::<Vec<_>>()
                .join(",");
            if allowed.is_empty() {
                String::new()
            } else {
                format!(" AND {col} IN ({allowed})")
            }
        }
        _ => String::new(),
    }
}

fn sanitize_u32_list(s: &str) -> String {
    s.split(',')
        .filter_map(|p| p.trim().parse::<u32>().ok())
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn sanitize_u64_list(s: &str) -> String {
    s.split(',')
        .filter_map(|p| p.trim().parse::<u64>().ok())
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",")
}
