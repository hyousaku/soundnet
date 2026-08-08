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

/// Deterministic packet-encoding id for a given (rate, channels) tuple.
/// Both sender and receiver derive the same id so they agree on the wire
/// format without out-of-band negotiation. Range chosen to stay inside the
/// valid RTP payload-type range [1; 127] but well clear of standard AVP
/// assignments (≤ 34).
pub fn encoding_id_for(rate: u32, channels: u8) -> i32 {
    let rate_idx = match rate {
        44_100 => 0,
        48_000 => 1,
        88_200 => 2,
        96_000 => 3,
        _ => 4,
    };
    let combo = (channels as i32).clamp(1, 32) * 5 + rate_idx;
    100 + (combo % 27) // stays in [100, 126]
}

/// Register (or re-register) a packet encoding matching our frame encoding
/// so libroc doesn't have to guess. We use the **multitrack** channel
/// layout for every route — it works for 1..32 channels without special
/// casing, and both sender and receiver agree on the id we return here.
///
/// Registration is idempotent from libroc's perspective: a duplicate
/// register returns a negative rc which we treat as "already registered,
/// same shape" and log at debug only.
pub fn ensure_encoding(ctx: *mut roc::roc_context, rate: u32, channels: u8) -> i32 {
    let id = encoding_id_for(rate, channels);
    let enc = roc::roc_media_encoding {
        rate,
        format: roc::roc_format::ROC_FORMAT_PCM_FLOAT32,
        channels: roc::roc_channel_layout::ROC_CHANNEL_LAYOUT_MULTITRACK,
        tracks: channels as u32,
    };
    let rc = unsafe { roc::roc_context_register_encoding(ctx, id, &enc) };
    if rc != 0 {
        tracing::debug!("register_encoding({id}, rate={rate}, ch={channels}) rc={rc} (likely already registered)");
    }
    id
}
