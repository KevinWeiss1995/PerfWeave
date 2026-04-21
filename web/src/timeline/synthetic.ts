// Synthetic client-side generator so the Week 1 frontend gate is testable
// without a running API. Generates N million events sampled from a realistic
// kernel/API/memcpy mix on 2 GPUs. Matches the instance buffer layout used
// by the real renderer 1:1, so perf numbers measured against this dataset
// carry to production.

import type { InstanceBatch, MetricSeries } from "./renderer";

export interface SyntheticOpts {
  numEvents: number;
  spanMs: number;
  numGpus: number;
  seed?: number;
}

/**
 * Generate `numEvents` kernel/memcpy/api events across `numGpus` GPUs,
 * spread over `spanMs` milliseconds. Returns one instance batch per
 * (gpu, category) lane.
 *
 * This is where we prove the 10M-events@60FPS gate. Call with numEvents=10_000_000
 * and check the chrome perf tab.
 */
export function generateSynthetic(opts: SyntheticOpts): {
  start: bigint;
  end: bigint;
  batches: InstanceBatch[];
  metrics: MetricSeries[];
} {
  const { numEvents, spanMs, numGpus } = opts;
  const rng = mulberry32(opts.seed ?? 1);

  const startNs = BigInt(Date.now()) * 1_000_000n;
  const spanNs = BigInt(spanMs) * 1_000_000n;
  const endNs = startNs + spanNs;

  // Lane ordering (top → bottom):
  //   gpu0 util (metric), gpu0 mem (metric), gpu0 power (metric),
  //   gpu0 kernels (activity), gpu0 memcpys, gpu0 api,
  //   gpu1 ...
  const LANES_PER_GPU = 6;

  // Three activity batches per gpu: KERNEL, MEMCPY, API.
  const perCat: Record<number, { start: number[]; width: number[]; corr: number[] }> = {};
  for (let g = 0; g < numGpus; g++) {
    for (const off of [0, 1, 2]) {
      perCat[g * 10 + off] = { start: [], width: [], corr: [] };
    }
  }

  // Generate events
  let corrCounter = 1;
  for (let i = 0; i < numEvents; i++) {
    const gpu = i % numGpus;
    const cat = pickCategory(rng);
    const t = Number(spanNs) * rng();
    const startEvent = Number(startNs) + t;
    const width = eventDuration(rng, cat);
    const corr = corrCounter++;
    const slot = perCat[gpu * 10 + cat];
    slot.start.push(startEvent);
    slot.width.push(width);
    slot.corr.push(corr & 0xffffffff);
  }

  const batches: InstanceBatch[] = [];
  for (let g = 0; g < numGpus; g++) {
    for (const cat of [0, 1, 2]) {
      const slot = perCat[g * 10 + cat];
      const laneIndex = g * LANES_PER_GPU + 3 + cat; // 3 metric lanes first
      batches.push({
        lane: laneIndex,
        colorId: (cat === 0 ? 0 : cat === 1 ? 1 : 2) as 0 | 1 | 2,
        starts: new Float64Array(slot.start),
        widths: new Float64Array(slot.width),
        corrLo: new Float32Array(slot.corr),
      });
    }
  }

  // Metrics: 3 per GPU, 1 sample per 10ms.
  const metrics: MetricSeries[] = [];
  const samples = Math.max(1, Math.floor(spanMs / 10));
  for (let g = 0; g < numGpus; g++) {
    for (let m = 0; m < 3; m++) {
      const pts = new Float64Array(samples * 2);
      for (let i = 0; i < samples; i++) {
        const ts = Number(startNs) + (i / samples) * Number(spanNs);
        const vn = 0.15 + 0.5 * Math.abs(Math.sin(i / (40 + m * 13))) + rng() * 0.1;
        pts[i * 2] = ts;
        pts[i * 2 + 1] = vn;
      }
      metrics.push({
        lane: g * LANES_PER_GPU + m,
        color: m === 0 ? [1.0, 0.72, 0.29] : m === 1 ? [0.53, 0.78, 1.0] : [0.97, 0.5, 0.3],
        points: pts,
      });
    }
  }

  return { start: startNs, end: endNs, batches, metrics };
}

function mulberry32(a: number): () => number {
  return function () {
    let t = (a += 0x6d2b79f5);
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function pickCategory(rng: () => number): 0 | 1 | 2 {
  const r = rng();
  if (r < 0.75) return 0; // kernel (majority)
  if (r < 0.88) return 1; // memcpy
  return 2; // api
}

function eventDuration(rng: () => number, cat: 0 | 1 | 2): number {
  // Durations in ns, roughly gamma-distributed.
  if (cat === 0) return 1000 + rng() * 300_000;
  if (cat === 1) return 1000 + rng() * 50_000;
  return 500 + rng() * 5_000;
}
