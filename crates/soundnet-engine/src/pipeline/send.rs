//! A route's send side.
//!
//! Two quite different things live behind one handle, and the asymmetry is
//! deliberate:
//!
//! * **A capture device** is scarce hardware that can only be opened once, so
//!   routes *share* one — the device gets an owning thread in
//!   `pipeline/capture.rs` and each route subscribes to it. That is where the
//!   audio loop and all the ALSA reasoning now live.
//! * **A preview tone** is synthesized, so there is nothing to contend for.
//!   Every tone route gets its own thread, below.
//!
//! What this module provides is one shape `routing` can hold either in, and
//! an honest answer from each accessor about which of the two it is talking
//! to — a tone has no ALSA buffer, no negotiated format, no card to overrun
//! and nothing to stall, and says so with sentinels rather than zeros.

use anyhow::Result;
use soundnet_protocol::{StreamSpec, UNKNOWN_FORMAT};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::pipeline::capture;
use crate::pipeline::fade::Fade;
use crate::pipeline::{publish_level, MAX_CONSECUTIVE_ERRORS, RESUME_FADE_MS};
use crate::tone;
use crate::transport::{sender, RocContext};

/// A route's send side: either its own tone-generating thread, or a
/// subscription to a capture device shared with other routes.
///
/// The two are not symmetrical and should not be forced to be. A tone source
/// is synthesized, so there is nothing to contend for and every route gets
/// its own thread; a capture device is scarce hardware, so routes share one
/// (see `pipeline/capture.rs`). What this type does is give `routing` one
/// shape to hold either in.
pub struct SendHandle {
    /// Bits of an f32 holding the rolling peak of what this route put on the
    /// wire. Read via `f32::from_bits(atomic.load(...))`.
    ///
    /// Per route even when the device is shared, and measured after the
    /// channel window is extracted: two routes off one interface usually
    /// carry different channels of it, and a meter driven by the wrong ones
    /// would be worse than no meter.
    level_bits: Arc<AtomicU32>,
    kind: Kind,
}

enum Kind {
    Tone(ToneWorker),
    Capture {
        registry: Arc<capture::CaptureRegistry>,
        owner: Arc<capture::CaptureOwner>,
        /// This route's own standing on the shared device — a device can
        /// outlive one of its routes, so "the device is fine" is not the same
        /// answer as "this route is fine".
        health: Arc<capture::SubscriberHealth>,
        route_id: String,
    },
}

/// A tone route's own thread and the state it publishes.
struct ToneWorker {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
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
    last_error: Arc<Mutex<Option<String>>>,
    /// A synthesized tone has no ALSA buffer to report, no device to
    /// negotiate a format with, no card to overrun and nothing to stall. All
    /// four stay at their "nothing to say" sentinels for the life of the
    /// route rather than reporting a zero that would look like a measurement.
    buffer_ns: Arc<AtomicU64>,
    format: Arc<AtomicU8>,
    xruns: Arc<AtomicUsize>,
    stalled: Arc<AtomicBool>,
}

impl SendHandle {
    /// Rolling peak of what this route put on the wire.
    pub fn level_bits(&self) -> Arc<AtomicU32> {
        self.level_bits.clone()
    }

    /// ALSA capture-buffer delay in nanoseconds — how much audio is queued in
    /// the device, sampled every ~200ms via `PCM::delay()`. This is the piece
    /// of latency roc's own e2e metric does not cover: frames sit here
    /// *before* `roc_sender_write` ever sees them.
    ///
    /// A property of the device, so every route sharing one reports the same
    /// figure. That is not double counting — it is one buffer, and they are
    /// all reading out of it.
    pub fn buffer_ns(&self) -> Arc<AtomicU64> {
        match &self.kind {
            Kind::Tone(t) => t.buffer_ns.clone(),
            Kind::Capture { owner, .. } => owner.buffer_ns.clone(),
        }
    }

    /// The format the capture device was actually opened with. Shared, for
    /// the same reason as `buffer_ns`.
    pub fn format(&self) -> Arc<AtomicU8> {
        match &self.kind {
            Kind::Tone(t) => t.format.clone(),
            Kind::Capture { owner, .. } => owner.format.clone(),
        }
    }

    /// Recovered capture xruns. An overrun means the device had a period
    /// ready before we came back for it, so those samples are simply gone.
    /// Shared: one read was late, and it was late for everyone on the card.
    pub fn xruns(&self) -> Arc<AtomicUsize> {
        match &self.kind {
            Kind::Tone(t) => t.xruns.clone(),
            Kind::Capture { owner, .. } => owner.xruns.clone(),
        }
    }

    /// True while the capture device has stopped producing periods — the same
    /// condition that logs "device stalled?", published so the UI can show it
    /// instead of a green "ok".
    pub fn stalled(&self) -> Arc<AtomicBool> {
        match &self.kind {
            Kind::Tone(t) => t.stalled.clone(),
            Kind::Capture { owner, .. } => owner.stalled.clone(),
        }
    }

