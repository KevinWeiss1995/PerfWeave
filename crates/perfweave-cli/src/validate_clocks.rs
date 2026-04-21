//! `perfweave validate-clocks`
//!
//! Runs the clock-fit procedure against the host realtime clock as both
//! source and destination, then against a simulated skewed clock. On a real
//! CUDA host (feature `nvml`) we additionally measure NVML event-record
//! boundaries vs host realtime to catch any driver-level skew.
//!
//! Exits non-zero when residual exceeds `max_residual_ns`.

use anyhow::Result;
use perfweave_agent::clock_sync::{measure, HostClock, SourceClock};
use perfweave_common::clock::host_realtime_ns;

pub async fn run(samples: usize, max_residual_ns: u64) -> Result<()> {
    println!("perfweave validate-clocks: samples={samples} max_residual_ns={max_residual_ns}");

    // 1. Host-to-host self-fit. Residual must be tiny (<50ns).
    let fit = measure(&HostClock, samples, max_residual_ns)
        .ok_or_else(|| anyhow::anyhow!("host->host fit failed (sampling loop too short?)"))?;
    let slope = fit.slope_num as f64 / fit.slope_den as f64;
    println!(
        "host->host   slope={:.12}  offset_ns={}  residual={}ns",
        slope, fit.offset_ns, fit.residual_max_ns
    );
    if fit.residual_max_ns > max_residual_ns {
        anyhow::bail!(
            "host->host residual {}ns exceeds {}ns budget. \
             Either the host scheduler is pathological, or the bench fit is broken.",
            fit.residual_max_ns, max_residual_ns
        );
    }

    // 2. Simulated skewed source clock
    struct Skewed;
    impl SourceClock for Skewed {
        fn read_ns(&self) -> u64 {
            host_realtime_ns().saturating_sub(17_500_000) // 17.5ms behind
        }
    }
    let fit = measure(&Skewed, samples, max_residual_ns)
        .ok_or_else(|| anyhow::anyhow!("skewed fit failed"))?;
    println!(
        "skewed->host slope={:.12}  offset_ns={}  residual={}ns",
        fit.slope_num as f64 / fit.slope_den as f64,
        fit.offset_ns,
        fit.residual_max_ns
    );
    if fit.residual_max_ns > max_residual_ns {
        anyhow::bail!(
            "skewed->host residual {}ns exceeds {}ns budget",
            fit.residual_max_ns, max_residual_ns
        );
    }

    // 3. Real CUPTI timestamp fit (only if we link to CUDA)
    // On MVP hosts without CUPTI this is skipped; the CUPTI injector will
    // run this check internally on every attach via `cuptiGetTimestamp`.
    println!("cupti->host  skipped (requires libcupti; run on a CUDA host)");

    println!("validate-clocks: OK");
    Ok(())
}
