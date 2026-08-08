pub mod receiver;
pub mod sender;

use anyhow::{anyhow, Result};
use roc_sys as roc;
use std::ffi::CString;
use std::ptr;
use std::sync::Arc;

/// One shared roc context per engine — pools packets, allocates worker threads.
pub struct RocContext {
    ctx: *mut roc::roc_context,
}

// SAFETY: roc_context is documented as thread-safe (see roc/context.h).
unsafe impl Send for RocContext {}
unsafe impl Sync for RocContext {}

impl RocContext {
    pub fn new() -> Result<Arc<Self>> {
        let cfg = roc::roc_context_config {
            max_packet_size: 0,
            max_frame_size: 0,
        };
        let mut ctx: *mut roc::roc_context = ptr::null_mut();
        let rc = unsafe { roc::roc_context_open(&cfg, &mut ctx) };
        if rc != 0 || ctx.is_null() {
            return Err(anyhow!("roc_context_open failed ({rc})"));
        }
        Ok(Arc::new(Self { ctx }))
    }

    pub fn raw(&self) -> *mut roc::roc_context {
        self.ctx
    }
}

impl Drop for RocContext {
    fn drop(&mut self) {
        unsafe {
            let _ = roc::roc_context_close(self.ctx);
        }
    }
}

/// Allocate and set an endpoint from a URI like `rtp+rs8m://host:port`.
pub(crate) fn endpoint_from_uri(uri: &str) -> Result<*mut roc::roc_endpoint> {
    let mut ep: *mut roc::roc_endpoint = ptr::null_mut();
    let rc = unsafe { roc::roc_endpoint_allocate(&mut ep) };
    if rc != 0 || ep.is_null() {
        return Err(anyhow!("roc_endpoint_allocate failed ({rc})"));
    }
    let cs = CString::new(uri)?;
    let rc = unsafe { roc::roc_endpoint_set_uri(ep, cs.as_ptr()) };
    if rc != 0 {
        unsafe { roc::roc_endpoint_deallocate(ep) };
        return Err(anyhow!("roc_endpoint_set_uri({uri}) failed ({rc})"));
    }
    Ok(ep)
}

pub(crate) fn endpoint_free(ep: *mut roc::roc_endpoint) {
    if !ep.is_null() {
        unsafe { roc::roc_endpoint_deallocate(ep) };
    }
}
