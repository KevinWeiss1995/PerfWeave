//! Batched writer into the `events` and `strings` tables.
//!
//! One `Sink` instance owns the ClickHouse client. Agents push batches
//! through an mpsc channel; the sink flushes every `max_batch_ms` or when
//! `max_batch_rows` is reached, whichever comes first. Back-pressure is
//! communicated to the gRPC handler by letting the mpsc queue fill up.

use anyhow::{Context, Result};
use clickhouse::{Client, Row};
use perfweave_proto::v1::{Batch, Category};
use serde::Serialize;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct SinkConfig {
    pub url: String,
    pub user: String,
    pub password: String,
    pub database: String,
    pub max_batch_rows: usize,
    pub max_batch_ms: u64,
}

impl Default for SinkConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:8123".into(),
            user: "default".into(),
            password: String::new(),
            database: "default".into(),
            max_batch_rows: 50_000,
            max_batch_ms: 500,
        }
    }
}

pub struct Sink {
    tx: mpsc::Sender<Batch>,
}

impl Sink {
    pub fn spawn(cfg: SinkConfig) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Batch>(64);
        tokio::spawn(run(cfg, rx));
        Ok(Self { tx })
    }

    pub async fn push(&self, b: Batch) -> Result<()> {
        self.tx.send(b).await.context("sink channel closed; collector is shutting down")
    }

    /// Non-blocking try_push used for the fast path.
    pub fn try_push(&self, b: Batch) -> std::result::Result<(), mpsc::error::TrySendError<Batch>> {
        self.tx.try_send(b)
    }
}

async fn run(cfg: SinkConfig, mut rx: mpsc::Receiver<Batch>) {
    let client = build_client(&cfg);

    let mut pending_events: Vec<EventRow> = Vec::with_capacity(cfg.max_batch_rows);
    let mut pending_strings: Vec<StringRow> = Vec::new();
    let mut pending_offsets: Vec<OffsetRow> = Vec::new();

    let mut ticker = tokio::time::interval(Duration::from_millis(cfg.max_batch_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(batch) = msg else { break };
                absorb(batch, &mut pending_events, &mut pending_strings, &mut pending_offsets);
                if pending_events.len() >= cfg.max_batch_rows {
                    flush(&client, &mut pending_events, &mut pending_strings, &mut pending_offsets).await;
                }
            }
            _ = ticker.tick() => {
                if !pending_events.is_empty() || !pending_strings.is_empty() || !pending_offsets.is_empty() {
                    flush(&client, &mut pending_events, &mut pending_strings, &mut pending_offsets).await;
                }
            }
        }
    }
    // Final flush before shutdown.
    flush(&client, &mut pending_events, &mut pending_strings, &mut pending_offsets).await;
}

fn build_client(cfg: &SinkConfig) -> Client {
    Client::default()
        .with_url(&cfg.url)
        .with_user(&cfg.user)
        .with_password(&cfg.password)
        .with_database(&cfg.database)
}

fn absorb(
    b: Batch,
    events: &mut Vec<EventRow>,
    strings: &mut Vec<StringRow>,
    offsets: &mut Vec<OffsetRow>,
) {
    for e in b.events {
        events.push(EventRow::from(e));
    }
    for s in b.strings {
        strings.push(StringRow { id: s.id, text: s.text });
    }
    for o in b.offsets {
        offsets.push(OffsetRow {
            fitted_at_ns: perfweave_common::clock::host_realtime_ns(),
            node_id: o.node_id,
            gpu_id: o.gpu_id as u8,
            offset_ns: o.offset_ns as i64,
            slope_num: o.slope_num as i64,
            slope_den: o.slope_den as i64,
            residual_max_ns: o.residual_max_ns,
        });
    }
}

async fn flush(
    client: &Client,
    events: &mut Vec<EventRow>,
    strings: &mut Vec<StringRow>,
    offsets: &mut Vec<OffsetRow>,
) {
    if !strings.is_empty() {
        if let Err(e) = insert(client, "strings", strings).await {
            tracing::error!(error=%e, "failed to insert strings; retrying on next tick");
            // leave in buffer; do not drop silently
            return;
        }
    }
    if !events.is_empty() {
        match insert(client, "events", events).await {
            Ok(()) => {}
            Err(e) => {
                tracing::error!(error=%e, rows=events.len(), "failed to insert events batch");
                return;
            }
        }
    }
    if !offsets.is_empty() {
        if let Err(e) = insert(client, "clock_offsets", offsets).await {
            tracing::error!(error=%e, "failed to insert clock_offsets");
            return;
        }
    }
    events.clear();
    strings.clear();
    offsets.clear();
}

