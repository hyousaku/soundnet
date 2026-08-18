//! Send pipeline: one thread that owns both the capture device and the roc
//! sender.
//!
//! This used to be two threads with a lock-free ring between them. That ring
//! was pure latency — three periods of it — and worse, it hid a real problem:
//! the ALSA side was paced by the sound card while the roc side paced itself
//! off a CPU timer (`ROC_CLOCK_SOURCE_INTERNAL`). Two clocks that don't agree
//! means the ring drifts towards full or empty, and both ends "handled" that
//! by silently dropping samples or substituting silence.
//!
//! Now the device's clock is the only clock: the thread blocks in
//! `snd_pcm_readi` for exactly one period and hands those samples straight to
//! roc, which is configured `ROC_CLOCK_SOURCE_EXTERNAL` and returns without
//! sleeping. Nothing is buffered in between, so there is nothing to drift.
//!
//! Blocking is also what makes it safe to run this thread at `SCHED_FIFO`
//! (see `rt.rs`): a real-time thread that never blocks would pin a core at
//! 100% and starve everything below it. Every loop below must keep exactly
//! one blocking point — that is the invariant the error handling here is
//! written to preserve.
//!
//! ## Where the blocking happens, and why it moved
//!
//! The loop used to block in `snd_pcm_readi` itself. That satisfied the
//! invariant but gave the thread no stop latency at all: `readi` returns when
//! the device says so, so how long a route took to stop was a hardware
//! question, and a device that stopped answering entirely (USB pulled
//! mid-stream, driver stuck in D state) kept its thread for the life of the
//! process. Nothing above could fix that; the caller can only ask.
//!
//! So the blocking point is now `snd_pcm_wait` with
//! `DEVICE_WAIT_TIMEOUT_MS`, immediately before the read. The properties that
//! matter:
//!
//! * **Still exactly one blocking call per iteration.** A wait that returns
//!   ready means at least `avail_min` frames are queued, and `avail_min`
//!   defaults to one period — so the `readi` after it is served from what is
//!   already there instead of waiting for it.
//! * **Still not a spin.** A timeout sends the loop back around to re-check
//!   the stop flag and wait again; the time is spent inside `poll`, not
//!   burning a real-time core. This is the one property that would make the
//!   whole change unsafe if it were got wrong.
//! * **Stop latency is now bounded** by the timeout, whatever the device
//!   does.
//!
//! The subtle cost is that `snd_pcm_wait` only observes the device while
//! `snd_pcm_readi` also *starts* one — see `pcm::ensure_capture_running` for
//! what that quietly used to do for us after every xrun recovery.

use anyhow::{bail, Result};
use soundnet_protocol::{StreamSpec, UNKNOWN_FORMAT};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::audio::format::alsa_to_f32;
use crate::audio::{pcm, window};
use crate::pipeline::fade::Fade;
use crate::pipeline::{
    publish_level, DEVICE_WAIT_TIMEOUT_MS, MAX_CONSECUTIVE_ERRORS, RESUME_FADE_MS, STALL_WARN_AFTER,
};
use crate::tone;
use crate::transport::{sender, RocContext};

pub struct SendHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: JoinHandle<()>,
    /// Rolling ALSA capture-buffer delay in nanoseconds — how much audio is
    /// currently queued in the device, sampled every ~200ms via
    /// `PCM::delay()`. This is the piece of latency roc's own e2e metric
    /// doesn't cover: frames sit here *before* `roc_sender_write` ever sees
    /// them. Stays at `u64::MAX` ("not measured") for a tone source — there
    /// is no ALSA buffer to report — and until the first sample for a real
    /// device.
    pub buffer_ns: Arc<AtomicU64>,
    /// The format the capture device was actually opened with, as
    /// `SampleFormat::as_u8`. Stays `UNKNOWN_FORMAT` until the device is
    /// open, and for the whole life of a tone source — there is no device to
    /// negotiate with, so there is no format to report.
    pub format: Arc<AtomicU8>,
    /// Bits of an f32 holding the rolling peak of what this pipeline put on
    /// the wire. Read via `f32::from_bits(atomic.load(...))`.
    ///
    /// Measured after the channel window is extracted, so it reflects the
    /// channels this route actually carries rather than everything the device
    /// handed over — a 16-channel interface streaming channels 5-6 should not
    /// show a meter driven by channel 1.
    pub level_bits: Arc<AtomicU32>,
    /// Monotonic count of recovered capture xruns. An overrun means the
    /// device had a period ready before we came back for it, so those
    /// samples are simply gone — an audible click, and for a long time one
    /// that no counter anywhere recorded.
    pub xruns: Arc<AtomicUsize>,
    /// Why this pipeline stopped, if it stopped on its own. Written once, as
    /// the thread unwinds. Without it the UI can only say that a worker
    /// exited — which names the symptom and withholds every fact that would
    /// let an operator act on it.
    ///
    /// Every access to this mutex ignores poisoning (`unwrap_or_else(|e|
    /// e.into_inner())`), reader and writer alike. Poisoning means some
    /// thread panicked while holding the lock, and the usual reason to
    /// respect that — the data behind it may be half-updated — cannot apply
    /// here: the guard is only ever held across a single move of a `String`
    /// that is either stored whole or not at all. There is no torn state to
    /// protect anyone from.
    ///
    /// Panicking on it would be actively harmful. This field exists to
    /// explain a failure, and it is written from the error path of a thread
    /// that is already on its way out; `unwrap()` there would replace a
    /// precise message like "device busy" with a second panic about a mutex,
    /// which is the one moment an operator can least afford to lose the
    /// first one.
    pub last_error: Arc<Mutex<Option<String>>>,
}

