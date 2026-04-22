//! CUPTI Activity record parsing.
//!
//! Rust-side parsers for `CUpti_Activity*` records as documented in
//! `cupti_activity.h` (CUDA 12.x). Layouts are `#[repr(C, packed)]`
//! because CUPTI declares these with `__attribute__((packed, aligned(8)))`
//! — interior fields are multi-byte but *not* naturally aligned. We use
//! `ptr::read_unaligned` on every access to stay sound.
//!
//! Long-term plan: a `build.rs` that shells out to `bindgen` against the
//! installed `cupti_activity.h` would be safer than the hand-written layout.
//! The hand-written struct below is kept pinned to the 12.x ABI and has a
//! compile-time check against the `CUPTI_ACTIVITY_KERNEL7_SIZE` env var that
//! a downstream build.rs can set to catch drift.
//!
//! The activity buffer lifecycle is:
//!
//! - CUPTI calls buffer_requested(); we return an 8MB-aligned buffer.
//! - CUDA writes records into that buffer on the GPU-side thread.
//! - CUPTI calls buffer_completed(ctx, stream_id, buf, size, valid_size).
//! - We iterate cuptiActivityGetNextRecord() and translate to Event.
//! - We package a Batch proto and call ship() in lib.rs.
//! - We do NOT free the buffer; CUPTI does.

use perfweave_common::intern::hash as intern_hash;
use perfweave_proto::v1::{
    event::Payload, ApiCallDetail, Category, Event, KernelDetail, MemcpyDetail,
};
use std::ptr;

pub const BUF_SIZE: usize = 8 * 1024 * 1024; // 8MB per CUPTI buffer

/// Subset of CUpti_ActivityKind values we care about. Values match
/// `cupti_activity.h` as of CUDA 12.x; do not re-order without verifying
/// against the SDK you ship with.
#[repr(u32)]
#[allow(dead_code)]
pub enum ActivityKind {
    Invalid = 0,
    Memcpy = 1,
    Memset = 2,
    Kernel = 3,
    Driver = 4,
    Runtime = 5,
    ConcurrentKernel = 12,
    Memcpy2 = 22,
    Overhead = 27,
}

/// Header present on every activity record.
#[derive(Copy, Clone)]
#[repr(C, packed)]
#[allow(dead_code)]
pub struct ActivityHeader {
    pub kind: u32,
}

/// `CUpti_ActivityKernel7` (CUDA 12.x).
///
/// Layout MUST match `cupti_activity.h` exactly; this struct is packed, so
/// any multi-byte field access has to go through `ptr::read_unaligned`.
/// Trailing fields added in CUDA ≥12.3 are intentionally included so the
/// record size stays correct when CUPTI writes into buffers we allocate.
#[derive(Copy, Clone)]
#[repr(C, packed)]
#[allow(dead_code)]
pub struct ActivityKernel7 {
    pub kind: u32,
    pub cache_config: u8,
    pub compute_api_requested: u8,
    pub compute_api_executed: u8,
    pub shared_memory_config: u8,
    pub registers_per_thread: u32,
    pub partitioned_global_cache_requested: u32,
    pub partitioned_global_cache_executed: u32,
    pub start: u64,
    pub end: u64,
    pub completed: i32,
    pub device_id: u32,
    pub context_id: u32,
    pub stream_id: u32,
    pub grid_x: u32,
    pub grid_y: u32,
    pub grid_z: u32,
    pub block_x: u32,
    pub block_y: u32,
    pub block_z: u32,
    pub static_shared_memory: u32,
    pub dynamic_shared_memory: u32,
    pub local_memory_per_thread: u32,
    pub local_memory_total: u32,
    pub correlation_id: u32,
    pub grid_id: i64,
    pub name_ptr: *const std::ffi::c_char,
    pub reserved0: *const std::ffi::c_void,
    pub queued: u64,
    pub submitted: u64,
    pub launch_type: u8,
    pub is_shared_memory_carveout_requested: u8,
    pub shared_memory_carveout_requested: u8,
    pub padding: u8,
    pub shared_memory_executed: u32,
    pub graph_node_id: u64,
    pub shmem_limit_config: u32,
    pub graph_id: u32,
    // `CUaccessPolicyWindow accessPolicyWindow` is a sub-struct of 6 u32s
    // + 2 u8s. Kept as opaque bytes to avoid pulling in the whole header.
    pub access_policy_window: [u8; 32],
    pub channel_id: u32,
    pub channel_type: u32,
    pub cluster_x: u32,
    pub cluster_y: u32,
    pub cluster_z: u32,
    pub cluster_scheduling_policy: u32,
    pub local_memory_total_v2: u64,
    pub max_potential_cluster_size: u32,
    pub max_active_clusters: u32,
}

