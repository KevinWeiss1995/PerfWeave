// React wrapper around the TimelineRenderer. Owns:
//   - canvas element and DPI
//   - viewport (pans/zooms its parent-controlled state; in Live mode it also
//     owns an internal RAF-driven virtual clock so the canvas scrolls
//     continuously at 60 fps even when metric samples only arrive at 1 Hz)
//   - wheel/drag/dblclick interaction (scroll = pan, Ctrl+scroll = zoom)
//   - left gutter with lane labels, right badges with live values, bottom
//     time axis with tick marks
//   - per-metric lane assignment by resolved metric name (so "util", "mem",
//     "power" always land in the same slot with the same color)

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  TimelineRenderer,
  type LaneLayout,
  type InstanceBatch,
  type MetricSeries,
} from "./renderer";
import { generateSynthetic } from "./synthetic";
import { fetchTimeline, resolveStrings } from "../api";
import { classify, METRIC_SLOTS, type MetricSpec } from "./metrics";
import type { SpikeMarker } from "../panels/SidePanel";
import type { LiveKernel } from "../live";

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
   *  of Arrow tiles. Keys are `${nodeId}:${gpuId}:${metricId}`. */
  liveSeries?: Map<string, { tsNs: number; value: number }[]> | null;
  /** When set, the activity (kernels) lane renders from this client-side
   *  kernel ring fed by `/api/live/kernels`. */
  liveKernels?: LiveKernel[] | null;
  /** Bumps ~10 Hz when new kernels arrive. Used as the effect heartbeat
   *  since the ring reference itself is stable. */
  liveKernelsTick?: number;
  /** Server's most recent sample ts_ns (from the SSE stream). Used to anchor
   *  the virtual clock so the tape scrolls in lockstep with the backend. */
  liveLastTsNs?: bigint;
  /** True when the user wants the timeline to follow real time. */
  live?: boolean;
}

const NUM_GPUS = 2;
const ACTIVITY_LANES_PER_GPU = 3;     // kernels, memcpy, api
const LANES_PER_GPU = METRIC_SLOTS + ACTIVITY_LANES_PER_GPU;
const METRIC_LANE_HEIGHT = 34;
const ACTIVITY_LANE_HEIGHT = 16;
const LIVE_WINDOW_NS = 60_000_000_000n;
const MIN_RANGE_NS = 1_000_000n;          // 1 ms
const MAX_RANGE_NS = 24n * 60n * 60n * 1_000_000_000n;   // 24 hr

const ACTIVITY_LABELS = ["kernels", "memcpy", "api"];
const ACTIVITY_COLORS: Array<[number, number, number]> = [
  [0.29, 0.64, 1.0],
  [0.39, 0.83, 0.65],
  [0.72, 0.55, 1.0],
];

