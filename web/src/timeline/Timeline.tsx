// React wrapper around the TimelineRenderer. Owns:
//   - canvas element and DPI
//   - viewport state (start_ns, end_ns)
//   - wheel/drag interaction
//   - click / shift-click / correlation highlight
//   - RAF loop that triggers tile fetches when the LOD threshold crosses

import { useEffect, useMemo, useRef, useState } from "react";
import {
  TimelineRenderer,
  type LaneLayout,
  type InstanceBatch,
  type MetricSeries,
} from "./renderer";
import { generateSynthetic } from "./synthetic";
import { fetchTimeline } from "../api";
import type { SpikeMarker } from "../panels/SidePanel";

export interface Selection {
  kind: "event";
  correlationLo: number;
  startNs: bigint;
  endNs: bigint;
  lane: number;
}

interface Props {
  onSelect?: (sel: Selection | null) => void;
  onMeasure?: (startNs: bigint, endNs: bigint) => void;
  useSynthetic?: boolean;
  viewport: { startNs: bigint; endNs: bigint };
  onViewportChange: (v: { startNs: bigint; endNs: bigint }) => void;
  spikes?: SpikeMarker[];
  onSpikeClick?: (s: SpikeMarker) => void;
  /** When set, metric lanes render from this client-side live ring instead
   *  of Arrow tiles. The keys are `${nodeId}:${gpuId}:${metricId}`. */
  liveSeries?: Map<string, { tsNs: number; value: number }[]> | null;
}

