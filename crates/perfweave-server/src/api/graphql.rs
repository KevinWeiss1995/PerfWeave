//! GraphQL control plane. Used for:
//!   - node / GPU inventory
//!   - metric + string dictionary lookup
//!   - spike queries
//!   - correlation lookup (given correlation_id, return the paired events)
//!   - kernel SOL drilldown
//!   - on-demand kernel-replay profiling (mutation)
//!
//! The timeline data path is Arrow IPC, and the live-tail path is SSE;
//! everything else is GraphQL.

use crate::replay::AgentRegistry;
use crate::store::{sink::Sink, Ch};
use async_graphql::{Context, EmptySubscription, Object, Schema, SimpleObject};
use clickhouse::Row;
use perfweave_proto::v1::{KernelSol, ProfileRequest};
use serde::Deserialize;
use std::sync::Arc;
use tokio_stream::StreamExt;

pub type AppSchema = Schema<Query, Mutation, EmptySubscription>;

pub struct Query;
pub struct Mutation;

pub fn build_schema(
    ch: Ch,
    sink: Arc<Sink>,
    agents: Arc<AgentRegistry>,
) -> AppSchema {
    Schema::build(Query, Mutation, EmptySubscription)
        .data(ch)
        .data(sink)
        .data(agents)
        .finish()
}

#[derive(SimpleObject, Row, Deserialize, Debug, Clone)]
pub struct Node {
    pub node_id: u32,
    pub hostname: String,
    pub agent_version: String,
    pub num_gpus: u8,
    pub last_seen_ns: u64,
}

#[derive(SimpleObject, Row, Deserialize, Debug, Clone)]
pub struct StringEntry {
    pub id: u64,
    pub text: String,
}

#[derive(SimpleObject, Row, Deserialize, Debug, Clone)]
pub struct Spike {
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

#[derive(SimpleObject, Row, Deserialize, Debug, Clone)]
pub struct CorrelatedEvent {
    pub ts_ns: u64,
    pub duration_ns: u64,
    pub gpu_id: u8,
    pub category: u8,
    pub name_id: u64,
    pub pid: u32,
    pub stream_id: u32,
}

#[derive(SimpleObject, Debug, Clone)]
pub struct TopKernelsInWindow {
    pub name_id: u64,
    pub name: String,
    pub total_duration_ns: u64,
    pub launches: u64,
    pub mean_duration_ns: u64,
}

#[derive(async_graphql::Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum Bottleneck {
    ComputeBound,
    MemoryBound,
    Unclassified,
}

#[derive(SimpleObject, Debug, Clone)]
pub struct KernelInWindow {
    pub correlation_id: u64,
    pub name_id: u64,
    pub name: String,
    pub ts_ns: u64,
    pub duration_ns: u64,
    pub gpu_id: u8,
    /// Speed-of-Light numbers (populated if we've profiled this kernel).
    pub sol: Option<KernelSolRow>,
}

#[derive(SimpleObject, Debug, Clone)]
pub struct KernelSolRow {
    pub sm_active_pct: f64,
    pub achieved_occupancy_pct: f64,
    pub dram_bw_pct: f64,
    pub l1_bw_pct: f64,
    pub l2_bw_pct: f64,
    pub inst_throughput_pct: f64,
    pub arithmetic_intensity: f64,
    pub achieved_gflops: f64,
    pub bound: String,
    pub confidence: String,
    pub source: String,
}

#[derive(SimpleObject, Debug, Clone)]
pub struct SpikeContext {
    /// The spike identity fields (so the client can match back to a row).
    pub bucket_start_ns: u64,
    pub bucket_width_ns: u64,
    pub gpu_id: u8,
    pub metric_id: u64,
    pub metric_name: String,
    /// Classification around the spike window (±500ms).
    pub bottleneck: Bottleneck,
    /// Average mem_util and gpu_util (percent) in the classification window.
    pub avg_mem_util: f64,
    pub avg_gpu_util: f64,
    /// Top kernels by total duration in the classification window.
    pub top_kernels: Vec<TopKernelsInWindow>,
    /// All kernels in ±500ms window, joined with kernel_sol if available.
    /// Used by SpikeDrilldown to render a Gantt + SOL cards in one hop.
    pub kernels_in_window: Vec<KernelInWindow>,
}

#[derive(SimpleObject, Debug, Clone)]
pub struct ProfileKernelsAck {
    pub node_id: u32,
    pub requested: u32,
    pub completed: u32,
    /// Resulting SOL rows. The UI can also watch /api/live if it wants the
    /// same data via SSE; returning here is the low-latency path.
    pub results: Vec<KernelSolRow>,
}

