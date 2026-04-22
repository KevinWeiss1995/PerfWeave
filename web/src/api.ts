// REST + GraphQL clients. The timeline uses Arrow IPC; everything else uses
// GraphQL. Calls are made against the Rust API which is proxied through Vite
// in dev and served as `/` in prod.

import { tableFromIPC, Table } from "apache-arrow";

export interface TimelineFetch {
  startNs: bigint;
  endNs: bigint;
  pixels: number;
  gpus?: number[];
  categories?: string[];
  kind?: "activity" | "metric" | "raw";
  metricIds?: bigint[];
}

export async function fetchTimeline(opts: TimelineFetch): Promise<{ table: Table; kind: string }> {
  const params = new URLSearchParams();
  params.set("start_ns", opts.startNs.toString());
  params.set("end_ns", opts.endNs.toString());
  params.set("pixels", String(opts.pixels));
  if (opts.gpus && opts.gpus.length) params.set("gpus", opts.gpus.join(","));
  if (opts.categories && opts.categories.length) params.set("categories", opts.categories.join(","));
  if (opts.kind) params.set("kind", opts.kind);
  if (opts.metricIds && opts.metricIds.length) {
    params.set("metric_ids", opts.metricIds.map((i) => i.toString()).join(","));
  }
  const r = await fetch(`/api/timeline?${params}`);
  if (!r.ok) throw new Error(`timeline ${r.status}: ${await r.text()}`);
  const kind = r.headers.get("X-Perfweave-Kind") ?? "activity";
  const buf = await r.arrayBuffer();
  const table = tableFromIPC(new Uint8Array(buf));
  return { table, kind };
}

export async function gql<T = unknown>(query: string, variables?: Record<string, unknown>): Promise<T> {
  const r = await fetch(`/graphql`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ query, variables }),
  });
  const body = await r.json();
  if (body.errors) throw new Error(body.errors.map((e: { message: string }) => e.message).join(", "));
  return body.data as T;
}

export async function resolveStrings(ids: bigint[]): Promise<Map<bigint, string>> {
  if (ids.length === 0) return new Map();
  const data = await gql<{ strings: { id: string; text: string }[] }>(
    `query ($ids: [UInt64!]!) { strings(ids: $ids) { id text } }`,
    { ids: ids.map((i) => i.toString()) },
  );
  const m = new Map<bigint, string>();
  for (const s of data.strings) m.set(BigInt(s.id), s.text);
  return m;
}

export async function fetchCorrelated(correlationId: bigint) {
  return gql<{ correlated: { tsNs: string; durationNs: string; gpuId: number; category: number; nameId: string; pid: number; streamId: number }[] }>(
    `query ($c: UInt64!) { correlated(correlationId: $c) { tsNs: ts_ns durationNs: duration_ns gpuId: gpu_id category nameId: name_id pid streamId: stream_id } }`,
    { c: correlationId.toString() },
  );
}

export async function fetchTopKernels(startNs: bigint, endNs: bigint, gpuId?: number) {
  return gql<{ topKernelsInWindow: { nameId: string; name: string; totalDurationNs: string; launches: string; meanDurationNs: string }[] }>(
    `query ($s: UInt64!, $e: UInt64!, $g: UInt8) {
      topKernelsInWindow(startNs: $s, endNs: $e, gpuId: $g, limit: 3) {
        nameId: name_id name totalDurationNs: total_duration_ns launches meanDurationNs: mean_duration_ns
      }
    }`,
    { s: startNs.toString(), e: endNs.toString(), g: gpuId ?? null },
  );
}

export async function fetchSpikes(startNs: bigint, endNs: bigint) {
  return gql<{ spikes: { bucketStartNs: string; bucketWidthNs: string; nodeId: number; gpuId: number; metricId: string; value: number; zMad: number }[] }>(
    `query ($s: UInt64!, $e: UInt64!) {
      spikes(startNs: $s, endNs: $e) {
        bucketStartNs: bucket_start_ns bucketWidthNs: bucket_width_ns
        nodeId: node_id gpuId: gpu_id metricId: metric_id value zMad: z_mad
      }
    }`,
    { s: startNs.toString(), e: endNs.toString() },
  );
}

export async function fetchSpikeContext(
  bucketStartNs: bigint,
  bucketWidthNs: bigint,
  gpuId: number,
  metricId: bigint,
): Promise<{ spikeContext: SpikeContext }> {
  return gql(
    `query ($bs: UInt64!, $bw: UInt64!, $g: UInt8!, $m: UInt64!) {
      spikeContext(bucketStartNs: $bs, bucketWidthNs: $bw, gpuId: $g, metricId: $m) {
        bucketStartNs: bucket_start_ns
        bucketWidthNs: bucket_width_ns
        gpuId: gpu_id
        metricId: metric_id
        metricName: metric_name
        bottleneck
        avgMemUtil: avg_mem_util
        avgGpuUtil: avg_gpu_util
        topKernels: top_kernels {
          nameId: name_id name totalDurationNs: total_duration_ns
          launches meanDurationNs: mean_duration_ns
        }
        kernelsInWindow: kernels_in_window {
          correlationId: correlation_id
          nameId: name_id
          name
          tsNs: ts_ns
          durationNs: duration_ns
          gpuId: gpu_id
          sol {
            smActivePct: sm_active_pct
            achievedOccupancyPct: achieved_occupancy_pct
            dramBwPct: dram_bw_pct
            l1BwPct: l1_bw_pct
            l2BwPct: l2_bw_pct
            instThroughputPct: inst_throughput_pct
            arithmeticIntensity: arithmetic_intensity
            achievedGflops: achieved_gflops
            bound
            confidence
            source
          }
        }
      }
    }`,
    {
      bs: bucketStartNs.toString(),
      bw: bucketWidthNs.toString(),
      g: gpuId,
      m: metricId.toString(),
    },
  );
}

export type ProfileKernelsAck = {
  nodeId: number;
  requested: number;
  completed: number;
  results: KernelSol[];
};

export async function profileKernels(
  nodeId: number,
  gpuId: number,
  windowStartNs: bigint,
  windowEndNs: bigint,
  correlationIds: bigint[],
  timeoutMs = 5000,
): Promise<{ profileKernels: ProfileKernelsAck }> {
  return gql(
    `mutation ($n: UInt32!, $g: UInt32!, $s: UInt64!, $e: UInt64!, $ids: [UInt64!]!, $t: UInt32) {
      profileKernels(
        nodeId: $n, gpuId: $g,
        windowStartNs: $s, windowEndNs: $e,
        correlationIds: $ids, timeoutMs: $t
      ) {
        nodeId: node_id
        requested
        completed
        results {
          smActivePct: sm_active_pct
          achievedOccupancyPct: achieved_occupancy_pct
          dramBwPct: dram_bw_pct
          l1BwPct: l1_bw_pct
          l2BwPct: l2_bw_pct
          instThroughputPct: inst_throughput_pct
          arithmeticIntensity: arithmetic_intensity
          achievedGflops: achieved_gflops
          bound confidence source
        }
      }
    }`,
    {
      n: nodeId,
      g: gpuId,
      s: windowStartNs.toString(),
      e: windowEndNs.toString(),
      ids: correlationIds.map((i) => i.toString()),
      t: timeoutMs,
    },
  );
}