export function Timeline({
  onSelect,
  onMeasure,
  useSynthetic = false,
  viewport,
  onViewportChange,
  spikes = [],
  onSpikeClick,
  liveSeries = null,
  liveKernels = null,
  liveKernelsTick = 0,
  liveLastTsNs = 0n,
  live = false,
}: Props) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const mainRef = useRef<HTMLDivElement | null>(null);
  const rendererRef = useRef<TimelineRenderer | null>(null);
  const [fps, setFps] = useState(0);
  const measureAnchor = useRef<bigint | null>(null);

  // Refs that the RAF loop reads on every frame. We keep these out of React
  // state because setting state on each frame (a) triggers re-renders and
  // (b) causes any effect that depends on them to tear down and re-fire,
  // which turns a 60 fps loop into a 1 fps loop.
  const nowNsRef = useRef<bigint>(liveLastTsNs || BigInt(Date.now()) * 1_000_000n);
  const viewportRef = useRef(viewport);
  const liveRef = useRef(live);
  const liveLastTsNsRef = useRef(liveLastTsNs);
  useEffect(() => { viewportRef.current = viewport; }, [viewport]);
  useEffect(() => { liveRef.current = live; }, [live]);
  useEffect(() => { liveLastTsNsRef.current = liveLastTsNs; }, [liveLastTsNs]);

  // --- Resolve metric_id → name lazily via /graphql. Keyed off
  //     liveLastTsNs because the live ring's Map reference is stable, so
  //     we need an explicit heartbeat to notice new metric_ids.
  const [metricCatalog, setMetricCatalog] = useState<Map<string, MetricSpec>>(new Map());
  useEffect(() => {
    if (!liveSeries) return;
    const unknown: bigint[] = [];
    for (const key of liveSeries.keys()) {
      const metricIdStr = key.split(":")[2];
      if (!metricCatalog.has(metricIdStr)) unknown.push(BigInt(metricIdStr));
    }
    if (unknown.length === 0) return;
    let cancelled = false;
    resolveStrings(unknown)
      .then((names) => {
        if (cancelled) return;
        setMetricCatalog((prev) => {
          const next = new Map(prev);
          for (const [id, name] of names) {
            const spec = classify(name);
            next.set(id.toString(), spec ?? {
              kind: "other",
              slot: METRIC_SLOTS - 1,
              rgb: [0.5, 0.6, 0.7],
              label: name,
              yMin: null,
              yMax: null,
              format: (v) => v.toFixed(1),
            });
          }
          return next;
        });
      })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [liveSeries, liveLastTsNs, metricCatalog]);

  // --- Lane layout. Stable: per GPU, 6 metric slots then 3 activity lanes.
  const lanes: LaneLayout[] = useMemo(() => {
    const out: LaneLayout[] = [];
    for (let g = 0; g < NUM_GPUS; g++) {
      for (let s = 0; s < METRIC_SLOTS; s++) {
        out.push({ index: out.length, heightPx: METRIC_LANE_HEIGHT, kind: "metric", label: `gpu${g}` });
      }
      for (let a = 0; a < ACTIVITY_LANES_PER_GPU; a++) {
        out.push({ index: out.length, heightPx: ACTIVITY_LANE_HEIGHT, kind: "activity", label: `gpu${g} ${ACTIVITY_LABELS[a]}` });
      }
    }
    return out;
  }, []);

  // --- Gutter model: one row per lane, with label + color + last known value.
  const gutterRows = useMemo(() => {
    type Row = { laneIdx: number; label: string; sub: string; rgb: [number, number, number]; heightPx: number };
    const rows: Row[] = [];
    const laneLiveValue = new Map<number, number>();   // laneIdx -> latest value
    if (liveSeries) {
      for (const [key, samples] of liveSeries) {
        if (samples.length === 0) continue;
        const [, gpuStr, mid] = key.split(":");
        const gpu = Number(gpuStr);
        const spec = metricCatalog.get(mid);
        if (!spec) continue;
        const laneIdx = gpu * LANES_PER_GPU + spec.slot;
        laneLiveValue.set(laneIdx, samples[samples.length - 1].value);
      }
    }
    for (let g = 0; g < NUM_GPUS; g++) {
      for (let s = 0; s < METRIC_SLOTS; s++) {
        const laneIdx = g * LANES_PER_GPU + s;
        const spec = firstSpecForSlot(metricCatalog, s);
        const val = laneLiveValue.get(laneIdx);
        rows.push({
          laneIdx,
          label: spec ? spec.label : slotPlaceholder(s),
          sub: `gpu${g}${val != null && spec ? " · " + spec.format(val) : ""}`,
          rgb: spec?.rgb ?? [0.4, 0.45, 0.5],
          heightPx: METRIC_LANE_HEIGHT,
        });
      }
      for (let a = 0; a < ACTIVITY_LANES_PER_GPU; a++) {
        const laneIdx = g * LANES_PER_GPU + METRIC_SLOTS + a;
        rows.push({
          laneIdx,
          label: ACTIVITY_LABELS[a],
          sub: `gpu${g}`,
          rgb: ACTIVITY_COLORS[a],
          heightPx: ACTIVITY_LANE_HEIGHT,
        });
      }
    }
    return rows;
  }, [metricCatalog, liveSeries]);

  // --- Initialize renderer
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

  // --- Load data when the viewport or live heartbeat ticks. In live mode
  //     `liveLastTsNs` changes ~1 Hz and is our signal that the ring has
  //     fresh samples (the Map reference itself is stable by design).
  useEffect(() => {
    const r = rendererRef.current;
    if (!r) return;
    r.setBaseline(viewport.startNs);

    if (useSynthetic) {
      const SIZE = 10_000_000;
      const { batches, metrics } = generateSynthetic({
        numEvents: SIZE,
        spanMs: Number(viewport.endNs - viewport.startNs) / 1_000_000,
        numGpus: NUM_GPUS,
        seed: 7,
      });
      r.setActivity(batches);
      r.setMetrics(metrics);
      return;
    }

    if (liveSeries) {
      r.setMetrics(
        liveRingToMetrics(liveSeries, metricCatalog, LANES_PER_GPU, nowNsRef.current),
      );
      return;
    }

    const width = canvasRef.current?.clientWidth ?? 1920;
    (async () => {
      try {
        const [activityResp, metricResp] = await Promise.all([
          fetchTimeline({ startNs: viewport.startNs, endNs: viewport.endNs, pixels: width, kind: "activity" }),
          fetchTimeline({ startNs: viewport.startNs, endNs: viewport.endNs, pixels: width, kind: "metric" }),
        ]);
        r.setActivity(arrowToBatches(activityResp.table, LANES_PER_GPU));
        r.setMetrics(arrowToMetrics(metricResp.table, metricCatalog, LANES_PER_GPU));
      } catch (e) {
        console.warn("timeline fetch failed; staying on previous data:", e);
      }
    })();
  }, [viewport, useSynthetic, lanes, liveSeries, liveLastTsNs, metricCatalog]);

  // --- Rebuild activity (kernels) lane from the live kernel ring. Keyed
  //     on liveKernelsTick (~10 Hz heartbeat) instead of the ring
  //     reference, which is stable.
  useEffect(() => {
    const r = rendererRef.current;
    if (!r) return;
    if (useSynthetic) return;
    if (!liveKernels) return;
    r.setActivity(liveKernelsToBatches(liveKernels, LANES_PER_GPU));
  }, [liveKernels, liveKernelsTick, useSynthetic]);

  // --- RAF: advance virtual clock, render. This effect mounts ONCE and
  //     reads everything through refs. Putting `nowNs` in state caused the
  //     effect to tear down on every frame, which is what gave us the
  //     pathological 1 fps.
  useEffect(() => {
    let frames: number[] = [];
    let raf = 0;
    let lastFpsSet = 0;
    const loop = (t: number) => {
      const r = rendererRef.current;
      const c = canvasRef.current;

      // Virtual clock. In live mode we advance by wall time so the tape
      // scrolls smoothly between 1 Hz samples. When a fresher server ts
      // arrives we snap forward so we stay aligned.
      if (liveRef.current) {
        const wall = BigInt(Date.now()) * 1_000_000n;
        const last = liveLastTsNsRef.current;
        const anchored = last > 0n && last > wall ? last : wall;
        nowNsRef.current =
          anchored > nowNsRef.current ? anchored : nowNsRef.current + 16_666_666n;
      }

      if (r && c) {
        const vp = viewportRef.current;
        const endNs = liveRef.current ? nowNsRef.current : vp.endNs;
        const startNs = liveRef.current
          ? endNs - (vp.endNs - vp.startNs)
          : vp.startNs;
        r.render({
          startNs,
          endNs,
          widthPx: c.clientWidth,
          heightPx: c.clientHeight,
        });
      }
      frames.push(t);
      while (frames.length > 0 && t - frames[0] > 1000) frames.shift();
      // Throttle fps setState to at most every 500ms so we don't re-render
      // the whole React tree on every frame.
      if (t - lastFpsSet > 500) {
        setFps(frames.length);
        lastFpsSet = t;
      }
      raf = requestAnimationFrame(loop);
    };
    raf = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(raf);
  }, []);

  // --- Effective viewport helper for pointer math. Used outside the RAF
  //     loop (wheel/click handlers and axis-tick memos). Relies on React
  //     re-rendering when liveLastTsNs changes, which happens ~1 Hz.
  const effViewport = useCallback(() => {
    if (live) {
      const range = viewport.endNs - viewport.startNs;
      const nowNs = nowNsRef.current;
      return { startNs: nowNs - range, endNs: nowNs };
    }
    return viewport;
  }, [live, viewport, liveLastTsNs]);

  // --- Wheel: plain = horizontal pan, Ctrl/Cmd = zoom at cursor.
  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault();
    const c = canvasRef.current;
    if (!c) return;
    const rect = c.getBoundingClientRect();
    const { startNs, endNs } = effViewport();
    const range = endNs - startNs;

    if (e.ctrlKey || e.metaKey) {
      const xPx = e.clientX - rect.left;
      const centerRatio = Math.max(0, Math.min(1, xPx / rect.width));
      const centerNs = startNs + BigInt(Math.floor(Number(range) * centerRatio));
      const factor = e.deltaY > 0 ? 1.25 : 0.8;
      let newRange = BigInt(Math.max(Number(MIN_RANGE_NS), Math.floor(Number(range) * factor)));
      if (newRange > MAX_RANGE_NS) newRange = MAX_RANGE_NS;
      const newStart = centerNs - BigInt(Math.floor(Number(newRange) * centerRatio));
      const newEnd = newStart + newRange;
      onViewportChange({ startNs: newStart, endNs: newEnd });
      return;
    }

    // Plain pan. One deltaY unit ≈ 1 pixel ≈ range/width.
    const deltaPx = (e.shiftKey ? e.deltaY : e.deltaY + e.deltaX);
    const dNs = BigInt(Math.floor((deltaPx / rect.width) * Number(range)));
    if (dNs === 0n) return;
    onViewportChange({ startNs: startNs + dNs, endNs: endNs + dNs });
  };

  const dragRef = useRef<{ x: number; startNs: bigint; endNs: bigint } | null>(null);
  const onMouseDown = (e: React.MouseEvent) => {
    const { startNs, endNs } = effViewport();
    dragRef.current = { x: e.clientX, startNs, endNs };
  };
  const onMouseMove = (e: React.MouseEvent) => {
    if (!dragRef.current) return;
    const c = canvasRef.current!;
    const dx = e.clientX - dragRef.current.x;
    const range = dragRef.current.endNs - dragRef.current.startNs;
    const dNs = BigInt(-Math.floor((dx / c.clientWidth) * Number(range)));
    onViewportChange({
      startNs: dragRef.current.startNs + dNs,
      endNs: dragRef.current.endNs + dNs,
    });
  };
  const onMouseUp = () => { dragRef.current = null; };

  // Double-click: snap to "now" with the default window. Feels like the
  // equivalent of hitting Home.
  const onDoubleClick = () => {
    const end = live ? nowNsRef.current : BigInt(Date.now()) * 1_000_000n;
    onViewportChange({ startNs: end - LIVE_WINDOW_NS, endNs: end });
  };

  const onClick = (e: React.MouseEvent) => {
    const r = rendererRef.current;
    const c = canvasRef.current;
    if (!r || !c) return;
    const rect = c.getBoundingClientRect();
    const xPx = e.clientX - rect.left;
    const yPx = e.clientY - rect.top;
    const v = effViewport();
    const hit = r.pick(
      { startNs: v.startNs, endNs: v.endNs, widthPx: c.clientWidth, heightPx: c.clientHeight },
      xPx,
      yPx,
    );
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

  // --- Spike pins (HTML absolute-positioned on top of canvas).
  const v = effViewport();
  const spikePins = spikes
    .map((s) => {
      const range = Number(v.endNs - v.startNs);
      if (range <= 0) return null;
      const dx = Number(s.bucketStartNs - v.startNs);
      const ratio = dx / range;
      if (ratio < -0.05 || ratio > 1.05) return null;
      return { s, ratio };
    })
    .filter(Boolean) as { s: SpikeMarker; ratio: number }[];

  // --- Time axis ticks.
  const axisTicks = useMemo(() => buildTicks(v.startNs, v.endNs), [v.startNs, v.endNs]);

  // Total lane stack height used for gutter vertical positions.
  const totalLaneH = lanes.reduce((a, l) => a + l.heightPx, 0);

  return (
    <div className="timeline">
      <div className="timeline-gutter" style={{ height: totalLaneH }}>
        {gutterRows.map((row) => (
          <div
            key={row.laneIdx}
            className="lane-label"
            style={{ height: row.heightPx }}
          >
            <span
              className="lane-swatch"
              style={{ background: rgbCss(row.rgb) }}
            />
            <span className="lane-label-text">
              <span className="lane-label-name">{row.label}</span>
              <span className="lane-label-sub">{row.sub}</span>
            </span>
          </div>
        ))}
      </div>

      <div className="timeline-main" ref={mainRef}>
        <canvas
          ref={canvasRef}
          onWheel={onWheel}
          onMouseDown={onMouseDown}
          onMouseMove={onMouseMove}
          onMouseUp={onMouseUp}
          onMouseLeave={onMouseUp}
          onClick={onClick}
          onDoubleClick={onDoubleClick}
        />
        {/* Right-edge fade: subtle "data incoming" gradient. */}
        {live && <div className="live-edge-fade" />}
        {/* Now line: vertical rule at the right edge in live mode. */}
        {live && <div className="now-line" />}
        {/* Spike pins. */}
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
      </div>

      {/* Time axis at the bottom. Renders HTML tick marks anchored to the
          main grid column so they line up with the canvas. */}
      <div className="timeline-axis-spacer" />
      <div className="timeline-axis">
        {axisTicks.map((tick, i) => (
          <div
            key={i}
            className={`axis-tick${tick.major ? " major" : ""}`}
            style={{ left: `${(tick.ratio * 100).toFixed(4)}%` }}
          >
            <span>{tick.label}</span>
          </div>
        ))}
      </div>

      <div className="overlay-hud">
        {fps} fps · {formatNs(v.endNs - v.startNs)} window
        {live && " · live"}
      </div>
    </div>
  );
}

