// Side panel — the UX payoff. Three modes, picked in order of specificity:
//   1. A shift-click measurement is active: show elapsed time.
//   2. An event is selected: show its correlated pair (API ↔ kernel).
//   3. Default: show spike cards (top-3 kernels + compute/memory classification)
//      and top kernels in the viewport, plus the actionable next step.
//
// The spike cards are the headline feature. Each card answers "why is my GPU
// slow at time T" in one glance: it names the metric that spiked, says
// whether the GPU was compute- or memory-bound at that instant, and lists
// the three kernels responsible for the most runtime in the surrounding
// 1-second window.

import { useEffect, useState } from "react";
import {
  fetchCorrelated,
  fetchTopKernels,
  fetchSpikes,
  fetchSpikeContext,
  resolveStrings,
  type SpikeContext,
} from "../api";

interface SelectionInfo {
  correlationLo: number;
  startNs: bigint;
  endNs: bigint;
  lane: number;
}

export interface Measurement {
  startNs: bigint;
  endNs: bigint;
}

export interface SpikeMarker {
  bucketStartNs: bigint;
  bucketWidthNs: bigint;
  nodeId: number;
  gpuId: number;
  metricId: bigint;
  zMad: number;
}

interface Props {
  selection: SelectionInfo | null;
  viewport: { startNs: bigint; endNs: bigint };
  measurement: Measurement | null;
  useSynthetic?: boolean;
  onSpikesChanged?: (spikes: SpikeMarker[]) => void;
  focusedSpike?: SpikeMarker | null;
}

interface TopKernel { name: string; totalDurationNs: bigint; launches: bigint; meanDurationNs: bigint; }

export function SidePanel({
  selection,
  viewport,
  measurement,
  useSynthetic = false,
  onSpikesChanged,
  focusedSpike,
}: Props) {
  const [topKernels, setTopKernels] = useState<TopKernel[]>([]);
  const [spikeCtx, setSpikeCtx] = useState<SpikeContext[]>([]);
  const [correlated, setCorrelated] = useState<{ category: number; name: string; gpuId: number }[]>([]);

  useEffect(() => {
    if (useSynthetic) return;
    let cancelled = false;
    (async () => {
      try {
        const tk = await fetchTopKernels(viewport.startNs, viewport.endNs);
        if (cancelled) return;
        setTopKernels(tk.topKernelsInWindow.map((r) => ({
          name: r.name,
          totalDurationNs: BigInt(r.totalDurationNs),
          launches: BigInt(r.launches),
          meanDurationNs: BigInt(r.meanDurationNs),
        })));
      } catch { /* stale viewport fine */ }

      try {
        const s = await fetchSpikes(viewport.startNs, viewport.endNs);
        if (cancelled) return;
        const spikes = s.spikes;
        const markers: SpikeMarker[] = spikes.map((sp) => ({
          bucketStartNs: BigInt(sp.bucketStartNs),
          bucketWidthNs: BigInt(sp.bucketWidthNs),
          nodeId: sp.nodeId,
          gpuId: sp.gpuId,
          metricId: BigInt(sp.metricId),
          zMad: sp.zMad,
        }));
        onSpikesChanged?.(markers);

        // Fetch context for the top-5 spikes by magnitude. Sequential is
        // fine: there are at most 5 and each query is ~3ms.
        const top = [...spikes]
          .sort((a, b) => b.zMad - a.zMad)
          .slice(0, 5);
        const ctxs: SpikeContext[] = [];
        for (const sp of top) {
          try {
            const r = await fetchSpikeContext(
              BigInt(sp.bucketStartNs),
              BigInt(sp.bucketWidthNs),
              sp.gpuId,
              BigInt(sp.metricId),
            );
            ctxs.push(r.spikeContext);
          } catch { /* drop this one */ }
        }
        if (!cancelled) setSpikeCtx(ctxs);
      } catch { /* */ }
    })();
    return () => { cancelled = true; };
  }, [viewport.startNs, viewport.endNs, useSynthetic, onSpikesChanged]);

  useEffect(() => {
    if (useSynthetic || !selection) { setCorrelated([]); return; }
    (async () => {
      try {
        const resp = await fetchCorrelated(BigInt(selection.correlationLo));
        const ids = resp.correlated.map((c) => BigInt(c.nameId));
        const strings = await resolveStrings(ids);
        setCorrelated(resp.correlated.map((c) => ({
          category: c.category,
          name: strings.get(BigInt(c.nameId)) ?? "<unknown>",
          gpuId: c.gpuId,
        })));
      } catch { /* */ }
    })();
  }, [selection?.correlationLo, useSynthetic]);

  if (measurement) {
    const elapsed = measurement.endNs - measurement.startNs;
    return (
      <aside className="panel">
        <h3>Measurement</h3>
        <div className="kv">
          <div className="k">Δ time</div><div>{formatNs(elapsed)}</div>
          <div className="k">from</div><div>{formatNsAbs(measurement.startNs)}</div>
          <div className="k">to</div><div>{formatNsAbs(measurement.endNs)}</div>
        </div>
        <p style={{ color: "var(--fg-dim)" }}>
          Shift-click another event to re-measure, or click elsewhere to clear.
        </p>
      </aside>
    );
  }

  if (selection) {
    return (
      <aside className="panel">
        <h3>Selected event</h3>
        <div className="kv">
          <div className="k">correlation</div><div>#{selection.correlationLo}</div>
          <div className="k">duration</div><div>{formatNs(selection.endNs - selection.startNs)}</div>
          <div className="k">lane</div><div>{selection.lane}</div>
        </div>
        <h3>Correlated events</h3>
        {correlated.length === 0 ? (
          <div style={{ color: "var(--fg-dim)" }}>No pair found in current data window.</div>
        ) : correlated.map((c, i) => (
          <div key={i} className="kernel-row">
            <div className="name">{categoryLabel(c.category)}: {c.name}</div>
            <div className="dur">gpu{c.gpuId}</div>
          </div>
        ))}
      </aside>
    );
  }

  return (
    <aside className="panel">
      <h3>Viewport summary</h3>
      <div className="kv">
        <div className="k">window</div><div>{formatNs(viewport.endNs - viewport.startNs)}</div>
        <div className="k">spikes</div>
        <div style={{ color: spikeCtx.length > 0 ? "var(--spike)" : "var(--fg-dim)" }}>
          {spikeCtx.length}
        </div>
      </div>

      {spikeCtx.length > 0 && (
        <>
          <h3>Spikes</h3>
          {spikeCtx.map((s, i) => (
            <SpikeCard
              key={`${s.bucketStartNs}-${s.gpuId}-${s.metricId}`}
              ctx={s}
              focused={
                focusedSpike != null &&
                BigInt(s.bucketStartNs) === focusedSpike.bucketStartNs &&
                s.gpuId === focusedSpike.gpuId &&
                BigInt(s.metricId) === focusedSpike.metricId
              }
              rank={i + 1}
            />
          ))}
        </>
      )}

      <h3>Top kernels in window</h3>
      {topKernels.length === 0 ? (
        <div style={{ color: "var(--fg-dim)" }}>No data yet. Start a workload or drag-drop a .nsys-rep.</div>
      ) : topKernels.map((k, i) => (
        <div key={i} className="kernel-row">
          <div className="name" title={k.name}>{k.name}</div>
          <div className="dur">{formatNs(k.totalDurationNs)}</div>
        </div>
      ))}
      <div className="suggestion">
        Suggested next action: drag-drop a <code>.nsys-rep</code> file into this window,
        or run <code>perfweave run -- ./your_program</code>.
      </div>
    </aside>
  );
}

