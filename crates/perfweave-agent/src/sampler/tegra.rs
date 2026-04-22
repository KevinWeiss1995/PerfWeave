//! Tegra/Jetson sampler.
//!
//! NVML on Tegra (Orin, Xavier) is crippled: `nvmlDeviceGetUtilizationRates`
//! returns `NVML_ERROR_NOT_SUPPORTED`, power/clock APIs are flaky, and
//! `nvmlDeviceGetMemoryInfo` reports the whole system DRAM (Tegra is UMA,
//! so there is no distinct VRAM pool). Trying to use NVML there gives the
//! UI a permanently flat line.
//!
//! Workaround: scrape sysfs directly. These paths are documented by NVIDIA
//! for L4T (Linux for Tegra) and have been stable since Xavier. On Orin AGX
//! the relevant nodes are:
//!
//!   * `/sys/class/devfreq/17000000.gv11b/cur_freq`     — GPU clock (Hz)
//!   * `/sys/class/devfreq/17000000.gv11b/load`         — SM "load" (0..1000)
//!   * `/sys/class/thermal/thermal_zone*/temp`          — temps (milli-°C)
//!   * `/sys/bus/i2c/drivers/ina3221/.../in_power*_input` — SoC power (mW)
//!   * `/proc/meminfo`                                  — UMA memory
//!
//! Any of these paths that don't exist on a given board are silently
//! skipped; we never let a missing node kill the sampler.
//!
//! This is platform-gated (linux only) but not feature-gated — the sampler
//! is so small it compiles into every agent and auto-selects itself when
//! NVML isn't viable. See `is_tegra()` for detection.

use super::Sampler;
use crate::ring::EventRing;
use async_trait::async_trait;
use perfweave_common::intern::hash as intern_hash;
use perfweave_proto::v1::{MetricFrame, MetricPoint};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub struct TegraSampler {
    pub node_id: u32,
    pub metric_hz: u32,
}

/// Heuristic: a Tegra system has `/proc/device-tree/model` containing
/// "NVIDIA" and either "Orin", "Xavier" or "Jetson". We also accept an
/// env override for CI and for devboards whose dtb model string is
/// customised.
pub fn is_tegra() -> bool {
    if std::env::var_os("PERFWEAVE_FORCE_TEGRA").is_some() {
        return true;
    }
    let Ok(model) = std::fs::read_to_string("/proc/device-tree/model") else {
        return false;
    };
    let m = model.to_ascii_lowercase();
    m.contains("nvidia") && (m.contains("orin") || m.contains("xavier") || m.contains("jetson"))
}

#[async_trait]
impl Sampler for TegraSampler {
    async fn run(
        self: Box<Self>,
        ring: Arc<EventRing>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let paths = discover_paths();
        tracing::info!(
            hz = self.metric_hz,
            gpu_clock = ?paths.gpu_clock.as_ref().map(|p| p.display().to_string()),
            gpu_load = ?paths.gpu_load.as_ref().map(|p| p.display().to_string()),
            temp_zones = paths.temp_zones.len(),
            power_rails = paths.power_rails.len(),
            "Tegra sampler online"
        );

        let ids = MetricIds::new();
        let period = Duration::from_millis((1000 / self.metric_hz.max(1)) as u64);
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown.changed() => if *shutdown.borrow() { break; },
                _ = tick.tick() => {
                    let now_ns = perfweave_common::clock::host_realtime_ns();
                    let frame = build_frame(&paths, &ids, now_ns, self.node_id);
                    ring.push_frame(frame);
                }
            }
        }
        tracing::info!("Tegra sampler stopping");
    }
}

#[derive(Debug, Default)]
struct DiscoveredPaths {
    gpu_clock: Option<PathBuf>,
    gpu_load: Option<PathBuf>,
    temp_zones: Vec<(String, PathBuf)>,
    /// INA3221 rails: (rail_name, path to milliwatts input).
    power_rails: Vec<(String, PathBuf)>,
}

