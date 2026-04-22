//! Batched append-only writer into the ClickHouse persistent plane.
//!
//! Scope (after the two-plane rework):
//!   * Activity events: KERNEL / MEMCPY / MEMSET / API_CALL / SYNC / MARKER / OVERHEAD
//!   * String interns
//!   * Clock offsets (audit trail)
//!   * Kernel SOL records
//!   * Persisted spike rows (written by `spike_detect`)
//!
//! Metric samples DO NOT flow through this sink. They live in the in-memory
//! FastRing and are periodically folded into `tiles_metric` at the 2s LOD
//! by a lightweight flusher (`crate::fast_ring::spawn_tiles_flusher`).

use anyhow::{Context, Result};
use clickhouse::{Client, Row};
use perfweave_proto::v1::{Batch, Category, Event, KernelSol};
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
    tx: mpsc::Sender<SinkMsg>,
}

enum SinkMsg {
    Batch(Batch),
    KernelSol { node_id: u32, sol: KernelSol },
    Spike(SpikeRow),
}

impl Sink {
    pub fn spawn(cfg: SinkConfig) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<SinkMsg>(256);
        tokio::spawn(run(cfg, rx));
        Ok(Self { tx })
    }

    pub async fn push(&self, b: Batch) -> Result<()> {
        self.tx
            .send(SinkMsg::Batch(b))
            .await
            .context("sink channel closed; server is shutting down")
    }

    /// Non-blocking try_push used for the fast path. Returns `Full` under
    /// back-pressure so the gRPC handler can surface it as a dropped count.
    pub fn try_push(&self, b: Batch) -> std::result::Result<(), mpsc::error::TrySendError<Batch>> {
        match self.tx.try_send(SinkMsg::Batch(b)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(SinkMsg::Batch(b))) => {
                Err(mpsc::error::TrySendError::Full(b))
            }
            Err(mpsc::error::TrySendError::Closed(SinkMsg::Batch(b))) => {
                Err(mpsc::error::TrySendError::Closed(b))
            }
            Err(_) => unreachable!("we just put a Batch in"),
        }
    }

    pub async fn push_kernel_sol(&self, node_id: u32, sol: KernelSol) -> Result<()> {
        self.tx
            .send(SinkMsg::KernelSol { node_id, sol })
            .await
            .context("sink channel closed")
    }

    pub async fn push_spike(&self, row: SpikeRow) -> Result<()> {
        self.tx
            .send(SinkMsg::Spike(row))
            .await
            .context("sink channel closed")
    }
}

async fn run(cfg: SinkConfig, mut rx: mpsc::Receiver<SinkMsg>) {
    let client = build_client(&cfg);

    let mut pending_events: Vec<EventRow> = Vec::with_capacity(cfg.max_batch_rows);
    let mut pending_strings: Vec<StringRow> = Vec::new();
    let mut pending_offsets: Vec<OffsetRow> = Vec::new();
    let mut pending_sol: Vec<KernelSolRow> = Vec::new();
    let mut pending_spikes: Vec<SpikeRow> = Vec::new();

    let mut ticker = tokio::time::interval(Duration::from_millis(cfg.max_batch_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    SinkMsg::Batch(b) => absorb(b, &mut pending_events, &mut pending_strings, &mut pending_offsets),
                    SinkMsg::KernelSol { node_id, sol } => pending_sol.push(KernelSolRow::from_proto(node_id, sol)),
                    SinkMsg::Spike(row) => pending_spikes.push(row),
                }
                if pending_events.len() >= cfg.max_batch_rows {
                    flush(
                        &client,
                        &mut pending_events,
                        &mut pending_strings,
                        &mut pending_offsets,
                        &mut pending_sol,
                        &mut pending_spikes,
                    ).await;
                }
            }
            _ = ticker.tick() => {
                if !pending_events.is_empty()
                    || !pending_strings.is_empty()
                    || !pending_offsets.is_empty()
                    || !pending_sol.is_empty()
                    || !pending_spikes.is_empty()
                {
                    flush(
                        &client,
                        &mut pending_events,
                        &mut pending_strings,
                        &mut pending_offsets,
                        &mut pending_sol,
                        &mut pending_spikes,
                    ).await;
                }
            }
        }
    }
    flush(
        &client,
        &mut pending_events,
        &mut pending_strings,
        &mut pending_offsets,
        &mut pending_sol,
        &mut pending_spikes,
    )
    .await;
}

fn build_client(cfg: &SinkConfig) -> Client {
    Client::default()
        .with_url(&cfg.url)
        .with_user(&cfg.user)
        .with_password(&cfg.password)
        .with_database(&cfg.database)
}