#[Object]
impl Query {
    async fn nodes(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Node>> {
        let ch = ctx.data::<Ch>()?;
        let rows: Vec<Node> = ch
            .client
            .query("SELECT node_id, hostname, agent_version, num_gpus, last_seen_ns FROM nodes FINAL ORDER BY node_id")
            .fetch_all()
            .await?;
        Ok(rows)
    }

    async fn strings(
        &self,
        ctx: &Context<'_>,
        ids: Vec<u64>,
    ) -> async_graphql::Result<Vec<StringEntry>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let ch = ctx.data::<Ch>()?;
        let ids_str = ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, text FROM strings FINAL WHERE id IN ({ids_str})",
        );
        let rows: Vec<StringEntry> = ch.client.query(&sql).fetch_all().await?;
        Ok(rows)
    }

    async fn spikes(
        &self,
        ctx: &Context<'_>,
        start_ns: u64,
        end_ns: u64,
    ) -> async_graphql::Result<Vec<Spike>> {
        let ch = ctx.data::<Ch>()?;
        let sql = format!(
            r#"
            SELECT bucket_start_ns, bucket_width_ns, node_id, gpu_id,
                   metric_id, value, median, mad, z_mad
            FROM spikes
            WHERE bucket_start_ns >= {start_ns}
              AND bucket_start_ns <  {end_ns}
              AND z_mad >= 3.5
            ORDER BY bucket_start_ns
            LIMIT 1000
            "#
        );
        Ok(ch.client.query(&sql).fetch_all().await?)
    }

    async fn correlated(
        &self,
        ctx: &Context<'_>,
        correlation_id: u64,
    ) -> async_graphql::Result<Vec<CorrelatedEvent>> {
        let ch = ctx.data::<Ch>()?;
        let sql = format!(
            r#"
            SELECT ts_ns, duration_ns, gpu_id, toUInt8(category) AS category,
                   name_id, pid, stream_id
            FROM events
            WHERE correlation_id = {correlation_id}
            ORDER BY ts_ns
            LIMIT 64
            "#
        );
        Ok(ch.client.query(&sql).fetch_all().await?)
    }

    async fn top_kernels_in_window(
        &self,
        ctx: &Context<'_>,
        start_ns: u64,
        end_ns: u64,
        gpu_id: Option<u8>,
        limit: Option<u32>,
    ) -> async_graphql::Result<Vec<TopKernelsInWindow>> {
        let ch = ctx.data::<Ch>()?;
        let limit = limit.unwrap_or(3).min(32);
        let gpu_clause = gpu_id.map(|g| format!(" AND gpu_id = {g}")).unwrap_or_default();
        let sql = format!(
            r#"
            SELECT
                name_id,
                any(s.text) AS name,
                sum(duration_ns) AS total_duration_ns,
                count()          AS launches,
                toUInt64(avg(duration_ns)) AS mean_duration_ns
            FROM events AS e
            LEFT JOIN strings AS s FINAL ON s.id = e.name_id
            WHERE category = 'KERNEL'
              AND ts_ns >= {start_ns}
              AND ts_ns <  {end_ns}
              {gpu_clause}
            GROUP BY name_id
            ORDER BY total_duration_ns DESC
            LIMIT {limit}
            "#
        );
        #[derive(Row, Deserialize)]
        struct R {
            name_id: u64,
            name: String,
            total_duration_ns: u64,
            launches: u64,
            mean_duration_ns: u64,
        }
        let rows: Vec<R> = ch.client.query(&sql).fetch_all().await?;
        Ok(rows
            .into_iter()
            .map(|r| TopKernelsInWindow {
                name_id: r.name_id,
                name: r.name,
                total_duration_ns: r.total_duration_ns,
                launches: r.launches,
                mean_duration_ns: r.mean_duration_ns,
            })
            .collect())
    }

    /// Drill-down for a specific spike.
    async fn spike_context(
        &self,
        ctx: &Context<'_>,
        bucket_start_ns: u64,
        bucket_width_ns: u64,
        gpu_id: u8,
        metric_id: u64,
    ) -> async_graphql::Result<SpikeContext> {
        let ch = ctx.data::<Ch>()?;

        const PAD: u64 = 500_000_000; // 500ms each side
        let win_start = bucket_start_ns.saturating_sub(PAD);
        let win_end = bucket_start_ns
            .saturating_add(bucket_width_ns)
            .saturating_add(PAD);

        let metric_name: String = {
            let sql = format!("SELECT text FROM strings FINAL WHERE id = {metric_id} LIMIT 1");
            ch.client
                .query(&sql)
                .fetch_optional::<String>()
                .await?
                .unwrap_or_else(|| format!("metric#{metric_id}"))
        };

        #[derive(Row, Deserialize, Default)]
        struct UtilRow {
            avg_gpu_util: f64,
            avg_mem_util: f64,
        }
        // NVML names: gpu.util.percent (SM utilization) and gpu.mem.util.percent
        // (memory controller utilization). We fall back to legacy names
        // "gpu_util"/"mem_util" if the strings table still has them for
        // backward compatibility with pre-rework imports.
        let util_sql = format!(
            r#"
            SELECT
              avgIf(value, name_id IN (SELECT id FROM strings FINAL WHERE text IN ('gpu.util.percent','gpu_util'))) AS avg_gpu_util,
              avgIf(value, name_id IN (SELECT id FROM strings FINAL WHERE text IN ('gpu.mem.util.percent','mem_util'))) AS avg_mem_util
            FROM events
            WHERE category = 'METRIC'
              AND gpu_id = {gpu_id}
              AND ts_ns >= {win_start}
              AND ts_ns <  {win_end}
            "#
        );
        let util: UtilRow = ch
            .client
            .query(&util_sql)
            .fetch_optional()
            .await?
            .unwrap_or_default();

        let bottleneck = if util.avg_mem_util > util.avg_gpu_util * 1.2 && util.avg_mem_util > 5.0 {
            Bottleneck::MemoryBound
        } else if util.avg_gpu_util > util.avg_mem_util * 1.2 && util.avg_gpu_util > 20.0 {
            Bottleneck::ComputeBound
        } else {
            Bottleneck::Unclassified
        };

        let kernels_sql = format!(
            r#"
            SELECT
                name_id,
                any(s.text) AS name,
                sum(duration_ns) AS total_duration_ns,
                count()          AS launches,
                toUInt64(avg(duration_ns)) AS mean_duration_ns
            FROM events AS e
            LEFT JOIN strings AS s FINAL ON s.id = e.name_id
            WHERE category = 'KERNEL'
              AND gpu_id = {gpu_id}
              AND ts_ns >= {win_start}
              AND ts_ns <  {win_end}
            GROUP BY name_id
            ORDER BY total_duration_ns DESC
            LIMIT 3
            "#
        );
        #[derive(Row, Deserialize)]
        struct K {
            name_id: u64,
            name: String,
            total_duration_ns: u64,
            launches: u64,
            mean_duration_ns: u64,
        }
        let krows: Vec<K> = ch.client.query(&kernels_sql).fetch_all().await?;
        let top_kernels = krows
            .into_iter()
            .map(|r| TopKernelsInWindow {
                name_id: r.name_id,
                name: r.name,
                total_duration_ns: r.total_duration_ns,
                launches: r.launches,
                mean_duration_ns: r.mean_duration_ns,
            })
            .collect();

        // Full kernel list + SOL join, sorted by duration desc. LIMIT 32 is
        // more than enough to render a Gantt; UI can paginate if it ever
        // isn't.
        let kiw_sql = format!(
            r#"
            SELECT
                e.correlation_id    AS correlation_id,
                e.name_id           AS name_id,
                coalesce(s.text,'') AS name,
                e.ts_ns             AS ts_ns,
                e.duration_ns       AS duration_ns,
                e.gpu_id            AS gpu_id,
                argMax(k.sm_active_pct,          k.confidence) AS sm,
                argMax(k.achieved_occupancy_pct, k.confidence) AS occ,
                argMax(k.dram_bw_pct,            k.confidence) AS dram,
                argMax(k.l1_bw_pct,              k.confidence) AS l1,
                argMax(k.l2_bw_pct,              k.confidence) AS l2,
                argMax(k.inst_throughput_pct,    k.confidence) AS inst,
                argMax(k.arithmetic_intensity,   k.confidence) AS ai,
                argMax(k.achieved_gflops,        k.confidence) AS gflops,
                argMax(toString(k.bound),        k.confidence) AS bound,
                argMax(toString(k.confidence),   k.confidence) AS confidence,
                argMax(toString(k.source),       k.confidence) AS source,
                count(k.correlation_id)                        AS sol_rows
            FROM events AS e
            LEFT JOIN strings    AS s FINAL ON s.id = e.name_id
            LEFT JOIN kernel_sol AS k        ON k.correlation_id = e.correlation_id
            WHERE e.category = 'KERNEL'
              AND e.gpu_id = {gpu_id}
              AND e.ts_ns >= {win_start}
              AND e.ts_ns <  {win_end}
            GROUP BY e.correlation_id, e.name_id, s.text, e.ts_ns, e.duration_ns, e.gpu_id
            ORDER BY e.duration_ns DESC
            LIMIT 32
            "#
        );
        #[derive(Row, Deserialize)]
        struct Kiw {
            correlation_id: u64,
            name_id: u64,
            name: String,
            ts_ns: u64,
            duration_ns: u64,
            gpu_id: u8,
            sm: f32,
            occ: f32,
            dram: f32,
            l1: f32,
            l2: f32,
            inst: f32,
            ai: f32,
            gflops: f32,
            bound: String,
            confidence: String,
            source: String,
            sol_rows: u64,
        }
        let kiw: Vec<Kiw> = ch.client.query(&kiw_sql).fetch_all().await?;
        let kernels_in_window = kiw
            .into_iter()
            .map(|r| KernelInWindow {
                correlation_id: r.correlation_id,
                name_id: r.name_id,
                name: r.name,
                ts_ns: r.ts_ns,
                duration_ns: r.duration_ns,
                gpu_id: r.gpu_id,
                sol: if r.sol_rows > 0 {
                    Some(KernelSolRow {
                        sm_active_pct: r.sm as f64,
                        achieved_occupancy_pct: r.occ as f64,
                        dram_bw_pct: r.dram as f64,
                        l1_bw_pct: r.l1 as f64,
                        l2_bw_pct: r.l2 as f64,
                        inst_throughput_pct: r.inst as f64,
                        arithmetic_intensity: r.ai as f64,
                        achieved_gflops: r.gflops as f64,
                        bound: r.bound,
                        confidence: r.confidence,
                        source: r.source,
                    })
                } else {
                    None
                },
            })
            .collect();

        Ok(SpikeContext {
            bucket_start_ns,
            bucket_width_ns,
            gpu_id,
            metric_id,
            metric_name,
            bottleneck,
            avg_mem_util: util.avg_mem_util,
            avg_gpu_util: util.avg_gpu_util,
            top_kernels,
            kernels_in_window,
        })
    }
}

