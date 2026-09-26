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
    publish_level, COLD_RESUME_FADE_MS, DEVICE_WAIT_TIMEOUT_MS, LONG_ABSENCE_MS,
    MAX_CONSECUTIVE_ERRORS, RESUME_FADE_MS, SILENCE_BEFORE_FADE_MS, STALL_WARN_AFTER,
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
    /// The format the playback device was actually opened with, as
    /// `SampleFormat::as_u8`; `UNKNOWN_FORMAT` until the device is open.
    pub format: Arc<AtomicU8>,
    /// Monotonic count of samples clamped at the rails on their way to the
    /// device. See `StreamStats::clipped_samples` for why this earns a
    /// counter of its own.
    pub clipped: Arc<AtomicUsize>,
    /// True while the playback device has stopped taking periods — the same
    /// condition that logs "device stalled?". See the same field on
    /// `SendHandle`.
    pub stalled: Arc<AtomicBool>,
    /// Why this pipeline stopped, if it stopped on its own. See the same
    /// field on `SendHandle`.
    pub last_error: Arc<Mutex<Option<String>>>,
    /// Periods currently kept queued in the playback device: 2 to start
    /// with, raised by repeated xruns. See `pcm::DepthGovernor`. 0 until
    /// the device is open.
    pub depth: Arc<AtomicU32>,
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
    let format = Arc::new(AtomicU8::new(UNKNOWN_FORMAT));
    let clipped = Arc::new(AtomicUsize::new(0));
    let stalled = Arc::new(AtomicBool::new(false));
    let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let depth = Arc::new(AtomicU32::new(0));

    let worker = Worker {
        stop: stop.clone(),
        stalled: stalled.clone(),
        level_bits: level_bits.clone(),
        xruns: xruns.clone(),
        buffer_ns: buffer_ns.clone(),
        e2e_ns: e2e_ns.clone(),
        format: format.clone(),
        clipped: clipped.clone(),
        depth: depth.clone(),
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
        format,
        clipped,
        stalled,
        last_error,
        depth,
    })
}

/// The atomics the worker publishes into, grouped so the loop signature
/// doesn't grow a parameter per metric.
struct Worker {
    stop: Arc<AtomicBool>,
    stalled: Arc<AtomicBool>,
    level_bits: Arc<AtomicU32>,
    xruns: Arc<AtomicUsize>,
    buffer_ns: Arc<AtomicU64>,
    e2e_ns: Arc<AtomicU64>,
    format: Arc<AtomicU8>,
    clipped: Arc<AtomicUsize>,
    depth: Arc<AtomicU32>,
}