export function Timeline({
  onSelect,
  onMeasure,
  useSynthetic = false,
  viewport,
  onViewportChange,
  spikes = [],
  onSpikeClick,
  liveSeries = null,
}: Props) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const rendererRef = useRef<TimelineRenderer | null>(null);
  const setViewport = onViewportChange;
  const [laneCount, setLaneCount] = useState(12);
  const [instanceCount, setInstanceCount] = useState(0);
  const [fps, setFps] = useState(0);
  const measureAnchor = useRef<bigint | null>(null);

  const lanes: LaneLayout[] = useMemo(() => {
    const out: LaneLayout[] = [];
    const NUM_GPUS = 2;
    for (let g = 0; g < NUM_GPUS; g++) {
      out.push({ index: out.length, heightPx: 28, kind: "metric", label: `gpu${g} util` });
      out.push({ index: out.length, heightPx: 28, kind: "metric", label: `gpu${g} mem` });
      out.push({ index: out.length, heightPx: 28, kind: "metric", label: `gpu${g} power` });
      out.push({ index: out.length, heightPx: 18, kind: "activity", label: `gpu${g} kernels` });
      out.push({ index: out.length, heightPx: 14, kind: "activity", label: `gpu${g} memcpy` });
      out.push({ index: out.length, heightPx: 14, kind: "activity", label: `gpu${g} api` });
    }
    setLaneCount(out.length);
    return out;
  }, []);

  // Initialize renderer
  useEffect(() => {
    const canvas = canvasRef.current!;
    const renderer = new TimelineRenderer(canvas);
    rendererRef.current = renderer;
    renderer.setLanes(lanes);

    const ro = new ResizeObserver(() => {
      if (!canvasRef.current) return;
      const { clientWidth, clientHeight } = canvasRef.current;
      renderer.resize(clientWidth, clientHeight, window.devicePixelRatio || 1);
    });
    ro.observe(canvas);

    return () => {
      ro.disconnect();
      rendererRef.current = null;
    };
  }, [lanes]);

  // Load data whenever viewport changes. Priority:
  //   1. Synthetic mode (Week 1 gate) — self-contained.
  //   2. Live mode — metrics from the client-side ring (no network), and
  //      activity lanes are left to whatever we last fetched. The Live
  //      kernels SSE will later drip-feed activity.
  //   3. Paused mode — Arrow tiles from /api/timeline.
  useEffect(() => {
    const r = rendererRef.current;
    if (!r) return;
    r.setBaseline(viewport.startNs);

    if (useSynthetic) {
      const SIZE = 10_000_000;
      const { batches, metrics } = generateSynthetic({
        numEvents: SIZE,
        spanMs: Number(viewport.endNs - viewport.startNs) / 1_000_000,
        numGpus: 2,
        seed: 7,
      });
      r.setActivity(batches);
      r.setMetrics(metrics);
      setInstanceCount(batches.reduce((a, b) => a + b.starts.length, 0));
      return;
    }

    if (liveSeries) {
      r.setMetrics(liveRingToMetrics(liveSeries, lanes));
      return;
    }

    const width = canvasRef.current?.clientWidth ?? 1920;
    (async () => {
      try {
        const [activityResp, metricResp] = await Promise.all([
          fetchTimeline({
            startNs: viewport.startNs,
            endNs: viewport.endNs,
            pixels: width,
            kind: "activity",
          }),
          fetchTimeline({
            startNs: viewport.startNs,
            endNs: viewport.endNs,
            pixels: width,
            kind: "metric",
          }),
        ]);
        r.setActivity(arrowToBatches(activityResp.table, lanes));
        r.setMetrics(arrowToMetrics(metricResp.table, lanes));
        setInstanceCount(activityResp.table.numRows);
      } catch (e) {
        console.warn("timeline fetch failed; staying on previous data:", e);
      }
    })();
  }, [viewport, useSynthetic, lanes, liveSeries]);

  // RAF loop
  useEffect(() => {
    let frameTimes: number[] = [];
    let raf = 0;
    const loop = (t: number) => {
      const r = rendererRef.current;
      const c = canvasRef.current;
      if (r && c) {
        r.render({
          startNs: viewport.startNs,
          endNs: viewport.endNs,
          widthPx: c.clientWidth,
          heightPx: c.clientHeight,
        });
      }
      frameTimes.push(t);
      while (frameTimes.length > 0 && t - frameTimes[0] > 1000) frameTimes.shift();
      setFps(frameTimes.length);
      raf = requestAnimationFrame(loop);
    };
    raf = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(raf);
  }, [viewport]);

  // Wheel → zoom; drag → pan.
  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault();
    const c = canvasRef.current;
    if (!c) return;
    const rect = c.getBoundingClientRect();
    const xPx = e.clientX - rect.left;
    const range = viewport.endNs - viewport.startNs;
    const centerRatio = xPx / rect.width;
    const centerNs = viewport.startNs + BigInt(Math.floor(Number(range) * centerRatio));
    const factor = e.deltaY > 0 ? 1.25 : 0.8;
    const newRange = BigInt(Math.max(1000, Math.floor(Number(range) * factor)));
    const newStart = centerNs - BigInt(Math.floor(Number(newRange) * centerRatio));
    const newEnd = newStart + newRange;
    setViewport({ startNs: newStart, endNs: newEnd });
  };

  const dragRef = useRef<{ x: number; startNs: bigint; endNs: bigint } | null>(null);

  const onMouseDown = (e: React.MouseEvent) => {
    dragRef.current = { x: e.clientX, startNs: viewport.startNs, endNs: viewport.endNs };
  };
  const onMouseMove = (e: React.MouseEvent) => {
    if (!dragRef.current) return;
    const c = canvasRef.current!;
    const dx = e.clientX - dragRef.current.x;
    const range = dragRef.current.endNs - dragRef.current.startNs;
    const dNs = BigInt(-Math.floor((dx / c.clientWidth) * Number(range)));
    setViewport({ startNs: dragRef.current.startNs + dNs, endNs: dragRef.current.endNs + dNs });
  };
  const onMouseUp = () => { dragRef.current = null; };

  const onClick = (e: React.MouseEvent) => {
    const r = rendererRef.current;
    const c = canvasRef.current;
    if (!r || !c) return;
    const rect = c.getBoundingClientRect();
    const xPx = e.clientX - rect.left;
    const yPx = e.clientY - rect.top;
    const viewportFull = {
      startNs: viewport.startNs,
      endNs: viewport.endNs,
      widthPx: c.clientWidth,
      heightPx: c.clientHeight,
    };
    const hit = r.pick(viewportFull, xPx, yPx);
    if (hit) {
      const batch = (r as unknown as { currentBatches: InstanceBatch[] }).currentBatches[hit.batch];
      const corr = batch.corrLo[hit.index] | 0;
      r.setHighlightCorrLo(corr);
      const startNs = BigInt(Math.floor(batch.starts[hit.index]));
      const endNs = startNs + BigInt(Math.floor(batch.widths[hit.index]));
      if (e.shiftKey && measureAnchor.current != null) {
        onMeasure?.(measureAnchor.current, startNs);
        measureAnchor.current = null;
      } else {
        measureAnchor.current = startNs;
        onSelect?.({ kind: "event", correlationLo: corr, startNs, endNs, lane: hit.lane });
      }
    } else {
      r.setHighlightCorrLo(0);
      onSelect?.(null);
    }
  };

  // Spike pins: sparse, rendered as HTML absolute-positioned divs on top
  // of the canvas. We keep them off the WebGL hot path because they change
  // at the spike-query cadence (~1 Hz), not every frame, and hit-testing
  // them as real DOM elements gives us tooltips and click handlers for
  // free.
  const spikePins = spikes
    .map((s) => {
      const range = Number(viewport.endNs - viewport.startNs);
      if (range <= 0) return null;
      const dx = Number(s.bucketStartNs - viewport.startNs);
      const ratio = dx / range;
      if (ratio < -0.05 || ratio > 1.05) return null;
      return { s, ratio };
    })
    .filter(Boolean) as { s: SpikeMarker; ratio: number }[];

  return (
    <div className="timeline">
      <canvas
        ref={canvasRef}
        onWheel={onWheel}
        onMouseDown={onMouseDown}
        onMouseMove={onMouseMove}
        onMouseUp={onMouseUp}
        onMouseLeave={onMouseUp}
        onClick={onClick}
      />
      <div className="spike-layer">
        {spikePins.map(({ s, ratio }, i) => (
          <div
            key={`${s.bucketStartNs.toString()}-${s.gpuId}-${s.metricId.toString()}-${i}`}
            className="spike-pin"
            style={{ left: `${(ratio * 100).toFixed(4)}%` }}
            title={`spike: gpu${s.gpuId} · z=${s.zMad.toFixed(1)}`}
            onClick={(ev) => { ev.stopPropagation(); onSpikeClick?.(s); }}
          />
        ))}
      </div>
      <div className="overlay-hud">
        {fps} fps · {instanceCount.toLocaleString()} events · {laneCount} lanes ·
        {` ${formatNs(viewport.endNs - viewport.startNs)} window`}
      </div>
    </div>
  );
}

