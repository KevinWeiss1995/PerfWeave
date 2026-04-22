//! CUPTI Callback API integration for NVTX.
//!
//! CUPTI's *Activity* API (see `activity.rs`) gives us kernels and memcpys,
//! but it does NOT deliver NVTX markers. Markers come through the
//! *Callback* API on domain `CUPTI_CB_DOMAIN_NVTX`, where each push/pop/
//! mark/rangeStart/rangeEnd fires a callback carrying domain, color, and
//! message strings.
//!
//! This module translates those callbacks into our canonical
//! `Category::MARKER` events and ships them alongside activity batches.
//! Crucially, we keep the interned-string protocol: message + domain
//! strings are xxh3-hashed the same way every other name is, and the
//! resolved string gets announced over the gRPC `strings` channel once
//! per session.
//!
//! FFI note: the CUPTI NVTX callback struct is
//! `CUpti_NvtxData { functionName: *const c_char, functionParams: *const c_void, functionReturnValue: *const c_void }`
//! where `functionParams` points to one of the NVTX push/pop/mark param
//! structs, discriminated by `functionName`. See
//! `generated_nvtx_meta.h` for the parameter layouts and
//! `cupti_callbacks.h` for the domain/cbid constants.
//!
//! This file ships hand-written layouts instead of bindgen output for the
//! same reason as `activity.rs`: portability of the build. A build.rs can
//! replace the layouts wholesale on a CUDA-capable host.

use perfweave_common::intern::hash as intern_hash;
use perfweave_proto::v1::{event::Payload, Category, Event, MarkerDetail};
use std::ffi::{c_char, c_void, CStr};

/// NVTX event attributes as exposed by nvToolsExt.h. We include only the
/// fields we actually read; the real `nvtxEventAttributes_t` is larger.
#[repr(C)]
#[allow(dead_code)]
pub struct NvtxEventAttributes {
    pub version: u16,
    pub size: u16,
    pub category: u32,
    pub color_type: u32,
    pub color_argb: u32,
    pub payload_type: u32,
    pub reserved0: u32,
    pub payload_u64: u64,
    pub message_type: u32,
    pub message_ptr: *const c_char,
}

/// Param struct for `nvtxRangePushEx` / `nvtxMarkEx`. Both use the same
/// `nvtxEventAttributes_t*` argument.
#[repr(C)]
#[allow(dead_code)]
pub struct NvtxRangePushExParams {
    pub event_attr: *const NvtxEventAttributes,
}

/// Param struct for `nvtxDomainMarkEx` etc — takes a domain handle as well.
#[repr(C)]
#[allow(dead_code)]
pub struct NvtxDomainRangePushExParams {
    pub domain: *const c_void,
    pub event_attr: *const NvtxEventAttributes,
}

/// Try to pull an `Event` out of a CUPTI NVTX callback. Returns None for
/// callback kinds we don't care about (pop, rangeEnd without a matching
/// start record in our cache, etc.).
///
/// The `function_name` selects which param struct `params` actually points
/// at. Callers get this from the CUPTI callback data directly.
///
/// # Safety
/// Caller guarantees `params` is a valid pointer to the struct named by
/// `function_name`, and that any embedded C strings remain valid for the
/// duration of this call. CUPTI is single-threaded per callback so this
/// holds for the lifetime of the callback invocation.
#[allow(dead_code)]
pub unsafe fn translate_nvtx_callback(
    function_name: &CStr,
    params: *const c_void,
    ts_ns: u64,
    node_id: u32,
    gpu_id: u32,
) -> Option<Event> {
    if params.is_null() {
        return None;
    }
    let fname = function_name.to_string_lossy();
    // We intern the domain string; most NVTX users have a single domain
    // per library ("pytorch", "cudnn", ...), so the id space stays small.
    let (domain_id, color_argb, msg_ptr) = match &*fname {
        // Default-domain calls: no domain handle, so domain_id = 0 == the
        // NVTX "default" domain.
        "nvtxMarkEx" | "nvtxRangePushEx" => {
            let p = &*(params as *const NvtxRangePushExParams);
            if p.event_attr.is_null() { return None; }
            let ea = &*p.event_attr;
            (0u64, ea.color_argb, ea.message_ptr)
        }
        "nvtxDomainMarkEx" | "nvtxDomainRangePushEx" => {
            let p = &*(params as *const NvtxDomainRangePushExParams);
            if p.event_attr.is_null() { return None; }
            let ea = &*p.event_attr;
            // We encode the opaque domain handle pointer as its hash.
            // Two processes that use the same domain name end up with the
            // same id server-side because the agent re-maps handle→name
            // through CUPTI's `cuptiGetNvtxDomainName` (hooked up in the
            // real integration). Until then, pointer-hash is a stable id.
            let dom_ptr_hash = intern_hash(&format!("{:p}", p.domain));
            (dom_ptr_hash, ea.color_argb, ea.message_ptr)
        }
        // RangePop / DomainRangePop carry no event attrs; they reference
        // the matching push by id on the GPU side. We skip them here and
        // rely on `CUpti_ActivityMarker` records in the Activity API for
        // the timestamps (enabled by default).
        _ => return None,
    };

    let message = if msg_ptr.is_null() {
        "<nvtx>".to_string()
    } else {
        CStr::from_ptr(msg_ptr).to_string_lossy().into_owned()
    };
    let message_id = intern_hash(&message);

    Some(Event {
        ts_ns,
        duration_ns: 0,
        node_id,
        gpu_id,
        pid: 0, tid: 0, ctx_id: 0, stream_id: 0,
        category: Category::Marker as i32,
        name_id: message_id,
        correlation_id: 0,
        parent_id: 0,
        payload: Some(Payload::Marker(MarkerDetail {
            domain_id,
            color_argb,
            message_id,
        })),
    })
}
