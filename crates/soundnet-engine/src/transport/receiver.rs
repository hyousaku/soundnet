//! Roc receiver: binds local UDP endpoints and decodes incoming frames.
//!
//! Like the sender this is a handle, not a worker. The thread that owns it is
//! the receive pipeline (see `pipeline/recv.rs`), which reads one period from
//! here and writes it straight to ALSA — no buffer in between, so the sound
//! card's clock is the only clock in the path and roc's jitter buffer is the
//! only place network timing slack is absorbed.

use anyhow::{anyhow, bail, Result};
use roc_sys as roc;
use soundnet_protocol::StreamSpec;
use std::sync::Arc;

use super::{endpoint_free, endpoint_from_uri, RocContext};

pub struct Receiver {
    raw: *mut roc::roc_receiver,
    /// Keeps the shared context alive for at least as long as this receiver.
    _ctx: Arc<RocContext>,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        unsafe {
            let _ = roc::roc_receiver_close(self.raw);
        }
    }
}

impl Receiver {
    /// Fill `samples` with one period of decoded audio.
    ///
    /// Non-blocking: configured `ROC_CLOCK_SOURCE_EXTERNAL`, so this returns
    /// as soon as it has produced a frame — zero-filled if no sender has
    /// connected yet, or resampled from the jitter buffer if one has. The
    /// caller then blocks in `snd_pcm_writei`, which is what paces the loop.
    pub fn read(&mut self, samples: &mut [f32]) -> Result<()> {
        let mut frame = roc::roc_frame {
            samples: samples.as_mut_ptr() as *mut _,
            samples_size: std::mem::size_of_val(samples),
        };
        let rc = unsafe { roc::roc_receiver_read(self.raw, &mut frame) };
        if rc != 0 {
            bail!("roc_receiver_read failed ({rc})");
        }
        Ok(())
    }

    /// Largest end-to-end latency (ns) across the connections feeding this
    /// receiver, or `None` when nothing is connected yet.
    ///
    /// `None` rather than 0 is load-bearing: a freshly connected RTCP session
    /// can legitimately report zero, so zero cannot double as "no data" —
    /// see `routing::ns_to_ms` and the honesty note on `StreamStats`.
    pub fn query_e2e_ns(&self) -> Option<u64> {
        let mut slot_metrics = roc::roc_receiver_metrics::default();
        let mut conn = [roc::roc_connection_metrics::default(); 8];
        let mut conn_count: usize = conn.len();
        let rc = unsafe {
            roc::roc_receiver_query(
                self.raw,
                roc::ROC_SLOT_DEFAULT,
                &mut slot_metrics,
                conn.as_mut_ptr(),
                &mut conn_count,
            )
        };
        if rc != 0 {
            return None;
        }
        let n = conn_count.min(conn.len());
        if n == 0 {
            return None;
        }
        conn[..n].iter().map(|c| c.e2e_latency).max()
    }
}

/// Open a receiver bound to `host`'s audio port trio (source, +1 repair,
/// +2 RTCP control).
pub fn open(
    ctx: Arc<RocContext>,
    host: &str,
    port: u16,
    spec: &StreamSpec,
) -> Result<Receiver> {
    // Both sides register the same custom encoding for this (rate, channels)
    // tuple so packets round-trip without libroc having to guess a match.
    ctx.ensure_encoding(spec.rate, spec.channels);
    let cfg = roc::roc_receiver_config {
        frame_encoding: roc::roc_media_encoding {
            rate: spec.rate,
            format: roc::roc_format::ROC_FORMAT_PCM_FLOAT32,
            channels: roc::roc_channel_layout::ROC_CHANNEL_LAYOUT_MULTITRACK,
            tracks: spec.channels as u32,
        },
        // The playback device's clock drives this pipeline — the thread
        // blocks in `snd_pcm_writei`, not here. With INTERNAL, roc would
        // also sleep on its own CPU timer, and the two clocks would drift
        // apart with nothing between them to absorb the difference.
        clock_source: roc::roc_clock_source::ROC_CLOCK_SOURCE_EXTERNAL,
        latency_tuner_backend: roc::roc_latency_tuner_backend::ROC_LATENCY_TUNER_BACKEND_DEFAULT,
        // Explicitly GRADUAL + SPEEX rather than DEFAULT: with a low
        // target_latency, DEFAULT auto-selects RESPONSIVE, which in turn
        // pulls in the BUILTIN resampler. That combination has a known
        // crash in roc-toolkit 0.4.0 (`roc_panic()` in
        // builtin_resampler.cpp: "ind_begin_prev > frame_size_ch_"), which
        // takes down the whole engine process via SIGABRT — not something
        // we can catch from Rust since it's a C++ abort(). GRADUAL+SPEEX is
        // roc's own recommended pairing for "cheap CPU" / non-extreme
        // latency use, and avoids the buggy code path entirely. Costs a few
        // ms of extra clock-sync smoothing, which is a fine trade for "the
        // process doesn't crash."
        latency_tuner_profile: roc::roc_latency_tuner_profile::ROC_LATENCY_TUNER_PROFILE_GRADUAL,
        resampler_backend: roc::roc_resampler_backend::ROC_RESAMPLER_BACKEND_SPEEX,
        resampler_profile: roc::roc_resampler_profile::ROC_RESAMPLER_PROFILE_DEFAULT,
        target_latency: (spec.target_latency_ms as u64) * 1_000_000,
        latency_tolerance: 0,
        no_playback_timeout: 0,
        choppy_playback_timeout: 0,
    };

    let mut raw: *mut roc::roc_receiver = std::ptr::null_mut();
    let rc = unsafe { roc::roc_receiver_open(ctx.raw(), &cfg, &mut raw) };
    if rc != 0 || raw.is_null() {
        return Err(anyhow!("roc_receiver_open failed ({rc})"));
    }
    // Wrap immediately so the Drop impl covers every early return below.
    let receiver = Receiver { raw, _ctx: ctx };

    let bind = |uri: String, iface: roc::roc_interface, what: &str| -> Result<()> {
        let ep = endpoint_from_uri(&uri)?;
        let rc =
            unsafe { roc::roc_receiver_bind(receiver.raw, roc::ROC_SLOT_DEFAULT, iface, ep) };
        endpoint_free(ep);
        if rc != 0 {
            bail!("receiver bind {what} failed ({rc})");
        }
        Ok(())
    };

    let source_uri = if spec.fec {
        format!("rtp+rs8m://{host}:{port}")
    } else {
        format!("rtp://{host}:{port}")
    };
    bind(source_uri, roc::roc_interface::ROC_INTERFACE_AUDIO_SOURCE, "source")?;

    if spec.fec {
        bind(
            format!("rs8m://{host}:{}", port + 1),
            roc::roc_interface::ROC_INTERFACE_AUDIO_REPAIR,
            "repair",
        )?;
    }

    // RTCP control endpoint — see transport/sender.rs for why this needs to
    // exist at all (without it, e2e_latency is always zero). `port + 2` is
    // this route's third port; see `routing::route_port`.
    bind(
        format!("rtcp://{host}:{}", port + 2),
        roc::roc_interface::ROC_INTERFACE_AUDIO_CONTROL,
        "control",
    )?;

    Ok(receiver)
}
