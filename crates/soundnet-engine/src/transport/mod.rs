pub mod receiver;
pub mod sender;

use anyhow::{anyhow, Result};
use roc_sys as roc;
use std::collections::HashSet;
use std::ffi::CString;
use std::ptr;
use std::sync::{Arc, Mutex};

/// One shared roc context per engine — pools packets, allocates worker threads.
pub struct RocContext {
    ctx: *mut roc::roc_context,
    /// (rate, channels) tuples we've already registered on this context so
    /// we don't re-register on every route apply and spam libroc's log with
    /// "payload type already exists" errors.
    registered: Mutex<HashSet<(u32, u8)>>,
}

// SAFETY: roc_context is documented as thread-safe (see roc/context.h).
unsafe impl Send for RocContext {}
unsafe impl Sync for RocContext {}

/// Route libroc's internal diagnostics into `tracing` instead of letting
/// them go to stderr at ERROR only.
///
/// libroc knows things about this stream that its public metrics API does
/// not expose — roc 0.4's `roc_connection_metrics` carries `e2e_latency` and
/// nothing else, so packet loss, jitter-buffer starvation and latency-tuner
/// activity are only ever visible here. When a glitch has to be attributed
/// to the network, the buffer, or the sound card, this is the only witness
/// for the middle one.
///
/// Level comes from `SOUNDNET_ROC_LOG` (error/info/note/debug/trace),
/// defaulting to `error` — roc's own default — because `debug` is per-packet
/// chatty enough to affect the audio threads it is reporting on.
fn install_roc_logger() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let level = match std::env::var("SOUNDNET_ROC_LOG").as_deref() {
            Ok("none") => roc::roc_log_level::ROC_LOG_NONE,
            Ok("info") => roc::roc_log_level::ROC_LOG_INFO,
            Ok("note") => roc::roc_log_level::ROC_LOG_NOTE,
            Ok("debug") => roc::roc_log_level::ROC_LOG_DEBUG,
            Ok("trace") => roc::roc_log_level::ROC_LOG_TRACE,
            _ => roc::roc_log_level::ROC_LOG_ERROR,
        };
        unsafe {
            roc::roc_log_set_handler(Some(roc_log_handler), std::ptr::null_mut());
            roc::roc_log_set_level(level);
        }
    });
}

/// # Safety
/// Called by libroc with a message valid only for the duration of this call.
/// Everything is read and formatted before returning; nothing is retained.
/// Must not unwind into C — hence the `catch_unwind`, which is cheap next to
/// the formatting we are already doing and turns a panic in a logging path
/// into a lost log line rather than an aborted process.
unsafe extern "C" fn roc_log_handler(msg: *const roc::roc_log_message, _arg: *mut std::ffi::c_void) {
    let _ = std::panic::catch_unwind(|| {
        let Some(msg) = msg.as_ref() else { return };
        let cstr = |p: *const std::ffi::c_char| -> String {
            if p.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        let module = cstr(msg.module);
        let text = cstr(msg.text);
        match msg.level {
            roc::roc_log_level::ROC_LOG_ERROR => tracing::error!(target: "roc", "{module}: {text}"),
            roc::roc_log_level::ROC_LOG_INFO | roc::roc_log_level::ROC_LOG_NOTE => {
                tracing::info!(target: "roc", "{module}: {text}")
            }
            _ => tracing::debug!(target: "roc", "{module}: {text}"),
        }
    });
}

impl RocContext {
    pub fn new() -> Result<Arc<Self>> {
        install_roc_logger();
        let cfg = roc::roc_context_config {
            max_packet_size: 0,
            max_frame_size: 0,
        };
        let mut ctx: *mut roc::roc_context = ptr::null_mut();
        let rc = unsafe { roc::roc_context_open(&cfg, &mut ctx) };
        if rc != 0 || ctx.is_null() {
            return Err(anyhow!("roc_context_open failed ({rc})"));
        }
        Ok(Arc::new(Self {
            ctx,
            registered: Mutex::new(HashSet::new()),
        }))
    }

    pub fn raw(&self) -> *mut roc::roc_context {
        self.ctx
    }

    /// Register the packet encoding for `(rate, channels)` on this context
    /// exactly once. Returns the encoding id both sender and receiver should
    /// use for that tuple.
    pub fn ensure_encoding(&self, rate: u32, channels: u8) -> i32 {
        let id = encoding_id_for(rate, channels);
        let mut seen = self.registered.lock().unwrap();
        if seen.contains(&(rate, channels)) {
            return id;
        }
        let enc = roc::roc_media_encoding {
            rate,
            format: roc::roc_format::ROC_FORMAT_PCM_FLOAT32,
            channels: roc::roc_channel_layout::ROC_CHANNEL_LAYOUT_MULTITRACK,
            tracks: channels as u32,
        };
        let rc = unsafe { roc::roc_context_register_encoding(self.ctx, id, &enc) };
        if rc != 0 {
            tracing::warn!(
                "register_encoding({id}, rate={rate}, ch={channels}) rc={rc}"
            );
        }
        seen.insert((rate, channels));
        id
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

// (encoding registration moved to RocContext::ensure_encoding so we can
//  dedupe registrations per context and not re-register the same (rate,
//  channels) tuple every time a route is applied.)
