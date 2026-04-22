// SpikeDrilldown — modal drawer that opens when the user clicks a spike pin
// (or a spike card's "Drilldown" button). Answers the core question the whole
// system exists to answer: "what happened on the GPU at this instant, and what
// should I do about it?".
//
// Layout (top to bottom):
//   1. Header: metric name, spike magnitude (z_mad), bottleneck pill.
//   2. Kernel Gantt: every kernel in the ±500ms window, drawn on a canvas,
//      sorted by start time. Bar color keyed by SM/DRAM boundness once SOL
//      data is available.
//   3. SOL cards: one card per distinct kernel name. Cards start as
//      "profiling…" placeholders and upgrade in-place when the replay-profile
//      mutation streams a result for them.
//
// On open we immediately call `profileKernels`. We only request kernels that
// don't already have a SOL row. Cards that already have SOL data from the
// ambient host-sampling path render straight away at LOW confidence; when
// the replay returns they upgrade to HIGH.

import { useEffect, useMemo, useRef, useState } from "react";
import {
  fetchSpikeContext,
  profileKernels,
  type SpikeContext,
  type KernelInWindow,
  type KernelSol,
} from "../api";
import type { SpikeMarker } from "./SidePanel";

interface Props {
  spike: SpikeMarker;
  onClose: () => void;
}

/** Classification window used server-side and matched here. */
const WINDOW_HALF_NS = 500_000_000n;

