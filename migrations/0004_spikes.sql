-- Spike detection. Rolling median + MAD over a 30-bucket window (30 * 262ms
-- ~= 7.8s at the 262ms LOD). A bucket is a spike when
--     |value - median| / MAD > 3.5   (approx 3-sigma for gaussian data).
--
-- Running entirely inside ClickHouse means the UI just selects from this
-- table within its viewport, with zero client-side computation.
--
-- We materialize spikes at the 262ms LOD which is the default timeline
-- resolution; finer LODs are computed on demand in the API when the viewport
-- is narrow enough.

CREATE TABLE IF NOT EXISTS spikes
(
    bucket_start_ns UInt64,
    bucket_width_ns UInt64,
    node_id         UInt32,
    gpu_id          UInt8,
    metric_id       UInt64,
    value           Float64,
    median          Float64,
    mad             Float64,
    z_mad           Float64
)
ENGINE = MergeTree
PARTITION BY toYYYYMMDD(toDateTime(intDiv(bucket_start_ns, 1000000000)))
ORDER BY (node_id, gpu_id, metric_id, bucket_start_ns)
-- TTL expression must return Date/DateTime (not DateTime64) per CH >= 23.x.
TTL toDateTime(intDiv(bucket_start_ns, 1000000000)) + INTERVAL 30 DAY;

-- A view the UI can SELECT from; the actual refresh is driven by the API
-- running an INSERT ... SELECT on a timer (ClickHouse lacks a true rolling
-- window MV before 24.x). This decision keeps the MVP working on 23.x LTS.
CREATE OR REPLACE VIEW spikes_recent_v AS
SELECT *
FROM spikes
WHERE z_mad >= 3.5
  AND bucket_start_ns >= toUnixTimestamp64Nano(now64(9)) - 3600000000000;