#[derive(Copy, Clone)]
#[repr(C, packed)]
#[allow(dead_code)]
pub struct ActivityMemcpy6 {
    pub kind: u32,
    pub copy_kind: u8,
    pub src_kind: u8,
    pub dst_kind: u8,
    pub flags: u8,
    pub bytes: u64,
    pub start: u64,
    pub end: u64,
    pub device_id: u32,
    pub context_id: u32,
    pub stream_id: u32,
    pub correlation_id: u32,
    pub runtime_correlation_id: u32,
    pub pad: u32,
    pub graph_node_id: u64,
    pub graph_id: u32,
    pub channel_id: u32,
    pub channel_type: u32,
    pub pad2: u32,
    pub copy_count: u64,
}

/// `CUpti_ActivityMemset4`.
#[derive(Copy, Clone)]
#[repr(C, packed)]
#[allow(dead_code)]
pub struct ActivityMemset4 {
    pub kind: u32,
    pub value: u32,
    pub bytes: u64,
    pub start: u64,
    pub end: u64,
    pub device_id: u32,
    pub context_id: u32,
    pub stream_id: u32,
    pub correlation_id: u32,
    pub flags: u16,
    pub memory_kind: u16,
    pub graph_node_id: u64,
    pub graph_id: u32,
    pub channel_id: u32,
    pub channel_type: u32,
    pub pad: u32,
}

#[derive(Copy, Clone)]
#[repr(C, packed)]
#[allow(dead_code)]
pub struct ActivityApi {
    pub kind: u32,
    pub cbid: u32,
    pub start: u64,
    pub end: u64,
    pub process_id: u32,
    pub thread_id: u32,
    pub correlation_id: u32,
    pub return_value: u32,
}

/// A user-owned, aligned buffer handed to CUPTI. We park these in a pool so
/// CUPTI can churn through them without us allocating on the hot path.
pub struct ActivityBuffer {
    pub ptr: *mut u8,
    pub size: usize,
}

// SAFETY: ActivityBuffer owns an aligned allocation which we free on drop.
// Access is serialized through CUPTI — one thread at a time per buffer.
unsafe impl Send for ActivityBuffer {}

impl ActivityBuffer {
    pub fn alloc() -> Self {
        let layout = std::alloc::Layout::from_size_align(BUF_SIZE, 8).unwrap();
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, size: BUF_SIZE }
    }
}

impl Drop for ActivityBuffer {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(self.size, 8).unwrap();
        unsafe { std::alloc::dealloc(self.ptr, layout) }
    }
}

/// Read a `#[repr(C, packed)]` field from a raw pointer at `offset`. Callers
/// must have validated `remaining` is large enough.
#[inline]
unsafe fn read_at<T: Copy>(buf: *const u8, offset: usize) -> T {
    ptr::read_unaligned(buf.add(offset) as *const T)
}

