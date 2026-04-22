//! Embedded SQL migrations. We ship the `.sql` files in the binary so
//! `perfweave-server` self-bootstraps a fresh ClickHouse with zero DBA.
//!
//! Order matters; do not reorder existing entries. Add new migrations to
//! the end.

use anyhow::{Context, Result};
use clickhouse::Client;

pub const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_events", include_str!("../../../../migrations/0001_events.sql")),
    ("0002_strings", include_str!("../../../../migrations/0002_strings.sql")),
    ("0003_tiles", include_str!("../../../../migrations/0003_tiles.sql")),
    ("0004_spikes", include_str!("../../../../migrations/0004_spikes.sql")),
    ("0005_nsys_imports", include_str!("../../../../migrations/0005_nsys_imports.sql")),
    ("0006_shed_metric_tiles", include_str!("../../../../migrations/0006_shed_metric_tiles.sql")),
    ("0007_markers", include_str!("../../../../migrations/0007_markers.sql")),
    ("0008_kernel_sol", include_str!("../../../../migrations/0008_kernel_sol.sql")),
];

pub async fn apply_all(ch: &super::Ch) -> Result<()> {
    apply(&*ch.client).await
}

pub async fn apply(client: &Client) -> Result<()> {
    // ClickHouse's HTTP interface requires one statement per request; we
    // split on `;` at EOL which is good enough for hand-written migrations.
    for (name, sql) in MIGRATIONS {
        tracing::info!(migration = name, "applying migration");
        for stmt in split_statements(sql) {
            let trimmed = stmt.trim();
            if trimmed.is_empty() || trimmed.starts_with("--") {
                continue;
            }
            client
                .query(trimmed)
                .execute()
                .await
                .with_context(|| format!("migration {name} failed at statement starting: {}", preview(trimmed)))?;
        }
    }
    tracing::info!("all migrations applied");
    Ok(())
}

fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in sql.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--") {
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
        if line.trim_end().ends_with(';') {
            out.push(std::mem::take(&mut buf));
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf);
    }
    out
}

fn preview(s: &str) -> String {
    s.chars().take(80).collect()
}