/// Count a recovered playback xrun and, if they have become a pattern,
/// queue one more period from now on. See `pcm::DepthGovernor` for when,
/// and `pcm::PLAYBACK_BUFFER_PERIODS` for why a larger period is not the
/// answer on the hardware where this happens.
fn note_xrun(
    pcm: &alsa::PCM,
    governor: &mut pcm::DepthGovernor,
    w: &Worker,
    alsa_name: &str,
    write_frames: usize,
    rate: u32,
) {
    w.xruns.fetch_add(1, Ordering::Relaxed);
    let Some(want) = governor.on_xrun(std::time::Instant::now()) else {
        tracing::warn!("playback {alsa_name} xrun recovered");
        return;
    };
    match pcm::set_playback_depth(pcm, write_frames, want) {
        Ok(applied) => {
            governor.settle(applied);
            w.depth.store(applied, Ordering::Relaxed);
            if applied == want {
                tracing::warn!(
                    "playback {alsa_name} xrun recovered; xruns are repeating, now keeping \
                     {applied} periods queued (+{} ms of latency per step)",
                    write_frames as u64 * 1000 / rate.max(1) as u64
                );
            } else {
                tracing::warn!(
                    "playback {alsa_name} xrun recovered; xruns are repeating but the device \
                     buffer is already as deep as it goes ({applied} periods)"
                );
            }
        }
        Err(err) => {
            tracing::warn!("playback {alsa_name} xrun recovered; deepening failed: {err:#}")
        }
    }
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

    let period_frames = spec.frames_per_period as usize;
    let mut governor = pcm::DepthGovernor::new();
    governor.settle(pcm::set_playback_depth(
        &pcm,
        period_frames,
        governor.depth(),
    )?);
    w.depth.store(governor.depth(), Ordering::Relaxed);

    let mut rx = receiver::open(ctx, bind_host, bind_port, spec)?;

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
    let mut fade = Fade::new(spec.rate);
    fade.arm(COLD_RESUME_FADE_MS);
    let frames_in = |ms: u32| spec.rate as u64 * ms as u64 / 1000;
    let silence_before_fade = frames_in(SILENCE_BEFORE_FADE_MS);
    let long_absence = frames_in(LONG_ABSENCE_MS);
    let mut silent_frames: u64 = 0;
    // Whether roc currently has a sender connected, sampled at the metrics
    // cadence below. `None` means the query failed and we do not know, which
    // falls back to the old exact-zeros proxy — the cautious direction.
    //
    // This is what tells "the sender went away" apart from "the sender is
    // right there, playing nothing". Both produce an unbroken run of exact
    // zeros, and for a long time this loop could only see the zeros.
    let mut sender_connected: Option<bool> = None;

    while !w.stop.load(Ordering::Relaxed) {
        // Set false by anything that went wrong this iteration. The error
        // budget is per *iteration*, not per call site: a healthy receiver
        // feeding a playback device that fails every single write must still
        // reach the limit and bail, and it wouldn't if a good read reset the
        // counter on the way past.
        let mut healthy = true;
        let mut read_ok = true;

        // Returns immediately (EXTERNAL clock): zero-filled if no sender has
        // connected yet, otherwise resampled out of the jitter buffer.
        if let Err(err) = rx.read(&mut floats) {
            healthy = false;
            read_ok = false;
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

        // What roc handed over, before any ramp of ours. A long run of exact
        // zeros followed by signal is the sender coming back, which is
        // precisely when the level is unknown and must not arrive as a step —
        // see `fade.rs` for the incident that is about.
        //
        // Samples past the rails are counted here, in the same pass, and
        // deliberately *before* the ramp — see the note on the meter below
        // for why the two measurements sit on opposite sides of it.
        let mut raw_peak = 0.0_f32;
        let mut over = 0usize;
        for &s in floats.iter() {
            let a = s.abs();
            if a > raw_peak {
                raw_peak = a;
            }
            if a > 1.0 {
                over += 1;
            }
        }
        if over > 0 {
            w.clipped.fetch_add(over, Ordering::Relaxed);
        }
        // Zeros only count as *absence* when nothing is actually sending.
        //
        // The exact-zeros test alone was a proxy for "roc has no session",
        // and it is wrong for any source that is digitally silent between
        // items — a PC playing back media, which is silent between every clip
        // by definition. There the proxy fires on every gap, and the cost is
        // not the harmless "fade-in over a passage that was already silent"
        // it was assumed to be: the ramp runs over the *returning* audio, so
        // the first seconds of the next clip come back inaudible. At an event
        // that is the failure, not the protection.
        //
        // roc knows the difference, so ask it. A connection in the slot means
        // a sender is there and the zeros are its silence; no connection
        // means it has genuinely gone, which is the case the ramp exists for
        // and is unchanged. When the query fails we cannot tell, so we keep
        // the old behaviour and stay cautious.
        //
        // A failed read counts as absence regardless: `floats` was filled
        // with zeros by the error path above, and those are ours, not the
        // sender's.
        let absent = zeros_mean_absence(read_ok, sender_connected);
        if raw_peak == 0.0 && absent {
            silent_frames = silent_frames.saturating_add(period_frames as u64);
        } else if raw_peak == 0.0 {
            // Connected and quiet. Not an absence, so nothing to ramp back
            // in from — but do not reset the counter either, or a sender
            // that drops out mid-silence would start counting from zero.
        } else {
            if silent_frames >= silence_before_fade {
                // How cautious to be depends on how long it was away. A short
                // gap is the same stream at the same level and only needs
                // declicking; past `LONG_ABSENCE_MS` something structural
                // happened at the other end and the level is no longer
                // something this engine knows.
                let cold = silent_frames >= long_absence;
                let ms = if cold {
                    COLD_RESUME_FADE_MS
                } else {
                    RESUME_FADE_MS
                };
                tracing::info!(
                    "recv pipeline {alsa_name}: audio resumed after {:.1}s of silence, \
                     ramping in over {ms}ms{}",
                    silent_frames as f64 / spec.rate as f64,
                    if cold {
                        " (level unknown after a long absence)"
                    } else {
                        ""
                    }
                );
                fade.arm(ms);
            }
            silent_frames = 0;
        }
        fade.apply(&mut floats, channels);

        // Rolling peak for the level meter, decayed so brief silence still
        // reads as quiet without the meter feeling twitchy. Measured after
        // the ramp, so the meter shows what actually leaves the machine.
        //
        // The clip counter is measured *before* it, a few lines up, and the
        // asymmetry is the point: the meter answers "what is coming out of
        // this machine", which the ramp is part of, while the clip counter
        // answers "what is the source doing", which the ramp is not.
        //
        // Counting after the ramp meant the counter read zero for the whole
        // two seconds of a cold resume, no matter how hard the incoming
        // audio was hitting the rails — the ramp had already scaled it under
        // 1.0. That is exactly backwards. A machine that has just rebooted
        // with its mixer at some unknown gain is the single most likely
        // moment for a gain accident, which is why the long ramp exists at
        // all, and the instrument for spotting one was closing its eyes for
        // precisely that window.
        let peak = floats.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
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
                Ok(true) => {
                    // Edge only — see the same arm in `send.rs`.
                    if stalled != 0 {
                        stalled = 0;
                        w.stalled.store(false, Ordering::Relaxed);
                    }
                }
                Ok(false) => {
                    // The device has not freed a period's worth of space.
                    // Not an xrun (nothing was starved — we have not handed
                    // it anything yet) and not counted against
                    // `consecutive_errors`, for the same reasons spelled out
                    // in `send.rs`.
                    stalled += 1;
                    if stalled == STALL_WARN_AFTER {
                        w.stalled.store(true, Ordering::Relaxed);
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
                        note_xrun(&pcm, &mut governor, w, alsa_name, period_frames, spec.rate);
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
                        note_xrun(&pcm, &mut governor, w, alsa_name, period_frames, spec.rate);
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
            match rx.query() {
                Some(metrics) => {
                    // Sampled here rather than every period: the thresholds
                    // this feeds are half a second and five seconds, so a
                    // ~200ms granularity on "is anybody sending" is far finer
                    // than it needs to be, and an FFI call per period on a
                    // SCHED_FIFO thread is not something to spend for nothing.
                    sender_connected = Some(metrics.connections > 0);
                    // Only overwrite the sentinel once there's an actual
                    // connection to report on — see `Receiver::query`.
                    if let Some(ns) = metrics.e2e_ns {
                        w.e2e_ns.store(ns, Ordering::Relaxed);
                    }
                }
                // The query itself failed. Don't claim to know.
                None => sender_connected = None,
            }
        }
    }
    Ok(())
}

/// Whether a period of exact zeros means the sender is *gone*, as opposed to
/// present and playing nothing.
///
/// Both look identical in the audio, and telling them apart is what decides
/// whether returning audio gets ramped in. Getting it wrong is costly in
/// both directions: treat a real absence as silence and a machine that
/// rebooted streams back at full scale with no warning (the incident
/// `fade.rs` exists for); treat silence as an absence and every gap between
/// clips on a PC source eats the first seconds of the next one.
///
/// `connected` is `None` when roc's query failed and we genuinely do not
/// know. That resolves to "absent", which is the cautious direction: a
/// needless ramp is survivable, an unannounced full-scale return is not.
///
/// A failed read is always an absence regardless of what roc says, because
/// the zeros in the buffer are then ours — written by the error path to
/// avoid replaying a stale period — and not the sender's.
fn zeros_mean_absence(read_ok: bool, connected: Option<bool>) -> bool {
    !read_ok || !connected.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::zeros_mean_absence;

    /// The case this function was added for. A PC source is digitally silent
    /// between clips, so the old "exact zeros mean no session" proxy fired on
    /// every gap — and the ramp it armed ran over the *returning* audio, not
    /// over the silence, so the first seconds of the next clip came back
    /// inaudible. At an event that is the failure, not the protection.
    #[test]
    fn a_connected_sender_playing_nothing_is_not_an_absence() {
        assert!(!zeros_mean_absence(true, Some(true)));
    }

    /// And the case the ramp exists for is untouched: nothing connected means
    /// the sender really has gone, and whatever comes back arrives at a level
    /// this engine has no knowledge of.
    #[test]
    fn nothing_connected_is_an_absence() {
        assert!(zeros_mean_absence(true, Some(false)));
    }

    /// Not knowing resolves to the cautious answer. A needless ramp is
    /// survivable; an unannounced full-scale return is the thing that put a
    /// bang through somebody's speakers.
    #[test]
    fn an_unanswerable_query_is_treated_as_an_absence() {
        assert!(zeros_mean_absence(true, None));
    }

    /// A failed read fills the buffer with zeros of our own making. Counting
    /// those as the sender's silence would let a receiver that is failing
    /// every read look like a healthy quiet one.
    #[test]
    fn our_own_zeros_after_a_failed_read_are_an_absence() {
        assert!(zeros_mean_absence(false, Some(true)));
        assert!(zeros_mean_absence(false, None));
    }
}
