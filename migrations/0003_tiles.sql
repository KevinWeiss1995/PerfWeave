-- Tile pyramid. One materialized view per resolution; each view aggregates
-- the raw events table into fixed-width time buckets. Widths are powers of 8
-- apart (3 bits of zoom per level) so LOD transitions in the UI are smooth.
--
-- For activity lanes (KERNEL/MEMCPY/API_CALL) we store count + sum_duration +
-- top-16 kernel names per bucket. For metric lanes we store p50/p99/max and
-- the argmax name. This gives the UI a one-row-per-pixel answer at any zoom.

-- Coarsest convenience helper
CREATE DATABASE IF NOT EXISTS perfweave;

-- --------------------------------------------------------------------------
-- Activity tile aggregator (categories != METRIC)
-- --------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS tiles_activity
(
    bucket_start_ns UInt64,
    bucket_width_ns UInt64,
    node_id         UInt32,
    gpu_id          UInt8,
    category        Enum8(
                        'METRIC'   = 1,
                        'API_CALL' = 2,
                        'KERNEL'   = 3,
                        'MEMCPY'   = 4,
                        'MEMSET'   = 5,
                        'SYNC'     = 6,
                        'OVERHEAD' = 7,
                        'MARKER'   = 8
                    ),
    count           AggregateFunction(count),
    sum_duration_ns AggregateFunction(sum, UInt64),
    max_duration_ns AggregateFunction(max, UInt64),
    -- top-16 names by total duration in this bucket
    top_names       AggregateFunction(topK(16), UInt64)
)
ENGINE = AggregatingMergeTree
PARTITION BY (bucket_width_ns, toStartOfDay(toDateTime64(bucket_start_ns / 1e9, 9)))
ORDER BY (bucket_width_ns, node_id, gpu_id, category, bucket_start_ns);

-- One materialized view per resolution. We inline the width constants so
-- ClickHouse can fold them during query.
CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_activity_1us_mv
TO tiles_activity AS
SELECT
    intDiv(ts_ns, 1000) * 1000            AS bucket_start_ns,
    toUInt64(1000)                        AS bucket_width_ns,
    node_id, gpu_id, category,
    countState()                          AS count,
    sumState(duration_ns)                 AS sum_duration_ns,
    maxState(duration_ns)                 AS max_duration_ns,
    topKState(16)(name_id)                AS top_names
FROM events
WHERE category != 'METRIC'
GROUP BY bucket_start_ns, bucket_width_ns, node_id, gpu_id, category;

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_activity_8us_mv
TO tiles_activity AS
SELECT
    intDiv(ts_ns, 8000) * 8000 AS bucket_start_ns,
    toUInt64(8000) AS bucket_width_ns,
    node_id, gpu_id, category,
    countState(), sumState(duration_ns), maxState(duration_ns), topKState(16)(name_id)
FROM events WHERE category != 'METRIC'
GROUP BY bucket_start_ns, bucket_width_ns, node_id, gpu_id, category;

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_activity_64us_mv
TO tiles_activity AS
SELECT intDiv(ts_ns, 64000) * 64000, toUInt64(64000), node_id, gpu_id, category,
       countState(), sumState(duration_ns), maxState(duration_ns), topKState(16)(name_id)
FROM events WHERE category != 'METRIC'
GROUP BY 1,2,3,4,5;

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_activity_512us_mv
TO tiles_activity AS
SELECT intDiv(ts_ns, 512000) * 512000, toUInt64(512000), node_id, gpu_id, category,
       countState(), sumState(duration_ns), maxState(duration_ns), topKState(16)(name_id)
FROM events WHERE category != 'METRIC'
GROUP BY 1,2,3,4,5;

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_activity_4ms_mv
TO tiles_activity AS
SELECT intDiv(ts_ns, 4096000) * 4096000, toUInt64(4096000), node_id, gpu_id, category,
       countState(), sumState(duration_ns), maxState(duration_ns), topKState(16)(name_id)
