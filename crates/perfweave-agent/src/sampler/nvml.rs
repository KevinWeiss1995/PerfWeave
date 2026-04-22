//! NVML sampler. Reads utilization, memory, power, clocks, and throttle
//! reasons at `metric_hz` Hz. NVML is the always-on baseline; overhead is
//! negligible (well under 0.1% CPU at 1 Hz, which is our default).
//!
//! Emits one `MetricFrame` per (gpu, tick) carrying every metric for that
//! GPU at that timestamp. This is *much* cheaper on the gRPC path than the
//! old "one Event per metric per tick" pattern (11 events per gpu per tick
//! collapsed into 1 frame with 11 points).
//!
//! Timestamps are host realtime (NVML samples are already wall-clock), so
//! there is no clock correction needed on the server for these frames.
//!
//! This module is only compiled with `--features nvml`.

use super::Sampler;
use crate::ring::EventRing;
use async_trait::async_trait;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::{Device, Nvml};
use perfweave_common::intern::hash as intern_hash;
use perfweave_proto::v1::{MetricFrame, MetricPoint};
use std::sync::Arc;
use std::time::Duration;

pub struct NvmlSampler {
    pub node_id: u32,
    pub metric_hz: u32,
}

#[async_trait]
impl Sampler for NvmlSampler {
    async fn run(
        self: Box<Self>,
        ring: Arc<EventRing>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let nvml = match Nvml::init() {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error=%e, "NVML init failed; run `perfweave doctor` to diagnose");
                return;
            }
        };

        let device_count = match nvml.device_count() {
            Ok(n) => n as u32,
            Err(e) => {
                tracing::error!(error=%e, "NVML device_count failed");
                return;
            }
        };
        tracing::info!(device_count, hz = self.metric_hz, "NVML sampler online");

        let period = Duration::from_millis((1000 / self.metric_hz.max(1)) as u64);
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let ids = MetricIds::new();

        loop {
            tokio::select! {
                _ = shutdown.changed() => if *shutdown.borrow() { break; },
                _ = tick.tick() => {
                    let now_ns = perfweave_common::clock::host_realtime_ns();
                    for idx in 0..device_count {
                        let dev = match nvml.device_by_index(idx) {
                            Ok(d) => d,
                            Err(e) => {
                                tracing::warn!(gpu=idx, error=%e, "device_by_index failed");
                                continue;
                            }
                        };
                        let frame = build_frame(&dev, now_ns, idx, self.node_id, &ids);
                        ring.push_frame(frame);
                    }
                }
            }
        }
        tracing::info!("NVML sampler stopping");
    }
}

struct MetricIds {
    util: u64,
    mem_util: u64,
    mem_used: u64,
    mem_free: u64,
    power: u64,
    power_limit: u64,
    sm_clock: u64,
    mem_clock: u64,
    temp: u64,
    fan: u64,
    throttle: u64,
}

impl MetricIds {
    fn new() -> Self {
        Self {
            util: intern_hash("gpu.util.percent"),
            mem_util: intern_hash("gpu.mem.util.percent"),
            mem_used: intern_hash("gpu.mem.used.bytes"),
            mem_free: intern_hash("gpu.mem.free.bytes"),
            power: intern_hash("gpu.power.watts"),
            power_limit: intern_hash("gpu.power.limit.watts"),
            sm_clock: intern_hash("gpu.sm.clock.hz"),
            mem_clock: intern_hash("gpu.mem.clock.hz"),
            temp: intern_hash("gpu.temp.celsius"),
            fan: intern_hash("gpu.fan.percent"),
            throttle: intern_hash("gpu.throttle.reasons.bitmask"),
        }
    }
}

fn build_frame(
    dev: &Device<'_>,
    now_ns: u64,
    gpu_id: u32,
    node_id: u32,
    ids: &MetricIds,
) -> MetricFrame {
    let mut points: Vec<MetricPoint> = Vec::with_capacity(11);
    let mut push = |metric_id: u64, v: f64| points.push(MetricPoint { metric_id, value: v });

    if let Ok(u) = dev.utilization_rates() {
        push(ids.util, u.gpu as f64);
        push(ids.mem_util, u.memory as f64);
    }
    if let Ok(m) = dev.memory_info() {
        push(ids.mem_used, m.used as f64);
        push(ids.mem_free, m.free as f64);
    }
    if let Ok(p) = dev.power_usage() {
        push(ids.power, p as f64 / 1000.0);
    }
    if let Ok(p) = dev.enforced_power_limit() {
        push(ids.power_limit, p as f64 / 1000.0);
    }
    if let Ok(c) = dev.clock_info(Clock::SM) {
        push(ids.sm_clock, c as f64 * 1.0e6);
    }
    if let Ok(c) = dev.clock_info(Clock::Memory) {
        push(ids.mem_clock, c as f64 * 1.0e6);
    }
    if let Ok(t) = dev.temperature(TemperatureSensor::Gpu) {
        push(ids.temp, t as f64);
    }
    if let Ok(f) = dev.fan_speed(0) {
        push(ids.fan, f as f64);
    }
    if let Ok(reasons) = dev.current_throttle_reasons() {
        push(ids.throttle, reasons.bits() as f64);
    }

    MetricFrame { node_id, gpu_id, ts_ns: now_ns, points }
}
