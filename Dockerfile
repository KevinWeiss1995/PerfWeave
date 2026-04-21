# Multi-stage build. Stage 1: Rust binaries. Stage 2: web bundle. Stage 3:
# CUDA 12.4 runtime with NVML + CUPTI + DCGM available at runtime. The final
# image is suitable for both single-host `perfweave start` and as a k8s
# DaemonSet.

# ---------------------------------------------------------------------------
FROM rust:1.89-slim AS rust-build
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler pkg-config libssl-dev build-essential ca-certificates \
  && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml rust-toolchain.toml ./
COPY proto ./proto
COPY migrations ./migrations
COPY crates ./crates
# Build with nvml feature so the image can run on CUDA hosts. CUPTI is a
# runtime dep of the separate cupti-inject cdylib; we build both.
RUN cargo build --release --features=nvml \
    -p perfweave-cli -p perfweave-agent -p perfweave-collector \
    -p perfweave-api -p perfweave-nsys-import -p perfweave-cupti-inject

# ---------------------------------------------------------------------------
FROM node:20-alpine AS web-build
WORKDIR /web
COPY web/package.json web/package-lock.json* ./
RUN npm ci || npm install
COPY web/ .
RUN npm run build

# ---------------------------------------------------------------------------
FROM nvidia/cuda:12.4.1-runtime-ubuntu22.04
LABEL org.opencontainers.image.title="perfweave"
LABEL org.opencontainers.image.source="https://github.com/perfweave/perfweave"
ENV PERFWEAVE_WEB_DIR=/opt/perfweave/web \
    PERFWEAVE_LOG_FORMAT=json

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 datacenter-gpu-manager \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/perfweave
COPY --from=rust-build /src/target/release/perfweave /usr/local/bin/
COPY --from=rust-build /src/target/release/perfweave-agent /usr/local/bin/
COPY --from=rust-build /src/target/release/perfweave-collector /usr/local/bin/
COPY --from=rust-build /src/target/release/perfweave-api /usr/local/bin/
COPY --from=rust-build /src/target/release/perfweave-nsys-import /usr/local/bin/
COPY --from=rust-build /src/target/release/libperfweave_cupti_inject.so /usr/local/lib/
COPY --from=web-build /web/dist /opt/perfweave/web

EXPOSE 7777 7778
ENTRYPOINT ["/usr/local/bin/perfweave"]
CMD ["start", "--api-listen", "0.0.0.0:7777", "--collector-listen", "0.0.0.0:7778", "--no-open"]
