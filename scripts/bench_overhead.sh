#!/usr/bin/env bash
# PerfWeave overhead validator.
#
# Runs three canonical CUDA workloads with and without PerfWeave's CUPTI
# injector attached, and computes the overhead percentage. Fails the script
# if any workload exceeds 5% overhead.
#
# Prerequisites (must be on PATH on a Linux + CUDA 12 host):
#   - nvidia-smi
#   - a CUDA-capable GCC
#   - python3 with pytorch installed (for the DL workloads)
#   - perfweave + libperfweave_cupti_inject.so already built
#
# Usage:
#   scripts/bench_overhead.sh [--iterations N]
#
# Output: a table and non-zero exit on budget violation.

set -euo pipefail

ITERATIONS="${ITERATIONS:-50}"
INJECT_SO="${PERFWEAVE_CUPTI_INJECT_SO:-target/release/libperfweave_cupti_inject.so}"
BUDGET_PERCENT=5

if [[ ! -f "$INJECT_SO" ]]; then
  echo "error: $INJECT_SO not found. Build with: cargo build --release -p perfweave-cupti-inject"
  exit 2
fi

run_workload() {
  local name="$1"
  local cmd="$2"
  echo "--- $name ---"

  local baseline_ns
  baseline_ns=$(bash -c "$cmd" 2>&1 | awk '/elapsed_ns=/{print $NF}' | tail -1 | tr -dc '0-9')
  if [[ -z "$baseline_ns" ]]; then echo "baseline output parse failed"; exit 3; fi

  local instrumented_ns
  instrumented_ns=$(
    CUDA_INJECTION64_PATH="$INJECT_SO" \
    PERFWEAVE_CUPTI_SOCK="/tmp/perfweave.cupti.sock" \
    bash -c "$cmd" 2>&1 | awk '/elapsed_ns=/{print $NF}' | tail -1 | tr -dc '0-9'
  )

  local overhead_pct
  overhead_pct=$(python3 -c "print(f'{(int($instrumented_ns) - int($baseline_ns)) / int($baseline_ns) * 100:.2f}')")
  printf "%-30s baseline=%s ns  inst=%s ns  overhead=%s%%\n" "$name" "$baseline_ns" "$instrumented_ns" "$overhead_pct"

  python3 -c "import sys; sys.exit(0 if float('$overhead_pct') <= $BUDGET_PERCENT else 1)"
}

# Workload 1: idle (only kernel is a no-op)
run_workload "idle" \
  "python3 -c 'import time,torch; torch.cuda.synchronize(); s=time.perf_counter_ns(); torch.cuda.synchronize(); print(f\"elapsed_ns={time.perf_counter_ns()-s}\")'"

# Workload 2: memory-bound SGEMM (large, low arithmetic intensity)
run_workload "memory-bound SGEMM" \
  "python3 - <<'PY'
import time, torch
torch.backends.cuda.matmul.allow_tf32 = False
a = torch.randn(1024, 8192, device='cuda')
b = torch.randn(8192, 1024, device='cuda')
torch.cuda.synchronize()
s = time.perf_counter_ns()
for _ in range($ITERATIONS):
    c = a @ b
torch.cuda.synchronize()
print(f'elapsed_ns={time.perf_counter_ns()-s}')
PY"

# Workload 3: compute-bound conv
run_workload "compute-bound conv" \
  "python3 - <<'PY'
import time, torch
x = torch.randn(64, 3, 512, 512, device='cuda')
w = torch.randn(64, 3, 7, 7, device='cuda')
torch.cuda.synchronize()
s = time.perf_counter_ns()
for _ in range($ITERATIONS):
    y = torch.nn.functional.conv2d(x, w, padding=3)
torch.cuda.synchronize()
print(f'elapsed_ns={time.perf_counter_ns()-s}')
PY"

echo "all workloads within ${BUDGET_PERCENT}% overhead budget"