/// Move non-metric events into the row buffer. Metric events are skipped:
/// the FastRing handles them on a separate path and they never touch CH.
fn absorb(
    b: Batch,
    events: &mut Vec<EventRow>,
    strings: &mut Vec<StringRow>,
    offsets: &mut Vec<OffsetRow>,
) {
    for e in b.events {
        if e.category == Category::Metric as i32 {
            continue;
        }
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
    sol: &mut Vec<KernelSolRow>,
    spikes: &mut Vec<SpikeRow>,
) {
    if !strings.is_empty() {
        if let Err(e) = insert(client, "strings", strings).await {
            tracing::error!(error=%e, "failed to insert strings; retrying on next tick");
            return;
        }
    }
    if !events.is_empty() {
        if let Err(e) = insert(client, "events", events).await {
            tracing::error!(error=%e, rows=events.len(), "failed to insert events batch");
            return;
        }
    }
    if !offsets.is_empty() {
        if let Err(e) = insert(client, "clock_offsets", offsets).await {
            tracing::error!(error=%e, "failed to insert clock_offsets");
            return;
        }
    }
    if !sol.is_empty() {
        if let Err(e) = insert(client, "kernel_sol", sol).await {
            tracing::error!(error=%e, "failed to insert kernel_sol");
            return;
        }
    }
    if !spikes.is_empty() {
        if let Err(e) = insert(client, "spikes", spikes).await {
            tracing::error!(error=%e, "failed to insert spikes");
            return;
        }
    }
    events.clear();
    strings.clear();
    offsets.clear();
    sol.clear();
    spikes.clear();
}

async fn insert<T: Row + Serialize>(client: &Client, table: &str, rows: &[T]) -> Result<()> {
    let mut insert = client.insert(table)?;
    for r in rows {
        insert.write(r).await?;
    }
    insert.end().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Row definitions. Order MUST match the CREATE TABLE columns in the
// corresponding migration, or the clickhouse crate will silently misalign
// columns and the first String after the drift explodes as TOO_LARGE_STRING.
// ---------------------------------------------------------------------------

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
    marker_domain_id: u64,
    marker_color: u32,
    marker_message_id: u64,
}

impl From<Event> for EventRow {
    fn from(e: Event) -> Self {
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
            memcpy_kind: 0,
            memcpy_bytes: 0, memcpy_src: 0, memcpy_dst: 0,
            api_cbid: 0, api_status: 0,
            marker_domain_id: 0, marker_color: 0, marker_message_id: 0,
        };
        match e.payload {
            Some(Payload::Metric(m)) => {
                // Should never land here after the absorb() filter, but be
                // defensive: write the metric fields so the schema stays
                // valid if somebody bypasses the filter.
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
            Some(Payload::Marker(mk)) => {
                row.marker_domain_id = mk.domain_id;
                row.marker_color = mk.color_argb;
                row.marker_message_id = mk.message_id;
            }
            Some(Payload::Sol(_)) | None => {}
        }
        row
    }
}

fn category_to_i8(c: Category) -> i8 {
    match c {
        Category::Unspecified => 1,
        Category::Metric => 1,
        Category::ApiCall => 2,
        Category::Kernel => 3,
        Category::Memcpy => 4,
        Category::Memset => 5,
        Category::Sync => 6,
        Category::Overhead => 7,
        Category::Marker => 8,
        // SOL is a payload category, not persisted on `events`; the row
        // lands in `kernel_sol`. Fall back to KERNEL for belt-and-braces.
        Category::Sol => 3,
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

#[derive(Row, Serialize, Debug)]
struct KernelSolRow {
    ts_ns: u64,
    node_id: u32,
    gpu_id: u8,
    correlation_id: u64,
    sm_active_pct: f32,
    achieved_occupancy_pct: f32,
    dram_bw_pct: f32,
    l1_bw_pct: f32,
    l2_bw_pct: f32,
    inst_throughput_pct: f32,
    theoretical_occupancy_pct: f32,
    arithmetic_intensity: f32,
    achieved_gflops: f32,
    bound: i8,
    confidence: i8,
    source: i8,
    stall_reasons: Vec<u32>,
    stall_counts: Vec<u32>,
}

impl KernelSolRow {
    fn from_proto(node_id: u32, s: KernelSol) -> Self {
        let bound = s.bound();
        let conf = s.confidence();
        let src = s.source();
        let (stall_reasons, stall_counts) = s
            .stalls
            .iter()
            .map(|s| (s.reason_id, s.count))
            .unzip::<_, _, Vec<u32>, Vec<u32>>();
        Self {
            ts_ns: perfweave_common::clock::host_realtime_ns(),
            node_id,
            gpu_id: 0,
            correlation_id: s.correlation_id,
            sm_active_pct: s.sm_active_pct,
            achieved_occupancy_pct: s.achieved_occupancy_pct,
            dram_bw_pct: s.dram_bw_pct,
            l1_bw_pct: s.l1_bw_pct,
            l2_bw_pct: s.l2_bw_pct,
            inst_throughput_pct: s.inst_throughput_pct,
            theoretical_occupancy_pct: s.theoretical_occupancy_pct,
            arithmetic_intensity: s.arithmetic_intensity,
            achieved_gflops: s.achieved_gflops,
            bound: bound as i8,
            confidence: conf as i8,
            source: src as i8,
            stall_reasons,
            stall_counts,
        }
    }
}

#[derive(Row, Serialize, Debug, Clone)]
pub struct SpikeRow {
    pub bucket_start_ns: u64,
    pub bucket_width_ns: u64,
    pub node_id: u32,
    pub gpu_id: u8,
    pub metric_id: u64,
    pub value: f64,
    pub median: f64,
    pub mad: f64,
    pub z_mad: f64,
}
