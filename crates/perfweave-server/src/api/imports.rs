//! Background import of `.nsys-rep` files.
//!
//! The nsys import path shells out to `nsys export --type sqlite <file>` and
//! ingests rows into the canonical `events` schema. This is the only place
//! in the system that uses the nsys CLI, and it is offline-only.
//!
//! Spike recomputation is gone. After the two-plane rework spikes are
//! detected live by `crate::spike_detect` from the FastRing and persisted
//! to the `spikes` table at detection time. There is no periodic "scan
//! the last 15 minutes" job anymore — that job was the #1 source of
//! ClickHouse CPU churn.

use crate::store::Ch;
use anyhow::{Context, Result};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct ImportResult {
    pub event_count: u64,
    pub ts_min_ns: u64,
    pub ts_max_ns: u64,
}

/// Import a .nsys-rep file. Shells out to `nsys export --type sqlite`,
/// then COPIES the relevant tables into our events schema.
///
/// This runs in a blocking task because `nsys` can take minutes on large
/// reports. Callers should spawn it on a worker.
pub async fn import_nsys(ch: &Ch, path: &Path, node_id: u32) -> Result<ImportResult> {
    let tmpdir = tempdir_in_current()?;
    let sqlite_path = tmpdir.join("trace.sqlite");

    tracing::info!(?path, ?sqlite_path, "running nsys export");
    let status = tokio::process::Command::new("nsys")
        .arg("export")
        .arg("--type").arg("sqlite")
        .arg("--force-overwrite=true")
        .arg("-o").arg(&sqlite_path)
        .arg(path)
        .status()
        .await
        .context("failed to spawn `nsys`. Install Nsight Systems (12.x) and ensure `nsys` is on PATH.")?;
    if !status.success() {
        anyhow::bail!("`nsys export` exited with {status}. Is the file a valid .nsys-rep?");
    }

    let nsys_sql = format!(
        r#"
        INSERT INTO events (ts_ns, duration_ns, node_id, gpu_id, pid, tid, ctx_id,
            stream_id, category, name_id, correlation_id, parent_id,
            grid_x, grid_y, grid_z, block_x, block_y, block_z,
            shared_static, shared_dynamic, registers, local_mem, launch_cbid)
        SELECT
            toUInt64(k.start)  AS ts_ns,
            toUInt64(k.end - k.start) AS duration_ns,
            toUInt32({node_id})       AS node_id,
            toUInt8(k.deviceId)       AS gpu_id,
            toUInt32(0)               AS pid,
            toUInt32(0)               AS tid,
            toUInt32(k.contextId)     AS ctx_id,
            toUInt32(k.streamId)      AS stream_id,
            CAST('KERNEL' AS Enum8(
               'METRIC'=1,'API_CALL'=2,'KERNEL'=3,'MEMCPY'=4,'MEMSET'=5,
               'SYNC'=6,'OVERHEAD'=7,'MARKER'=8)) AS category,
            xxHash64(coalesce(names.value, ''))   AS name_id,
            toUInt64(k.correlationId) AS correlation_id,
            0                         AS parent_id,
            toUInt32(k.gridX), toUInt32(k.gridY), toUInt32(k.gridZ),
            toUInt32(k.blockX), toUInt32(k.blockY), toUInt32(k.blockZ),
            toUInt32(k.staticSharedMemory), toUInt32(k.dynamicSharedMemory),
            0, toUInt32(k.localMemoryPerThread), 0
        FROM sqlite('{sqlite}', 'CUPTI_ACTIVITY_KIND_KERNEL') AS k
        LEFT JOIN sqlite('{sqlite}', 'StringIds') AS names
            ON names.id = k.shortName
        "#,
        sqlite = sqlite_path.display(),
        node_id = node_id,
    );

    ch.client.query(&nsys_sql).execute().await.context("ingesting nsys kernels failed")?;

    let (count, min_ns, max_ns) = window_stats(ch, node_id).await?;
    Ok(ImportResult { event_count: count, ts_min_ns: min_ns, ts_max_ns: max_ns })
}

async fn window_stats(ch: &Ch, node_id: u32) -> Result<(u64, u64, u64)> {
    #[derive(Row, Deserialize)]
    struct S { c: u64, mn: u64, mx: u64 }
    let sql = format!(
        "SELECT count() AS c, min(ts_ns) AS mn, max(ts_ns) AS mx FROM events WHERE node_id = {node_id}"
    );
    let s: S = ch.client.query(&sql).fetch_one().await?;
    Ok((s.c, s.mn, s.mx))
}

fn tempdir_in_current() -> Result<std::path::PathBuf> {
    let p = std::env::temp_dir().join(format!("perfweave-import-{}", std::process::id()));
    std::fs::create_dir_all(&p)?;
    Ok(p)
}
