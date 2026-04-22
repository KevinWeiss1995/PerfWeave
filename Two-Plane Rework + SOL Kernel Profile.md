Two-Plane Rework + SOL Kernel Profile

Problem in one paragraph

ClickHouse is at 315% CPU because every metric sample (thousands/sec under synthetic) lands in the events table and fans out through 12 materialized views building tile pyramids. Spike detection then re-scans 15 minutes of the 262ms LOD every 10s. Meanwhile the UI has no live stream, and "click a spike" does nothing beyond CSS highlighting. The user wants steady live metrics, deep spike drill-down, and a Nsight-Compute-grade Speed-of-Light report per kernel — automatically, on every perfweave run.

New architecture (two planes)

flowchart LR
    subgraph agent [perfweave-agent]
        NVML[NVML 10Hz] --> fastRing[fast ring]
        CUPTI[CUPTI Activity] --> slowQ[activity queue]
        SOL[CUPTI Profiling + PC Sampling] --> slowQ
        NVTX[CUPTI Callback NVTX] --> slowQ
    end

    fastRing -->|"gRPC 1Hz frames"| collFast[Collector fast plane]
    slowQ -->|"gRPC batches"| collSlow[Collector slow plane]

    collFast --> apiRing[API in-proc ring 120s]
    collSlow --> CH[(ClickHouse)]

    apiRing --> SSE["/api/live SSE"]
    apiRing --> detect[in-proc spike detector]
    detect -->|"only spikes persist"| CH

    SSE --> UI[WebGL timeline]
    CH -->|"viewport queries"| UI
    UI --> drill[SpikeDrilldown pane]
    drill --> CH

Key rule: high-rate metrics never touch ClickHouse. They live in the collector's ring and stream to the UI via SSE. Only kernels, memcpys, api calls, markers, kernel-SOL records, and detected spikes persist. ClickHouse is used for history and cross-session query, not for the live-tail path.

Part 1 — CPU: split ingest into fast + slow planes

1.1 Collector: in-memory fast-metric ring





Add crates/perfweave-collector/src/fast_ring.rs: Arc<RwLock<FastRing>> with 120s at 1Hz + 10s at 10Hz per (node_id, gpu_id, metric_id). Lockless read path via arc_swap of a versioned snapshot.



In crates/perfweave-collector/src/server.rs split incoming batches: Category::Metric → fast ring; everything else → existing ClickHouse sink.



Expose FastRing via an Arc-clone to the API.

1.2 Agent: aggregated metric frames, not per-sample events





In crates/perfweave-agent/src/sampler/nvml.rs, locally fold 10 samples/sec into a MetricFrame { ts_ns, gpu_id, metrics[] } and push at 1Hz (keep full 10Hz only as a recent_10hz tail in the collector ring). 10× fewer gRPC frames, no per-sample CH rows.



Drop synthetic's metric_hz.max(1000) forcing in crates/perfweave-agent/src/main.rs:28; default to configured metric_hz (10). Keep --synthetic-burst flag for explicit stress-test runs.

1.3 Migration: retire metric-tile MVs





New migrations/0006_shed_metric_tiles.sql:





DROP VIEW tiles_metric_8us_mv, tiles_metric_4ms_mv, tiles_metric_262ms_mv, tiles_metric_2s_mv.



Keep tiles_metric as a history table, but it's now fed by a periodic INSERT INTO tiles_metric SELECT ... from the API flushing the fast ring at 2s cadence (coarsest LOD only).



Collapse activity tiles from 8 LODs to 2 (32ms and 2s) in migrations/0003_tiles.sql: inner LODs are derived at query time via ROUND(ts_ns / width) with skip-index. Drops 6 MV writes per insert.

1.4 In-process spike detector





New crates/perfweave-api/src/spike_detect.rs: rolling median + MAD over the fast ring (60s window, recomputed at 1Hz over only the new second). Emits SpikeEvent to a broadcast channel and INSERTs just that row into spikes.



Delete the tokio::spawn loop in crates/perfweave-api/src/app.rs:124 and the INSERT INTO spikes SELECT ... WINDOW ... SQL in crates/perfweave-api/src/imports.rs:19.

Part 2 — Live SSE stream to UI

2.1 New endpoint