async fn insert<T: Row + Serialize>(
    client: &Client,
    table: &str,
    rows: &[T],
) -> Result<()> {
    let mut insert = client.insert(table)?;
    for r in rows {
        insert.write(r).await?;
    }
    insert.end().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Row definitions. Keep fields in the SAME order as the CREATE TABLE columns
// in migrations/0001_events.sql — the clickhouse-rs crate writes columns in
// the order derived from the Row impl.
// ---------------------------------------------------------------------------

// IMPORTANT: `category` and `memcpy_kind` are `Enum8` in ClickHouse. In
// the RowBinary wire format an Enum8 is a single signed byte (the
// discriminant). Serializing them as `String` here causes silent column
// misalignment that eventually explodes as
// `Too large string size: ... (TOO_LARGE_STRING_SIZE)` when enough drift
// accumulates to hit a real String column with a garbage length prefix.
#[derive(Row, Serialize, Debug)]
pub struct EventRow {
    ts_ns: u64,
    duration_ns: u64,
    node_id: u32,
    gpu_id: u8,
    pid: u32,
    tid: u32,
    ctx_id: u32,
    stream_id: u32,
    category: i8,
    name_id: u64,
    correlation_id: u64,
    parent_id: u64,
    metric_id: u64,
    metric_value: f64,
    metric_unit: String,
    grid_x: u32,
    grid_y: u32,
    grid_z: u32,
    block_x: u32,
    block_y: u32,
    block_z: u32,
    shared_static: u32,
    shared_dynamic: u32,
    registers: u32,
    local_mem: u32,
    launch_cbid: u32,
    memcpy_kind: i8,
    memcpy_bytes: u64,
    memcpy_src: u32,
    memcpy_dst: u32,
    api_cbid: u32,
    api_status: u32,
}

impl From<perfweave_proto::v1::Event> for EventRow {
    fn from(e: perfweave_proto::v1::Event) -> Self {
        use perfweave_proto::v1::event::Payload;
        let mut row = EventRow {
            ts_ns: e.ts_ns,
            duration_ns: e.duration_ns,
            node_id: e.node_id,
            gpu_id: e.gpu_id as u8,
            pid: e.pid,
            tid: e.tid,
            ctx_id: e.ctx_id,
            stream_id: e.stream_id,
            category: category_to_i8(e.category()),
            name_id: e.name_id,
            correlation_id: e.correlation_id,
            parent_id: e.parent_id,
            metric_id: 0,
            metric_value: 0.0,
            metric_unit: String::new(),
            grid_x: 0, grid_y: 0, grid_z: 0,
            block_x: 0, block_y: 0, block_z: 0,
            shared_static: 0, shared_dynamic: 0,
            registers: 0, local_mem: 0, launch_cbid: 0,
            memcpy_kind: 0, // UNKNOWN
            memcpy_bytes: 0, memcpy_src: 0, memcpy_dst: 0,
            api_cbid: 0, api_status: 0,
        };
        match e.payload {
            Some(Payload::Metric(m)) => {
                row.metric_id = m.metric_id;
                row.metric_value = m.value;
                row.metric_unit = m.unit;
            }
            Some(Payload::Kernel(k)) => {
                row.grid_x = k.grid_x; row.grid_y = k.grid_y; row.grid_z = k.grid_z;
                row.block_x = k.block_x; row.block_y = k.block_y; row.block_z = k.block_z;
                row.shared_static = k.static_shared_mem_bytes;
                row.shared_dynamic = k.dynamic_shared_mem_bytes;
                row.registers = k.registers_per_thread;
                row.local_mem = k.local_mem_per_thread;
                row.launch_cbid = k.launch_cbid;
            }
            Some(Payload::Memcpy(m)) => {
                row.memcpy_kind = memcpy_kind_to_i8(m.kind());
                row.memcpy_bytes = m.bytes;
                row.memcpy_src = m.src_device;
                row.memcpy_dst = m.dst_device;
            }
            Some(Payload::Api(a)) => {
                row.api_cbid = a.cbid;
                row.api_status = a.status;
            }
            Some(Payload::Marker(_)) | None => {}
        }
        row
    }
}

// Values MUST match the Enum8 definitions in migrations/0001_events.sql.
// If you change them there, change them here (or the CH server will reject
// the byte with a "Unknown element ... for type Enum" error).
fn category_to_i8(c: Category) -> i8 {
    match c {
        Category::Unspecified => 1, // fall back to METRIC; never emitted in practice
        Category::Metric => 1,
        Category::ApiCall => 2,
        Category::Kernel => 3,
        Category::Memcpy => 4,
        Category::Memset => 5,
        Category::Sync => 6,
        Category::Overhead => 7,
        Category::Marker => 8,
    }
}

fn memcpy_kind_to_i8(k: perfweave_proto::v1::memcpy_detail::Kind) -> i8 {
    use perfweave_proto::v1::memcpy_detail::Kind;
    match k {
        Kind::Unknown => 0,
        Kind::H2d => 1,
        Kind::D2h => 2,
        Kind::D2d => 3,
        Kind::H2h => 4,
        Kind::P2p => 5,
    }
}

#[derive(Row, Serialize, Debug)]
struct StringRow {
    id: u64,
    text: String,
}

#[derive(Row, Serialize, Debug)]
struct OffsetRow {
    fitted_at_ns: u64,
    node_id: u32,
    gpu_id: u8,
    offset_ns: i64,
    slope_num: i64,
    slope_den: i64,
    residual_max_ns: u64,
}