// --- Helpers ---------------------------------------------------------------

function rgbCss([r, g, b]: [number, number, number]): string {
  return `rgb(${Math.round(r * 255)}, ${Math.round(g * 255)}, ${Math.round(b * 255)})`;
}

function firstSpecForSlot(catalog: Map<string, MetricSpec>, slot: number): MetricSpec | null {
  for (const spec of catalog.values()) if (spec.slot === slot) return spec;
  return null;
}

function slotPlaceholder(slot: number): string {
  return ["SM util", "mem", "power", "temp", "SM clock", "fan"][slot] ?? `slot ${slot}`;
}

interface AxisTick { ratio: number; label: string; major: boolean; }

function buildTicks(startNs: bigint, endNs: bigint): AxisTick[] {
  const rangeNs = endNs - startNs;
  if (rangeNs <= 0n) return [];
  const rangeMs = Number(rangeNs) / 1e6;
  // Pick a step that yields 5..10 labels.
  const stepMs = chooseStep(rangeMs / 8);
  const out: AxisTick[] = [];
  // Snap the first tick to the next multiple of step that falls inside the range.
  const startMs = Number(startNs / 1_000_000n);
  const firstTick = Math.ceil(startMs / stepMs) * stepMs;
  for (let t = firstTick; ; t += stepMs) {
    const ratio = (t - startMs) / rangeMs;
    if (ratio > 1.001) break;
    if (ratio < -0.001) continue;
    out.push({
      ratio,
      label: formatWallClockLabel(t, stepMs),
      major: Math.abs(t % (stepMs * 5)) < 1e-3,
    });
  }
  return out;
}