fn discover_paths() -> DiscoveredPaths {
    let mut out = DiscoveredPaths::default();

    // GPU devfreq nodes. Orin AGX uses 17000000.gv11b, Xavier uses
    // 17000000.gp10b, various dev kits use 13000000.gpu. We probe all of
    // them and keep the first that exists.
    const DEVFREQ_GPU_CANDIDATES: &[&str] = &[
        "/sys/class/devfreq/17000000.gv11b",
        "/sys/class/devfreq/17000000.gp10b",
        "/sys/class/devfreq/57000000.gpu",
        "/sys/class/devfreq/13000000.gpu",
    ];
    for p in DEVFREQ_GPU_CANDIDATES {
        let root = Path::new(p);
        if root.exists() {
            if root.join("cur_freq").exists() {
                out.gpu_clock = Some(root.join("cur_freq"));
            }
            if root.join("device").join("load").exists() {
                out.gpu_load = Some(root.join("device").join("load"));
            } else if root.join("load").exists() {
                out.gpu_load = Some(root.join("load"));
            }
            break;
        }
    }

    // Thermal zones. On Orin the per-zone `type` file tells us what it
    // measures; we keep the interesting ones (CPU, GPU, AO, SoC) and drop
    // board-level noise.
    if let Ok(rd) = std::fs::read_dir("/sys/class/thermal") {
        for e in rd.flatten() {
            let p = e.path();
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else { continue };
            if !name.starts_with("thermal_zone") { continue; }
            let type_path = p.join("type");
            let temp_path = p.join("temp");
            if !type_path.exists() || !temp_path.exists() { continue; }
            let Ok(t) = std::fs::read_to_string(&type_path) else { continue };
            let t = t.trim().to_string();
            let tl = t.to_ascii_lowercase();
            if tl.contains("gpu") || tl.contains("cpu") || tl.contains("soc") || tl.contains("tj") {
                out.temp_zones.push((t, temp_path));
            }
        }
    }

    // INA3221 power rails. Three-channel shunt ADCs that L4T exposes under
    // /sys/bus/i2c/drivers/ina3221/*/hwmon/hwmon*/in_power*_input (in mW).
    // The rail labels live at in_label*.
    if let Ok(rd) = std::fs::read_dir("/sys/bus/i2c/drivers/ina3221") {
        for dev in rd.flatten() {
            let hwmon_root = dev.path().join("hwmon");
            let Ok(hwmon_rd) = std::fs::read_dir(&hwmon_root) else { continue };
            for hw in hwmon_rd.flatten() {
                let hwp = hw.path();
                for idx in 0..8u8 {
                    let val = hwp.join(format!("in_power{idx}_input"));
                    let lab = hwp.join(format!("in_label{idx}"));
                    if val.exists() {
                        let label = std::fs::read_to_string(&lab)
                            .ok()
                            .map(|s| s.trim().to_string())
                            .unwrap_or_else(|| format!("rail{idx}"));
                        out.power_rails.push((label, val));
                    }
                }
            }
        }
    }

    out
}

struct MetricIds {
    util: u64,
    sm_clock: u64,
    mem_used: u64,
    mem_free: u64,
    power_total: u64,
    temp: u64,
}

impl MetricIds {
    fn new() -> Self {
        Self {
            util: intern_hash("gpu.util.percent"),
            sm_clock: intern_hash("gpu.sm.clock.hz"),
            mem_used: intern_hash("gpu.mem.used.bytes"),
            mem_free: intern_hash("gpu.mem.free.bytes"),
            power_total: intern_hash("soc.power.watts"),
            temp: intern_hash("gpu.temp.celsius"),
        }
    }
}

fn build_frame(paths: &DiscoveredPaths, ids: &MetricIds, now_ns: u64, node_id: u32) -> MetricFrame {
    let mut points: Vec<MetricPoint> = Vec::with_capacity(6);

    // GPU load on devfreq is 0..1000 → percent.
    if let Some(p) = &paths.gpu_load {
        if let Some(v) = read_num::<f64>(p) {
            points.push(MetricPoint { metric_id: ids.util, value: v / 10.0 });
        }
    }
    if let Some(p) = &paths.gpu_clock {
        if let Some(v) = read_num::<f64>(p) {
            points.push(MetricPoint { metric_id: ids.sm_clock, value: v });
        }
    }
    // Temperature: pick the hottest GPU-labeled zone; millidegrees → °C.
    let gpu_temp = paths
        .temp_zones
        .iter()
        .filter(|(t, _)| t.to_ascii_lowercase().contains("gpu"))
        .filter_map(|(_, p)| read_num::<f64>(p).map(|v| v / 1000.0))
        .fold(None::<f64>, |acc, v| Some(acc.map_or(v, |a| a.max(v))));
    if let Some(t) = gpu_temp {
        points.push(MetricPoint { metric_id: ids.temp, value: t });
    }
    // UMA memory: /proc/meminfo. Tegra has no separate VRAM so we report
    // system memory as the GPU memory numbers; users understand UMA.
    if let Some((used, free)) = read_meminfo() {
        points.push(MetricPoint { metric_id: ids.mem_used, value: used as f64 });
        points.push(MetricPoint { metric_id: ids.mem_free, value: free as f64 });
    }
    // Sum every INA3221 rail into a single SoC power number. Individual
    // rails are noisy and board-specific; total is what the UI wants.
    let total_mw: f64 = paths
        .power_rails
        .iter()
        .filter_map(|(_, p)| read_num::<f64>(p))
        .sum();
    if total_mw > 0.0 {
        points.push(MetricPoint { metric_id: ids.power_total, value: total_mw / 1000.0 });
    }

    MetricFrame { node_id, gpu_id: 0, ts_ns: now_ns, points }
}

fn read_num<T: std::str::FromStr>(path: &Path) -> Option<T> {
    let raw = std::fs::read_to_string(path).ok()?;
    raw.trim().parse::<T>().ok()
}

fn read_meminfo() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total_kb = None;
    let mut free_kb = None;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = rest.split_whitespace().next().and_then(|v| v.parse::<u64>().ok());
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            free_kb = rest.split_whitespace().next().and_then(|v| v.parse::<u64>().ok());
        }
    }
    match (total_kb, free_kb) {
        (Some(t), Some(f)) => Some(((t.saturating_sub(f)) * 1024, f * 1024)),
        _ => None,
    }
}
