//! GraphQL control plane. Used for:
//!   - node / GPU inventory
//!   - metric + string dictionary lookup
//!   - spike queries
//!   - correlation lookup (given correlation_id, return the paired events)
//!
//! Kept narrow on purpose. The timeline data path is Arrow IPC, not GraphQL.

use crate::ch::Ch;
use async_graphql::{Context, EmptySubscription, Object, Schema, SimpleObject};
use clickhouse::Row;
use serde::Deserialize;

pub type AppSchema = Schema<Query, Mutation, EmptySubscription>;

pub struct Query;
pub struct Mutation;

pub fn build_schema(ch: Ch) -> AppSchema {
    Schema::build(Query, Mutation, EmptySubscription)
        .data(ch)
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

    /// Drill-down for a specific spike. Gives the UI everything it needs to
    /// explain "why is GPU slow right here?" in one hop:
    ///   - classify as compute-bound vs memory-bound using average NVML
    ///     utilization around the spike
    ///   - list the top-3 kernels live during the same window
    ///   - include the metric name so the badge tooltip is meaningful
    ///
    /// Window is the spike bucket expanded by ±500ms, which is large enough
    /// to catch the kernel wave responsible for the spike without pulling in
    /// neighbor workloads.
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
        let util_sql = format!(
            r#"
            SELECT
              avgIf(value, name_id = (SELECT id FROM strings FINAL WHERE text = 'gpu_util' LIMIT 1)) AS avg_gpu_util,
              avgIf(value, name_id = (SELECT id FROM strings FINAL WHERE text = 'mem_util' LIMIT 1)) AS avg_mem_util
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

        // Classifier: the GPU is memory-bound during the spike if memory
        // controller utilization is meaningfully above SM utilization, and
        // compute-bound in the opposite case. The 1.2× factor gives us
        // hysteresis so we don't flip on noise.
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
        })
    }
}

#[Object]
impl Mutation {
    /// Trigger spike recomputation for a time window. Normally this is done
    /// on a timer; exposed here for tests and manual recomputation.
    async fn recompute_spikes(
        &self,
        ctx: &Context<'_>,
        start_ns: u64,
        end_ns: u64,
    ) -> async_graphql::Result<u64> {
        let ch = ctx.data::<Ch>()?;
        let inserted = crate::imports::recompute_spikes_window(ch, start_ns, end_ns).await?;
        Ok(inserted)
    }
}
