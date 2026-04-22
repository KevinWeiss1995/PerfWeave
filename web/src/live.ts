// Client-side subscription to /api/live (metrics + spike notifications) and
// /api/live/kernels (kernel activity). Maintains a compact per-series ring
// of the last 120 seconds at 1Hz so the Timeline renderer can draw the live
// tail without hitting the server at every frame.
//
// Why a client-side ring at all? The server already keeps a FastRing, but
// re-requesting the last 120s on every viewport change would flood the
// network. The ring here is a local cache that matches the server exactly
// for the live tail; when Live mode is off we fall back to Arrow tiles.

import { useEffect, useRef, useState } from "react";

export interface LiveMetricSample {
  tsNs: number;
  value: number;
}

export interface LiveSeriesKey {
  nodeId: number;
  gpuId: number;
  metricId: string;
}

interface LiveFrameMsg {
  type: "metric";
  ts_ns: number;
  server_now_ns?: number;
  gpus: {
    node_id: number;
    gpu_id: number;
    metrics: { metric_id: number | string; latest: number }[];
  }[];
}

interface LiveSpikeMsg {
  type: "spike";
  ts_ns: number;
  node_id: number;
  gpu_id: number;
  metric_id: number | string;
  value: number;
  z_mad: number;
}

/** Kernel activity event from /api/live/kernels. Matches
 *  `LiveKernelEvent` in crates/perfweave-server/src/live.rs. */
export interface LiveKernel {
  tsNs: number;
  durationNs: number;
  nodeId: number;
  gpuId: number;
  correlationId: string;  // UInt64 → keep as string, convert at use-site
  nameId: string;
}

interface LiveKernelMsgWire {
  ts_ns: number;
  duration_ns: number;
  node_id: number;
  gpu_id: number;
  correlation_id: number | string;
  name_id: number | string;
  server_now_ns?: number;
}

export type LiveSeriesMap = Map<string, LiveMetricSample[]>;

/** Max samples we keep per series. 120s @ 1Hz = 120 points. */
const RING_SIZE = 120;

/** Max kernel events we keep in the live ring. 200k events at ~10k
 *  kernels/sec gives ~20s of a very dense workload; most workloads are
 *  far less dense. Renderer can handle well over a million so this is a
 *  comfortable ceiling. */
const KERNEL_RING_SIZE = 200_000;

function seriesKey(nodeId: number, gpuId: number, metricId: string | number): string {
  return `${nodeId}:${gpuId}:${metricId}`;
}

export interface LiveStream {
  /** True once the EventSource has received its first frame. */
  connected: boolean;
  /** Most recent server ts_ns we've seen (for the Timeline to follow). */
  lastTsNs: bigint;
  /** Per-series metric ring. Reference identity is stable across frames
   *  — use `lastTsNs` (~1 Hz) as the heartbeat signal if you need to
   *  re-read it. */
  series: LiveSeriesMap;
  /** Kernel activity ring, appended to as `/api/live/kernels` pushes. */
  kernels: LiveKernel[];
  /** Bumped whenever `kernels` receives new entries (kernels can arrive
   *  faster than the 1 Hz metric heartbeat, so we need a separate tick). */
  kernelsTick: number;
  /** Unbounded spike notifications for the side panel. */
  spikes: LiveSpikeMsg[];
  /** Rolling estimate of (server_wall_clock - client_wall_clock) in ns.
   *  Used by the Timeline to map "client performance.now" to server ns so
   *  the live tape doesn't drift when the server runs on a remote host
   *  with an independent RTC. NaN until first frame. */
  skewNs: number;
  /** Age of the newest live sample we've seen, in ms. Updated at each
   *  frame; the UI surfaces this as a stale-data indicator in the HUD. */
  lastSampleAgeMs: number;
  /** Frames / kernels per second over the last window. Surfaced in HUD. */
  metricRateHz: number;
  kernelRateHz: number;
}

