//! Receive pipeline: one thread that owns both the roc receiver and the
//! playback device.
//!
//! The mirror image of `send.rs`, and it exists for the same reason: the ring
//! buffer that used to sit between the roc reader and the ALSA writer was
//! three periods of latency that appeared in no metric, and it was papering
//! over two clocks that don't agree.
//!
//! Now `roc_receiver_read` is `ROC_CLOCK_SOURCE_EXTERNAL` — it returns a
//! frame immediately rather than sleeping — and the thread blocks in
//! `snd_pcm_writei` instead. The playback device's clock paces everything,
//! and roc's jitter buffer (sized by the route's `target_latency_ms`) is the
//! single place where network timing slack is absorbed. That is what it is
//! for; our ring was a worse copy of it.
//!
//! As in `send.rs`, the blocking write is what makes `SCHED_FIFO` safe here.
//! Keep exactly one blocking point in the loop.

use anyhow::{bail, Result};
use soundnet_protocol::{StreamSpec, UNKNOWN_FORMAT};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::audio::format::f32_to_alsa;
use crate::audio::pcm;
use crate::pipeline::MAX_CONSECUTIVE_ERRORS;
use crate::transport::{receiver, RocContext};

pub struct RecvHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: JoinHandle<()>,
    /// Bits of an f32 holding the rolling peak level over the last period.
    /// Read via `f32::from_bits(atomic.load(...))`.
    pub level_bits: Arc<AtomicU32>,
    /// Monotonic counter of xruns since the pipeline started.
    pub xruns: Arc<AtomicUsize>,
    /// Rolling ALSA playback-buffer delay in nanoseconds — frames sit here
    /// *after* roc hands them over and before they reach the speaker, which
    /// is why roc's own e2e figure doesn't cover it. `u64::MAX` means "not
    /// measured yet".
    pub buffer_ns: Arc<AtomicU64>,
    /// Last observed roc end-to-end latency in nanoseconds. Requires the
    /// RTCP control endpoint; `u64::MAX` until a sender actually connects.
    pub e2e_ns: Arc<AtomicU64>,
    /// Reserved for a jitter estimate. libroc 0.4 dropped the `niq_latency`
    /// field the old estimate was derived from, so this stays at 0 rather
    /// than reporting a number nothing computes.
    pub jitter_ns: Arc<AtomicU64>,
    /// The format the playback device was actually opened with, as
    /// `SampleFormat::as_u8`; `UNKNOWN_FORMAT` until the device is open.
    pub format: Arc<AtomicU8>,
    /// Monotonic count of samples clamped at the rails on their way to the
    /// device. See `StreamStats::clipped_samples` for why this earns a
    /// counter of its own.
    pub clipped: Arc<AtomicUsize>,
}

