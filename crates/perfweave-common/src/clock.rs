//! Clock alignment between GPU-side sources (CUPTI, DCGM driver callbacks)
//! and the host `CLOCK_REALTIME`.
//!
//! Invariant: every event stored in ClickHouse carries a host-corrected
//! nanosecond timestamp, so `ORDER BY ts_ns` produces a globally consistent
//! ordering regardless of which source produced the event.
//!
//! The mapping is affine: `host_ns = offset_ns + slope_num / slope_den * gpu_ns`.
//! We keep slope as a rational to avoid f64 accumulation over long captures.

use std::time::{SystemTime, UNIX_EPOCH};

/// Affine mapping from a monotonic source clock into host CLOCK_REALTIME ns.
#[derive(Copy, Clone, Debug)]
pub struct ClockOffset {
    pub offset_ns: i128,
    pub slope_num: i128,
    pub slope_den: i128,
    pub residual_max_ns: u64,
    pub fitted_at_host_ns: u64,
}

impl ClockOffset {
    pub const IDENTITY: Self = Self {
        offset_ns: 0,
        slope_num: 1,
        slope_den: 1,
        residual_max_ns: 0,
        fitted_at_host_ns: 0,
    };

    /// Map a raw source-clock timestamp to host-corrected ns.
    #[inline]
    pub fn map(&self, src_ns: u64) -> u64 {
        let src = src_ns as i128;
        let host = self.offset_ns + (self.slope_num * src) / self.slope_den;
        host.max(0) as u64
    }
}

/// One paired observation used to fit a clock offset.
#[derive(Copy, Clone, Debug)]
pub struct Pair {
    pub src_ns: u64,
    pub host_ns: u64,
}

/// Fit an affine mapping using a robust Theil-Sen estimator.
/// Requires at least two pairs; returns residual_max_ns so callers can alert.
///
/// Complexity is O(n^2) which is fine for n <= 1024 (our sampling budget).
pub fn fit_theil_sen(pairs: &[Pair]) -> Option<ClockOffset> {
    if pairs.len() < 2 {
        return None;
    }

    let n = pairs.len();
    // Collect pairwise slopes as rationals (dst_ns delta over src_ns delta).
    // Use i128 arithmetic; at 1s between samples the numerator is ~1e9 which
    // stays well within i128 after multiplication by i128::MAX constants.
    let mut slopes: Vec<f64> = Vec::with_capacity(n * (n - 1) / 2);
    for i in 0..n {
        for j in (i + 1)..n {
            let d_src = pairs[j].src_ns as i128 - pairs[i].src_ns as i128;
            if d_src == 0 {
                continue;
            }
            let d_host = pairs[j].host_ns as i128 - pairs[i].host_ns as i128;
            slopes.push(d_host as f64 / d_src as f64);
        }
    }
    if slopes.is_empty() {
        return None;
    }
    slopes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let slope = slopes[slopes.len() / 2];

    // Median offset under that slope.
    let mut offsets: Vec<f64> = pairs
        .iter()
        .map(|p| p.host_ns as f64 - slope * p.src_ns as f64)
        .collect();
    offsets.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let offset = offsets[offsets.len() / 2];

    // Residual
    let mut res_max: f64 = 0.0;
    for p in pairs {
        let predicted = offset + slope * p.src_ns as f64;
        let residual = (p.host_ns as f64 - predicted).abs();
        if residual > res_max {
            res_max = residual;
        }
    }

    // Convert slope to a rational with denominator 1<<30 for determinism.
    let den: i128 = 1 << 30;
    let num: i128 = (slope * den as f64).round() as i128;

    Some(ClockOffset {
        offset_ns: offset.round() as i128,
        slope_num: num,
        slope_den: den,
        residual_max_ns: res_max.ceil() as u64,
        fitted_at_host_ns: host_realtime_ns(),
    })
}

/// Current host `CLOCK_REALTIME` as unsigned nanoseconds since the unix epoch.
pub fn host_realtime_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Monotonic ns (CLOCK_MONOTONIC on Linux; macOS uses the Mach absolute time
/// translated to ns by std::time::Instant). Used in short timing loops during
/// clock alignment — never written to ClickHouse.
pub fn host_monotonic_ns() -> u64 {
    // std::time::Instant is opaque but internally monotonic. We anchor it to
    // the first call so callers get a ns counter.
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_roundtrip() {
        let id = ClockOffset::IDENTITY;
        assert_eq!(id.map(123_456_789), 123_456_789);
    }

    #[test]
    fn fit_recovers_known_mapping() {
        // Synthesize pairs under host = 2 * src + 1_000_000, no noise.
        let pairs: Vec<Pair> = (0..64)
            .map(|i| Pair {
                src_ns: (i as u64) * 1_000_000,
                host_ns: 2 * (i as u64) * 1_000_000 + 1_000_000,
            })
            .collect();
        let fit = fit_theil_sen(&pairs).unwrap();
        // slope ~= 2.0
        let slope = fit.slope_num as f64 / fit.slope_den as f64;
        assert!((slope - 2.0).abs() < 1e-6, "slope was {slope}");
        assert!(fit.residual_max_ns <= 1, "residual {}", fit.residual_max_ns);
    }

    #[test]
    fn theil_sen_is_robust_to_single_outlier() {
        let mut pairs: Vec<Pair> = (0..32)
            .map(|i| Pair {
                src_ns: (i as u64) * 1_000_000,
                host_ns: (i as u64) * 1_000_000 + 500,
            })
            .collect();
        // Inject a wild outlier.
        pairs[10].host_ns += 5_000_000;
        let fit = fit_theil_sen(&pairs).unwrap();
        let slope = fit.slope_num as f64 / fit.slope_den as f64;
        assert!((slope - 1.0).abs() < 1e-3);
    }
}