export function useLiveStream(enabled: boolean): LiveStream {
  const [connected, setConnected] = useState(false);
  const [lastTsNs, setLastTsNs] = useState<bigint>(0n);
  const seriesRef = useRef<LiveSeriesMap>(new Map());
  const spikesRef = useRef<LiveSpikeMsg[]>([]);
  const kernelsRef = useRef<LiveKernel[]>([]);
  const [kernelsTick, setKernelsTick] = useState(0);
  // Batch kernelsTick state updates at ~10 Hz so a heavy workload
  // (thousands of kernels/sec over SSE) doesn't cause thousands of React
  // re-renders.
  const kernelsPendingRef = useRef(false);

  // Skew estimator: EMA of (server_now_ns - client_now_ns). One-way only,
  // so it absorbs transport latency into the estimate — good enough for
  // tape alignment (tens of ms granularity) since both clocks are NTP-disciplined
  // most of the time. Frame-internal ordering uses the server ts directly.
  const skewRef = useRef<number>(NaN);
  const [skewNs, setSkewNs] = useState<number>(NaN);
  const skewEmitPendingRef = useRef(false);

  // Rolling sample-rate counters. We update the state at ~2 Hz to avoid
  // thrashing React; exact rate is computed over a ~1s window via
  // timestamps-of-last-N-frames.
  const metricTimesRef = useRef<number[]>([]);
  const kernelTimesRef = useRef<number[]>([]);
  const [metricRateHz, setMetricRateHz] = useState(0);
  const [kernelRateHz, setKernelRateHz] = useState(0);
  const [lastSampleAgeMs, setLastSampleAgeMs] = useState(Number.POSITIVE_INFINITY);

  useEffect(() => {
    if (!enabled) return;
    const esMetric = new EventSource("/api/live");
    const esKernel = new EventSource("/api/live/kernels");

    const updateSkew = (serverNowNs: number | undefined) => {
      if (!serverNowNs) return;
      const clientNowNs = Date.now() * 1e6 + (performance.now() % 1) * 1e6;
      const observed = serverNowNs - clientNowNs;
      // Heavy EMA on purpose — individual frames sit atop 200-500ms of SSE
      // variance; we only care about slow clock drift between hosts.
      const prev = skewRef.current;
      const next = Number.isFinite(prev) ? prev * 0.9 + observed * 0.1 : observed;
      skewRef.current = next;
      // Push to React state at most ~1 Hz.
      if (!skewEmitPendingRef.current) {
        skewEmitPendingRef.current = true;
        setTimeout(() => {
          skewEmitPendingRef.current = false;
          setSkewNs(skewRef.current);
        }, 1000);
      }
    };

    const bumpRate = (arr: number[], setter: (n: number) => void) => {
      const now = performance.now();
      arr.push(now);
      // Keep only the last ~2s of timestamps.
      while (arr.length > 0 && now - arr[0] > 2000) arr.shift();
      // Rate = samples / window_seconds, clamped at minimum 0.5s window
      // so a single sample doesn't read infinity.
      const window = Math.max(500, now - (arr[0] ?? now));
      const rate = (arr.length * 1000) / window;
      setter(rate);
    };

    esMetric.addEventListener("metric", (ev) => {
      try {
        const msg = JSON.parse((ev as MessageEvent).data) as LiveFrameMsg;
        const ring = seriesRef.current;
        for (const g of msg.gpus) {
          for (const m of g.metrics) {
            const k = seriesKey(g.node_id, g.gpu_id, String(m.metric_id));
            let arr = ring.get(k);
            if (!arr) {
              arr = [];
              ring.set(k, arr);
            }
            arr.push({ tsNs: msg.ts_ns, value: m.latest });
            if (arr.length > RING_SIZE) arr.splice(0, arr.length - RING_SIZE);
          }
        }
        setLastTsNs(BigInt(msg.ts_ns));
        setLastSampleAgeMs(0);
        updateSkew(msg.server_now_ns);
        bumpRate(metricTimesRef.current, setMetricRateHz);
        setConnected(true);
      } catch (e) {
        console.warn("bad live metric frame", e);
      }
    });

    esMetric.addEventListener("spike", (ev) => {
      try {
        const msg = JSON.parse((ev as MessageEvent).data) as LiveSpikeMsg;
        spikesRef.current.push(msg);
        if (spikesRef.current.length > 512) {
          spikesRef.current.splice(0, spikesRef.current.length - 512);
        }
      } catch (e) {
        console.warn("bad live spike frame", e);
      }
    });

    esKernel.addEventListener("kernel", (ev) => {
      try {
        const msg = JSON.parse((ev as MessageEvent).data) as LiveKernelMsgWire;
        const ring = kernelsRef.current;
        ring.push({
          tsNs: msg.ts_ns,
          durationNs: msg.duration_ns,
          nodeId: msg.node_id,
          gpuId: msg.gpu_id,
          correlationId: String(msg.correlation_id),
          nameId: String(msg.name_id),
        });
        if (ring.length > KERNEL_RING_SIZE) {
          ring.splice(0, ring.length - KERNEL_RING_SIZE);
        }
        updateSkew(msg.server_now_ns);
        bumpRate(kernelTimesRef.current, setKernelRateHz);
        if (!kernelsPendingRef.current) {
          kernelsPendingRef.current = true;
          setTimeout(() => {
            kernelsPendingRef.current = false;
            setKernelsTick((t) => (t + 1) | 0);
          }, 100);
        }
      } catch (e) {
        console.warn("bad live kernel frame", e);
      }
    });

    const onError = () => setConnected(false);
    esMetric.onerror = onError;
    esKernel.onerror = onError;

    // Tick stale age every 500ms from a single interval; this is the only
    // place we expose "how long since last sample" to the UI.
    const staleTimer = setInterval(() => {
      const arr = metricTimesRef.current;
      const last = arr[arr.length - 1];
      if (last === undefined) {
        setLastSampleAgeMs(Number.POSITIVE_INFINITY);
      } else {
        setLastSampleAgeMs(performance.now() - last);
      }
    }, 500);

    return () => {
      esMetric.close();
      esKernel.close();
      clearInterval(staleTimer);
      setConnected(false);
    };
  }, [enabled]);

  return {
    connected,
    lastTsNs,
    series: seriesRef.current,
    kernels: kernelsRef.current,
    kernelsTick,
    spikes: spikesRef.current,
    skewNs,
    lastSampleAgeMs,
    metricRateHz,
    kernelRateHz,
  };
}