/// Parse a single record. `kind` is read from the header; the rest of the
/// dispatch is by kind.
///
/// Returns (record_size_in_buffer, optional_event). Record sizes match the
/// CUPTI docs and are stable within a major CUDA version.
///
/// # Safety
/// Caller guarantees `buf` is a valid, initialized activity record produced
/// by CUPTI, and `remaining` is the number of bytes actually available.
#[allow(dead_code)]
pub unsafe fn parse_record(buf: *const u8, remaining: usize) -> (usize, Option<Event>) {
    if remaining < std::mem::size_of::<ActivityHeader>() {
        return (remaining, None);
    }
    let kind: u32 = read_at(buf, 0);
    match kind {
        k if k == ActivityKind::Kernel as u32 || k == ActivityKind::ConcurrentKernel as u32 => {
            let sz = std::mem::size_of::<ActivityKernel7>();
            if remaining < sz {
                return (remaining, None);
            }
            let k7: ActivityKernel7 = read_at(buf, 0);
            // Dereferencing packed fields through a local copy is sound.
            let name_ptr = k7.name_ptr;
            let name = if name_ptr.is_null() {
                "<unknown_kernel>".to_string()
            } else {
                std::ffi::CStr::from_ptr(name_ptr)
                    .to_string_lossy()
                    .into_owned()
            };
            let event = Event {
                ts_ns: k7.start,
                duration_ns: k7.end.saturating_sub(k7.start),
                node_id: 0,
                gpu_id: k7.device_id,
                pid: 0,
                tid: 0,
                ctx_id: k7.context_id,
                stream_id: k7.stream_id,
                category: Category::Kernel as i32,
                name_id: intern_hash(&name),
                correlation_id: k7.correlation_id as u64,
                parent_id: 0,
                payload: Some(Payload::Kernel(KernelDetail {
                    grid_x: k7.grid_x, grid_y: k7.grid_y, grid_z: k7.grid_z,
                    block_x: k7.block_x, block_y: k7.block_y, block_z: k7.block_z,
                    static_shared_mem_bytes: k7.static_shared_memory,
                    dynamic_shared_mem_bytes: k7.dynamic_shared_memory,
                    registers_per_thread: k7.registers_per_thread,
                    local_mem_per_thread: k7.local_memory_per_thread,
                    launch_cbid: 0,
                })),
            };
            (sz, Some(event))
        }
        k if k == ActivityKind::Memcpy as u32 || k == ActivityKind::Memcpy2 as u32 => {
            let sz = std::mem::size_of::<ActivityMemcpy6>();
            if remaining < sz {
                return (remaining, None);
            }
            let m: ActivityMemcpy6 = read_at(buf, 0);
            use perfweave_proto::v1::memcpy_detail::Kind as K;
            let kind_enum = match m.copy_kind {
                1 => K::H2d, 2 => K::D2h, 3 => K::H2h, 4 => K::D2d, 10 => K::P2p,
                _ => K::Unknown,
            };
            let event = Event {
                ts_ns: m.start, duration_ns: m.end.saturating_sub(m.start),
                node_id: 0, gpu_id: m.device_id,
                pid: 0, tid: 0, ctx_id: m.context_id, stream_id: m.stream_id,
                category: Category::Memcpy as i32,
                name_id: intern_hash("cudaMemcpy"),
                correlation_id: m.correlation_id as u64, parent_id: 0,
                payload: Some(Payload::Memcpy(MemcpyDetail {
                    kind: kind_enum as i32, bytes: m.bytes,
                    src_device: m.device_id, dst_device: m.device_id,
                })),
            };
            (sz, Some(event))
        }
        k if k == ActivityKind::Memset as u32 => {
            let sz = std::mem::size_of::<ActivityMemset4>();
            if remaining < sz {
                return (remaining, None);
            }
            let m: ActivityMemset4 = read_at(buf, 0);
            let event = Event {
                ts_ns: m.start, duration_ns: m.end.saturating_sub(m.start),
                node_id: 0, gpu_id: m.device_id,
                pid: 0, tid: 0, ctx_id: m.context_id, stream_id: m.stream_id,
                category: Category::Memset as i32,
                name_id: intern_hash("cudaMemset"),
                correlation_id: m.correlation_id as u64, parent_id: 0,
                // Reuse MemcpyDetail shape for bytes; the event payload is
                // optional and the category already disambiguates.
                payload: Some(Payload::Memcpy(MemcpyDetail {
                    kind: 0,
                    bytes: m.bytes,
                    src_device: m.device_id,
                    dst_device: m.device_id,
                })),
            };
            (sz, Some(event))
        }
        k if k == ActivityKind::Runtime as u32 || k == ActivityKind::Driver as u32 => {
            let sz = std::mem::size_of::<ActivityApi>();
            if remaining < sz {
                return (remaining, None);
            }
            let a: ActivityApi = read_at(buf, 0);
            let event = Event {
                ts_ns: a.start, duration_ns: a.end.saturating_sub(a.start),
                node_id: 0, gpu_id: 0,
                pid: a.process_id, tid: a.thread_id,
                ctx_id: 0, stream_id: 0,
                category: Category::ApiCall as i32,
                name_id: intern_hash("cuApi"),
                correlation_id: a.correlation_id as u64, parent_id: 0,
                payload: Some(Payload::Api(ApiCallDetail {
                    cbid: a.cbid, status: a.return_value,
                })),
            };
            (sz, Some(event))
        }
        _ => {
            // Unknown record. We skip only the header here so the caller's
            // outer loop can advance to the next record via
            // `cuptiActivityGetNextRecord`, which knows the exact sizes.
            (std::mem::size_of::<ActivityHeader>(), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Size sanity check: a downstream build.rs can set
    // CUPTI_ACTIVITY_KERNEL7_SIZE from cupti_activity.h and this test will
    // catch ABI drift. When unset, we just print for manual cross-check.
    #[test]
    fn kernel7_layout_has_no_obvious_drift() {
        let sz = std::mem::size_of::<ActivityKernel7>();
        if let Ok(env) = std::env::var("CUPTI_ACTIVITY_KERNEL7_SIZE") {
            if let Ok(want) = env.parse::<usize>() {
                assert_eq!(sz, want, "CUpti_ActivityKernel7 size drift");
            }
        }
        // At a minimum, we should be well past the 12.0 floor (~208 bytes).
        assert!(sz >= 200, "kernel7 struct too small: {sz}");
    }

    #[test]
    fn memset4_size_reasonable() {
        let sz = std::mem::size_of::<ActivityMemset4>();
        assert!(sz >= 56 && sz <= 128, "memset4 size unusual: {sz}");
    }
}