FROM events WHERE category != 'METRIC'
GROUP BY 1,2,3,4,5;

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_activity_32ms_mv
TO tiles_activity AS
SELECT intDiv(ts_ns, 32768000) * 32768000, toUInt64(32768000), node_id, gpu_id, category,
       countState(), sumState(duration_ns), maxState(duration_ns), topKState(16)(name_id)
FROM events WHERE category != 'METRIC'
GROUP BY 1,2,3,4,5;

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_activity_262ms_mv
TO tiles_activity AS
SELECT intDiv(ts_ns, 262144000) * 262144000, toUInt64(262144000), node_id, gpu_id, category,
       countState(), sumState(duration_ns), maxState(duration_ns), topKState(16)(name_id)
FROM events WHERE category != 'METRIC'
GROUP BY 1,2,3,4,5;

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_activity_2s_mv
TO tiles_activity AS
SELECT intDiv(ts_ns, 2097152000) * 2097152000, toUInt64(2097152000), node_id, gpu_id, category,
       countState(), sumState(duration_ns), maxState(duration_ns), topKState(16)(name_id)
FROM events WHERE category != 'METRIC'
GROUP BY 1,2,3,4,5;

-- --------------------------------------------------------------------------
-- Metric tile aggregator (category = METRIC)
-- --------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS tiles_metric
(
    bucket_start_ns UInt64,
    bucket_width_ns UInt64,
    node_id         UInt32,
    gpu_id          UInt8,
    metric_id       UInt64,
    count           AggregateFunction(count),
    sum_value       AggregateFunction(sum, Float64),
    min_value       AggregateFunction(min, Float64),
    max_value       AggregateFunction(max, Float64),
    p50_value       AggregateFunction(quantileTDigest(0.5), Float64),
    p99_value       AggregateFunction(quantileTDigest(0.99), Float64)
)
ENGINE = AggregatingMergeTree
PARTITION BY (bucket_width_ns, toStartOfDay(toDateTime64(bucket_start_ns / 1e9, 9)))
ORDER BY (bucket_width_ns, node_id, gpu_id, metric_id, bucket_start_ns);

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_metric_8us_mv
TO tiles_metric AS
SELECT intDiv(ts_ns, 8000) * 8000 AS bucket_start_ns,
       toUInt64(8000) AS bucket_width_ns,
       node_id, gpu_id, metric_id,
       countState(),
       sumState(metric_value),
       minState(metric_value),
       maxState(metric_value),
       quantileTDigestState(0.5)(metric_value),
       quantileTDigestState(0.99)(metric_value)
FROM events WHERE category = 'METRIC'
GROUP BY 1,2,3,4,5;

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_metric_4ms_mv
TO tiles_metric AS
SELECT intDiv(ts_ns, 4096000) * 4096000, toUInt64(4096000),
       node_id, gpu_id, metric_id,
       countState(), sumState(metric_value), minState(metric_value),
       maxState(metric_value), quantileTDigestState(0.5)(metric_value),
       quantileTDigestState(0.99)(metric_value)
FROM events WHERE category = 'METRIC'
GROUP BY 1,2,3,4,5;

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_metric_262ms_mv
TO tiles_metric AS
SELECT intDiv(ts_ns, 262144000) * 262144000, toUInt64(262144000),
       node_id, gpu_id, metric_id,
       countState(), sumState(metric_value), minState(metric_value),
       maxState(metric_value), quantileTDigestState(0.5)(metric_value),
       quantileTDigestState(0.99)(metric_value)
FROM events WHERE category = 'METRIC'
GROUP BY 1,2,3,4,5;

CREATE MATERIALIZED VIEW IF NOT EXISTS tiles_metric_2s_mv
TO tiles_metric AS
SELECT intDiv(ts_ns, 2097152000) * 2097152000, toUInt64(2097152000),
       node_id, gpu_id, metric_id,
       countState(), sumState(metric_value), minState(metric_value),
       maxState(metric_value), quantileTDigestState(0.5)(metric_value),
       quantileTDigestState(0.99)(metric_value)
FROM events WHERE category = 'METRIC'
GROUP BY 1,2,3,4,5;
