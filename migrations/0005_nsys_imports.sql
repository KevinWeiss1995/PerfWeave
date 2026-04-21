-- Record of offline trace imports. Users drag-drop .nsys-rep or .ncu-rep
-- files; the importer records one row here so the UI can show the merged
-- traces in the session picker.
CREATE TABLE IF NOT EXISTS imports
(
    import_id     UUID,
    created_at_ns UInt64 DEFAULT now64(9),
    filename      String,
    kind          Enum8('NSYS_REP'=1,'NCU_REP'=2),
    node_id       UInt32,
    event_count   UInt64,
    bytes_read    UInt64,
    ts_min_ns     UInt64,
    ts_max_ns     UInt64,
    status        Enum8('QUEUED'=1,'RUNNING'=2,'DONE'=3,'FAILED'=4),
    error_message String DEFAULT ''
)
ENGINE = ReplacingMergeTree(created_at_ns)
ORDER BY import_id;