// Decode activity tiles. We produce one instance per non-empty bucket per
// (gpu, category). Rectangle width maps to sum_duration_ns (min 1px in time
// units); color comes from the category byte.
function arrowToBatches(table: import("apache-arrow").Table, lanes: LaneLayout[]): InstanceBatch[] {
  const n = table.numRows;
  if (n === 0) return [];
  const col = (name: string) => table.getChild(name)!;
  const bucketStart = col("bucket_start_ns");
  const bucketWidth = col("bucket_width_ns");
  const gpuIdCol = col("gpu_id");
  const categoryCol = col("category");
  const topName = col("top_name_id");

  // Bucket rows by (gpu_id, category) → lane index.
  const groups = new Map<number, { s: number[]; w: number[]; corr: number[]; color: 0 | 1 | 2 | 5 }>();
  const LANES_PER_GPU = 6;
  for (let i = 0; i < n; i++) {
    const gpu = Number(gpuIdCol.get(i));
    const cat = Number(categoryCol.get(i));
    // Map CUPTI category enum (1..8) to activity lane offset (3..5).
    let laneOffset = -1;
    let color: 0 | 1 | 2 | 5 = 5;
    switch (cat) {
      case 3: laneOffset = 3; color = 0; break;            // KERNEL
      case 4: case 5: laneOffset = 4; color = 1; break;    // MEMCPY / MEMSET
      case 2: laneOffset = 5; color = 2; break;            // API_CALL
      default: continue;
    }
    const laneIdx = gpu * LANES_PER_GPU + laneOffset;
    const key = laneIdx * 10 + color;
    let g = groups.get(key);
    if (!g) {
      g = { s: [], w: [], corr: [], color };
      groups.set(key, g);
    }
    const s = Number(bucketStart.get(i) as bigint | number);
    const w = Number(bucketWidth.get(i) as bigint | number);
    // For tile rendering we draw the whole bucket width so density is
    // preserved visually. Correlation for tiles is the top kernel name.
    g.s.push(s);
    g.w.push(w);
    g.corr.push(Number(topName.get(i) as bigint | number) & 0xffffffff);
  }

  const out: InstanceBatch[] = [];
  for (const [key, g] of groups) {
    const lane = Math.floor(key / 10);
    if (!lanes[lane]) continue;
    out.push({
      lane,
      colorId: g.color,
      starts: Float64Array.from(g.s),
      widths: Float64Array.from(g.w),
      corrLo: Float32Array.from(g.corr),
    });
  }
  return out;
}

