# PerfWeave

Unified GPU observability and profiling on a single time-aligned timeline.
Metrics, traces, and kernel events in one view, one binary.

## What makes this different

- **One timeline for everything.** Metrics (NVML/DCGM/Tegra sysfs), CUDA
  runtime/driver API calls, kernels, and memcpys share one
  nanosecond-precision time axis. Click an API call — the kernel it
  launched highlights automatically via CUPTI correlation ids.
- **Two-plane architecture.** A live plane (in-memory ring, WebSocket) for
  real-time anomaly hunting, and a replay plane (ClickHouse) for
  drilldown and offline import. See
  [`Two-Plane Rework + SOL Kernel Profile.md`](./Two-Plane%20Rework%20%2B%20SOL%20Kernel%20Profile.md).
- **Zero-config.** `perfweave start` auto-detects GPUs (NVML on x86, Tegra
  sysfs on Jetson), brings up the server against local ClickHouse, and
  opens the UI.
- **Browser UI, WebGL2, designed for 10M events.** No SVG, no D3, no
  Grafana plugins. Tile pyramid in ClickHouse → Arrow IPC → instanced
  quads. Pan/zoom from 1s windows to 1µs windows without a reload.
- **Overhead budget < 5%.** CUPTI Activity-only by default. Callback API
  and PM host sampling (SOL kernel profile) are opt-in.

## Quick start

### x86 CUDA host (full telemetry)

```bash
docker compose up -d clickhouse

cargo build --release --features nvml \
  -p perfweave-cli -p perfweave-agent \
  -p perfweave-server -p perfweave-cupti-inject

cd web && npm install && npm run build && cd ..

./target/release/perfweave start --web-dir web/dist
# http://localhost:7777 opens automatically
# `start` also launches an in-process agent by default (NVML).
```

### Jetson / Tegra (Orin, Xavier)

NVML is crippled on Tegra (no utilization, UMA memory reporting is wrong,
flaky power). The agent auto-detects L4T and uses a sysfs sampler
instead. **Do not pass `--features nvml` on Jetson.**

```bash
docker compose up -d clickhouse

cargo build --release \
  -p perfweave-cli -p perfweave-agent \
  -p perfweave-server -p perfweave-cupti-inject

cd web && npm install && npm run build && cd ..

./target/release/perfweave start --web-dir web/dist
# Force the Tegra sampler even on non-L4T kernels: PERFWEAVE_FORCE_TEGRA=1
```

### Any host (synthetic, no GPU required)

```bash
docker compose up -d clickhouse
cargo build --release -p perfweave-cli -p perfweave-agent -p perfweave-server
./target/release/perfweave start --web-dir web/dist --synthetic
```

### Multi-node

On each GPU node (build with `--features nvml` on x86, plain on Jetson):

```bash
./target/release/perfweave-agent --collector http://<server>:7778
```

On the server:

```bash
./target/release/perfweave start --no-local-agent --web-dir web/dist
```

### Pure-UI work (no backend)

```bash
cd web && npm run dev
# http://localhost:5173/?synthetic
```

## Layout

```
.
├── Cargo.toml                         — workspace
├── proto/perfweave.proto              — canonical event schema
├── migrations/                        — ClickHouse DDL (embedded in the binary)
├── crates/
│   ├── perfweave-common/              — shared types, clock alignment, ports, logging
│   ├── perfweave-proto/               — tonic/prost build of proto/
│   ├── perfweave-agent/               — NVML/DCGM/CUPTI/Tegra sampler host
│   │                                    (features: nvml, dcgm, cupti; tegra is always on)
│   ├── perfweave-cupti-inject/        — cdylib loaded via CUDA_INJECTION64_PATH
│   │                                    (Activity API + Callback API + PM host sampling)
│   ├── perfweave-server/              — gRPC ingest + live ring + replay + axum API
│   │                                    + GraphQL + Arrow IPC (replaces api/collector)
│   ├── perfweave-cli/                 — the `perfweave` binary (start/run/doctor/import)
│   └── perfweave-nsys-import/         — offline .nsys-rep → events
├── web/                               — Vite + React + raw WebGL2
└── scripts/                           — overhead and frontend benchmarks
```

## Feature matrix per source

| Source           | Live | Offline | Overhead | Notes                                |
|------------------|------|---------|----------|--------------------------------------|
| NVML (x86)       | yes  | —       | <0.1%    | utilization, mem, power, clocks      |
| Tegra sysfs      | yes  | —       | <0.1%    | util, mem (UMA), power rails (Jetson)|
| DCGM embedded    | yes  | —       | <0.3%    | SM occupancy, DRAM, NVLink, PCIe     |
| CUPTI Activity   | yes  | —       | ~1–3%    | kernels, memcpys, runtime/driver API |
| CUPTI PM (SOL)   | opt-in | —     | ~3–5%    | 6 SOL metrics / kernel, one pass     |
| .nsys-rep        | —    | yes     | 0        | drag-drop import via `nsys export`   |
| .ncu-rep         | —    | stub    | 0        | metrics merged onto kernel rows      |

## Ports

- `7777` — UI / REST / GraphQL / SSE / WebSocket (HTTP)
- `7778` — Server gRPC ingest (agents connect here)
- `7780` — Agent local RPC (used by the server for on-demand kernel replay)
- `8123` / `9000` — ClickHouse HTTP / native

## Validating the MVP gates

```bash
# Week 1: clock alignment < 2µs
cargo run --release -p perfweave-cli -- validate-clocks

# Week 1: 10M events at 60 FPS
# see scripts/bench_frontend.md

# Week 4: <5% CUPTI overhead on real workloads
./scripts/bench_overhead.sh
```

## Operational checks

```bash
perfweave doctor            # environment diagnostics with fixes
perfweave doctor --json     # machine-readable
```

## Upgrading from pre-rework

If you see `package ID specification 'perfweave-api' did not match any
packages`, your build command is from before the two-plane rework. The
old `perfweave-api` and `perfweave-collector` crates have been merged
into **`perfweave-server`**. Replace both `-p` flags with
`-p perfweave-server`.

## License

Apache-2.0