impl RecvHandle {
    pub fn stop_and_join(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

/// Spawn the receive side of a route: bind `bind_host:bind_port` (plus the
/// +1 repair and +2 control ports) and play into `alsa_name`.
pub fn spawn(
    alsa_name: &str,
    spec: &StreamSpec,
    ctx: Arc<RocContext>,
    bind_host: &str,
    bind_port: u16,
) -> Result<RecvHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let level_bits = Arc::new(AtomicU32::new(0));
    let xruns = Arc::new(AtomicUsize::new(0));
    let buffer_ns = Arc::new(AtomicU64::new(u64::MAX));
    let e2e_ns = Arc::new(AtomicU64::new(u64::MAX));
    let jitter_ns = Arc::new(AtomicU64::new(0));
    let format = Arc::new(AtomicU8::new(UNKNOWN_FORMAT));
    let clipped = Arc::new(AtomicUsize::new(0));

    let worker = Worker {
        stop: stop.clone(),
        level_bits: level_bits.clone(),
        xruns: xruns.clone(),
        buffer_ns: buffer_ns.clone(),
        e2e_ns: e2e_ns.clone(),
        format: format.clone(),
        clipped: clipped.clone(),
    };
    let alsa_name = alsa_name.to_string();
    let bind_host = bind_host.to_string();
    let spec = spec.clone();

    let thread = thread::Builder::new()
        .name(format!("recv-{alsa_name}"))
        .spawn(move || {
            crate::rt::raise_thread_priority("recv pipeline", crate::rt::PRIO_RECV);
            if let Err(err) = run(&alsa_name, &spec, ctx, &bind_host, bind_port, &worker) {
                tracing::error!("recv pipeline {bind_host}:{bind_port} -> {alsa_name} failed: {err:#}");
            }
        })?;

    Ok(RecvHandle { stop, thread, level_bits, xruns, buffer_ns, e2e_ns, jitter_ns, format, clipped })
}

/// The atomics the worker publishes into, grouped so the loop signature
/// doesn't grow a parameter per metric.
struct Worker {
    stop: Arc<AtomicBool>,
    level_bits: Arc<AtomicU32>,
    xruns: Arc<AtomicUsize>,
    buffer_ns: Arc<AtomicU64>,
    e2e_ns: Arc<AtomicU64>,
    format: Arc<AtomicU8>,
    clipped: Arc<AtomicUsize>,
}

fn run(
    alsa_name: &str,
    spec: &StreamSpec,
    ctx: Arc<RocContext>,
    bind_host: &str,
    bind_port: u16,
    w: &Worker,
) -> Result<()> {
    let (pcm, format) = pcm::open(alsa_name, alsa::Direction::Playback, spec)?;
    w.format.store(format.as_u8(), Ordering::Relaxed);
    let io = pcm.io_bytes();

    let mut rx = receiver::open(ctx, bind_host, bind_port, spec)?;

    let period_frames = spec.frames_per_period as usize;
    let period_samples = period_frames * spec.channels as usize;
    let mut floats: Vec<f32> = vec![0.0; period_samples];
    let frame_bytes = spec.channels as usize * format.bytes_per_sample();
    let mut raw: Vec<u8> = Vec::with_capacity(period_frames * frame_bytes);

    let metrics_every = pcm::metrics_every(spec.rate, period_frames);
    let mut ticks = 0_usize;
    let mut consecutive_errors = 0_u32;

    while !w.stop.load(Ordering::Relaxed) {
        // Set false by anything that went wrong this iteration. The error
        // budget is per *iteration*, not per call site: a healthy receiver
        // feeding a playback device that fails every single write must still
        // reach the limit and bail, and it wouldn't if a good read reset the
        // counter on the way past.
        let mut healthy = true;

        // Returns immediately (EXTERNAL clock): zero-filled if no sender has
        // connected yet, otherwise resampled out of the jitter buffer.
        if let Err(err) = rx.read(&mut floats) {
            healthy = false;
            tracing::warn!("recv pipeline {alsa_name}: {err:#}");
            // Deliberately fall through to the write below rather than
            // `continue`: `floats` still holds the previous period, and
            // skipping the write would skip this loop's only blocking call.
            // A real-time thread spinning on a failing receiver would pin a
            // core; playing one stale period costs nothing by comparison.
        }

        // Rolling peak for the level meter, decayed so brief silence still
        // reads as quiet without the meter feeling twitchy. The same pass
        // counts samples past the rails, since `f32_to_alsa` clamps them a
        // few lines below and the information would be gone by then.
        let mut peak = 0.0_f32;
        let mut over = 0usize;
        for &s in floats.iter() {
            let a = s.abs();
            if a > peak {
                peak = a;
            }
            if a > 1.0 {
                over += 1;
            }
        }
        if over > 0 {
            w.clipped.fetch_add(over, Ordering::Relaxed);
        }
        let prev = f32::from_bits(w.level_bits.load(Ordering::Relaxed));
        let smoothed = if peak > prev { peak } else { prev * 0.7 + peak * 0.3 };
        w.level_bits.store(smoothed.to_bits(), Ordering::Relaxed);

        f32_to_alsa(format, &floats, &mut raw);

        // The one blocking point in this loop. The device is opened
        // blocking, so a short write means the syscall was interrupted
        // rather than the buffer being full — push the remainder instead of
        // dropping it, which would be an audible click every time.
        let mut written = 0usize;
        while written < period_frames {
            match io.writei(&raw[written * frame_bytes..]) {
                Ok(frames) if frames > 0 => written += frames,
                // Zero frames written with no error shouldn't happen on a
                // blocking device. Bail out of the inner loop rather than
                // retrying: with roc's read non-blocking, a device stuck
                // returning zero would otherwise be an unbounded spin at
                // real-time priority.
                Ok(_) => {
                    healthy = false;
                    break;
                }
                Err(err) => {
                    if pcm.try_recover(err, false).is_ok() {
                        w.xruns.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("playback {alsa_name} xrun recovered");
                        healthy = false;
                        break;
                    }
                    bail!("playback {alsa_name} write: {err}");
                }
            }
        }

        if !healthy {
            consecutive_errors += 1;
            if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                bail!(
                    "recv pipeline {alsa_name}: {consecutive_errors} consecutive failed periods, giving up"
                );
            }
            // The device buffer was reset by the recovery, so this period is
            // gone either way. Go around rather than sampling metrics off a
            // stream that's mid-restart.
            continue;
        }
        consecutive_errors = 0;

        ticks += 1;
        if ticks >= metrics_every {
            ticks = 0;
            if let Some(ns) = pcm::delay_ns(&pcm, spec.rate) {
                w.buffer_ns.store(ns, Ordering::Relaxed);
            }
            // Only overwrite the sentinel once there's an actual connection
            // to report on — see `Receiver::query_e2e_ns`.
            if let Some(ns) = rx.query_e2e_ns() {
                w.e2e_ns.store(ns, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}
