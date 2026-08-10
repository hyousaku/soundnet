//! Shared ALSA device setup for both directions of a route.
//!
//! Capture and playback need byte-identical hardware parameter negotiation —
//! same format fallback, same `_near` handling, same period/periods choice —
//! and when the two drifted apart in earlier versions the symptom was a
//! stream that opened fine and then sounded subtly wrong in one direction
//! only. Keeping it in one place makes that class of bug impossible.

use anyhow::{anyhow, Context, Result};
use soundnet_protocol::{SampleFormat, StreamSpec};

use crate::audio::format::{pick_format, to_alsa_format};

/// Open `alsa_name` for `dir` and negotiate `spec`. Returns the PCM plus the
/// format actually negotiated, which may differ from `spec.alsa_format` —
/// every caller must convert samples using the *returned* format, never the
/// requested one, or the audio is garbled rather than erroring.
pub fn open(
    alsa_name: &str,
    dir: alsa::Direction,
    spec: &StreamSpec,
) -> Result<(alsa::PCM, SampleFormat)> {
    let what = match dir {
        alsa::Direction::Capture => "capture",
        alsa::Direction::Playback => "playback",
    };
    let pcm = alsa::PCM::new(alsa_name, dir, false)
        .with_context(|| format!("open {what} {alsa_name}"))?;

    let format = {
        let hwp = alsa::pcm::HwParams::any(&pcm)?;
        hwp.set_access(alsa::pcm::Access::RWInterleaved)?;
        // Unlike channels/rate/period below, ALSA has no "_near" for format —
        // an exact mismatch (e.g. a raw hw: device that only does S24_3LE
        // when the route asked for S16LE) would otherwise kill the worker
        // outright. Substituting is safe because the wire format is always
        // f32; we just need to convert using whatever we actually opened.
        let format = pick_format(spec.alsa_format, |f| hwp.test_format(to_alsa_format(f)).is_ok())
            .ok_or_else(|| anyhow!("{alsa_name}: no supported {what} format"))?;
        if format != spec.alsa_format {
            tracing::warn!(
                "{what} {alsa_name}: requested format {:?} unsupported, using {:?} instead",
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
        // Two periods (double buffering) is the standard low-latency choice —
        // one period's worth of hardware buffer being drained while the other
        // fills, vs. three periods of slack we don't need. ValueOr::Nearest
        // means a device that insists on more (some cheap USB DACs refuse 2)
        // gets bumped up instead of failing to open.
        hwp.set_periods(2, alsa::ValueOr::Nearest)?;
        pcm.hw_params(&hwp)?;
        format
    };

    Ok((pcm, format))
}

/// How much audio is currently queued in the device, in nanoseconds.
///
/// `None` when the driver won't say. The negative clamp matters: `delay()`
/// goes briefly negative right after an xrun recovery, before the driver's
/// pointers resettle, and casting that to `u64` would store a value near
/// `u64::MAX` — which is exactly the sentinel the stats path reads as "not
/// measured", so an xrun would masquerade as a dead metric.
pub fn delay_ns(pcm: &alsa::PCM, rate: u32) -> Option<u64> {
    let frames = pcm.delay().ok()?;
    Some((frames.max(0) as u64) * 1_000_000_000 / rate.max(1) as u64)
}

/// Periods between metric samples, for a ~200ms cadence. The audio threads
/// run SCHED_FIFO and `PCM::delay()` is a syscall, so it's sampled on the
/// same cadence as the stats pump that consumes it rather than every period.
pub fn metrics_every(rate: u32, period_frames: usize) -> usize {
    (rate as usize / 5 / period_frames.max(1)).max(1)
}

#[cfg(test)]
mod tests {
    use super::metrics_every;

    #[test]
    fn metrics_cadence_is_about_200ms() {
        // 48kHz / 64-frame periods = 750 periods/s → 150 periods per 200ms.
        assert_eq!(metrics_every(48_000, 64), 150);
        assert_eq!(metrics_every(48_000, 128), 75);
        // A period longer than the whole 200ms window still has to sample
        // *something*, so the floor is every period rather than zero (which
        // would make the `ticks >= metrics_every` check fire constantly).
        assert_eq!(metrics_every(48_000, 48_000), 1);
        assert_eq!(metrics_every(0, 64), 1);
    }
}
