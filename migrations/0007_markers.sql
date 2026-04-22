-- NVTX / range markers. Previously `Payload::Marker` events were silently
-- dropped by the collector because there were no columns to write them
-- to. The sidecar-join option needs a GROUP BY on every marker query to
-- look sane; adding sparse columns on the existing `events` table is
-- cheaper in both CH storage (LowCardinality + sparse default) and code
-- complexity at the query path.
--
-- `marker_message_id` is the interned message body; `Event.name_id` is the
-- NVTX range tag. They may be identical (common for cheap push/pop) or
-- different (message text != tag for NVTX_RESOURCE_ATTRIBUTE_APPLIED).

ALTER TABLE events
    ADD COLUMN IF NOT EXISTS marker_domain_id UInt64 DEFAULT 0,
    ADD COLUMN IF NOT EXISTS marker_color     UInt32 DEFAULT 0,
    ADD COLUMN IF NOT EXISTS marker_message_id UInt64 DEFAULT 0;

-- Extend `nodes` with device peaks so the UI can draw rooflines without
-- hardcoding per-GPU numbers. Values are set by the agent on Register.
ALTER TABLE nodes
    ADD COLUMN IF NOT EXISTS peak_fp32_gflops Float32 DEFAULT 0,
    ADD COLUMN IF NOT EXISTS peak_dram_gb_s   Float32 DEFAULT 0,
    ADD COLUMN IF NOT EXISTS device_name      String  DEFAULT '',
    ADD COLUMN IF NOT EXISTS compute_capability String DEFAULT '';