function chooseStep(targetMs: number): number {
  const candidates = [
    1, 5, 10, 50, 100, 500,
    1_000, 5_000, 10_000, 30_000, 60_000,
    5 * 60_000, 15 * 60_000, 60 * 60_000,
  ];
  for (const c of candidates) if (c >= targetMs) return c;
  return 60 * 60_000;
}

function formatWallClockLabel(ms: number, stepMs: number): string {
  const d = new Date(ms);
  if (stepMs < 1000) {
    return `${d.toLocaleTimeString([], { hour12: false })}.${String(ms % 1000).padStart(3, "0")}`;
  }
  if (stepMs < 60_000) {
    return d.toLocaleTimeString([], { hour12: false });
  }
  return d.toLocaleTimeString([], { hour12: false, hour: "2-digit", minute: "2-digit" });
}

// Decode activity tiles → one InstanceBatch per (gpu, category).
function arrowToBatches(table: import("apache-arrow").Table, lanesPerGpu: number): InstanceBatch[] {
  const n = table.numRows;
  if (n === 0) return [];
  const col = (name: string) => table.getChild(name)!;
  const bucketStart = col("bucket_start_ns");
  const bucketWidth = col("bucket_width_ns");
  const gpuIdCol = col("gpu_id");
  const categoryCol = col("category");
  const topName = col("top_name_id");

  const groups = new Map<number, { s: number[]; w: number[]; corr: number[]; color: 0 | 1 | 2 | 5 }>();
  for (let i = 0; i < n; i++) {
    const gpu = Number(gpuIdCol.get(i));
    const cat = Number(categoryCol.get(i));
    let laneOffset = -1;
    let color: 0 | 1 | 2 | 5 = 5;
    switch (cat) {
      case 3: laneOffset = METRIC_SLOTS + 0; color = 0; break;  // KERNEL
      case 4: case 5: laneOffset = METRIC_SLOTS + 1; color = 1; break;   // MEMCPY / MEMSET
      case 2: laneOffset = METRIC_SLOTS + 2; color = 2; break;  // API_CALL
      default: continue;
    }
    const laneIdx = gpu * lanesPerGpu + laneOffset;
    const key = laneIdx * 10 + color;
    let g = groups.get(key);
    if (!g) { g = { s: [], w: [], corr: [], color }; groups.set(key, g); }
    const s = Number(bucketStart.get(i) as bigint | number);
    const w = Number(bucketWidth.get(i) as bigint | number);
    g.s.push(s);
    g.w.push(w);
    g.corr.push(Number(topName.get(i) as bigint | number) & 0xffffffff);
  }

  const out: InstanceBatch[] = [];
  for (const [key, g] of groups) {
    const lane = Math.floor(key / 10);
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

// Decode metric tiles → one MetricSeries per (gpu, metric_id).
function arrowToMetrics(
  table: import("apache-arrow").Table,
  catalog: Map<string, MetricSpec>,
  lanesPerGpu: number,
): MetricSeries[] {
  const n = table.numRows;
  if (n === 0) return [];
  const col = (name: string) => table.getChild(name)!;
  const bucketStart = col("bucket_start_ns");
  const gpuCol = col("gpu_id");
  const metricIdCol = col("metric_id");
  const p99 = col("p99");

  const groups = new Map<string, { ts: number[]; v: number[]; gpu: number; mid: string }>();
  for (let i = 0; i < n; i++) {
    const gpu = Number(gpuCol.get(i));
    const mid = (metricIdCol.get(i) as bigint).toString();
    const key = `${gpu}:${mid}`;
    let g = groups.get(key);
    if (!g) { g = { ts: [], v: [], gpu, mid }; groups.set(key, g); }
    g.ts.push(Number(bucketStart.get(i) as bigint | number));
    g.v.push(Number(p99.get(i)));
  }

  const out: MetricSeries[] = [];
  for (const [, g] of groups) {
    const spec = catalog.get(g.mid);
    if (!spec) continue;
    const laneIdx = g.gpu * lanesPerGpu + spec.slot;
    pushSeries(out, laneIdx, spec, g.ts, g.v);
  }
  return out;
}

// Convert the live kernel ring → renderer InstanceBatches. One batch per
// GPU drops into the "kernels" activity lane. Duration clamps to 1 ns so
// zero-length entries still get a 1 px bar at close zooms.
function liveKernelsToBatches(kernels: LiveKernel[], lanesPerGpu: number): InstanceBatch[] {
  if (kernels.length === 0) return [];
  const perGpu = new Map<number, { s: number[]; w: number[]; corr: number[] }>();
  for (const k of kernels) {
    let g = perGpu.get(k.gpuId);
    if (!g) {
      g = { s: [], w: [], corr: [] };
      perGpu.set(k.gpuId, g);
    }
    g.s.push(k.tsNs);
    g.w.push(Math.max(1, k.durationNs));
    // Correlation lo32 — store the lower 32 bits of name_id (u64 string)
    // so clicking a kernel bar can highlight all sibling launches of the
    // same kernel. We parse the string as BigInt and mask.
    try {
      g.corr.push(Number(BigInt(k.correlationId) & 0xffffffffn));
    } catch {
      g.corr.push(0);
    }
  }
  const out: InstanceBatch[] = [];
  for (const [gpuId, g] of perGpu) {
    const laneIdx = gpuId * lanesPerGpu + METRIC_SLOTS + 0; // kernels is activity slot 0
    out.push({
      lane: laneIdx,
      colorId: 0, // KERNEL color from shader palette
      starts: Float64Array.from(g.s),
      widths: Float64Array.from(g.w),
      corrLo: Float32Array.from(g.corr),
    });
  }
  return out;
}

// Convert the client-side live ring → renderer MetricSeries. If a metric
// has been classified, use its fixed yMin/yMax; otherwise autoscale. Also
// extrapolates a flat segment well past "now" so that the trailing edge
// of the line always reaches the right side of the viewport, even between
// 1 Hz SSE frames while the RAF-driven virtual clock keeps scrolling.
function liveRingToMetrics(
  ring: Map<string, { tsNs: number; value: number }[]>,
  catalog: Map<string, MetricSpec>,
  lanesPerGpu: number,
  nowNs: bigint,
): MetricSeries[] {
  const out: MetricSeries[] = [];
  const nowF = Number(nowNs);
  // 10 minutes of flat tail. The shader clips to the viewport anyway, and
  // 10 min covers every zoom level we care about for MVP (max viewport is
  // 24 h, but nobody looks at >5 min live). Keeps us from recomputing the
  // ring on every RAF frame.
  const TAIL_NS = 600 * 1_000_000_000;
  for (const [key, samples] of ring) {
    if (samples.length < 1) continue;
    const [, gpuStr, mid] = key.split(":");
    const gpu = Number(gpuStr);
    const spec = catalog.get(mid);
    if (!spec) continue;
    const laneIdx = gpu * lanesPerGpu + spec.slot;

    const ts: number[] = new Array(samples.length + 1);
    const vs: number[] = new Array(samples.length + 1);
    for (let i = 0; i < samples.length; i++) {
      ts[i] = samples[i].tsNs;
      vs[i] = samples[i].value;
    }
    const lastTs = ts[samples.length - 1];
    ts[samples.length] = Math.max(lastTs, nowF) + TAIL_NS;
    vs[samples.length] = vs[samples.length - 1];

    pushSeries(out, laneIdx, spec, ts, vs);
  }
  return out;
}

function pushSeries(
  out: MetricSeries[],
  laneIdx: number,
  spec: MetricSpec,
  ts: number[],
  vs: number[],
) {
  const n = ts.length;
  if (n === 0) return;
  let vmin = spec.yMin;
  let vmax = spec.yMax;
  if (vmin == null || vmax == null) {
    let a = Infinity, b = -Infinity;
    for (let i = 0; i < n; i++) {
      if (vs[i] < a) a = vs[i];
      if (vs[i] > b) b = vs[i];
    }
    if (vmin == null) vmin = a;
    if (vmax == null) vmax = Math.max(b, a + 1);
  }
  const span = Math.max(1e-9, vmax - vmin);
  const pts = new Float64Array(n * 2);
  for (let i = 0; i < n; i++) {
    pts[i * 2] = ts[i];
    pts[i * 2 + 1] = Math.max(0, Math.min(1, (vs[i] - vmin) / span));
  }
  out.push({ lane: laneIdx, color: spec.rgb, points: pts });
}

function formatNs(n: bigint): string {
  const v = Number(n);
  if (v < 1_000) return `${v}ns`;
  if (v < 1_000_000) return `${(v / 1_000).toFixed(1)}µs`;
  if (v < 1_000_000_000) return `${(v / 1_000_000).toFixed(1)}ms`;
  if (v < 60_000_000_000) return `${(v / 1_000_000_000).toFixed(1)}s`;
  return `${(v / 60_000_000_000).toFixed(1)}min`;
}