GET /api/live in crates/perfweave-api/src/timeline.rs using axum::response::sse. Emits one event/sec:

  {"ts_ns":...,"gpus":[{"gpu_id":0,"sm":75.3,"mem":62.1,"power":220,"dram_bw_pct":82.5,"sm_active_pct":78.1}], "new_spikes":[...], "new_kernels_count":14}
  






/api/live/kernels second SSE channel for activity pins (low-rate, pushes kernel records as they arrive from CUPTI).

2.2 UI: live-tail mode





web/src/timeline/Timeline.tsx: add a followNow: boolean state (default true). When true, viewport end advances to Date.now() on each SSE frame; metric overlay appended to a client-side ring; panning left disables follow.



web/src/api.ts: add subscribeLive(onFrame) returning EventSource.

Part 3 — Speed-of-Light kernel report

3.1 CUPTI Profiling API (PM sampling, continuous mode)





Extend crates/perfweave-cupti-inject/src/lib.rs with a new profiler.rs module using cuptiProfilerBeginSession + CUPTI_PROFILER_HOST_SAMPLING. Collect 6 metrics in one pass (SM 7.0+ safe, works on Jetson Orin/Volta/Ampere/Ada/Hopper):





sm__cycles_active.avg.pct_of_peak_sustained_elapsed → SM Active %



smsp__warps_active.avg.pct_of_peak_sustained_active → Achieved Occupancy %



dram__bytes.sum.per_second.pct_of_peak_sustained_elapsed → DRAM BW %



l1tex__t_sectors.sum.per_second.pct_of_peak_sustained_elapsed → L1/Tex BW %



lts__t_sectors.sum.per_second.pct_of_peak_sustained_elapsed → L2 BW %



smsp__inst_executed.sum.per_second.pct_of_peak_sustained_elapsed → Inst Throughput %



Attribute samples to kernels by timestamp interval overlap with the Activity kernel records (same correlation_id path).



SPECULATION (flag): on Jetson Orin under production loads, 6-metric PM sampling at ~1kHz has been reported around 2–4% overhead. I'd budget 3% and measure; if over, drop to 4 metrics and publish L1/L2 as opt-in.

3.2 CUPTI PC Sampling (continuous)





Add pc_sampling.rs: cuptiPCSamplingEnable with stall-reason counters at 1ms interval. Aggregate to top-5 stall reasons per kernel by correlation_id.



Emit as part of the SOL record.

3.3 CUPTI Callback API for NVTX





Add callback.rs: subscribe to CUPTI_CB_DOMAIN_NVTX (push/pop range, mark). Emit Payload::Marker(MarkerDetail{...}) with domain/name/color/correlation_id.



In crates/perfweave-collector/src/clickhouse_sink.rs:268, stop dropping markers: write to events with category='MARKER'; store color/domain in the payload columns (reuse existing aux or add a thin sidecar).

3.4 Real register count





In crates/perfweave-cupti-inject/src/activity.rs:191, replace the hardcoded registers_per_thread: 0 by reading registersPerThread from CUpti_ActivityKernel7 (already in the struct — the field was just zeroed). No new API needed.

3.5 New payload + schema





proto/perfweave.proto: add KernelSol as a 6th oneof payload variant:

  message KernelSol {
    uint64 correlation_id = 1;
    float sm_active_pct = 2;
    float achieved_occupancy_pct = 3;
    float dram_bw_pct = 4;
    float l1_bw_pct = 5;
    float l2_bw_pct = 6;
    float inst_throughput_pct = 7;
    float theoretical_occupancy_pct = 8;
    float arithmetic_intensity = 9; // flop/byte
    float achieved_gflops = 10;
    enum Bound { UNKNOWN = 0; MEMORY = 1; COMPUTE = 2; LATENCY = 3; BALANCED = 4; }
    Bound bound = 11;
    repeated StallCount stalls = 12; // top-5
  }
  message StallCount { uint32 reason_id = 1; uint32 count = 2; }
  






New migrations/0007_kernel_sol.sql adds a kernel_sol table keyed by (ts_ns, gpu_id, correlation_id) so we don't bloat events rows. Joined to kernel activity at query time.

3.6 Classification rule (deterministic, documented)





In the inject process after PM sampling closes a kernel range:





dram_bw_pct > 70 && sm_active_pct < 50 → MEMORY



sm_active_pct > 70 && dram_bw_pct < 50 → COMPUTE