// Decode metric tiles. We produce one MetricSeries per (gpu, metric_id).
// Value is normalized per series to [0..1] on the client for MVP; a later
// iteration can do proper axis units per metric.
function arrowToMetrics(table: import("apache-arrow").Table, lanes: LaneLayout[]): MetricSeries[] {
  const n = table.numRows;
  if (n === 0) return [];
  const col = (name: string) => table.getChild(name)!;
  const bucketStart = col("bucket_start_ns");
  const gpuCol = col("gpu_id");
  const metricIdCol = col("metric_id");
  const p99 = col("p99");

  const groups = new Map<string, { ts: number[]; v: number[]; gpu: number }>();
  for (let i = 0; i < n; i++) {
    const gpu = Number(gpuCol.get(i));
    const mid = (metricIdCol.get(i) as bigint).toString();
    const key = `${gpu}:${mid}`;
    let g = groups.get(key);
    if (!g) {
      g = { ts: [], v: [], gpu };
      groups.set(key, g);
    }
    g.ts.push(Number(bucketStart.get(i) as bigint | number));
    g.v.push(Number(p99.get(i)));
  }

  const LANES_PER_GPU = 6;
  const out: MetricSeries[] = [];
  let seriesIndex = 0;
  for (const [, g] of groups) {
    // Assign one of the 3 metric lanes round-robin per gpu; good enough
    // for MVP. A later iteration maps known metric_ids to fixed lanes.
    const laneIdx = g.gpu * LANES_PER_GPU + (seriesIndex % 3);
    seriesIndex++;
    if (!lanes[laneIdx]) continue;
    const n = g.ts.length;
    const pts = new Float64Array(n * 2);
    let vmin = Infinity, vmax = -Infinity;
    for (let i = 0; i < n; i++) { vmin = Math.min(vmin, g.v[i]); vmax = Math.max(vmax, g.v[i]); }
    const span = Math.max(1, vmax - vmin);
    for (let i = 0; i < n; i++) {
      pts[i * 2] = g.ts[i];
      pts[i * 2 + 1] = (g.v[i] - vmin) / span;
    }
    out.push({
      lane: laneIdx,
      color: [1.0, 0.72, 0.29],
      points: pts,
    });
  }
  return out;
}

// Convert the client-side live ring into renderer MetricSeries. We do
// per-series min/max normalization, same as the Arrow path, so the two
// sources look identical when switching between Paused and Live.
function liveRingToMetrics(
  ring: Map<string, { tsNs: number; value: number }[]>,
  lanes: LaneLayout[],
): MetricSeries[] {
  const LANES_PER_GPU = 6;
  const out: MetricSeries[] = [];
  let seriesIndex = 0;
  // Group by gpu so round-robin lane assignment is stable across frames.
  const sortedKeys = [...ring.keys()].sort();
  const perGpu = new Map<number, number>();
  for (const k of sortedKeys) {
    const samples = ring.get(k);
    if (!samples || samples.length < 2) continue;
    const parts = k.split(":");
    const gpu = Number(parts[1]);
    const laneSlot = perGpu.get(gpu) ?? 0;
    perGpu.set(gpu, laneSlot + 1);
    const laneIdx = gpu * LANES_PER_GPU + (laneSlot % 3);
    if (!lanes[laneIdx]) continue;
    const n = samples.length;
    const pts = new Float64Array(n * 2);
    let vmin = Infinity;
    let vmax = -Infinity;
    for (let i = 0; i < n; i++) {
      const v = samples[i].value;
      if (v < vmin) vmin = v;
      if (v > vmax) vmax = v;
    }
    const span = Math.max(1e-9, vmax - vmin);
    for (let i = 0; i < n; i++) {
      pts[i * 2] = samples[i].tsNs;
      pts[i * 2 + 1] = (samples[i].value - vmin) / span;
    }
    out.push({
      lane: laneIdx,
      color: [0.45, 0.9, 0.55],
      points: pts,
    });
    seriesIndex++;
  }
  void seriesIndex;
  return out;
}

function formatNs(n: bigint): string {
  const v = Number(n);
  if (v < 1_000) return `${v}ns`;
  if (v < 1_000_000) return `${(v / 1_000).toFixed(1)}µs`;
  if (v < 1_000_000_000) return `${(v / 1_000_000).toFixed(1)}ms`;
  if (v < 60_000_000_000) return `${(v / 1_000_000_000).toFixed(1)}s`;
  return `${(v / 60_000_000_000).toFixed(1)}min`;
}
