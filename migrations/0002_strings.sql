-- String dictionary. Keyed by xxh3-64 of the text. Populated via upserts from
-- the collector on first observation. ReplacingMergeTree means a re-announce
-- with the same id is idempotent; the FINAL modifier collapses duplicates
-- at query time, but we rarely need it since ids are stable.
CREATE TABLE IF NOT EXISTS strings
(
    id    UInt64,
    text  String,
    first_seen_ns UInt64 DEFAULT now64(9)
)
ENGINE = ReplacingMergeTree(first_seen_ns)
ORDER BY id;

-- Clock offsets per (node, gpu). Written every 10s by each agent; retained
-- forever so we can audit corrections.
CREATE TABLE IF NOT EXISTS clock_offsets
(
    fitted_at_ns    UInt64,
    node_id         UInt32,
    gpu_id          UInt8,
    offset_ns       Int64,
    slope_num       Int64,
    slope_den       Int64,
    residual_max_ns UInt64
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(toDateTime64(fitted_at_ns / 1e9, 9))
ORDER BY (node_id, gpu_id, fitted_at_ns);

-- Known nodes + hostnames.
CREATE TABLE IF NOT EXISTS nodes
(
    node_id       UInt32,
    hostname      String,
    agent_version String,
    num_gpus      UInt8,
    first_seen_ns UInt64 DEFAULT now64(9),
    last_seen_ns  UInt64 DEFAULT now64(9)
)
ENGINE = ReplacingMergeTree(last_seen_ns)
ORDER BY node_id;
