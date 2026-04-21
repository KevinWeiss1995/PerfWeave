//! Synthetic sampler: generates a realistic mix of metrics and kernel
//! activity so the UI + API can be developed and load-tested on machines
//! without an NVIDIA GPU. NOT a mock of NVML/CUPTI — it writes the SAME
//! canonical events, just with stochastic values.

use super::Sampler;
use crate::ring::EventRing;
use async_trait::async_trait;
use perfweave_common::intern::hash as intern_hash;
use perfweave_proto::v1::{
    event::Payload, Category, Event, KernelDetail, MemcpyDetail, MetricSample,
};
use rand::{rngs::SmallRng, Rng, SeedableRng};
use rand_distr::Distribution;
use std::sync::Arc;

pub struct SyntheticSampler {
    pub num_gpus: u32,
    pub metric_hz: u32,
    pub node_id: u32,
    pub seed: u64,
}

#[async_trait]
impl Sampler for SyntheticSampler {
    async fn run(
        self: Box<Self>,
        ring: Arc<EventRing>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut rng = SmallRng::seed_from_u64(self.seed);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(
            (1000 / self.metric_hz.max(1)) as u64,
        ));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Kernel name pool — the hash is deterministic so the dictionary is stable.
        let kernel_names: Vec<(&str, u64)> = KERNEL_NAMES
            .iter()
            .map(|n| (*n, intern_hash(n)))
            .collect();
        let api_names: Vec<(&str, u64)> = API_NAMES.iter().map(|n| (*n, intern_hash(n))).collect();
        let metric_names: Vec<(&str, u64)> = METRIC_NAMES
            .iter()
            .map(|n| (*n, intern_hash(n)))
            .collect();

        let mut next_corr: u64 = 1;

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    let now_ns = perfweave_common::clock::host_realtime_ns();
                    for gpu in 0..self.num_gpus {
                        // Metrics
                        for (name, id) in &metric_names {
                            let (value, unit) = synth_metric(name, &mut rng);
                            ring.push(Event {
                                ts_ns: now_ns,
                                duration_ns: 0,
                                node_id: self.node_id,
                                gpu_id: gpu,
                                pid: 0, tid: 0, ctx_id: 0, stream_id: 0,
                                category: Category::Metric as i32,
                                name_id: *id,
                                correlation_id: 0,
                                parent_id: 0,
                                payload: Some(Payload::Metric(MetricSample {
                                    metric_id: *id,
                                    value,
                                    unit: unit.to_string(),
                                })),
                            });
                        }
                        // Burst of kernel + api + memcpy activity
                        let kernels_this_tick: u32 = rng.gen_range(4..32);
                        for _ in 0..kernels_this_tick {
                            let (kname, kid) = kernel_names[rng.gen_range(0..kernel_names.len())];
                            let _ = kname;
                            let (aname, aid) = api_names[rng.gen_range(0..api_names.len())];
                            let _ = aname;
                            let corr = next_corr;
                            next_corr += 1;
                            let api_start = now_ns + rng.gen_range(0..1_000_000);
                            let api_dur: u64 = rng.gen_range(500..5_000);
                            let kernel_start = api_start + api_dur + rng.gen_range(1_000..20_000);
                            let kernel_dur: u64 = gamma_us(&mut rng, 80.0, 3.0);
                            ring.push(Event {
                                ts_ns: api_start, duration_ns: api_dur,
                                node_id: self.node_id, gpu_id: gpu,
                                pid: 12345, tid: 1, ctx_id: 1, stream_id: rng.gen_range(0..4),
                                category: Category::ApiCall as i32,
                                name_id: aid, correlation_id: corr, parent_id: 0,
                                payload: Some(Payload::Api(perfweave_proto::v1::ApiCallDetail {
                                    cbid: rng.gen_range(1..400), status: 0,
                                })),
                            });
                            ring.push(Event {
                                ts_ns: kernel_start, duration_ns: kernel_dur,
                                node_id: self.node_id, gpu_id: gpu,
                                pid: 12345, tid: 1, ctx_id: 1, stream_id: rng.gen_range(0..4),
                                category: Category::Kernel as i32,
                                name_id: kid, correlation_id: corr, parent_id: 0,
                                payload: Some(Payload::Kernel(KernelDetail {
                                    grid_x: rng.gen_range(1..512),
                                    grid_y: 1, grid_z: 1,
                                    block_x: 128, block_y: 1, block_z: 1,
                                    static_shared_mem_bytes: 0,
                                    dynamic_shared_mem_bytes: rng.gen_range(0..16384),
                                    registers_per_thread: rng.gen_range(16..64),
                                    local_mem_per_thread: 0,
                                    launch_cbid: 211,
                                })),
                            });
                        }
                        // Memcpy
                        if rng.gen_bool(0.3) {
                            let bytes: u64 = rng.gen_range(4_096..16_777_216);
                            let dur = ((bytes as f64 / 12.0e9) * 1e9) as u64 + rng.gen_range(500..2000);
                            ring.push(Event {
                                ts_ns: now_ns + rng.gen_range(0..500_000),
                                duration_ns: dur,
                                node_id: self.node_id, gpu_id: gpu,
                                pid: 12345, tid: 1, ctx_id: 1, stream_id: 0,
                                category: Category::Memcpy as i32,
                                name_id: intern_hash("cudaMemcpyH2D"),
                                correlation_id: 0, parent_id: 0,
                                payload: Some(Payload::Memcpy(MemcpyDetail {
                                    kind: perfweave_proto::v1::memcpy_detail::Kind::H2d as i32,
                                    bytes, src_device: 0, dst_device: gpu,
                                })),
                            });
                        }
                    }
                }
            }
        }
    }
}

