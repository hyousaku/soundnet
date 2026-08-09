//! ALSA capture worker.
//!
//! Opens a PCM device (or synthesizes a test tone), reads periods of samples,
//! converts to interleaved f32, and pushes them to a bounded ring buffer that
//! the transport side drains. Runs on a dedicated OS thread so the audio loop
//! isn't at the mercy of the tokio scheduler.

use anyhow::{anyhow, Context, Result};
use rtrb::{Producer, RingBuffer};
use soundnet_protocol::StreamSpec;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::audio::format::{alsa_to_f32, pick_format, to_alsa_format};
use crate::tone;

pub struct CaptureHandle {
    pub consumer: rtrb::Consumer<f32>,
    pub control: CaptureControl,
}

/// Just the parts you keep once you've handed the consumer to a downstream worker.
pub struct CaptureControl {
    pub stop: Arc<AtomicBool>,
    pub thread: JoinHandle<()>,
}

impl CaptureControl {
    pub fn stop_and_join(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

pub fn spawn(alsa_name: &str, spec: &StreamSpec) -> Result<CaptureHandle> {
    // Ring holds ~4 periods worth of interleaved samples — plenty of slack
    // between the capture worker and the transport pump.
    let ring_cap = (spec.frames_per_period as usize) * (spec.channels as usize) * 8;
    let (mut prod, cons) = RingBuffer::<f32>::new(ring_cap);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();

    let alsa_name = alsa_name.to_string();
    let spec = spec.clone();

    let thread = if let Some(freq) = alsa_name.strip_prefix(tone::TONE_PREFIX) {
        let freq: f32 = freq.parse().unwrap_or(440.0);
        thread::Builder::new()
            .name(format!("cap-tone-{alsa_name}"))
            .spawn(move || tone_worker(freq, spec, &mut prod, stop_worker))?
    } else {
        thread::Builder::new()
            .name(format!("cap-{alsa_name}"))
            .spawn(move || {
                if let Err(err) = alsa_worker(&alsa_name, &spec, &mut prod, &stop_worker) {
                    tracing::error!("capture {alsa_name} failed: {err:#}");
                }
            })?
    };

    Ok(CaptureHandle {
        consumer: cons,
        control: CaptureControl { stop, thread },
    })
}

fn alsa_worker(
    alsa_name: &str,
    spec: &StreamSpec,
    prod: &mut Producer<f32>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let pcm = alsa::PCM::new(alsa_name, alsa::Direction::Capture, false)
        .with_context(|| format!("open capture {alsa_name}"))?;
    let format;
    {
        let hwp = alsa::pcm::HwParams::any(&pcm)?;
        hwp.set_access(alsa::pcm::Access::RWInterleaved)?;
        // Unlike channels/rate/period below, ALSA has no "_near" for format —
        // an exact mismatch (e.g. a raw hw: device that only does S24_3LE
        // when the route asked for S16LE) would otherwise kill the worker
        // outright. Substituting is safe because the wire format is always
        // f32; we just need to convert using whatever we actually opened.
        format = pick_format(spec.alsa_format, |f| hwp.test_format(to_alsa_format(f)).is_ok())
            .ok_or_else(|| anyhow!("{alsa_name}: no supported capture format"))?;
        if format != spec.alsa_format {
            tracing::warn!(
                "capture {alsa_name}: requested format {:?} unsupported, using {:?} instead",
                spec.alsa_format,
                format
            );
        }
        hwp.set_format(to_alsa_format(format))?;
        // *_near variants let the driver pick the closest supported value —
        // USB DACs commonly reject exact rate/period requests.
        hwp.set_channels_near(spec.channels as u32)?;
        hwp.set_rate_near(spec.rate, alsa::ValueOr::Nearest)?;
        hwp.set_period_size_near(spec.frames_per_period as i64, alsa::ValueOr::Nearest)?;
        hwp.set_periods(3, alsa::ValueOr::Nearest)?;
        pcm.hw_params(&hwp)?;
    }
    let io = pcm.io_bytes();
    pcm.start().ok(); // Ignore EAGAIN — first read will start it.

    let frame_bytes = spec.channels as usize * format.bytes_per_sample();
    let period_frames = spec.frames_per_period as usize;
    let mut raw = vec![0u8; period_frames * frame_bytes];
    let mut floats: Vec<f32> = Vec::with_capacity(period_frames * spec.channels as usize);

    while !stop.load(Ordering::Relaxed) {
        match io.readi(&mut raw) {
            Ok(_) => {}
            Err(err) => {
                if let Some(errno) = err.errno().checked_abs() {
                    if pcm.try_recover(err, false).is_ok() {
                        tracing::warn!("capture xrun recovered ({errno})");
                        continue;
                    }
                }
                return Err(anyhow!("capture read: {err}"));
            }
        }
        alsa_to_f32(format, &raw, &mut floats);
        for &s in &floats {
            // Drop samples if the transport side has fallen behind — better a
            // brief glitch than an unbounded buffer.
            let _ = prod.push(s);
        }
    }
    Ok(())
}

fn tone_worker(
    freq: f32,
    spec: StreamSpec,
    prod: &mut Producer<f32>,
    stop: Arc<AtomicBool>,
) {
    let period_frames = spec.frames_per_period as usize;
    let mut buf: Vec<f32> = Vec::with_capacity(period_frames * spec.channels as usize);
    let mut phase = 0.0f32;
    let interval = std::time::Duration::from_micros(
        1_000_000u64 * period_frames as u64 / spec.rate as u64,
    );
    let mut next = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) {
        tone::generate(freq, spec.rate, spec.channels, period_frames, &mut phase, &mut buf);
        for &s in &buf {
            let _ = prod.push(s);
        }
        next += interval;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}