function SpikeCard({ ctx, focused, rank }: { ctx: SpikeContext; focused: boolean; rank: number }) {
  const bound = ctx.bottleneck;
  const color =
    bound === "MEMORY_BOUND" ? "var(--bound-mem)"
    : bound === "COMPUTE_BOUND" ? "var(--bound-compute)"
    : "var(--fg-dim)";
  const label =
    bound === "MEMORY_BOUND" ? "memory-bound"
    : bound === "COMPUTE_BOUND" ? "compute-bound"
    : "unclassified";
  return (
    <div className={`spike-card${focused ? " focused" : ""}`}>
      <div className="spike-head">
        <span className="spike-rank">#{rank}</span>
        <span className="spike-metric">{ctx.metricName}</span>
        <span className="spike-gpu">gpu{ctx.gpuId}</span>
        <span className="spike-bound" style={{ color, borderColor: color }}>{label}</span>
      </div>
      <div className="spike-sub">
        at {formatNsAbs(BigInt(ctx.bucketStartNs))}
        {" · "}
        SM {ctx.avgGpuUtil.toFixed(0)}% / MEM {ctx.avgMemUtil.toFixed(0)}%
      </div>
      {ctx.topKernels.length === 0 ? (
        <div style={{ color: "var(--fg-dim)", padding: "4px 0" }}>No kernels in ±500ms window.</div>
      ) : ctx.topKernels.map((k, i) => (
        <div key={i} className="kernel-row">
          <div className="name" title={k.name}>{k.name}</div>
          <div className="dur">{formatNs(BigInt(k.totalDurationNs))}</div>
        </div>
      ))}
    </div>
  );
}

function formatNs(v: bigint | number): string {
  const n = typeof v === "bigint" ? Number(v) : v;
  if (n < 1_000) return `${n}ns`;
  if (n < 1_000_000) return `${(n / 1_000).toFixed(2)}µs`;
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(2)}ms`;
  return `${(n / 1_000_000_000).toFixed(2)}s`;
}
function formatNsAbs(v: bigint): string {
  const ms = Number(v / 1_000_000n);
  const d = new Date(ms);
  return d.toLocaleTimeString() + "." + String(Number(v) % 1_000_000).padStart(6, "0");
}
function categoryLabel(c: number): string {
  return ["?", "metric", "api", "kernel", "memcpy", "memset", "sync", "overhead", "marker"][c] ?? "?";
}
