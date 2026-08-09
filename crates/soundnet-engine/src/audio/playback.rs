//! ALSA playback worker. Consumes f32 samples pushed by the transport side.

use anyhow::{anyhow, Context, Result};
use rtrb::{Consumer, RingBuffer, Producer};
use soundnet_protocol::StreamSpec;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::audio::format::{f32_to_alsa, pick_format, to_alsa_format};

pub struct PlaybackHandle {
    pub producer: Producer<f32>,
    pub control: PlaybackControl,
    /// Bits of an f32 holding the rolling peak level over the last period.
    /// Read via `f32::from_bits(atomic.load(...))`.
    pub level_bits: Arc<AtomicU32>,
    /// Monotonic counter of xruns since the worker started.
    pub xruns: Arc<AtomicUsize>,
}

pub struct PlaybackControl {
    pub stop: Arc<AtomicBool>,
    pub thread: JoinHandle<()>,
}

impl PlaybackControl {
    pub fn stop_and_join(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

pub fn spawn(alsa_name: &str, spec: &StreamSpec) -> Result<PlaybackHandle> {
    // See capture.rs: 3 periods of slack between the transport pump and
    // ALSA is enough to absorb a single scheduling hiccup without becoming
    // steady-state latency of its own.
    let ring_cap = (spec.frames_per_period as usize) * (spec.channels as usize) * 3;
    let (prod, cons) = RingBuffer::<f32>::new(ring_cap);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();

    let alsa_name = alsa_name.to_string();
    let spec = spec.clone();
    let level_bits = Arc::new(AtomicU32::new(0));
    let xruns = Arc::new(AtomicUsize::new(0));
    let level_worker = level_bits.clone();
    let xruns_worker = xruns.clone();
    let thread = thread::Builder::new()
        .name(format!("pb-{alsa_name}"))
        .spawn(move || {
            crate::rt::raise_thread_priority("playback", crate::rt::PRIO_PLAYBACK);
            if let Err(err) = worker(&alsa_name, &spec, cons, &stop_worker, &level_worker, &xruns_worker) {
                tracing::error!("playback {alsa_name} failed: {err:#}");
            }
        })?;

    Ok(PlaybackHandle {
        producer: prod,
        control: PlaybackControl { stop, thread },
        level_bits,
        xruns,
    })
}

fn worker(
    alsa_name: &str,
    spec: &StreamSpec,
    mut cons: Consumer<f32>,
    stop: &Arc<AtomicBool>,
    level_bits: &Arc<AtomicU32>,
    xruns: &Arc<AtomicUsize>,
) -> Result<()> {
    let pcm = alsa::PCM::new(alsa_name, alsa::Direction::Playback, false)
        .with_context(|| format!("open playback {alsa_name}"))?;
    let format;
    {
        let hwp = alsa::pcm::HwParams::any(&pcm)?;
        hwp.set_access(alsa::pcm::Access::RWInterleaved)?;
        // See capture.rs: format has no "_near" fallback in ALSA itself, so
        // a raw hw: device that doesn't natively support the requested
        // format would otherwise kill the worker instead of just using
        // whatever it does support (transparent, since the wire is f32).
        format = pick_format(spec.alsa_format, |f| hwp.test_format(to_alsa_format(f)).is_ok())
            .ok_or_else(|| anyhow!("{alsa_name}: no supported playback format"))?;
        if format != spec.alsa_format {
            tracing::warn!(
                "playback {alsa_name}: requested format {:?} unsupported, using {:?} instead",
                spec.alsa_format,
                format
            );
        }
        hwp.set_format(to_alsa_format(format))?;
        hwp.set_channels_near(spec.channels as u32)?;
        hwp.set_rate_near(spec.rate, alsa::ValueOr::Nearest)?;
        hwp.set_period_size_near(spec.frames_per_period as i64, alsa::ValueOr::Nearest)?;
        // See capture.rs: 2 periods is the low-latency default; Nearest
        // falls back to whatever the device actually supports.
        hwp.set_periods(2, alsa::ValueOr::Nearest)?;
        pcm.hw_params(&hwp)?;
    }
    let io = pcm.io_bytes();

    let frame_bytes = spec.channels as usize * format.bytes_per_sample();
    let period_frames = spec.frames_per_period as usize;
    let period_samples = period_frames * spec.channels as usize;

    let mut floats: Vec<f32> = vec![0.0; period_samples];
    let mut raw: Vec<u8> = Vec::with_capacity(period_frames * frame_bytes);

    while !stop.load(Ordering::Relaxed) {
        // If the roc receiver worker died, don't spin forever writing silence.
        if cons.is_abandoned() && cons.slots() == 0 {
            tracing::info!("playback: receiver upstream gone, exiting");
            break;
        }
        // Drain up to a period. Fill silence if underrun.
        for slot in floats.iter_mut() {
            *slot = cons.pop().unwrap_or(0.0);
        }
        // Rolling peak for the level meter.
        let mut peak = 0.0_f32;
        for &s in &floats {
            let a = s.abs();
            if a > peak {
                peak = a;
            }
        }
        // Decay towards the new value so brief silence still reads as quiet
        // without the meter feeling twitchy.
        let prev = f32::from_bits(level_bits.load(Ordering::Relaxed));
        let smoothed = if peak > prev { peak } else { prev * 0.7 + peak * 0.3 };
        level_bits.store(smoothed.to_bits(), Ordering::Relaxed);

        f32_to_alsa(format, &floats, &mut raw);
        match io.writei(&raw) {
            Ok(_) => {}
            Err(err) => {
                if pcm.try_recover(err, false).is_ok() {
                    xruns.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("playback xrun recovered");
                    continue;
                }
                return Err(anyhow!("playback write: {err}"));
            }
        }
    }
    Ok(())
}