impl SendHandle {
    /// Ask the pipeline to stop, without waiting for it.
    ///
    /// Split out from the join so a caller tearing down several routes at
    /// once can raise every flag first and then wait once — see
    /// `routing::shutdown_all`. On its own this returns immediately; the
    /// thread doesn't notice until it next comes around the top of its loop.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Stop the pipeline and wait for its thread to be gone.
    ///
    /// This blocks for however long the thread takes to return from whatever
    /// ALSA call it is currently inside, which is why `routing` only ever
    /// calls it from a blocking-pool thread.
    pub fn stop_and_join(self) {
        self.request_stop();
        let _ = self.thread.join();
    }
}

/// Spawn the send side of a route: read from `alsa_name` (or synthesize a
/// tone, for `tone:` names) and stream to `dst_host`'s audio port trio.
/// `outgoing` pins the NIC packets leave from; `None` leaves it to the OS.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    alsa_name: &str,
    spec: &StreamSpec,
    ctx: Arc<RocContext>,
    dst_host: &str,
    dst_port: u16,
    outgoing: Option<IpAddr>,
    channel_offset: u8,
) -> Result<SendHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let level_bits = Arc::new(AtomicU32::new(0));
    let buffer_ns = Arc::new(AtomicU64::new(u64::MAX));
    let format = Arc::new(AtomicU8::new(UNKNOWN_FORMAT));
    let xruns = Arc::new(AtomicUsize::new(0));
    let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let stop_worker = stop.clone();
    let level_worker = level_bits.clone();
    let buffer_worker = buffer_ns.clone();
    let format_worker = format.clone();
    let xruns_worker = xruns.clone();
    let error_worker = last_error.clone();
    let alsa_name = alsa_name.to_string();
    let dst_host = dst_host.to_string();
    let spec = spec.clone();

    let thread = thread::Builder::new()
        .name(format!("send-{alsa_name}"))
        .spawn(move || {
            crate::rt::raise_thread_priority("send pipeline", crate::rt::PRIO_SEND);
            let result = match alsa_name.strip_prefix(tone::TONE_PREFIX) {
                Some(freq) => tone_loop(
                    freq.parse().unwrap_or(440.0),
                    &spec,
                    ctx,
                    &dst_host,
                    dst_port,
                    outgoing,
                    &stop_worker,
                    &level_worker,
                ),
                None => alsa_loop(
                    &alsa_name,
                    &spec,
                    ctx,
                    &dst_host,
                    dst_port,
                    outgoing,
                    &stop_worker,
                    &level_worker,
                    &buffer_worker,
                    &format_worker,
                    &xruns_worker,
                    channel_offset as usize,
                ),
            };
            if let Err(err) = result {
                tracing::error!(
                    "send pipeline {alsa_name} -> {dst_host}:{dst_port} failed: {err:#}"
                );
                // Poisoning ignored on purpose — see the doc on
                // `SendHandle::last_error`.
                *error_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("{err:#}"));
            }
        })?;

    Ok(SendHandle {
        stop,
        thread,
        level_bits,
        buffer_ns,
        format,
        xruns,
        last_error,
    })
}

/// Largest absolute sample in a period.
///
/// Deliberately not also counting samples past full scale the way the playback
/// side does: what arrives here has already been through the interface's
/// converter, so anything clipped was clipped in hardware before this engine
/// saw it, and a counter here would name SoundNet as the culprit for something
/// it cannot see and did not do. How close the signal runs to the rails is the
/// part we can report honestly, and that is what the meter shows.
fn peak_of(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()))
}

