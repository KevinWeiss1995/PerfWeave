# PerfWeave

Unified GPU observability and profiling on a single time-aligned timeline.
Metrics, traces, and kernel events in one view, one binary.

## What makes this different

- **One timeline for everything.** Metrics (NVML/DCGM), CUDA runtime/driver
  API calls, kernels, and memcpys share one nanosecond-precision time axis.
  Click an API call — the kernel it launched highlights automatically via
  CUPTI correlation ids.
- **Zero-config.** `perfweave start` detects GPUs via NVML, brings up a
  collector + API against local ClickHouse, and opens the UI.
- **Browser UI, WebGL2, designed for 10M events.** No SVG, no D3, no
  Grafana plugins. Tile pyramid in ClickHouse → Arrow IPC → instanced
  quads. Pan/zoom from 1s windows to 1µs windows without a reload.
- **Overhead budget < 5%.** CUPTI Activity-only by default. Callback API
  is opt-in. PC sampling is post-MVP.

## Quick start

### On a CUDA host (full telemetry)

```bash
docker compose up -d clickhouse
cargo build --release --features nvml \
  -p perfweave-cli -p perfweave-agent -p perfweave-collector \
  -p perfweave-api -p perfweave-cupti-inject

cd web && npm install && npm run build && cd ..

./target/release/perfweave start --web-dir web/dist
# http://localhost:7777 opens automatically
# `start` also launches an in-process agent by default (NVML).
```

### On any host (synthetic, no GPU required)

```bash
docker compose up -d clickhouse
cargo build --release    # no GPU features
./target/release/perfweave start --web-dir web/dist --synthetic
```

### Multi-node

On each GPU node (after building with the `nvml` feature):

```bash
./target/release/perfweave-agent --collector http://<server>:7778
```

On the server:

```bash
./target/release/perfweave start --no-local-agent --web-dir web/dist
```

Or for pure-UI work without any backend:

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
│   ├── perfweave-common/              — shared types, clock alignment, logging
│   ├── perfweave-proto/               — tonic/prost build of proto/
│   ├── perfweave-agent/               — NVML/DCGM/CUPTI sampler host (features: nvml, dcgm, cupti)
│   ├── perfweave-cupti-inject/        — cdylib loaded via CUDA_INJECTION64_PATH
│   ├── perfweave-collector/           — gRPC sink → ClickHouse
│   ├── perfweave-api/                 — axum + GraphQL + Arrow IPC
│   ├── perfweave-cli/                 — the `perfweave` binary (start/run/doctor)
│   └── perfweave-nsys-import/         — offline .nsys-rep → events
├── web/                               — Vite + React + raw WebGL2
└── scripts/                           — overhead and frontend benchmarks
```

## Feature matrix per source

| Source         | Live | Offline | Overhead | Notes                                |
|----------------|------|---------|----------|--------------------------------------|
| NVML           | yes  | —       | <0.1%    | utilization, mem, power, clocks      |
| DCGM embedded  | yes  | —       | <0.3%    | SM occupancy, DRAM, NVLink, PCIe     |
| CUPTI Activity | yes  | —       | ~1–3%    | kernels, memcpys, runtime/driver API |
| .nsys-rep      | —    | yes     | 0        | drag-drop import via `nsys export`   |
| .ncu-rep       | —    | stub    | 0        | metrics merged onto kernel rows      |

## Ports

- `7777` — UI / REST / GraphQL (HTTP)
- `7778` — Collector gRPC (agents connect here)
- `8123/9000` — ClickHouse

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

## License

Apache-2.0
