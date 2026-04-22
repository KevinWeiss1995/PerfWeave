-- Two-Plane rework: shed unnecessary materialized views. Before this change
-- every metric sample fanned out through 4 metric-tile MVs and 8 activity
-- MVs for a total of 12 writes per ingested event. That put ClickHouse at
-- 315% CPU under synthetic load.
--
-- New regime:
--   * Metric samples no longer hit `events` at all (they live in the
--     server's in-memory FastRing and are flushed into `tiles_metric` at
--     the 2s LOD every 2 seconds).
--   * Activity tiles keep only the 32ms and 2s LODs; finer LODs are
--     computed on demand from `events` via intDiv, which the skip indexes
--     on `events(ts_ns)` + `events(correlation_id)` handle cheaply.
--
-- Net: ~10x fewer CH writes per ingested event.

DROP VIEW IF EXISTS tiles_metric_8us_mv;
DROP VIEW IF EXISTS tiles_metric_4ms_mv;
DROP VIEW IF EXISTS tiles_metric_262ms_mv;
DROP VIEW IF EXISTS tiles_metric_2s_mv;

DROP VIEW IF EXISTS tiles_activity_1us_mv;
DROP VIEW IF EXISTS tiles_activity_8us_mv;
DROP VIEW IF EXISTS tiles_activity_64us_mv;
DROP VIEW IF EXISTS tiles_activity_512us_mv;
DROP VIEW IF EXISTS tiles_activity_4ms_mv;
DROP VIEW IF EXISTS tiles_activity_262ms_mv;

-- Re-assert the two surviving activity MVs (no-op if they already exist;
-- CREATE MATERIALIZED VIEW IF NOT EXISTS is idempotent and the DROPs above
-- only remove the others).
CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_activity_32ms_mv
TO tiles_activity AS
SELECT intDiv(ts_ns, 32768000) * 32768000, toUInt64(32768000), node_id, gpu_id, category,
       countState(), sumState(duration_ns), maxState(duration_ns), topKState(16)(name_id)
FROM events WHERE category != 'METRIC'
GROUP BY 1,2,3,4,5;

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_activity_2s_mv
TO tiles_activity AS
SELECT intDiv(ts_ns, 2097152000) * 2097152000, toUInt64(2097152000), node_id, gpu_id, category,
       countState(), sumState(duration_ns), maxState(duration_ns), topKState(16)(name_id)
FROM events WHERE category != 'METRIC'
GROUP BY 1,2,3,4,5;