export function SpikeDrilldown({ spike, onClose }: Props) {
  const [ctx, setCtx] = useState<SpikeContext | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Map from name_id → freshly-profiled SOL row from the replay mutation.
  // Takes precedence over the host-sampling SOL attached in spikeContext.
  const [liveSol, setLiveSol] = useState<Map<string, KernelSol>>(new Map());
  const [profilingStatus, setProfilingStatus] = useState<
    "idle" | "profiling" | "done" | "failed" | "skipped"
  >("idle");

  useEffect(() => {
    let cancelled = false;
    setCtx(null);
    setError(null);
    setLiveSol(new Map());
    setProfilingStatus("idle");
    (async () => {
      try {
        const r = await fetchSpikeContext(
          spike.bucketStartNs,
          spike.bucketWidthNs,
          spike.gpuId,
          spike.metricId,
        );
        if (cancelled) return;
        setCtx(r.spikeContext);

        // Kick off replay-profiling for kernels without SOL data yet.
        // Cap the request at 8 correlation_ids to keep replay bounded.
        const needsProfile = r.spikeContext.kernelsInWindow
          .filter((k) => !k.sol || k.sol.confidence !== "High")
          .slice(0, 8);
        if (needsProfile.length === 0) {
          setProfilingStatus("skipped");
          return;
        }
        const mid = BigInt(r.spikeContext.bucketStartNs);
        const startNs = mid - WINDOW_HALF_NS;
        const endNs = mid + WINDOW_HALF_NS;
        setProfilingStatus("profiling");
        try {
          const resp = await profileKernels(
            spike.nodeId,
            spike.gpuId,
            startNs,
            endNs,
            needsProfile.map((k) => BigInt(k.correlationId)),
            5_000,
          );
          if (cancelled) return;
          const byName = new Map<string, KernelSol>();
          // The server returns SOL rows in the same order as requested
          // correlation_ids, so we can zip.
          resp.profileKernels.results.forEach((sol, i) => {
            const k = needsProfile[i];
            if (!k) return;
            byName.set(k.nameId, sol);
          });
          setLiveSol(byName);
          setProfilingStatus(resp.profileKernels.completed > 0 ? "done" : "failed");
        } catch (e) {
          if (!cancelled) {
            setProfilingStatus("failed");
            console.warn("profileKernels failed", e);
          }
        }
      } catch (e) {
        if (!cancelled) setError(String(e));
      }
    })();
    return () => { cancelled = true; };
  }, [spike.bucketStartNs, spike.gpuId, spike.metricId, spike.nodeId]);

  // Close on Esc.
  useEffect(() => {
    const h = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose]);

  const unifiedKernels = useMemo(() => {
    if (!ctx) return [] as (KernelInWindow & { solFinal: KernelSol | null })[];
    return ctx.kernelsInWindow.map((k) => ({
      ...k,
      solFinal: liveSol.get(k.nameId) ?? k.sol ?? null,
    }));
  }, [ctx, liveSol]);

  // One card per distinct kernel name, summarising total time + launches.
  const solCards = useMemo(() => {
    const byName = new Map<
      string,
      { name: string; nameId: string; totalNs: bigint; launches: number; sol: KernelSol | null }
    >();
    for (const k of unifiedKernels) {
      const entry = byName.get(k.nameId);
      if (entry) {
        entry.totalNs += BigInt(k.durationNs);
        entry.launches += 1;
        if (!entry.sol && k.solFinal) entry.sol = k.solFinal;
      } else {
        byName.set(k.nameId, {
          name: k.name,
          nameId: k.nameId,
          totalNs: BigInt(k.durationNs),
          launches: 1,
          sol: k.solFinal,
        });
      }
    }
    return [...byName.values()].sort((a, b) => (a.totalNs < b.totalNs ? 1 : -1));
  }, [unifiedKernels]);

  return (
    <div className="drilldown-overlay" onClick={onClose}>
      <div className="drilldown" onClick={(e) => e.stopPropagation()}>
        <div className="drilldown-head">
          <div>
            <div className="drilldown-title">
              {ctx ? ctx.metricName : "Spike drilldown"}
              {" "}
              <span className="drilldown-gpu">gpu{spike.gpuId}</span>
            </div>
            <div className="drilldown-sub">
              z={spike.zMad.toFixed(2)} · at {formatAbs(spike.bucketStartNs)}
              {ctx && (
                <>
                  {" · "}
                  <BoundPill bound={ctx.bottleneck} />
                  {" · SM "}{ctx.avgGpuUtil.toFixed(0)}%
                  {" / MEM "}{ctx.avgMemUtil.toFixed(0)}%
                </>
              )}
            </div>
          </div>
          <button className="drilldown-close" onClick={onClose} title="Close (Esc)">×</button>
        </div>

        {error ? (
          <div className="drilldown-empty">Failed to load: {error}</div>
        ) : !ctx ? (
          <div className="drilldown-empty">Loading spike context…</div>
        ) : (
          <>
            <div className="drilldown-section">
              <div className="drilldown-section-head">
                <span>Kernel Gantt (±500ms)</span>
                <span className="drilldown-hint">
                  {unifiedKernels.length} kernels · {solCards.length} unique
                </span>
              </div>
              <KernelGantt
                kernels={unifiedKernels}
                centerNs={BigInt(ctx.bucketStartNs)}
                halfWidthNs={WINDOW_HALF_NS}
              />
            </div>

            <div className="drilldown-section">
              <div className="drilldown-section-head">
                <span>Speed-of-Light</span>
                <span className="drilldown-hint">{profilingStatusLabel(profilingStatus)}</span>
              </div>
              {solCards.length === 0 ? (
                <div className="drilldown-empty">No kernels in window.</div>
              ) : (
                <div className="sol-grid">
                  {solCards.map((k) => (
                    <SolCard
                      key={k.nameId}
                      name={k.name}
                      launches={k.launches}
                      totalNs={k.totalNs}
                      sol={k.sol}
                      profiling={profilingStatus === "profiling" && !k.sol}
                    />
                  ))}
                </div>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function KernelGantt({
  kernels,
  centerNs,
  halfWidthNs,
}: {
  kernels: (KernelInWindow & { solFinal: KernelSol | null })[];
  centerNs: bigint;
  halfWidthNs: bigint;
}) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const wrapRef = useRef<HTMLDivElement | null>(null);
  const [width, setWidth] = useState(600);

  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setWidth(el.clientWidth));
    ro.observe(el);
    setWidth(el.clientWidth);
    return () => ro.disconnect();
  }, []);

  const rowsByStream = useMemo(() => {
    // Group by stream (which we don't have here) — fall back to packing rows
    // greedily by start time so overlapping kernels stack vertically.
    const sorted = [...kernels].sort((a, b) =>
      BigInt(a.tsNs) < BigInt(b.tsNs) ? -1 : 1,
    );
    const rows: { endNs: bigint; items: typeof kernels }[] = [];
    for (const k of sorted) {
      const kStart = BigInt(k.tsNs);
      const kEnd = kStart + BigInt(k.durationNs);
      let placed = false;
      for (const r of rows) {
        if (kStart >= r.endNs) {
          r.items.push(k);
          r.endNs = kEnd;
          placed = true;
          break;
        }
      }
      if (!placed) rows.push({ endNs: kEnd, items: [k] });
    }
    return rows;
  }, [kernels]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const dpr = window.devicePixelRatio || 1;
    const ROW_H = 14;
    const GAP = 2;
    const PAD_X = 8;
    const PAD_Y = 20; // room for the center marker label
    const rowCount = Math.max(1, rowsByStream.length);
    const cssW = width;
    const cssH = PAD_Y + rowCount * (ROW_H + GAP) + 8;
    canvas.width = cssW * dpr;
    canvas.height = cssH * dpr;
    canvas.style.width = `${cssW}px`;
    canvas.style.height = `${cssH}px`;
    const ctx2 = canvas.getContext("2d");
    if (!ctx2) return;
    ctx2.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx2.clearRect(0, 0, cssW, cssH);

    const startNs = centerNs - halfWidthNs;
    const endNs = centerNs + halfWidthNs;
    const spanNs = Number(endNs - startNs);
    const plotW = cssW - PAD_X * 2;

    // Background lanes.
    for (let r = 0; r < rowCount; r++) {
      ctx2.fillStyle = r % 2 === 0 ? "#0f141a" : "#12181f";
      ctx2.fillRect(0, PAD_Y + r * (ROW_H + GAP), cssW, ROW_H);
    }

    // Center marker (the spike bucket).
    const cx = PAD_X + (Number(centerNs - startNs) / spanNs) * plotW;
    ctx2.strokeStyle = "rgba(255, 74, 107, 0.9)";
    ctx2.lineWidth = 1;
    ctx2.beginPath();
    ctx2.moveTo(cx + 0.5, PAD_Y - 4);
    ctx2.lineTo(cx + 0.5, cssH - 4);
    ctx2.stroke();
    ctx2.fillStyle = "var(--spike)";
    ctx2.fillStyle = "rgba(255, 74, 107, 1)";
    ctx2.font = "10px ui-monospace, monospace";
    ctx2.textBaseline = "top";
    ctx2.fillText("spike", cx + 4, 4);

    // Time ticks at ±500, ±250, 0.
    ctx2.fillStyle = "#5c6773";
    ctx2.font = "10px ui-monospace, monospace";
    const ticks = [-500, -250, 0, 250, 500];
    for (const ms of ticks) {
      const tNs = centerNs + BigInt(ms) * 1_000_000n;
      const x = PAD_X + (Number(tNs - startNs) / spanNs) * plotW;
      ctx2.fillText(`${ms}ms`, x + 2, cssH - 12);
      ctx2.strokeStyle = "#1d232b";
      ctx2.beginPath();
      ctx2.moveTo(x + 0.5, PAD_Y);
      ctx2.lineTo(x + 0.5, cssH - 14);
      ctx2.stroke();
    }

    // Bars.
    for (let r = 0; r < rowsByStream.length; r++) {
      const row = rowsByStream[r];
      const y = PAD_Y + r * (ROW_H + GAP);
      for (const k of row.items) {
        const kStart = BigInt(k.tsNs);
        const kDur = BigInt(k.durationNs);
        const x0 = PAD_X + (Number(kStart - startNs) / spanNs) * plotW;
        const x1 = PAD_X + (Number(kStart + kDur - startNs) / spanNs) * plotW;
        const w = Math.max(1, x1 - x0);
        ctx2.fillStyle = kernelBarColor(k.solFinal);
        ctx2.fillRect(x0, y, w, ROW_H);
        if (w > 40) {
          ctx2.fillStyle = "#0b0d10";
          ctx2.font = "10px ui-monospace, monospace";
          const label = shortKernelName(k.name);
          ctx2.fillText(label, x0 + 3, y + 3);
        }
      }
    }
  }, [rowsByStream, width, centerNs, halfWidthNs]);

  return (
    <div ref={wrapRef} className="gantt-wrap">
      <canvas ref={canvasRef} />
    </div>
  );
}

function SolCard({
  name,
  launches,
  totalNs,
  sol,
  profiling,
}: {
  name: string;
  launches: number;
  totalNs: bigint;
  sol: KernelSol | null;
  profiling: boolean;
}) {
  const confLabel = sol?.confidence ?? (profiling ? "profiling…" : "no data");
  const confClass =
    sol?.confidence === "High" ? "conf-high"
    : sol?.confidence === "Medium" ? "conf-med"
    : sol?.confidence === "Low" ? "conf-low"
    : profiling ? "conf-pending"
    : "conf-none";
  return (
    <div className={`sol-card ${confClass}`}>
      <div className="sol-head">
        <span className="sol-name" title={name}>{shortKernelName(name)}</span>
        <span className="sol-conf">{confLabel}</span>
      </div>
      <div className="sol-sub">
        {launches} launch{launches === 1 ? "" : "es"} · total {formatNs(totalNs)}
        {sol && (
          <>
            {" · "}
            <span style={{ color: boundColor(sol.bound) }}>{sol.bound.toLowerCase()}</span>
          </>
        )}
      </div>
      <div className="sol-bars">
        <SolBar label="SM active" pct={sol?.smActivePct} />
        <SolBar label="DRAM BW" pct={sol?.dramBwPct} />
        <SolBar label="L2 BW" pct={sol?.l2BwPct} />
        <SolBar label="Occupancy" pct={sol?.achievedOccupancyPct} />
      </div>
    </div>
  );
}

function SolBar({ label, pct }: { label: string; pct: number | undefined }) {
  const v = typeof pct === "number" ? Math.max(0, Math.min(100, pct)) : null;
  const color =
    v === null ? "#2a323c"
    : v >= 80 ? "var(--bound-compute)"
    : v >= 40 ? "var(--accent-hot)"
    : "var(--bound-mem)";
  return (
    <div className="sol-bar-row">
      <div className="sol-bar-label">{label}</div>
      <div className="sol-bar-track">
        <div
          className="sol-bar-fill"
          style={{ width: v === null ? "0%" : `${v}%`, background: color }}
        />
      </div>
      <div className="sol-bar-value">{v === null ? "—" : `${v.toFixed(0)}%`}</div>
    </div>
  );
}

function BoundPill({ bound }: { bound: SpikeContext["bottleneck"] }) {
  const color =
    bound === "MEMORY_BOUND" ? "var(--bound-mem)"
    : bound === "COMPUTE_BOUND" ? "var(--bound-compute)"
    : "var(--fg-dim)";
  const label =
    bound === "MEMORY_BOUND" ? "memory-bound"
    : bound === "COMPUTE_BOUND" ? "compute-bound"
    : "unclassified";
  return (
    <span
      className="drilldown-bound-pill"
      style={{ color, borderColor: color }}
    >
      {label}
    </span>
  );
}

function profilingStatusLabel(s: string): string {
  switch (s) {
    case "idle": return "";
    case "profiling": return "replay-profiling…";
    case "done": return "replay complete";
    case "failed": return "replay failed (showing low-confidence data)";
    case "skipped": return "host-sampling data is high-confidence";
    default: return s;
  }
}

function kernelBarColor(sol: KernelSol | null): string {
  if (!sol) return "#3a4250";
  switch (sol.bound) {
    case "Memory": return "#63d3a6";
    case "Compute": return "#4aa3ff";
    case "Latency": return "#b78bff";
    case "Balanced": return "#ffb84a";
    default: return "#3a4250";
  }
}
function boundColor(b: string): string {
  switch (b) {
    case "Memory": return "var(--bound-mem)";
    case "Compute": return "var(--bound-compute)";
    case "Latency": return "var(--api)";
    case "Balanced": return "var(--accent-hot)";
    default: return "var(--fg-dim)";
  }
}
function shortKernelName(n: string): string {
  // Mangled CUDA names are often 200+ chars. Strip template params and
  // return trailing identifier plus truncation if still too long.
  const noTpl = n.replace(/<[^>]*>/g, "<…>");
  if (noTpl.length <= 60) return noTpl;
  return "…" + noTpl.slice(noTpl.length - 59);
}
function formatNs(v: bigint | number): string {
  const n = typeof v === "bigint" ? Number(v) : v;
  if (n < 1_000) return `${n}ns`;
  if (n < 1_000_000) return `${(n / 1_000).toFixed(2)}µs`;
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(2)}ms`;
  return `${(n / 1_000_000_000).toFixed(2)}s`;
}
function formatAbs(v: bigint): string {
  const ms = Number(v / 1_000_000n);
  const d = new Date(ms);
  return d.toLocaleTimeString() + "." + String(Number(v) % 1_000_000).padStart(6, "0");
}
