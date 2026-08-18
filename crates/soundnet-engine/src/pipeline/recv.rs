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
//! As in `send.rs`, blocking is what makes `SCHED_FIFO` safe here. Keep
//! exactly one blocking point in the loop.
//!
//! ## Where the blocking happens
//!
//! Same move as `send.rs`, for the same reason: the loop no longer blocks in
//! `snd_pcm_writei` but in `snd_pcm_wait` with `DEVICE_WAIT_TIMEOUT_MS` just
//! before it, so a stop request is answered within the timeout no matter what
//! the device is doing. A wait that returns ready means at least `avail_min`
//! frames of space — one period by default — so the write it guards is taken
//! by the buffer rather than waiting for room, and a timeout goes back around
//! to re-check the stop flag having spent its time inside `poll` rather than
//! spinning a real-time core.
//!
//! One asymmetry with the capture side: the wait sits *inside* the
//! short-write retry loop here. `writei` can come back having taken only part
//! of a period when a signal lands mid-syscall, and the remainder has to be
//! bounded too, or a wedged device could still hold the thread through the
//! back half of a period forever.
//!
//! The other asymmetry is what `send.rs` needs and this file does not:
//! nothing here corresponds to `pcm::ensure_capture_running`, because a
//! prepared playback stream has an empty buffer — the wait returns
//! immediately and the write starts the stream itself.

use anyhow::{bail, Result};
use soundnet_protocol::{StreamSpec, UNKNOWN_FORMAT};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::audio::format::f32_to_alsa;
use crate::audio::{pcm, window};
use crate::pipeline::fade::Fade;
use crate::pipeline::{
    publish_level, DEVICE_WAIT_TIMEOUT_MS, MAX_CONSECUTIVE_ERRORS, RESUME_FADE_MS,
    SILENCE_BEFORE_FADE_MS, STALL_WARN_AFTER,
};
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
    /// Why this pipeline stopped, if it stopped on its own. See the same
    /// field on `SendHandle`.
    pub last_error: Arc<Mutex<Option<String>>>,
}

impl RecvHandle {
    /// Ask the pipeline to stop, without waiting for it. See the same method
    /// on `SendHandle`.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Stop the pipeline and wait for its thread to be gone. Blocks for
    /// however long the thread takes to return from `snd_pcm_writei`, so
    /// `routing` only calls it from a blocking-pool thread.
    pub fn stop_and_join(self) {
        self.request_stop();
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
    channel_offset: u8,
) -> Result<RecvHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let level_bits = Arc::new(AtomicU32::new(0));
    let xruns = Arc::new(AtomicUsize::new(0));
    let buffer_ns = Arc::new(AtomicU64::new(u64::MAX));
    let e2e_ns = Arc::new(AtomicU64::new(u64::MAX));
    let jitter_ns = Arc::new(AtomicU64::new(0));
    let format = Arc::new(AtomicU8::new(UNKNOWN_FORMAT));
    let clipped = Arc::new(AtomicUsize::new(0));
    let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let worker = Worker {
        stop: stop.clone(),
        level_bits: level_bits.clone(),
        xruns: xruns.clone(),
        buffer_ns: buffer_ns.clone(),
        e2e_ns: e2e_ns.clone(),
        format: format.clone(),
        clipped: clipped.clone(),
    };
    let error_worker = last_error.clone();
    let alsa_name = alsa_name.to_string();
    let bind_host = bind_host.to_string();
    let spec = spec.clone();

