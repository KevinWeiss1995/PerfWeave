//! LOD picker. Given a (start, end) range and a target pixel width, choose
//! the coarsest tile width such that approximately one bucket maps to one
//! pixel. If the finest tile is still coarser than (end-start)/pixels, we
//! serve raw events — the query path handles that branch.

use perfweave_common::TILE_WIDTHS_NS;

pub fn choose_tile(range_ns: u64, pixels: u32) -> Option<u64> {
    if pixels == 0 {
        return TILE_WIDTHS_NS.first().copied();
    }
    let target = range_ns / pixels as u64;
    // Pick smallest tile >= target, else largest tile.
    let mut chosen: Option<u64> = None;
    for &w in TILE_WIDTHS_NS {
        if w >= target {
            chosen = Some(w);
            break;
        }
    }
    chosen.or_else(|| TILE_WIDTHS_NS.last().copied())
}

pub fn tile_table_name(_bucket_width_ns: u64, metric: bool) -> &'static str {
    if metric {
        "tiles_metric"
    } else {
        "tiles_activity"
    }
}

/// Return the tile width we should fall through to when the chosen width is
/// finer than (range/pixels) but we still want tile-level aggregation. Used
/// as the raw-event trigger: if `range_ns < TILE_WIDTHS_NS[0] * pixels` we
/// serve raw events.
pub fn should_serve_raw(range_ns: u64, pixels: u32) -> bool {
    pixels > 0 && range_ns / (pixels as u64) < TILE_WIDTHS_NS[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_second_over_1024_px_picks_4ms() {
        // 1s / 1024 px ~= 976us per px → pick 4ms (the 4_096_000 bucket).
        let w = choose_tile(1_000_000_000, 1024).unwrap();
        assert_eq!(w, 4_096_000);
    }

    #[test]
    fn narrow_range_serves_raw() {
        // 100us range over 1024 px: 97.6ns/px; finer than 1us → raw.
        assert!(should_serve_raw(100_000, 1024));
    }
}
