//! `perfweave doctor`: environment checks with actionable fixes.
//!
//! Every check reports one of:
//!   ok | warn | fail
//! along with a single sentence explaining what to do. Non-zero exit if any
//! `fail` occurs.

use anyhow::Result;
use serde::Serialize;

#[derive(Serialize, Debug)]
pub struct Report {
    checks: Vec<Check>,
    summary: &'static str,
}

#[derive(Serialize, Debug)]
pub struct Check {
    name: &'static str,
    status: &'static str,
    detail: String,
    fix: Option<&'static str>,
}

pub async fn run(json: bool) -> Result<()> {
    let mut checks = Vec::new();
    checks.push(check_os());
    checks.push(check_nvml().await);
    checks.push(check_cupti());
    checks.push(check_dcgm());
    checks.push(check_nsys());
    checks.push(check_clickhouse().await);

    let failed = checks.iter().any(|c| c.status == "fail");
    let report = Report {
        checks,
        summary: if failed { "some checks failed; see fixes above" } else { "ready" },
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for c in &report.checks {
            let tag = match c.status {
                "ok" => "OK ",
                "warn" => "WARN",
                _ => "FAIL",
            };
            println!("[{tag}] {:<14} {}", c.name, c.detail);
            if let Some(fix) = c.fix {
                if c.status != "ok" {
                    println!("       fix: {fix}");
                }
            }
        }
        println!();
        println!("summary: {}", report.summary);
    }

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn check_os() -> Check {
    let os = std::env::consts::OS;
    if os == "linux" {
        Check { name: "os", status: "ok", detail: format!("linux {}", std::env::consts::ARCH), fix: None }
    } else {
        Check {
            name: "os",
            status: "warn",
            detail: format!("{os}: CUPTI/DCGM unavailable; only synthetic mode will work"),
            fix: Some("run on a Linux + CUDA 12+ host for real GPU telemetry"),
        }
    }
}

async fn check_nvml() -> Check {
    #[cfg(feature = "nvml")]
    {
        match nvml_wrapper::Nvml::init() {
            Ok(n) => match n.device_count() {
                Ok(c) => Check { name: "nvml", status: "ok", detail: format!("{c} device(s) detected"), fix: None },
                Err(e) => Check { name: "nvml", status: "fail", detail: format!("device_count: {e}"), fix: Some("update the NVIDIA driver; `nvidia-smi` should work") },
            },
            Err(e) => Check {
                name: "nvml", status: "fail",
                detail: format!("init failed: {e}"),
                fix: Some("install the NVIDIA driver; libnvidia-ml.so.1 must be on the loader path"),
            },
        }
    }
    #[cfg(not(feature = "nvml"))]
    {
        Check {
            name: "nvml", status: "warn",
            detail: "binary built without NVML feature".into(),
            fix: Some("rebuild with `cargo build --release --features nvml,cupti`"),
        }
    }
}

fn check_cupti() -> Check {
    let candidates = [
        "/usr/local/cuda/extras/CUPTI/lib64/libcupti.so",
        "/usr/local/cuda/lib64/libcupti.so",
        "/opt/cuda/extras/CUPTI/lib64/libcupti.so",
    ];
    for p in candidates {
        if std::path::Path::new(p).exists() {
            return Check { name: "cupti", status: "ok", detail: format!("found {p}"), fix: None };
        }
    }
    Check {
        name: "cupti", status: "warn",
        detail: "libcupti.so not found in standard locations".into(),
        fix: Some("install CUDA Toolkit 12.x; CUPTI ships under $CUDA/extras/CUPTI"),
    }
}

fn check_dcgm() -> Check {
    for p in ["/usr/lib/x86_64-linux-gnu/libdcgm.so.3", "/usr/lib64/libdcgm.so.3", "/usr/local/lib/libdcgm.so.3"] {
        if std::path::Path::new(p).exists() {
            return Check { name: "dcgm", status: "ok", detail: format!("found {p}"), fix: None };
        }
    }
    Check {
        name: "dcgm", status: "warn",
        detail: "libdcgm.so.3 not found (optional; profiling metrics will be absent)".into(),
        fix: Some("install datacenter-gpu-manager (`apt install datacenter-gpu-manager`)"),
    }
}

fn check_nsys() -> Check {
    match std::process::Command::new("nsys").arg("--version").output() {
        Ok(o) if o.status.success() => Check {
            name: "nsys", status: "ok",
            detail: String::from_utf8_lossy(&o.stdout).trim().to_string(),
            fix: None,
        },
        _ => Check {
            name: "nsys", status: "warn",
            detail: "`nsys` not on PATH (offline import disabled)".into(),
            fix: Some("install Nsight Systems 2024.x or newer; add nsys to PATH"),
        },
    }
}

async fn check_clickhouse() -> Check {
    let url = std::env::var("PERFWEAVE_CH_URL").unwrap_or_else(|_| "http://127.0.0.1:8123".into());
    let client = clickhouse::Client::default().with_url(&url);
    match client.query("SELECT 1").fetch_one::<u8>().await {
        Ok(_) => Check { name: "clickhouse", status: "ok", detail: url, fix: None },
        Err(e) => Check {
            name: "clickhouse", status: "fail",
            detail: format!("{url}: {e}"),
            fix: Some("start ClickHouse: docker run -d -p 8123:8123 -p 9000:9000 clickhouse/clickhouse-server:24.8"),
        },
    }
}
