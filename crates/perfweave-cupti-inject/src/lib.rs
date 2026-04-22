//! CUPTI injection library. Loaded into the target CUDA process via
//! `LD_PRELOAD=libperfweave_cupti_inject.so` (or used with CUPTI's
//! `CUDA_INJECTION64_PATH` env var, which is the documented path that
//! survives MPI launchers and container entrypoints).
//!
//! Responsibilities:
//! 1. Subscribe to the CUPTI Activity API for:
//!     - CUPTI_ACTIVITY_KIND_KERNEL and CONCURRENT_KERNEL
//!     - CUPTI_ACTIVITY_KIND_MEMCPY and MEMCPY2
//!     - CUPTI_ACTIVITY_KIND_MEMSET
//!     - CUPTI_ACTIVITY_KIND_RUNTIME and DRIVER (API ranges)
//!     - CUPTI_ACTIVITY_KIND_OVERHEAD (report overhead back into UI)
//! 2. Provide buffer request/complete callbacks; parse records into our
//!    canonical `Event` proto; ship over a Unix socket to the agent.
//! 3. On library unload, flush the buffer, close the socket.
//!
//! Overhead budget: activity-only mode stays well under 3% for typical
//! DL training (per CUPTI docs); we do NOT enable PC sampling or synchronous
//! callbacks by default — those are opt-in via env var.

use parking_lot::Mutex;
use perfweave_proto::v1::Batch;
use prost::Message;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

mod activity;
mod callback;
#[allow(dead_code)]
mod pm;
mod transport;

pub use activity::ActivityBuffer;

/// Initialize CUPTI. Called from the `InitializeInjection` symbol (the entry
/// point CUDA expects when `CUDA_INJECTION64_PATH` is set) or from the cdylib
/// ctor on plain `LD_PRELOAD`.
///
/// Returns 0 on success to match CUPTI's expected ABI.
///
/// Real integration steps (implemented when `build` feature is enabled and
/// the toolchain has CUPTI headers on the include path):
///
/// ```ignore
/// extern "C" fn InitializeInjection() -> c_int {
///     cupti::Activity::enable(CUPTI_ACTIVITY_KIND_KERNEL);
///     cupti::Activity::enable(CUPTI_ACTIVITY_KIND_MEMCPY);
///     cupti::Activity::enable(CUPTI_ACTIVITY_KIND_RUNTIME);
///     cupti::Activity::enable(CUPTI_ACTIVITY_KIND_DRIVER);
///     cupti::Activity::register_callbacks(buffer_requested, buffer_completed);
///     0
/// }
/// ```
///
/// The `build` gate wraps the real CUPTI bindings so this crate builds on
/// any host; on a CUDA-capable build host we generate bindings from
/// `$CUPTI_PATH/include/cupti.h` via `bindgen`.
#[no_mangle]
pub extern "C" fn InitializeInjection() -> i32 {
    perfweave_common::logging::init("cupti-inject");
    // `connect()` already retries with backoff for up to 10s. If it still
    // fails, we keep the injector installed (return 0 = success) so the
    // target CUDA process does not die: ship() will attempt its own
    // reconnect per batch and we'd rather observe a late-started agent
    // than refuse to run the user's workload.
    match transport::connect() {
        Ok(conn) => {
            GLOBAL.lock().replace(Injector { transport: conn });
            tracing::info!("perfweave CUPTI injector initialized");
        }
        Err(e) => {
            tracing::warn!(
                error=%e,
                "cupti injector could not reach agent within init budget; \
                 will retry on each batch. Kernels may be lost until the agent \
                 is up. Run `perfweave doctor` to diagnose."
            );
        }
    }
    0
}

struct Injector {
    // Read inside `ship()` once CUPTI's `ActivityBufferCompleted` callback is
    // wired up under the `build` feature. Kept here so we don't reconnect
    // per-batch.
    #[allow(dead_code)]
    transport: transport::AgentConn,
}

static GLOBAL: Mutex<Option<Injector>> = Mutex::new(None);

/// Called from CUPTI's `ActivityBufferCompleted` callback (via extern C in
/// activity.rs). Takes ownership of the parsed `Batch` and forwards it to
/// the agent.
///
/// Currently dead on hosts built without the `build` feature: the CUPTI
/// subscription that drives it is feature-gated. The symbol is kept so the
/// callback wiring is a drop-in change when the feature is turned on.
#[allow(dead_code)]
pub(crate) fn ship(batch: Batch) {
    let mut guard = GLOBAL.lock();
    if guard.is_none() {
        // Init couldn't reach the agent within the init budget. Try once
        // more cheaply here so a late-started agent still gets every
        // subsequent batch. If it still fails we drop *this* batch only
        // and try again next time.
        match transport::reconnect(std::time::Duration::from_millis(200)) {
            Ok(conn) => {
                *guard = Some(Injector { transport: conn });
                tracing::info!("perfweave CUPTI injector connected to agent");
            }
            Err(_) => {
                DROPPED_BATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
    }
    let injector = guard.as_mut().expect("just set above");
    let mut buf = Vec::with_capacity(batch.encoded_len() + 4);
    let len = batch.encoded_len() as u32;
    buf.extend_from_slice(&len.to_le_bytes());
    if let Err(e) = batch.encode(&mut buf) {
        tracing::warn!(error=%e, "encode batch failed");
        return;
    }
    if injector.transport.stream.write_all(&buf).is_ok() {
        return;
    }
    // Write failed → agent likely restarted. Try a single short reconnect
    // and retry the write. If reconnect fails too, drop the batch but
    // surface a counter so the UI can flag live-stream drops rather than
    // silently losing kernels.
    DROPPED_BATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    match transport::reconnect(std::time::Duration::from_millis(500)) {
        Ok(conn) => {
            injector.transport = conn;
            if let Err(e) = injector.transport.stream.write_all(&buf) {
                tracing::warn!(error=%e, "CUPTI ship failed after reconnect");
            } else {
                tracing::info!("CUPTI ship reconnected to agent");
            }
        }
        Err(e) => {
            tracing::warn!(error=%e, "CUPTI agent unreachable; dropping batch");
        }
    }
}

/// Exposed so the agent (and, eventually, the UI) can surface drops.
pub static DROPPED_BATCHES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

// Placeholder to make the cdylib non-empty on build hosts without CUPTI.
// The agent side handles absence gracefully (empty Unix socket).
#[no_mangle]
pub extern "C" fn perfweave_cupti_noop() {}

#[allow(dead_code)]
fn _ensure_unix_sym_used(_s: Arc<UnixStream>) {}