achieved_occupancy_pct < 25 → LATENCY



else → BALANCED



User-visible as a colored pill; matches Nsight Compute's "GPU Speed of Light" classification semantics.

Part 4 — Spike drill-down split pane

4.1 New React component





web/src/panels/SpikeDrilldown.tsx, replacing the cosmetic focused highlight behavior in web/src/panels/SidePanel.tsx:219. Opens as a bottom drawer on pin click via the existing onSpikeClick hook.



Layout: left 60% = stacked lane renderer for ±5s window; right 40% = kernel detail cards.



Lanes (reuse WebGL renderer, feed narrow viewport):





Waveform lane: SM active %, DRAM BW %, occupancy %, power — overlaid.



Kernel Gantt: per-stream, durations, hover shows name + correlation.



NVTX lane: marker ranges as background bands with names.

4.2 Kernel SOL cards





For each kernel in the window (from events JOIN kernel_sol ON correlation_id):





Roofline dot (AI × achieved GFLOPS) against device peak — static SVG.



6-bar SOL chart (SM / Occ / DRAM / L1 / L2 / Inst).



Launch config pill: grid × block, shared=X KB, regs=Y, theo_occ=Z%.



Stall bar (top-5 reasons).



Bound-classification pill.

4.3 New GraphQL field





Extend spikeContext in crates/perfweave-api/src/graphql.rs:232: instead of top-3 by total duration, return a kernels_in_window list with full SOL payload joined from kernel_sol.

Part 5 — Nits + validation





The ./matmul_tiled.cu perm error that started this thread is unrelated to the architecture: the user tried to run a .cu source file as an executable. Add a friendly error in crates/perfweave-cli/src/main.rs's cmd_run when argv[0] ends in .cu / .cpp: "that looks like source, not a binary — did you forget nvcc -o mm mm.cu?".



perfweave doctor gains a "live plane healthy" check (SSE connects + ring advancing).



Overhead validation on Jetson Orin: target ClickHouse < 30% single-core, perfweave stack < 15% total, SOL+PC sampling overhead on compute-bound SGEMM < 5%.


TODO:

Add in-memory FastRing in collector (120s @ 1Hz + 10s @ 10Hz per series). Route Category::Metric into ring instead of ClickHouse sink.

Fold NVML samples into 1Hz MetricFrames in agent; drop synthetic's forced 1000Hz; add --synthetic-burst stress flag.

Drop 4 tiles_metric MVs and 6 of 8 tiles_activity MVs; keep only 32ms + 2s LODs. Write migrations/0006_shed_metric_tiles.sql.

Move spike detector to API process over the FastRing (rolling median+MAD, 60s). Persist only detected spikes. Delete the 10s CH recompute loop.

Implement GET /api/live SSE (per-second frame: metrics + new spikes + new kernel counts) and /api/live/kernels.

Add followNow live-tail mode to Timeline; consume SSE; auto-advance viewport; pausing on pan-left.

Add CUPTI Profiling API (PM host sampling) module to perfweave-cupti-inject collecting 6 SOL metrics in one pass.

Add CUPTI PC Sampling continuous mode; aggregate top-5 stall reasons per kernel by correlation_id.

Add CUPTI Callback API subscriber for CUPTI_CB_DOMAIN_NVTX; emit MarkerDetail events; stop dropping markers in the collector sink.

Fill registers_per_thread from CUpti_ActivityKernel7.registersPerThread (remove hardcoded 0).

Add KernelSol + StallCount to proto/perfweave.proto; add migrations/0007_kernel_sol.sql; wire collector to persist kernel_sol rows.

Implement deterministic bound classification (MEMORY/COMPUTE/LATENCY/BALANCED) at sampling close in inject.

Build SpikeDrilldown component: waveform lane, kernel Gantt, NVTX lane, SOL cards with roofline + 6-bar + stalls + classification pill. Wire onSpikeClick.

Extend spikeContext GraphQL resolver to return kernels_in_window with joined kernel_sol payload instead of GROUP BY name_id top-3.

Add friendly error in cmd_run when argv[0] is a .cu/.cpp source instead of a binary.

Re-measure overhead on idle / memory-bound SGEMM / compute-bound conv on Jetson; enforce <5% with SOL+PC on; drop L1/L2 metrics if over budget.