//! Arrow IPC serialization of timeline tiles. Used both for activity lanes
//! and metric lanes. The schema is intentionally simple and columnar so the
//! frontend can zero-copy it into WebGL buffers after one Arrow decode.

use anyhow::Result;
use arrow::array::{Float64Array, UInt32Array, UInt64Array, UInt8Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use bytes::Bytes;
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct ActivityTileRows {
    pub bucket_start_ns: Vec<u64>,
    pub bucket_width_ns: Vec<u64>,
    pub gpu_id: Vec<u8>,
    pub category: Vec<u8>,
    pub count: Vec<u64>,
    pub sum_duration_ns: Vec<u64>,
    pub max_duration_ns: Vec<u64>,
    pub top_name_id: Vec<u64>,
}

pub fn activity_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("bucket_start_ns", DataType::UInt64, false),
        Field::new("bucket_width_ns", DataType::UInt64, false),
        Field::new("gpu_id", DataType::UInt8, false),
        Field::new("category", DataType::UInt8, false),
        Field::new("count", DataType::UInt64, false),
        Field::new("sum_duration_ns", DataType::UInt64, false),
        Field::new("max_duration_ns", DataType::UInt64, false),
        Field::new("top_name_id", DataType::UInt64, false),
    ]))
}

pub fn activity_to_arrow(rows: &ActivityTileRows) -> Result<Bytes> {
    let schema = activity_schema();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(rows.bucket_start_ns.clone())),
            Arc::new(UInt64Array::from(rows.bucket_width_ns.clone())),
            Arc::new(UInt8Array::from(rows.gpu_id.clone())),
            Arc::new(UInt8Array::from(rows.category.clone())),
            Arc::new(UInt64Array::from(rows.count.clone())),
            Arc::new(UInt64Array::from(rows.sum_duration_ns.clone())),
            Arc::new(UInt64Array::from(rows.max_duration_ns.clone())),
            Arc::new(UInt64Array::from(rows.top_name_id.clone())),
        ],
    )?;
    encode(batch, schema)
}

#[derive(Debug, Default)]
pub struct MetricTileRows {
    pub bucket_start_ns: Vec<u64>,
    pub bucket_width_ns: Vec<u64>,
    pub gpu_id: Vec<u8>,
    pub metric_id: Vec<u64>,
    pub p50: Vec<f64>,
    pub p99: Vec<f64>,
    pub min: Vec<f64>,
    pub max: Vec<f64>,
}

pub fn metric_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("bucket_start_ns", DataType::UInt64, false),
        Field::new("bucket_width_ns", DataType::UInt64, false),
        Field::new("gpu_id", DataType::UInt8, false),
        Field::new("metric_id", DataType::UInt64, false),
        Field::new("p50", DataType::Float64, false),
        Field::new("p99", DataType::Float64, false),
        Field::new("min", DataType::Float64, false),
        Field::new("max", DataType::Float64, false),
    ]))
}

pub fn metric_to_arrow(rows: &MetricTileRows) -> Result<Bytes> {
    let schema = metric_schema();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(rows.bucket_start_ns.clone())),
            Arc::new(UInt64Array::from(rows.bucket_width_ns.clone())),
            Arc::new(UInt8Array::from(rows.gpu_id.clone())),
            Arc::new(UInt64Array::from(rows.metric_id.clone())),
            Arc::new(Float64Array::from(rows.p50.clone())),
            Arc::new(Float64Array::from(rows.p99.clone())),
            Arc::new(Float64Array::from(rows.min.clone())),
            Arc::new(Float64Array::from(rows.max.clone())),
        ],
    )?;
    encode(batch, schema)
}

#[derive(Debug, Default)]
pub struct RawEventRows {
    pub ts_ns: Vec<u64>,
    pub duration_ns: Vec<u64>,
    pub gpu_id: Vec<u8>,
    pub category: Vec<u8>,
    pub name_id: Vec<u64>,
    pub correlation_id: Vec<u64>,
    pub pid: Vec<u32>,
    pub stream_id: Vec<u32>,
}

pub fn raw_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("ts_ns", DataType::UInt64, false),
        Field::new("duration_ns", DataType::UInt64, false),
        Field::new("gpu_id", DataType::UInt8, false),
        Field::new("category", DataType::UInt8, false),
        Field::new("name_id", DataType::UInt64, false),
        Field::new("correlation_id", DataType::UInt64, false),
        Field::new("pid", DataType::UInt32, false),
        Field::new("stream_id", DataType::UInt32, false),
    ]))
}

pub fn raw_to_arrow(rows: &RawEventRows) -> Result<Bytes> {
    let schema = raw_schema();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(rows.ts_ns.clone())),
            Arc::new(UInt64Array::from(rows.duration_ns.clone())),
            Arc::new(UInt8Array::from(rows.gpu_id.clone())),
            Arc::new(UInt8Array::from(rows.category.clone())),
            Arc::new(UInt64Array::from(rows.name_id.clone())),
            Arc::new(UInt64Array::from(rows.correlation_id.clone())),
            Arc::new(UInt32Array::from(rows.pid.clone())),
            Arc::new(UInt32Array::from(rows.stream_id.clone())),
        ],
    )?;
    encode(batch, schema)
}

fn encode(batch: RecordBatch, schema: Arc<Schema>) -> Result<Bytes> {
    let mut buf = Vec::<u8>::with_capacity(batch.get_array_memory_size() + 1024);
    {
        let mut w = StreamWriter::try_new(&mut buf, &schema)?;
        w.write(&batch)?;
        w.finish()?;
    }
    Ok(Bytes::from(buf))
}