#[Object]
impl Mutation {
    /// Ask the owning agent to replay-profile the given kernels and stream
    /// back `KernelSol` rows. Persists every result into `kernel_sol` so
    /// the next `spikeContext` for the same window returns high-confidence
    /// numbers without a round-trip.
    async fn profile_kernels(
        &self,
        ctx: &Context<'_>,
        node_id: u32,
        gpu_id: u32,
        window_start_ns: u64,
        window_end_ns: u64,
        correlation_ids: Vec<u64>,
        timeout_ms: Option<u32>,
    ) -> async_graphql::Result<ProfileKernelsAck> {
        let agents = ctx.data::<Arc<AgentRegistry>>()?;
        let sink = ctx.data::<Arc<Sink>>()?;
        let requested = correlation_ids.len() as u32;

        let req = ProfileRequest {
            gpu_id,
            window_start_ns,
            window_end_ns,
            correlation_ids: correlation_ids.clone(),
            name_ids: Vec::new(),
            timeout_ms: timeout_ms.unwrap_or(5_000),
        };
        let mut stream = crate::replay::profile_kernels(agents, node_id, req)
            .await
            .map_err(|e| async_graphql::Error::new(format!("profile_kernels: {e:#}")))?;

        let mut results: Vec<KernelSolRow> = Vec::new();
        let mut completed: u32 = 0;
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(sol) => {
                    completed += 1;
                    // Persist and return.
                    let sol_clone = sol.clone();
                    if let Err(e) = sink.push_kernel_sol(node_id, sol_clone).await {
                        tracing::warn!(error=%e, "failed to persist kernel_sol");
                    }
                    results.push(kernel_sol_to_row(&sol));
                }
                Err(e) => {
                    tracing::warn!(error=%e, "replay stream error");
                    break;
                }
            }
        }

        Ok(ProfileKernelsAck { node_id, requested, completed, results })
    }
}

fn kernel_sol_to_row(s: &KernelSol) -> KernelSolRow {
    let bound = format!("{:?}", s.bound());
    let conf = format!("{:?}", s.confidence());
    let src = format!("{:?}", s.source());
    KernelSolRow {
        sm_active_pct: s.sm_active_pct as f64,
        achieved_occupancy_pct: s.achieved_occupancy_pct as f64,
        dram_bw_pct: s.dram_bw_pct as f64,
        l1_bw_pct: s.l1_bw_pct as f64,
        l2_bw_pct: s.l2_bw_pct as f64,
        inst_throughput_pct: s.inst_throughput_pct as f64,
        arithmetic_intensity: s.arithmetic_intensity as f64,
        achieved_gflops: s.achieved_gflops as f64,
        bound,
        confidence: conf,
        source: src,
    }
}
