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
//! The blocking read is also what makes it safe to run this thread at
//! `SCHED_FIFO` (see `rt.rs`): a real-time thread that never blocks would pin
//! a core at 100% and starve everything below it. Every loop below must keep
//! exactly one blocking point — that is the invariant the error handling here
//! is written to preserve.

use anyhow::{bail, Result};
use soundnet_protocol::{StreamSpec, UNKNOWN_FORMAT};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::audio::format::alsa_to_f32;
use crate::audio::{pcm, window};
use crate::pipeline::MAX_CONSECUTIVE_ERRORS;
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
    /// Monotonic count of recovered capture xruns. An overrun means the
    /// device had a period ready before we came back for it, so those
    /// samples are simply gone — an audible click, and for a long time one
    /// that no counter anywhere recorded.
    pub xruns: Arc<AtomicUsize>,
    /// Why this pipeline stopped, if it stopped on its own. Written once, as
    /// the thread unwinds. Without it the UI can only say that a worker
    /// exited — which names the symptom and withholds every fact that would
    /// let an operator act on it.
    pub last_error: Arc<Mutex<Option<String>>>,
}

impl SendHandle {
    pub fn stop_and_join(self) {
        self.stop.store(true, Ordering::Relaxed);
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
    let buffer_ns = Arc::new(AtomicU64::new(u64::MAX));
    let format = Arc::new(AtomicU8::new(UNKNOWN_FORMAT));
    let xruns = Arc::new(AtomicUsize::new(0));
    let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let stop_worker = stop.clone();
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
                ),
                None => alsa_loop(
                    &alsa_name,
                    &spec,
                    ctx,
                    &dst_host,
                    dst_port,
                    outgoing,
                    &stop_worker,
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
                *error_worker.lock().unwrap() = Some(format!("{err:#}"));
            }
        })?;

    Ok(SendHandle {
        stop,
        thread,
        buffer_ns,
        format,
        xruns,
        last_error,
    })
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
    pcm.start().ok(); // Ignore EAGAIN — the first read will start it.

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

    while !stop.load(Ordering::Relaxed) {
        // The one blocking point in this loop. Everything below returns
        // promptly, which is exactly why this must not be skipped on an
        // error path — see the module docs.
        let frames = match io.readi(&mut raw) {
            Ok(frames) => frames,
            Err(err) => {
                if pcm.try_recover(err, false).is_ok() {
                    xruns.fetch_add(1, Ordering::Relaxed);
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        bail!("capture {alsa_name}: {consecutive_errors} consecutive xruns, giving up");
                    }
                    tracing::warn!("capture {alsa_name} xrun recovered");
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
) -> Result<()> {
    let mut sender = sender::open(ctx, dst_host, dst_port, spec, outgoing)?;

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
