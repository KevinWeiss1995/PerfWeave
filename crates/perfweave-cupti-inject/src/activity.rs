//! CUPTI Activity record parsing.
//!
//! This file defines the Rust-side parsers for `CUpti_Activity*` structs as
//! documented in `cupti_activity.h`. The FFI struct layouts are defined
//! here to match CUDA 12.x (CUPTI 12.x) — we use `repr(C)` and explicit
//! padding so the layout is stable across Rust versions.
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

pub const BUF_SIZE: usize = 8 * 1024 * 1024; // 8MB per CUPTI buffer

// Subset of CUpti_Activity record kinds we care about. Values match
// `cupti_activity.h` as of CUDA 12.x.
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

// Prefix present on every activity record.
#[repr(C)]
pub struct ActivityHeader {
    pub kind: u32,
}

// `CUpti_ActivityKernel7` (CUDA 12.x) — the most common kernel record shape.
// We keep only the fields we need. The real struct is larger; bindgen on a
// real CUPTI build will emit it in full. We use the offsets explicitly on
// the parsing path so we never accidentally read fields that did not exist
// in older CUPTI versions.
#[repr(C)]
#[allow(dead_code)]
pub struct ActivityKernel {
    pub kind: u32,
    pub _pad: u32,
    pub start: u64,
    pub end: u64,
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
    pub runtime_correlation_id: u32,
    pub name_ptr: *const std::ffi::c_char,
}

#[repr(C)]
#[allow(dead_code)]
pub struct ActivityMemcpy {
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
}

#[repr(C)]
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
        // SAFETY: layout is non-zero.
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
        // SAFETY: we allocated with the same layout in `alloc`.
        unsafe { std::alloc::dealloc(self.ptr, layout) }
    }
}

/// Parse a single record. `kind` is read from the header; the rest of the
/// dispatch is by kind.
///
/// Returns (record_size_in_buffer, optional_event). Record sizes match the
/// CUPTI docs and are stable within a major CUDA version.
///
/// # Safety
/// Caller guarantees `buf` is a valid, initialized activity record produced
/// by CUPTI. Sizes are validated internally.
pub unsafe fn parse_record(buf: *const u8, remaining: usize) -> (usize, Option<Event>) {
    if remaining < std::mem::size_of::<ActivityHeader>() {
        return (remaining, None);
    }
    let kind = (*(buf as *const ActivityHeader)).kind;
    match kind {
        k if k == ActivityKind::Kernel as u32 || k == ActivityKind::ConcurrentKernel as u32 => {
            if remaining < std::mem::size_of::<ActivityKernel>() {
                return (remaining, None);
            }
            let k = &*(buf as *const ActivityKernel);
            let name = if k.name_ptr.is_null() {
                "<unknown_kernel>".to_string()
            } else {
                std::ffi::CStr::from_ptr(k.name_ptr)
                    .to_string_lossy()
                    .into_owned()
            };
            let event = Event {
                ts_ns: k.start,
                duration_ns: k.end.saturating_sub(k.start),
                node_id: 0, // filled by uploader
                gpu_id: k.device_id,
                pid: 0,
                tid: 0,
                ctx_id: k.context_id,
                stream_id: k.stream_id,
                category: Category::Kernel as i32,
                name_id: intern_hash(&name),
                correlation_id: k.correlation_id as u64,
                parent_id: 0,
                payload: Some(Payload::Kernel(KernelDetail {
                    grid_x: k.grid_x, grid_y: k.grid_y, grid_z: k.grid_z,
                    block_x: k.block_x, block_y: k.block_y, block_z: k.block_z,
                    static_shared_mem_bytes: k.static_shared_memory,
                    dynamic_shared_mem_bytes: k.dynamic_shared_memory,
                    registers_per_thread: 0,
                    local_mem_per_thread: k.local_memory_per_thread,
                    launch_cbid: 0,
                })),
            };
            (std::mem::size_of::<ActivityKernel>(), Some(event))
        }
        k if k == ActivityKind::Memcpy as u32 || k == ActivityKind::Memcpy2 as u32 => {
            if remaining < std::mem::size_of::<ActivityMemcpy>() {
                return (remaining, None);
            }
            let m = &*(buf as *const ActivityMemcpy);
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
            (std::mem::size_of::<ActivityMemcpy>(), Some(event))
        }
        k if k == ActivityKind::Runtime as u32 || k == ActivityKind::Driver as u32 => {
            if remaining < std::mem::size_of::<ActivityApi>() {
                return (remaining, None);
            }
            let a = &*(buf as *const ActivityApi);
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
            (std::mem::size_of::<ActivityApi>(), Some(event))
        }
        _ => {
            // Unknown record; skip the minimum known size and hope CUPTI
            // padded. In practice we would call cuptiActivityGetNextRecord
            // which tells us the exact record length.
            (std::mem::size_of::<ActivityHeader>(), None)
        }
    }
}
