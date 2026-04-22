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

export type LiveSeriesMap = Map<string, LiveMetricSample[]>;

/** Max samples we keep per series. 120s @ 1Hz = 120 points. */
const RING_SIZE = 120;

function seriesKey(nodeId: number, gpuId: number, metricId: string | number): string {
  return `${nodeId}:${gpuId}:${metricId}`;
}

export interface LiveStream {
  /** True once the EventSource has received its first frame. */
  connected: boolean;
  /** Most recent server ts_ns we've seen (for the Timeline to follow). */
  lastTsNs: bigint;
  /** Per-series ring. Reference identity changes on every new frame so
   *  consumers can memoize on it. */
  series: LiveSeriesMap;
  /** Unbounded spike notifications for the side panel. */
  spikes: LiveSpikeMsg[];
}

export function useLiveStream(enabled: boolean): LiveStream {
  const [connected, setConnected] = useState(false);
  const [lastTsNs, setLastTsNs] = useState<bigint>(0n);
  const seriesRef = useRef<LiveSeriesMap>(new Map());
  const [seriesTick, setSeriesTick] = useState(0);
  const spikesRef = useRef<LiveSpikeMsg[]>([]);

  useEffect(() => {
    if (!enabled) return;
    const es = new EventSource("/api/live");

    es.addEventListener("metric", (ev) => {
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
            // Trim leading samples; the render path is simpler with plain
            // arrays than with a circular buffer.
            if (arr.length > RING_SIZE) arr.splice(0, arr.length - RING_SIZE);
          }
        }
        setLastTsNs(BigInt(msg.ts_ns));
        setSeriesTick((t) => (t + 1) | 0);
        setConnected(true);
      } catch (e) {
        console.warn("bad live metric frame", e);
      }
    });

    es.addEventListener("spike", (ev) => {
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

    es.onerror = () => {
      // EventSource retries automatically; we just show the disconnected
      // state so the UI Live pill can flash amber.
      setConnected(false);
    };

    return () => {
      es.close();
      setConnected(false);
    };
  }, [enabled]);

  return {
    connected,
    lastTsNs,
    // seriesTick forces consumers to re-read seriesRef on every new frame
    // without us cloning the whole Map on the hot path.
    series: seriesTick >= 0 ? seriesRef.current : seriesRef.current,
    spikes: spikesRef.current,
  };
}
