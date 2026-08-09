//! Roc receiver wrapper: binds local UDP endpoints and pumps decoded samples
//! into a producer ring that the playback side drains.

use anyhow::{anyhow, Result};
use roc_sys as roc;
use rtrb::Producer;
use soundnet_protocol::StreamSpec;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use super::{endpoint_free, endpoint_from_uri, RocContext};

pub struct ReceiverHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: JoinHandle<()>,
    /// Last observed end-to-end latency in nanoseconds, refreshed every
    /// ~200ms by the receiver thread via `roc_receiver_query`. Requires an
    /// RTCP control endpoint (see `run` below) — without it libroc always
    /// reports zero. Starts (and stays) at `u64::MAX` — "not measured yet"
    /// — until the first query call sees an active connection; routing.rs's
    /// `ns_to_ms` is what turns that sentinel back into `None` for the UI,
    /// since a real zero is a value this metric can legitimately take and
    /// so can't double as "no data".
    pub e2e_latency_ns: Arc<AtomicU64>,
    /// Rolling standard deviation of `niq_latency` in nanoseconds — a proxy
    /// for network jitter. Small = smooth stream, big = jittery.
    pub jitter_ns: Arc<AtomicU64>,
}

impl ReceiverHandle {
    pub fn stop_and_join(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

pub fn spawn(
    ctx: Arc<RocContext>,
    bind_host: &str,
    bind_port: u16,
    spec: &StreamSpec,
    mut producer: Producer<f32>,
) -> Result<ReceiverHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let e2e = Arc::new(AtomicU64::new(u64::MAX));
    let jitter = Arc::new(AtomicU64::new(0));
    let e2e_worker = e2e.clone();
    let jitter_worker = jitter.clone();
    let bind_host = bind_host.to_string();
    let spec = spec.clone();

    let thread = thread::Builder::new()
        .name(format!("roc-rx-{bind_host}-{bind_port}"))
        .spawn(move || {
            crate::rt::raise_thread_priority("roc receiver", crate::rt::PRIO_RECEIVER);
            if let Err(err) = run(&ctx, &bind_host, bind_port, &spec, &mut producer, &stop_worker, &e2e_worker, &jitter_worker) {
                tracing::error!("receiver on {bind_host}:{bind_port} failed: {err:#}");
            }
        })?;
    Ok(ReceiverHandle { stop, thread, e2e_latency_ns: e2e, jitter_ns: jitter })
}

fn run(
    ctx: &Arc<RocContext>,
    host: &str,
    port: u16,
    spec: &StreamSpec,
    producer: &mut Producer<f32>,
    stop: &Arc<AtomicBool>,
    e2e_out: &Arc<AtomicU64>,
    jitter_out: &Arc<AtomicU64>,
) -> Result<()> {
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
        clock_source: roc::roc_clock_source::ROC_CLOCK_SOURCE_INTERNAL,
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

    let mut receiver: *mut roc::roc_receiver = std::ptr::null_mut();
    let rc = unsafe { roc::roc_receiver_open(ctx.raw(), &cfg, &mut receiver) };
    if rc != 0 || receiver.is_null() {
        return Err(anyhow!("roc_receiver_open failed ({rc})"));
    }
    struct DropReceiver(*mut roc::roc_receiver);
    impl Drop for DropReceiver {
        fn drop(&mut self) {
            unsafe { let _ = roc::roc_receiver_close(self.0); }
        }
    }
    let _drop_receiver = DropReceiver(receiver);

    let source_uri = if spec.fec {
        format!("rtp+rs8m://{host}:{port}")
    } else {
        format!("rtp://{host}:{port}")
    };
    let source_ep = endpoint_from_uri(&source_uri)?;
    let rc = unsafe {
        roc::roc_receiver_bind(
            receiver,
            roc::ROC_SLOT_DEFAULT,
            roc::roc_interface::ROC_INTERFACE_AUDIO_SOURCE,
            source_ep,
        )
    };
    endpoint_free(source_ep);
    if rc != 0 {
        return Err(anyhow!("receiver bind source failed ({rc})"));
    }

    if spec.fec {
        let repair_uri = format!("rs8m://{host}:{}", port + 1);
        let repair_ep = endpoint_from_uri(&repair_uri)?;
        let rc = unsafe {
            roc::roc_receiver_bind(
                receiver,
                roc::ROC_SLOT_DEFAULT,
                roc::roc_interface::ROC_INTERFACE_AUDIO_REPAIR,
                repair_ep,
            )
        };
        endpoint_free(repair_ep);
        if rc != 0 {
            return Err(anyhow!("receiver bind repair failed ({rc})"));
        }
    }

    // RTCP control endpoint — see transport/sender.rs for why this needs to
    // exist at all (without it, e2e_latency below is always zero). `port +
    // 2` is this route's third port; see `routing::route_port`.
    let control_uri = format!("rtcp://{host}:{}", port + 2);
    let control_ep = endpoint_from_uri(&control_uri)?;
    let rc = unsafe {
        roc::roc_receiver_bind(
            receiver,
            roc::ROC_SLOT_DEFAULT,
            roc::roc_interface::ROC_INTERFACE_AUDIO_CONTROL,
            control_ep,
        )
    };
    endpoint_free(control_ep);
    if rc != 0 {
        return Err(anyhow!("receiver bind control failed ({rc})"));
    }

    let period_frames = spec.frames_per_period as usize;
    let period_samples = period_frames * spec.channels as usize;
    let mut buf: Vec<f32> = vec![0.0; period_samples];

    // How many periods between metric snapshots (~200ms cadence).
    let metrics_every = (spec.rate as usize / 5 / period_frames.max(1)).max(1);
    let mut ticks = 0_usize;
    let _ = jitter_out; // libroc 0.4 dropped niq_latency, so jitter stays 0.

    while !stop.load(Ordering::Relaxed) {
        let mut frame = roc::roc_frame {
            samples: buf.as_mut_ptr() as *mut _,
            samples_size: buf.len() * std::mem::size_of::<f32>(),
        };
        let rc = unsafe { roc::roc_receiver_read(receiver, &mut frame) };
        if rc != 0 {
            tracing::warn!("roc_receiver_read returned {rc}");
            continue;
        }
        for &s in &buf {
            // Drop if playback fell behind — never block roc.
            let _ = producer.push(s);
        }

        ticks += 1;
        if ticks >= metrics_every {
            ticks = 0;
            let mut slot_metrics = roc::roc_receiver_metrics::default();
            let mut conn = [roc::roc_connection_metrics::default(); 8];
            let mut conn_count: usize = conn.len();
            let rc = unsafe {
                roc::roc_receiver_query(
                    receiver,
                    roc::ROC_SLOT_DEFAULT,
                    &mut slot_metrics,
                    conn.as_mut_ptr(),
                    &mut conn_count,
                )
            };
            if rc == 0 {
                let n = conn_count.min(conn.len());
                // Only overwrite the sentinel once there's an actual
                // connection to report on — an empty conn array (no sender
                // has connected yet, or it dropped) would otherwise store a
                // literal 0 that reads identically to "real measured zero
                // latency" downstream instead of "no data".
                if n > 0 {
                    // Report the largest e2e_latency across connections.
                    let worst_e2e = conn[..n].iter().map(|c| c.e2e_latency).max().unwrap_or(0);
                    e2e_out.store(worst_e2e, Ordering::Relaxed);
                }
            }
        }
    }
    Ok(())
}
