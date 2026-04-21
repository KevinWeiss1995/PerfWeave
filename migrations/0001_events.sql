-- Canonical event table. One row per event (metric samples included).
-- ORDER BY supports the two dominant query shapes:
--   (a) time-range on a single GPU + category (timeline rendering)
--   (b) correlation-id lookups for drilldown (skip index below).
--
-- PARTITION BY hour keeps TTL and drop operations cheap. Events for a given
-- hour live in one part per node, so ClickHouse can prune instantly.
CREATE TABLE IF NOT EXISTS events
(
    ts_ns          UInt64 CODEC(Delta, ZSTD(1)),
    duration_ns    UInt64 CODEC(ZSTD(1)),
    node_id        UInt32,
    gpu_id         UInt8,
    pid            UInt32,
    tid            UInt32,
    ctx_id         UInt32,
    stream_id      UInt32,
    category       Enum8(
                       'METRIC'   = 1,
                       'API_CALL' = 2,
                       'KERNEL'   = 3,
                       'MEMCPY'   = 4,
                       'MEMSET'   = 5,
                       'SYNC'     = 6,
                       'OVERHEAD' = 7,
                       'MARKER'   = 8
                   ),
    name_id        UInt64 CODEC(ZSTD(1)),
    correlation_id UInt64 CODEC(ZSTD(1)),
    parent_id      UInt64 CODEC(ZSTD(1)),

    -- Metric payload (sparse; null for non-metric rows).
    metric_id      UInt64,
    metric_value   Float64 CODEC(Gorilla, ZSTD(1)),
    metric_unit    LowCardinality(String),

    -- Kernel payload.
    grid_x         UInt32 DEFAULT 0,
    grid_y         UInt32 DEFAULT 0,
    grid_z         UInt32 DEFAULT 0,
    block_x        UInt32 DEFAULT 0,
    block_y        UInt32 DEFAULT 0,
    block_z        UInt32 DEFAULT 0,
    shared_static  UInt32 DEFAULT 0,
    shared_dynamic UInt32 DEFAULT 0,
    registers      UInt32 DEFAULT 0,
    local_mem      UInt32 DEFAULT 0,
    launch_cbid    UInt32 DEFAULT 0,

    -- Memcpy payload.
    memcpy_kind    Enum8('UNKNOWN'=0,'H2D'=1,'D2H'=2,'D2D'=3,'H2H'=4,'P2P'=5) DEFAULT 'UNKNOWN',
    memcpy_bytes   UInt64 DEFAULT 0,
    memcpy_src     UInt32 DEFAULT 0,
    memcpy_dst     UInt32 DEFAULT 0,

    -- API payload.
    api_cbid       UInt32 DEFAULT 0,
    api_status     UInt32 DEFAULT 0,

    INDEX idx_correlation correlation_id TYPE bloom_filter(0.01) GRANULARITY 4,
    INDEX idx_name        name_id        TYPE bloom_filter(0.01) GRANULARITY 4,
    INDEX idx_pid         pid            TYPE minmax             GRANULARITY 4
)
ENGINE = MergeTree
PARTITION BY toStartOfHour(toDateTime64(ts_ns / 1e9, 9))
ORDER BY (node_id, gpu_id, category, ts_ns)
SETTINGS index_granularity = 8192;