fn synth_metric(name: &str, rng: &mut SmallRng) -> (f64, &'static str) {
    match name {
        "gpu.util.percent" => (clamp(rng.gen_range(0.0..100.0) + sinewave(rng, 20.0), 0.0, 100.0), "percent"),
        "gpu.mem.used.bytes" => ((rng.gen_range(8e9..20e9)) as f64, "bytes"),
        "gpu.power.watts" => (rng.gen_range(60.0..350.0), "watts"),
        "gpu.sm.clock.hz" => (rng.gen_range(1.0e9..1.9e9), "hz"),
        "gpu.mem.bandwidth.bytes_per_sec" => (rng.gen_range(0.0..900e9), "bytes_per_sec"),
        _ => (0.0, ""),
    }
}

fn sinewave(rng: &mut SmallRng, amplitude: f64) -> f64 {
    // Injects a slow wave so the metric lane has visible structure in demos.
    let t = rng.gen::<f64>();
    (t * std::f64::consts::TAU).sin() * amplitude
}

fn clamp(v: f64, lo: f64, hi: f64) -> f64 { v.max(lo).min(hi) }

fn gamma_us(rng: &mut SmallRng, mean_us: f64, shape: f64) -> u64 {
    let scale = mean_us / shape;
    let g = rand_distr::Gamma::new(shape, scale).unwrap();
    ((g.sample(rng) * 1_000.0) as u64).max(500)
}

const METRIC_NAMES: &[&str] = &[
    "gpu.util.percent",
    "gpu.mem.used.bytes",
    "gpu.power.watts",
    "gpu.sm.clock.hz",
    "gpu.mem.bandwidth.bytes_per_sec",
];

const KERNEL_NAMES: &[&str] = &[
    "void cutlass::Kernel<cutlass::gemm::kernel::Gemm>(...)",
    "void at::native::vectorized_elementwise_kernel<...>(int, ...)",
    "void at::native::reduce_kernel<512, 1, ...>()",
    "void flash_attention::mha_fwd_kernel<...>()",
    "void cub::DeviceReduceKernel<...>()",
    "void nccl::ncclAllReduceRing<...>()",
    "void cudnn::conv::Conv2dKernel<...>()",
];

const API_NAMES: &[&str] = &[
    "cuLaunchKernel", "cudaMemcpyAsync", "cudaMalloc", "cudaFree",
    "cudaStreamSynchronize", "cudaEventRecord", "cudaMemsetAsync",
];
