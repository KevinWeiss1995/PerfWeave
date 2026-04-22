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
  /** EMA of (server_wall_ns - client_wall_ns). Added to client wall-clock
   *  when computing the virtual "now" so the tape stays aligned when the
   *  server runs on a different host. NaN until first SSE frame. */
  skewNs?: number;
  /** ms since the newest live sample arrived. Timeline surfaces this in
   *  the HUD and dims the now-line when stale. */
  lastSampleAgeMs?: number;
  /** Live sample rates, surfaced in the HUD. */
  metricRateHz?: number;
  kernelRateHz?: number;
  /** True if the EventSource currently has an open connection. */
  connected?: boolean;
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
  skewNs = NaN,
  lastSampleAgeMs = Number.POSITIVE_INFINITY,
  metricRateHz = 0,
  kernelRateHz = 0,
  connected = false,
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
  const skewNsRef = useRef<number>(skewNs);
  useEffect(() => { viewportRef.current = viewport; }, [viewport]);
  useEffect(() => { liveRef.current = live; }, [live]);
  useEffect(() => { liveLastTsNsRef.current = liveLastTsNs; }, [liveLastTsNs]);
  useEffect(() => { skewNsRef.current = skewNs; }, [skewNs]);

  // --- Resolve metric_id → name lazily via /graphql. Keyed off
  //     liveLastTsNs because the live ring's Map reference is stable, so
  //     we need an explicit heartbeat to notice new metric_ids.
  const [metricCatalog, setMetricCatalog] = useState<Map<string, MetricSpec>>(new Map());
  // Kernel-name cache. Keyed by nameId (u64 as string). We fill it lazily
  // from live kernels as they arrive; used by the hover tooltip.
  const kernelNameCacheRef = useRef<Map<string, string>>(new Map());
  const [kernelNameVersion, setKernelNameVersion] = useState(0);
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

  // --- Gutter model: one row per lane.
  //
  // Multiple metrics can map to the same slot (e.g. on Jetson both
  // `soc.power.gpu.watts` and `gpu.power.watts` classify to the power
  // slot; on discrete GPUs `mem.util` and `mem.used` both land on mem).
  // Rather than silently pick one and clobber the other, we show the
  // primary label and stack sub-values for every metric that actually has
  // live data on this (gpu, slot).
  const gutterRows = useMemo(() => {
    type ValuePart = { label: string; value: string };
    type Row = {
      laneIdx: number;
      label: string;
      subGpu: string;
      parts: ValuePart[];
      rgb: [number, number, number];
      heightPx: number;
    };

    // (gpu, slot) -> list of {spec, latest}
    type Entry = { spec: MetricSpec; latest: number };
    const perLane = new Map<number, Entry[]>();
    if (liveSeries) {
      for (const [key, samples] of liveSeries) {
        if (samples.length === 0) continue;
        const [, gpuStr, mid] = key.split(":");
        const gpu = Number(gpuStr);
        const spec = metricCatalog.get(mid);
        if (!spec) continue;
        const laneIdx = gpu * LANES_PER_GPU + spec.slot;
        let arr = perLane.get(laneIdx);
        if (!arr) {
          arr = [];
          perLane.set(laneIdx, arr);
        }
        arr.push({ spec, latest: samples[samples.length - 1].value });
      }
    }

    const rows: Row[] = [];
    for (let g = 0; g < NUM_GPUS; g++) {
      for (let s = 0; s < METRIC_SLOTS; s++) {
        const laneIdx = g * LANES_PER_GPU + s;
        const entries = perLane.get(laneIdx) ?? [];
        entries.sort((a, b) => a.spec.label.localeCompare(b.spec.label));
        const primary = entries[0]?.spec ?? firstSpecForSlot(metricCatalog, s);
        const parts: ValuePart[] =
          entries.length > 0
            ? entries.map((e) => ({
                label: e.spec.label,
                value: e.spec.format(e.latest),
              }))
            : [];
        rows.push({
          laneIdx,
          label: primary ? primary.label : slotPlaceholder(s),
          subGpu: `gpu${g}`,
          parts,
          rgb: primary?.rgb ?? [0.4, 0.45, 0.5],
          heightPx: METRIC_LANE_HEIGHT,
        });
      }
      for (let a = 0; a < ACTIVITY_LANES_PER_GPU; a++) {
        const laneIdx = g * LANES_PER_GPU + METRIC_SLOTS + a;
        rows.push({
          laneIdx,
          label: ACTIVITY_LABELS[a],
          subGpu: `gpu${g}`,
          parts: [],
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

    const applyResize = () => {
      if (!canvasRef.current) return;
      const { clientWidth, clientHeight } = canvasRef.current;
      renderer.resize(clientWidth, clientHeight, window.devicePixelRatio || 1);
    };

    const ro = new ResizeObserver(applyResize);
    ro.observe(canvas);

    // DPR changes (external monitor hot-swap, browser zoom) don't fire
    // ResizeObserver because the CSS size doesn't change. We listen for
    // them via a `(resolution: <current>dppx)` media query that flips as
    // soon as DPR moves.
    let mql: MediaQueryList | null = null;
    const hookDpr = () => {
      mql?.removeEventListener?.("change", onDprChange);
      const dpr = window.devicePixelRatio || 1;
      mql = window.matchMedia(`(resolution: ${dpr}dppx)`);
      mql.addEventListener?.("change", onDprChange);
    };
    const onDprChange = () => {
      applyResize();
      hookDpr(); // media query is DPR-specific; re-hook to the new DPR
    };
    hookDpr();

    return () => {
      ro.disconnect();
      mql?.removeEventListener?.("change", onDprChange);
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

  // Parallel to the renderer's batches: `kernelsByBatchRef.current[b][i]`
  // is the LiveKernel that produced the bar at `{batch: b, index: i}`.
  // Populated alongside setActivity; read by hover/click hit-testing.
  const kernelsByBatchRef = useRef<LiveKernel[][]>([]);

  // --- Rebuild activity (kernels) lane from the live kernel ring. Keyed
  //     on liveKernelsTick (~10 Hz heartbeat) instead of the ring
  //     reference, which is stable.
  useEffect(() => {
    const r = rendererRef.current;
    if (!r) return;
    if (useSynthetic) return;
    if (!liveKernels) return;
    const { batches, kernelsByBatch } = liveKernelsToBatches(liveKernels, LANES_PER_GPU);
    r.setActivity(batches);
    kernelsByBatchRef.current = kernelsByBatch;
  }, [liveKernels, liveKernelsTick, useSynthetic]);

  // --- Resolve kernel name_id → symbol name lazily. We opportunistically
  //     batch unknown ids so we do one roundtrip per 10 Hz heartbeat.
  useEffect(() => {
    if (!liveKernels || liveKernels.length === 0) return;
    const cache = kernelNameCacheRef.current;
    const unknown = new Set<bigint>();
    for (const k of liveKernels) {
      if (!cache.has(k.nameId)) {
        try {
          unknown.add(BigInt(k.nameId));
        } catch {
          // malformed id, skip
        }
      }
    }
    if (unknown.size === 0) return;
    let cancelled = false;
    resolveStrings(Array.from(unknown))
      .then((names) => {
        if (cancelled) return;
        for (const [id, name] of names) {
          cache.set(id.toString(), name);
        }
        setKernelNameVersion((v) => (v + 1) | 0);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [liveKernels, liveKernelsTick]);

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
        // Use server wall-clock as our "now" when we have a skew estimate;
        // otherwise fall back to the client clock. Keeps the tape aligned
        // across hosts (Jetson-over-SSH, docker-on-laptop, etc).
        const clientWall = BigInt(Date.now()) * 1_000_000n;
        const skew = skewNsRef.current;
        const serverWall = Number.isFinite(skew)
          ? clientWall + BigInt(Math.round(skew))
          : clientWall;
        const last = liveLastTsNsRef.current;
        const anchored = last > 0n && last > serverWall ? last : serverWall;
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

  const dragRef = useRef<{ x: number; startNs: bigint; endNs: bigint; moved: boolean } | null>(null);

  // Hover state for the kernel tooltip. Kept in state because the tooltip
  // is a small React subtree; the gutter/axis memos aren't affected.
  interface KernelHover {
    x: number;          // pointer x relative to timeline-main
    y: number;
    kernel: LiveKernel;
    name: string | null;
    siblingCount: number;
  }
  const [hoverKernel, setHoverKernel] = useState<KernelHover | null>(null);

  const onMouseDown = (e: React.MouseEvent) => {
    const { startNs, endNs } = effViewport();
    dragRef.current = { x: e.clientX, startNs, endNs, moved: false };
  };
  const onMouseMove = (e: React.MouseEvent) => {
    // Pan drag has priority over hover — we don't want tooltips chasing the
    // cursor while the user is dragging the timeline.
    if (dragRef.current) {
      const c = canvasRef.current!;
      const dx = e.clientX - dragRef.current.x;
      if (Math.abs(dx) > 2) dragRef.current.moved = true;
      const range = dragRef.current.endNs - dragRef.current.startNs;
      const dNs = BigInt(-Math.floor((dx / c.clientWidth) * Number(range)));
      onViewportChange({
        startNs: dragRef.current.startNs + dNs,
        endNs: dragRef.current.endNs + dNs,
      });
      if (hoverKernel) setHoverKernel(null);
      return;
    }

    // Hover hit-test against the kernel activity lanes only; metric lanes
    // don't have per-bar data to show.
    const r = rendererRef.current;
    const c = canvasRef.current;
    const main = mainRef.current;
    if (!r || !c || !main) return;
    const rect = c.getBoundingClientRect();
    const xPx = e.clientX - rect.left;
    const yPx = e.clientY - rect.top;
    const vp = effViewport();
    const hit = r.pick(
      { startNs: vp.startNs, endNs: vp.endNs, widthPx: c.clientWidth, heightPx: c.clientHeight },
      xPx,
      yPx,
    );
    if (!hit) {
      if (hoverKernel) setHoverKernel(null);
      return;
    }
    const refs = kernelsByBatchRef.current[hit.batch];
    const kernel = refs?.[hit.index];
    if (!kernel) {
      if (hoverKernel) setHoverKernel(null);
      return;
    }
    const name = kernelNameCacheRef.current.get(kernel.nameId) ?? null;
    // Count siblings in the current window. O(n) but n is capped by the
    // renderer's live kernel ring (≤200k) — a single scan of the parallel
    // array is ~0.2ms in practice and we throttle via pointermove rate.
    let siblingCount = 0;
    const liveRing = liveKernels ?? [];
    for (let i = 0; i < liveRing.length; i++) {
      if (liveRing[i].nameId === kernel.nameId) siblingCount++;
    }
    const mainRect = main.getBoundingClientRect();
    setHoverKernel({
      x: e.clientX - mainRect.left,
      y: e.clientY - mainRect.top,
      kernel,
      name,
      siblingCount,
    });
  };
  const lastDragMovedRef = useRef(false);
  const onMouseUp = () => {
    lastDragMovedRef.current = dragRef.current?.moved ?? false;
    dragRef.current = null;
  };
  const onMouseLeave = () => {
    lastDragMovedRef.current = dragRef.current?.moved ?? false;
    dragRef.current = null;
    if (hoverKernel) setHoverKernel(null);
  };

  // Double-click: snap to "now" with the default window. Feels like the
  // equivalent of hitting Home.
  const onDoubleClick = () => {
    const end = live ? nowNsRef.current : BigInt(Date.now()) * 1_000_000n;
    onViewportChange({ startNs: end - LIVE_WINDOW_NS, endNs: end });
  };

  const onClick = (e: React.MouseEvent) => {
    // Suppress click-to-select if the user was panning. Without this, every
    // finished drag also selects whichever kernel landed under the cursor.
    if (lastDragMovedRef.current) {
      lastDragMovedRef.current = false;
      return;
    }
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
              <span className="lane-label-name">
                {row.label}
                <span className="lane-label-gpu">{row.subGpu}</span>
              </span>
              {row.parts.length > 0 && (
                <span className="lane-label-values" title={row.parts.map((p) => p.label).join(" · ")}>
                  {row.parts.map((p, i) => (
                    <span key={p.label} className="lane-label-value">
                      {i > 0 && <span className="lane-label-sep">·</span>}
                      {p.value}
                    </span>
                  ))}
                </span>
              )}
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
          onMouseLeave={onMouseLeave}
          onClick={onClick}
          onDoubleClick={onDoubleClick}
        />
        {hoverKernel && (
          <KernelTooltip
            hover={hoverKernel}
            nameFallback={kernelNameCacheRef.current.get(hoverKernel.kernel.nameId) ?? hoverKernel.name}
            nameVersion={kernelNameVersion}
          />
        )}
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
        <span className="hud-item">{fps} fps</span>
        <span className="hud-sep">·</span>
        <span className="hud-item">{formatNs(v.endNs - v.startNs)} window</span>
        {live && (
          <>
            <span className="hud-sep">·</span>
            <span className={`hud-pill ${connectionClass(connected, lastSampleAgeMs)}`}>
              {connectionLabel(connected, lastSampleAgeMs)}
            </span>
            <span className="hud-sep">·</span>
            <span className="hud-item" title="metric frames / kernel events per second">
              {metricRateHz.toFixed(1)} Hz · {formatKernelRate(kernelRateHz)}
            </span>
            {Number.isFinite(skewNs) && Math.abs(skewNs) > 5_000_000 && (
              <>
                <span className="hud-sep">·</span>
                <span
                  className="hud-item hud-warn"
                  title="Estimated clock skew between server and this browser (including SSE transport latency)."
                >
                  skew {formatSkew(skewNs)}
                </span>
              </>
            )}
          </>
        )}
      </div>
    </div>
  );
}

function connectionClass(connected: boolean, ageMs: number): string {
  if (!connected) return "hud-pill--down";
  if (!Number.isFinite(ageMs) || ageMs > 3000) return "hud-pill--stale";
  return "hud-pill--live";
}

function connectionLabel(connected: boolean, ageMs: number): string {
  if (!connected) return "DISCONNECTED";
  if (!Number.isFinite(ageMs)) return "CONNECTING";
  if (ageMs > 3000) return `STALE ${(ageMs / 1000).toFixed(1)}s`;
  return "LIVE";
}

function formatKernelRate(k: number): string {
  if (k <= 0) return "0 kern/s";
  if (k >= 1000) return `${(k / 1000).toFixed(1)}k kern/s`;
  return `${k.toFixed(0)} kern/s`;
}

function formatSkew(ns: number): string {
  const absMs = Math.abs(ns) / 1e6;
  const sign = ns >= 0 ? "+" : "-";
  if (absMs < 1000) return `${sign}${absMs.toFixed(0)}ms`;
  return `${sign}${(absMs / 1000).toFixed(1)}s`;
}

// --- Helpers ---------------------------------------------------------------

function rgbCss([r, g, b]: [number, number, number]): string {
  return `rgb(${Math.round(r * 255)}, ${Math.round(g * 255)}, ${Math.round(b * 255)})`;
}

interface KernelHoverProps {
  hover: {
    x: number;
    y: number;
    kernel: LiveKernel;
    name: string | null;
    siblingCount: number;
  };
  nameFallback: string | null;
  // Only here so the tooltip re-renders when the name cache updates.
  nameVersion: number;
}

function KernelTooltip({ hover, nameFallback }: KernelHoverProps) {
  const k = hover.kernel;
  const name = nameFallback ?? `#${shortId(k.nameId)}`;
  // Offset tooltip so the cursor doesn't obscure it. Flip to the left side
  // if we're too close to the right edge.
  const style: React.CSSProperties = {
    left: hover.x + 14,
    top: hover.y + 14,
  };
  return (
    <div className="kernel-tooltip" style={style}>
      <div className="kernel-tooltip__name" title={name}>{name}</div>
      <div className="kernel-tooltip__row">
        <span className="k">duration</span>
        <span className="v">{formatNs(BigInt(Math.max(1, k.durationNs)))}</span>
      </div>
      <div className="kernel-tooltip__row">
        <span className="k">gpu</span>
        <span className="v">{k.gpuId}</span>
      </div>
      <div className="kernel-tooltip__row">
        <span className="k">corr</span>
        <span className="v">{shortId(k.correlationId)}</span>
      </div>
      <div className="kernel-tooltip__row">
        <span className="k">launches (ring)</span>
        <span className="v">{hover.siblingCount.toLocaleString()}</span>
      </div>
      <div className="kernel-tooltip__hint">click → highlight siblings</div>
    </div>
  );
}

function shortId(id: string): string {
  if (id.length <= 10) return id;
  return id.slice(0, 6) + "…" + id.slice(-3);
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
//
// We store `nameId.lo32` (NOT `correlation_id`) in `corrLo` because the
// renderer uses that field to highlight "all instances that share this
// id" — and what users actually want on click is "show me every launch
// of *this* kernel", which corresponds to name_id. correlation_id is
// globally unique per launch, so highlighting on it would only ever
// light up one bar. The nameId-based metadata also drives the hover
// tooltip.
function liveKernelsToBatches(
  kernels: LiveKernel[],
  lanesPerGpu: number,
): { batches: InstanceBatch[]; kernelsByBatch: LiveKernel[][] } {
  if (kernels.length === 0) return { batches: [], kernelsByBatch: [] };
  const perGpu = new Map<
    number,
    { s: number[]; w: number[]; nameLo: number[]; refs: LiveKernel[] }
  >();
  for (const k of kernels) {
    let g = perGpu.get(k.gpuId);
    if (!g) {
      g = { s: [], w: [], nameLo: [], refs: [] };
      perGpu.set(k.gpuId, g);
    }
    g.s.push(k.tsNs);
    g.w.push(Math.max(1, k.durationNs));
    try {
      g.nameLo.push(Number(BigInt(k.nameId) & 0xffffffffn));
    } catch {
      g.nameLo.push(0);
    }
    g.refs.push(k);
  }
  const batches: InstanceBatch[] = [];
  const kernelsByBatch: LiveKernel[][] = [];
  for (const [gpuId, g] of perGpu) {
    const laneIdx = gpuId * lanesPerGpu + METRIC_SLOTS + 0;
    batches.push({
      lane: laneIdx,
      colorId: 0,
      starts: Float64Array.from(g.s),
      widths: Float64Array.from(g.w),
      corrLo: Float32Array.from(g.nameLo),
    });
    kernelsByBatch.push(g.refs);
  }
  return { batches, kernelsByBatch };
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
