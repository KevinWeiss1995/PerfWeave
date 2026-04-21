//! NVML sampler. Reads utilization, memory, power, clocks, and throttle
//! reasons at `metric_hz` Hz. NVML is the always-on baseline; overhead is
//! negligible (well under 0.1% CPU at 100 Hz).
//!
//! All values are emitted as `Category::METRIC` events with host realtime
//! timestamps. NVML returns its samples on host realtime already, so there
//! is no clock correction — the agent's ClockOffset for NVML is IDENTITY.
//!
//! This module is only compiled with `--features nvml`; otherwise the binary
//! has no CUDA runtime dependency.

use super::Sampler;
use crate::ring::EventRing;
use async_trait::async_trait;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::{Device, Nvml};
use perfweave_common::intern::hash as intern_hash;
use perfweave_proto::v1::{event::Payload, Category, Event, MetricSample};
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
        tracing::info!(device_count, "NVML sampler online");

        let period = Duration::from_millis((1000 / self.metric_hz.max(1)) as u64);
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let util_id = intern_hash("gpu.util.percent");
        let mem_util_id = intern_hash("gpu.mem.util.percent");
        let mem_used_id = intern_hash("gpu.mem.used.bytes");
        let mem_free_id = intern_hash("gpu.mem.free.bytes");
        let power_id = intern_hash("gpu.power.watts");
        let power_limit_id = intern_hash("gpu.power.limit.watts");
        let sm_clock_id = intern_hash("gpu.sm.clock.hz");
        let mem_clock_id = intern_hash("gpu.mem.clock.hz");
        let temp_id = intern_hash("gpu.temp.celsius");
        let fan_id = intern_hash("gpu.fan.percent");
        let throttle_id = intern_hash("gpu.throttle.reasons.bitmask");

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
                        sample_device(&dev, now_ns, idx, self.node_id, &ring,
                            util_id, mem_util_id, mem_used_id, mem_free_id, power_id,
                            power_limit_id, sm_clock_id, mem_clock_id, temp_id, fan_id,
                            throttle_id);
                    }
                }
            }
        }
        tracing::info!("NVML sampler stopping");
    }
}

#[allow(clippy::too_many_arguments)]
fn sample_device(
    dev: &Device<'_>,
    now_ns: u64,
    gpu_id: u32,
    node_id: u32,
    ring: &EventRing,
    util_id: u64,
    mem_util_id: u64,
    mem_used_id: u64,
    mem_free_id: u64,
    power_id: u64,
    power_limit_id: u64,
    sm_clock_id: u64,
    mem_clock_id: u64,
    temp_id: u64,
    fan_id: u64,
    throttle_id: u64,
) {
    let emit = |metric_id: u64, value: f64, unit: &str| {
        ring.push(Event {
            ts_ns: now_ns,
            duration_ns: 0,
            node_id,
            gpu_id,
            pid: 0, tid: 0, ctx_id: 0, stream_id: 0,
            category: Category::Metric as i32,
            name_id: metric_id,
            correlation_id: 0,
            parent_id: 0,
            payload: Some(Payload::Metric(MetricSample {
                metric_id, value, unit: unit.to_string(),
            })),
        });
    };

    if let Ok(u) = dev.utilization_rates() {
        emit(util_id, u.gpu as f64, "percent");
        emit(mem_util_id, u.memory as f64, "percent");
    }
    if let Ok(m) = dev.memory_info() {
        emit(mem_used_id, m.used as f64, "bytes");
        emit(mem_free_id, m.free as f64, "bytes");
    }
    if let Ok(p) = dev.power_usage() {
        emit(power_id, p as f64 / 1000.0, "watts");
    }
    if let Ok(p) = dev.enforced_power_limit() {
        emit(power_limit_id, p as f64 / 1000.0, "watts");
    }
    if let Ok(c) = dev.clock_info(Clock::SM) {
        emit(sm_clock_id, c as f64 * 1.0e6, "hz");
    }
    if let Ok(c) = dev.clock_info(Clock::Memory) {
        emit(mem_clock_id, c as f64 * 1.0e6, "hz");
    }
    if let Ok(t) = dev.temperature(TemperatureSensor::Gpu) {
        emit(temp_id, t as f64, "celsius");
    }
    if let Ok(f) = dev.fan_speed(0) {
        emit(fan_id, f as f64, "percent");
    }
    if let Ok(reasons) = dev.current_throttle_reasons() {
        emit(throttle_id, reasons.bits() as f64, "bitmask");
    }
}