    let thread = thread::Builder::new()
        .name(format!("recv-{alsa_name}"))
        .spawn(move || {
            crate::rt::raise_thread_priority("recv pipeline", crate::rt::PRIO_RECV);
            if let Err(err) = run(
                &alsa_name,
                &spec,
                ctx,
                &bind_host,
                bind_port,
                &worker,
                channel_offset as usize,
            ) {
                tracing::error!(
                    "recv pipeline {bind_host}:{bind_port} -> {alsa_name} failed: {err:#}"
                );
                // Poisoning ignored on purpose — see the doc on
                // `SendHandle::last_error`.
                *error_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("{err:#}"));
            }
        })?;

    Ok(RecvHandle {
        stop,
        thread,
        level_bits,
        xruns,
        buffer_ns,
        e2e_ns,
        jitter_ns,
        format,
        clipped,
        last_error,
    })
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
    channel_offset: usize,
) -> Result<()> {
    // As in send.rs: open exactly wide enough to reach the window's far edge.
    let channels = spec.channels as usize;
    let device_channels = channel_offset + channels;
    let (pcm, format) = pcm::open(
        alsa_name,
        alsa::Direction::Playback,
        spec,
        device_channels as u32,
    )?;
    w.format.store(format.as_u8(), Ordering::Relaxed);
    let io = pcm.io_bytes();

    let mut rx = receiver::open(ctx, bind_host, bind_port, spec)?;

    let period_frames = spec.frames_per_period as usize;
    let period_samples = period_frames * channels;
    // What roc hands over (the window), and what the device is written
    // (full width, with every channel this route doesn't drive left silent).
    let mut floats: Vec<f32> = vec![0.0; period_samples];
    let mut device_floats: Vec<f32> = Vec::with_capacity(period_frames * device_channels);
    let frame_bytes = device_channels * format.bytes_per_sample();
    let mut raw: Vec<u8> = Vec::with_capacity(period_frames * frame_bytes);

    let metrics_every = pcm::metrics_every(spec.rate, period_frames);
    let mut ticks = 0_usize;
    let mut consecutive_errors = 0_u32;
    let mut stalled = 0_u32;

    // Ramp the output up whenever audio arrives after the stream has been
    // away — see `fade.rs` for the incident this is here for. Armed from the
    // start, because a route that has only just opened is the same situation
    // as one whose sender vanished and came back: about to play material this
    // engine has never seen at a level nobody has checked.
    let mut fade = Fade::new(spec.rate, RESUME_FADE_MS);
    fade.arm();
    let silence_before_fade = spec.rate as u64 * SILENCE_BEFORE_FADE_MS as u64 / 1000;
    let mut silent_frames: u64 = 0;

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
            // Silence, not the previous period. Falling through to the write
            // is deliberate — skipping it would skip this loop's only
            // blocking call, and a real-time thread spinning on a failing
            // receiver would pin a core — but *what* gets written matters.
            // `floats` still holds the last period roc produced, and replaying
            // it means a receiver failing every read emits that fragment over
            // and over at period rate. If the last thing through was loud,
            // so is the loop. Writing a period of silence keeps the pacing
            // and cannot make noise.
            floats.fill(0.0);
        }

        // What roc handed over, before any ramp of ours. Exact zeros mean it
        // has no session — see `SILENCE_BEFORE_FADE_MS` for why that is a
        // sound enough proxy — so a long run of them followed by signal is
        // the sender coming back, which is precisely when the level is
        // unknown and must not arrive as a step.
        let raw_peak = floats.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
        if raw_peak == 0.0 {
            silent_frames = silent_frames.saturating_add(period_frames as u64);
        } else {
            if silent_frames >= silence_before_fade {
                tracing::info!(
                    "recv pipeline {alsa_name}: audio resumed after {:.1}s of silence, \
                     ramping in over {RESUME_FADE_MS}ms",
                    silent_frames as f64 / spec.rate as f64
                );
                fade.arm();
            }
            silent_frames = 0;
        }
        fade.apply(&mut floats, channels);

        // Rolling peak for the level meter, decayed so brief silence still
        // reads as quiet without the meter feeling twitchy. Measured after
        // the ramp, so the meter shows what actually leaves the machine. The
        // same pass counts samples past the rails, since `f32_to_alsa` clamps
        // them a few lines below and the information would be gone by then.
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
        publish_level(&w.level_bits, peak);

        window::scatter(
            &floats,
            channels,
            device_channels,
            channel_offset,
            &mut device_floats,
        );
        f32_to_alsa(format, &device_floats, &mut raw);

        // The one blocking point in this loop. The device is opened
        // blocking, so a short write means the syscall was interrupted
        // rather than the buffer being full — push the remainder instead of
        // dropping it, which would be an audible click every time.
        //
        // The wait is inside the retry loop rather than above it so the
        // remainder after a short write is bounded too: a signal landing
        // mid-`writei` must not put this thread back into an open-ended
        // wait for a device that may never take the rest.
        let mut written = 0usize;
        while written < period_frames {
            match pcm.wait(Some(DEVICE_WAIT_TIMEOUT_MS)) {
                Ok(true) => stalled = 0,
                Ok(false) => {
                    // The device has not freed a period's worth of space.
                    // Not an xrun (nothing was starved — we have not handed
                    // it anything yet) and not counted against
                    // `consecutive_errors`, for the same reasons spelled out
                    // in `send.rs`.
                    stalled += 1;
                    if stalled == STALL_WARN_AFTER {
                        tracing::warn!(
                            "playback {alsa_name}: device has taken nothing for {}ms — stalled? \
                             The route is still stoppable; nothing is being counted as an xrun.",
                            STALL_WARN_AFTER * DEVICE_WAIT_TIMEOUT_MS
                        );
                    }
                    // The whole point of the timeout: without this the outer
                    // loop's stop check is unreachable while the device is
                    // wedged.
                    if w.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    continue;
                }
                Err(err) => {
                    if pcm.try_recover(err, false).is_ok() {
                        w.xruns.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("playback {alsa_name} xrun recovered");
                        healthy = false;
                        break;
                    }
                    bail!("playback {alsa_name} wait: {err}");
                }
            }

            // Ready: at least `avail_min` frames of space, which defaults to
            // one period, so this write is taken by the buffer rather than
            // waiting for room. Playback needs no equivalent of
            // `ensure_capture_running` — a prepared playback stream has an
            // empty buffer, so the wait returns immediately and the write
            // starts the stream itself via `start_threshold`.
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
