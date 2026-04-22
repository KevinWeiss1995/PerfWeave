// Metric catalog: classifies agent-emitted metric names into fixed lane
// slots, colors, and value formatters. Without this, every metric series
// gets a round-robin lane and a single color, which is what the old
// timeline did and which made the UI completely unreadable.
//
// The agent emits names like "gpu.util.percent", "gpu.mem.used.bytes",
// "soc.power.watts", etc. (see crates/perfweave-agent/src/sampler/). The
// live SSE stream only carries the xxh3_64 hash of each name; the UI
// resolves names lazily via /graphql `strings` and caches the result.

export type MetricKind =
  | "util"        // 0..100 %
  | "mem"         // bytes or % (normalized to 0..1 per-series)
  | "power"       // watts
  | "temp"        // celsius
  | "clock"       // hz
  | "fan"         // 0..100 %
  | "other";

export interface MetricSpec {
  kind: MetricKind;
  /** Lane slot within a GPU, stable across sessions. Lower = higher on screen. */
  slot: number;
  /** Display color as [r,g,b] in 0..1. Also used for the gutter swatch. */
  rgb: [number, number, number];
  /** Short label shown in the lane gutter. */
  label: string;
  /** Fixed min value for the Y axis of this lane. null = autoscale. */
  yMin: number | null;
  /** Fixed max value for the Y axis of this lane. null = autoscale. */
  yMax: number | null;
  /** Pretty-print a raw value for the live badge. */
  format: (v: number) => string;
}

const PCT = (v: number) => `${v.toFixed(0)}%`;
const W = (v: number) => `${v.toFixed(0)}W`;
const C = (v: number) => `${v.toFixed(0)}°C`;
const GHZ = (v: number) => `${(v / 1e9).toFixed(2)}GHz`;
const BYTES = (v: number) => {
  if (v < 1024) return `${v}B`;
  if (v < 1024 ** 2) return `${(v / 1024).toFixed(1)}K`;
  if (v < 1024 ** 3) return `${(v / 1024 ** 2).toFixed(1)}M`;
  if (v < 1024 ** 4) return `${(v / 1024 ** 3).toFixed(1)}G`;
  return `${(v / 1024 ** 4).toFixed(1)}T`;
};

/** Order of slots on screen: util on top, then mem, power, temp, clock, fan. */
export function classify(name: string): MetricSpec | null {
  const n = name.toLowerCase();
  if (n.endsWith("util.percent") || n === "gpu.util.percent") {
    return {
      kind: "util",
      slot: 0,
      rgb: [0.29, 0.64, 1.0],
      label: "SM util",
      yMin: 0,
      yMax: 100,
      format: PCT,
    };
  }
  if (n.endsWith("mem.util.percent")) {
    return {
      kind: "mem",
      slot: 1,
      rgb: [0.39, 0.83, 0.65],
      label: "MEM util",
      yMin: 0,
      yMax: 100,
      format: PCT,
    };
  }
  if (n.endsWith("mem.used.bytes")) {
    return {
      kind: "mem",
      slot: 1,
      rgb: [0.39, 0.83, 0.65],
      label: "MEM used",
      yMin: null,
      yMax: null,
      format: BYTES,
    };
  }
  if (n.endsWith("mem.free.bytes")) {
    // Free is the inverse view of used; skip to keep the lane clean.
    return null;
  }
  if (n.endsWith("power.watts") || n === "soc.power.watts") {
    return {
      kind: "power",
      slot: 2,
      rgb: [1.0, 0.72, 0.29],
      label: "power",
      yMin: 0,
      yMax: null,
      format: W,
    };
  }
  if (n.endsWith("power.limit.watts")) {
    // Displayed implicitly as the lane's yMax hint (future work); hide for MVP.
    return null;
  }
  if (n.endsWith("temp.celsius")) {
    return {
      kind: "temp",
      slot: 3,
      rgb: [1.0, 0.45, 0.42],
      label: "temp",
      yMin: 20,
      yMax: 95,
      format: C,
    };
  }
  if (n.endsWith("sm.clock.hz") || n.endsWith("clock.hz")) {
    return {
      kind: "clock",
      slot: 4,
      rgb: [0.72, 0.55, 1.0],
      label: "SM clock",
      yMin: 0,
      yMax: null,
      format: GHZ,
    };
  }
  if (n.endsWith("fan.percent")) {
    return {
      kind: "fan",
      slot: 5,
      rgb: [0.6, 0.7, 0.8],
      label: "fan",
      yMin: 0,
      yMax: 100,
      format: PCT,
    };
  }
  return null;
}

/** Total number of metric slots (one lane per slot per GPU). */
export const METRIC_SLOTS = 6;