    /// Whether the thread behind this route has stopped. For a shared device
    /// that means the device's thread, which is right: every route on it
    /// depended on that one read.
    pub fn is_dead(&self) -> bool {
        match &self.kind {
            Kind::Tone(t) => t.thread.is_finished(),
            // Either the device stopped, or this route alone was dropped from
            // it after its sender failed repeatedly. Both mean this route is
            // not moving audio, which is the question being asked.
            Kind::Capture { owner, health, .. } => owner.is_dead() || health.has_failed(),
        }
    }

    /// Why this route's send side stopped, as reported by the thread itself.
    pub fn failure_reason(&self) -> Option<String> {
        match &self.kind {
            Kind::Tone(t) => t
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            // This route's own failure first: if it was dropped from a device
            // that is otherwise healthy, the device has nothing to report and
            // saying so would lose the only explanation there is.
            Kind::Capture { owner, health, .. } => {
                health.reason().or_else(|| owner.failure_reason())
            }
        }
    }

    /// Ask the pipeline to stop, without waiting for it.
    ///
    /// Split out from the join so a caller tearing down several routes at
    /// once can raise every flag first and then wait once — see
    /// `routing::shutdown_all`. On its own this returns immediately; the
    /// thread does not notice until it next comes around the top of its loop.
    ///
    /// For a shared device this is a no-op unless this is the last route on
    /// it: stopping the device would take the audio of every other route with
    /// it, and `stop_and_join` is where that decision is actually made.
    pub fn request_stop(&self) {
        match &self.kind {
            Kind::Tone(t) => t.stop.store(true, Ordering::Relaxed),
            Kind::Capture { .. } => {}
        }
    }

    /// Stop this route's send side and wait for anything it owned to be gone.
    ///
    /// This blocks for however long a thread takes to return from whatever
    /// ALSA call it is currently inside, which is why `routing` only ever
    /// calls it from a blocking-pool thread. For a shared device only the
    /// last route out pays that cost — and it must, because "the route
    /// stopped" has to mean "the card is free" or a route restarted straight
    /// afterwards would race its own predecessor for the hardware.
    pub fn stop_and_join(self) {
        match self.kind {
            Kind::Tone(t) => {
                t.stop.store(true, Ordering::Relaxed);
                let _ = t.thread.join();
            }
            Kind::Capture {
                registry,
                owner,
                route_id,
                ..
            } => registry.unsubscribe(&owner.alsa_name, &route_id),
        }
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
    route_id: &str,
    registry: &Arc<capture::CaptureRegistry>,
) -> Result<SendHandle> {
    if let Some(freq) = alsa_name.strip_prefix(tone::TONE_PREFIX) {
        return spawn_tone(
            freq.parse().unwrap_or(440.0),
            alsa_name,
            spec,
            ctx,
            dst_host,
            dst_port,
            outgoing,
        );
    }

    // The sender is built lazily: `subscribe` only calls this once it knows
    // the device will actually take this route, so a route that is about to
    // be refused does not open a UDP socket first.
    let dst = dst_host.to_string();
    let build = || sender::open(ctx, &dst, dst_port, spec, outgoing);

    let sub = registry.subscribe(
        alsa_name,
        spec,
        route_id,
        channel_offset as usize,
        spec.channels as usize,
        build,
    )?;

    Ok(SendHandle {
        level_bits: sub.level_bits,
        kind: Kind::Capture {
            registry: registry.clone(),
            owner: sub.owner,
            health: sub.health,
            route_id: route_id.to_string(),
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_tone(
    freq: f32,
    alsa_name: &str,
    spec: &StreamSpec,
    ctx: Arc<RocContext>,
    dst_host: &str,
    dst_port: u16,
    outgoing: Option<IpAddr>,
) -> Result<SendHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let level_bits = Arc::new(AtomicU32::new(0));
    let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let stop_worker = stop.clone();
    let level_worker = level_bits.clone();
    let error_worker = last_error.clone();
    let name = alsa_name.to_string();
    let dst = dst_host.to_string();
    let spec = spec.clone();

    let thread = thread::Builder::new()
        .name(format!("send-{alsa_name}"))
        .spawn(move || {
            crate::rt::raise_thread_priority("send pipeline", crate::rt::PRIO_SEND);
            if let Err(err) = tone_loop(
                freq,
                &spec,
                ctx,
                &dst,
                dst_port,
                outgoing,
                &stop_worker,
                &level_worker,
            ) {
                tracing::error!("send pipeline {name} -> {dst}:{dst_port} failed: {err:#}");
                // Poisoning ignored on purpose — see the doc on
                // `ToneWorker::last_error`.
                *error_worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("{err:#}"));
            }
        })?;

    Ok(SendHandle {
        level_bits,
        kind: Kind::Tone(ToneWorker {
            stop,
            thread,
            last_error,
            buffer_ns: Arc::new(AtomicU64::new(u64::MAX)),
            format: Arc::new(AtomicU8::new(UNKNOWN_FORMAT)),
            xruns: Arc::new(AtomicUsize::new(0)),
            stalled: Arc::new(AtomicBool::new(false)),
        }),
    })
}

/// Largest absolute sample in a period.
fn peak_of(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()))
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
