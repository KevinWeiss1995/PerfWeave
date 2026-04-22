-- Kernel Speed-of-Light table. One row per (kernel launch, source). Joined
-- to `events` by correlation_id at query time.
--
-- We keep SOL out of `events` so we don't bloat the hot path schema with
-- a dozen float columns that are null for 99.9% of events. Projections on
-- `events(correlation_id)` make the join cheap.
--
-- `source` records whether the numbers came from always-on PM host
-- sampling (low confidence for short kernels, one row per kernel window)
-- or an on-demand replay (high confidence, exact counters). The UI prefers
-- the highest-confidence row when both exist.

CREATE TABLE IF NOT EXISTS kernel_sol
(
    ts_ns          UInt64 CODEC(Delta, ZSTD(1)),
    node_id        UInt32,
    gpu_id         UInt8,
    correlation_id UInt64,

    sm_active_pct          Float32 CODEC(ZSTD(1)),
    achieved_occupancy_pct Float32 CODEC(ZSTD(1)),
    dram_bw_pct            Float32 CODEC(ZSTD(1)),
    l1_bw_pct              Float32 CODEC(ZSTD(1)),
    l2_bw_pct              Float32 CODEC(ZSTD(1)),
    inst_throughput_pct    Float32 CODEC(ZSTD(1)),
    theoretical_occupancy_pct Float32 CODEC(ZSTD(1)),

    arithmetic_intensity   Float32 CODEC(ZSTD(1)),
    achieved_gflops        Float32 CODEC(ZSTD(1)),

    bound Enum8(
        'UNKNOWN'  = 0,
        'MEMORY'   = 1,
        'COMPUTE'  = 2,
        'LATENCY'  = 3,
        'BALANCED' = 4
    ) DEFAULT 'UNKNOWN',

    confidence Enum8(
        'UNKNOWN' = 0,
        'LOW'     = 1,
        'HIGH'    = 2
    ) DEFAULT 'UNKNOWN',

    source Enum8(
        'UNKNOWN'       = 0,
        'HOST_SAMPLING' = 1,
        'REPLAY'        = 2
    ) DEFAULT 'UNKNOWN',

    -- Top-5 stall reasons packed into two arrays. reason ids come from
    -- CUPTI's PC-sampling stall-reason enum; we store them opaquely so a
    -- CUPTI-version bump can't break historical queries.
    stall_reasons Array(UInt32) CODEC(ZSTD(1)),
    stall_counts  Array(UInt32) CODEC(ZSTD(1)),

    INDEX idx_corr correlation_id TYPE bloom_filter(0.01) GRANULARITY 4
)
ENGINE = MergeTree
PARTITION BY toStartOfDay(toDateTime64(ts_ns / 1e9, 9))
ORDER BY (node_id, gpu_id, correlation_id, ts_ns)
SETTINGS index_granularity = 8192;