#[allow(clippy::too_many_arguments)]
fn alsa_loop(
    alsa_name: &str,
    spec: &StreamSpec,
    ctx: Arc<RocContext>,
    dst_host: &str,
    dst_port: u16,
    outgoing: Option<IpAddr>,
    stop: &Arc<AtomicBool>,
    level_bits: &Arc<AtomicU32>,
    buffer_ns: &Arc<AtomicU64>,
    format_out: &Arc<AtomicU8>,
    xruns: &Arc<AtomicUsize>,
    channel_offset: usize,
) -> Result<()> {
    // Open as many channels as it takes to reach the far edge of the window,
    // and no more: to read a device's channel 5 you must open 5 channels, but
    // opening all 16 of a 16-channel interface to carry one of them is wasted
    // USB bandwidth on every period.
    let channels = spec.channels as usize;
    let device_channels = channel_offset + channels;
    let (pcm, format) = pcm::open(
        alsa_name,
        alsa::Direction::Capture,
        spec,
        device_channels as u32,
    )?;
    format_out.store(format.as_u8(), Ordering::Relaxed);
    let io = pcm.io_bytes();
    // Not an optimization any more: the loop below waits before it reads, and
    // a wait on a prepared-but-not-started capture stream never returns data.
    // See `pcm::ensure_capture_running`.
    pcm::ensure_capture_running(&pcm);

    let mut sender = sender::open(ctx, dst_host, dst_port, spec, outgoing)?;

    let frame_bytes = device_channels * format.bytes_per_sample();
    let period_frames = spec.frames_per_period as usize;
    let mut raw = vec![0u8; period_frames * frame_bytes];
    // Everything the device gives us, then just the window that goes on the wire.
    let mut device_floats: Vec<f32> = Vec::with_capacity(period_frames * device_channels);
    let mut floats: Vec<f32> = Vec::with_capacity(period_frames * channels);

    let metrics_every = pcm::metrics_every(spec.rate, period_frames);
    let mut ticks = 0_usize;
    let mut consecutive_errors = 0_u32;
    let mut stalled = 0_u32;

    // Ramp in from silence whenever this device starts producing, which means
    // at open and again after every xrun recovery. Both are moments when the
    // driver has just (re)started its DMA ring, and the first period or two
    // out of a freshly started ring is not reliably audio — on some hardware
    // it is whatever was in that memory. Shipping that at full scale is one
    // of the ways a remote machine's speakers get a bang out of nowhere.
    let mut fade = Fade::new(spec.rate);
    fade.arm(RESUME_FADE_MS);

    while !stop.load(Ordering::Relaxed) {
        // The one blocking point in this loop — see the module docs. Waiting
        // here rather than inside `readi` is what bounds how long a stop
        // request can go unheard; everything below returns promptly, which is
        // why no error path may skip it.
        match pcm.wait(Some(DEVICE_WAIT_TIMEOUT_MS)) {
            Ok(true) => stalled = 0,
            Ok(false) => {
                // Timed out: the device has not produced a period yet.
                //
                // Emphatically **not** an xrun — nothing was lost, nothing
                // was late, the device simply has not spoken. Counting it as
                // one would corrupt the only number that tells an operator
                // whether their period size is too aggressive.
                //
                // It is not counted against `consecutive_errors` either, so a
                // stalled device does not eventually kill its own route. That
                // is deliberate: the supervisor would restart the route
                // straight back into the same wedged device, and
                // `snd_pcm_open` on one of those can block far longer than
                // this loop ever does. A route that is stopped cleanly and
                // says so beats a restart loop that cannot be stopped at all.
                stalled += 1;
                if stalled == STALL_WARN_AFTER {
                    tracing::warn!(
                        "capture {alsa_name}: no period for {}ms — device stalled? \
                         The route is still stoppable; nothing is being counted as an xrun.",
                        STALL_WARN_AFTER * DEVICE_WAIT_TIMEOUT_MS
                    );
                }
                pcm::ensure_capture_running(&pcm);
                continue;
            }
            Err(err) => {
                if pcm.try_recover(err, false).is_ok() {
                    xruns.fetch_add(1, Ordering::Relaxed);
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        bail!(
                            "capture {alsa_name}: {consecutive_errors} consecutive xruns, giving up"
                        );
                    }
                    tracing::warn!("capture {alsa_name} xrun recovered");
                    pcm::ensure_capture_running(&pcm);
                    // The ring was just reset; treat what comes out of it
                    // next like a fresh start.
                    fade.arm(RESUME_FADE_MS);
                    continue;
                }
                bail!("capture {alsa_name} wait: {err}");
            }
        }

        // The wait returned ready, which means at least `avail_min` frames
        // are queued, and `avail_min` defaults to one period — so this read
        // is satisfied from what is already there rather than blocking for
        // it. An error here is still possible (an xrun between the wait and
        // the read) and is handled the same way.
        let frames = match io.readi(&mut raw) {
            Ok(frames) => frames,
            Err(err) => {
                if pcm.try_recover(err, false).is_ok() {
                    xruns.fetch_add(1, Ordering::Relaxed);
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        bail!(
                            "capture {alsa_name}: {consecutive_errors} consecutive xruns, giving up"
                        );
                    }
                    tracing::warn!("capture {alsa_name} xrun recovered");
                    pcm::ensure_capture_running(&pcm);
                    // The ring was just reset; treat what comes out of it
                    // next like a fresh start.
                    fade.arm(RESUME_FADE_MS);
                    continue;
                }
                bail!("capture {alsa_name} read: {err}");
            }
        };
        // A short read (signal during the syscall) would otherwise send the
        // tail of the *previous* period again, so convert only what arrived.
        alsa_to_f32(format, &raw[..frames * frame_bytes], &mut device_floats);
        window::extract(
            &device_floats,
            device_channels,
            channel_offset,
            channels,
            &mut floats,
        );
        fade.apply(&mut floats, channels);
        publish_level(level_bits, peak_of(&floats));

        if let Err(err) = sender.write(&mut floats) {
            consecutive_errors += 1;
            if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                return Err(err.context("sender failed repeatedly"));
            }
            tracing::warn!("send pipeline {alsa_name}: {err:#}");
            continue;
        }
        consecutive_errors = 0;

        ticks += 1;
        if ticks >= metrics_every {
            ticks = 0;
            if let Some(ns) = pcm::delay_ns(&pcm, spec.rate) {
                buffer_ns.store(ns, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

/// Preview tone source. There is no capture device here, so the wall clock
/// stands in for the sound card: generate a period, hand it to roc, sleep
/// until the period would have elapsed. That sleep is this loop's blocking
/// point, and it keeps `buffer_ns` at the "not measured" sentinel for the
/// life of the route — a synthesized tone has no ALSA buffer to report, and
/// claiming zero would assert a precision that doesn't exist.
#[allow(clippy::too_many_arguments)]
fn tone_loop(
    freq: f32,
    spec: &StreamSpec,
    ctx: Arc<RocContext>,
    dst_host: &str,
    dst_port: u16,
    outgoing: Option<IpAddr>,
    stop: &Arc<AtomicBool>,
    level_bits: &Arc<AtomicU32>,
) -> Result<()> {
    let mut sender = sender::open(ctx, dst_host, dst_port, spec, outgoing)?;
    // A test tone is a known, bounded amplitude, so this is not about safety
    // here — it is so a preview tone arrives as a note rather than as a click
    // into whatever monitors happen to be up.
    let mut fade = Fade::new(spec.rate);
    fade.arm(RESUME_FADE_MS);

    let period_frames = spec.frames_per_period as usize;
    let mut buf: Vec<f32> = Vec::with_capacity(period_frames * spec.channels as usize);
    let mut phase = 0.0f32;
    let interval =
        std::time::Duration::from_nanos(1_000_000_000u64 * period_frames as u64 / spec.rate as u64);
    let mut next = std::time::Instant::now();
    let mut consecutive_errors = 0_u32;

    while !stop.load(Ordering::Relaxed) {
        tone::generate(
            freq,
            spec.rate,
            spec.channels,
            period_frames,
            &mut phase,
            &mut buf,
        );
        fade.apply(&mut buf, spec.channels as usize);
        publish_level(level_bits, peak_of(&buf));
        if let Err(err) = sender.write(&mut buf) {
            consecutive_errors += 1;
            if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                return Err(err.context("tone sender failed repeatedly"));
            }
            tracing::warn!("tone pipeline: {err:#}");
        } else {
            consecutive_errors = 0;
        }

        next += interval;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            // Fell behind (scheduling hiccup, or the machine was suspended).
            // Re-anchor rather than trying to catch up with a burst of
            // periods, which would only make the receiver's jitter buffer
            // overflow.
            next = now;
        }
    }
    Ok(())
}
