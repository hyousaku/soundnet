//! Shared ALSA device setup for both directions of a route.
//!
//! Capture and playback need byte-identical hardware parameter negotiation —
//! same format fallback, same `_near` handling, same period/periods choice —
//! and when the two drifted apart in earlier versions the symptom was a
//! stream that opened fine and then sounded subtly wrong in one direction
//! only. Keeping it in one place makes that class of bug impossible.

use anyhow::{anyhow, bail, Context, Result};
use soundnet_protocol::{SampleFormat, StreamSpec};

use crate::audio::format::{pick_format, to_alsa_format};

/// Open `alsa_name` for `dir` and negotiate `spec`. Returns the PCM plus the
/// format actually negotiated, which may differ from `spec.alsa_format` —
/// every caller must convert samples using the *returned* format, never the
/// requested one, or the audio is garbled rather than erroring.
///
/// `device_channels` is how wide to open the device, which is *not* the
/// route's channel count: reaching a device's channel 5 means opening at
/// least 5 channels and then taking a window out of the middle. See
/// `audio::window`.
///
/// Rate and channel count get no such latitude. They are set with `_near`
/// like everything else, so ALSA will quietly hand back the closest thing the
/// device can do — and unlike format, there is no honest way to absorb that:
///
/// * Both engines register roc's packet encoding from the route's `(rate,
///   channels)`. The two ends negotiate with their own devices independently,
///   so a silent substitution means the sender and the receiver disagree
///   about what a frame *is*. "Use whatever we got" cannot fix it; there is
///   no shared value to use.
/// * Everything downstream computes frame strides from `spec.channels` and
///   timing from `spec.rate`. A device that answered differently produces
///   scrambled interleaving, or samples labelled with a rate they were not
///   captured at.
///
/// So a mismatch is an error here, named precisely, rather than a stream that
/// opens successfully and sounds wrong.
pub fn open(
    alsa_name: &str,
    dir: alsa::Direction,
    spec: &StreamSpec,
    device_channels: u32,
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
        let format = pick_format(spec.alsa_format, |f| {
            hwp.test_format(to_alsa_format(f)).is_ok()
        })
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
        hwp.set_channels_near(device_channels)?;
        hwp.set_rate_near(spec.rate, alsa::ValueOr::Nearest)?;
        hwp.set_period_size_near(spec.frames_per_period as i64, alsa::ValueOr::Nearest)?;
        match dir {
            // Two periods (double buffering) is the standard low-latency
            // choice — one period's worth of hardware buffer being drained
            // while the other fills, vs. three periods of slack we don't
            // need. ValueOr::Nearest means a device that insists on more
            // (some cheap USB DACs refuse 2) gets bumped up instead of
            // failing to open.
            alsa::Direction::Capture => hwp.set_periods(2, alsa::ValueOr::Nearest)?,
            // Playback gets a deep *allocation* but not a deep *fill*: how
            // much is actually queued is set in software (see
            // `set_playback_depth`), starting at the same two periods as
            // above. The room is there so the depth can be raised on a
            // running stream when two periods turn out not to be enough —
            // which, on some hardware, no period size fixes (see
            // `PLAYBACK_BUFFER_PERIODS`). A driver that will not give that
            // much gets whatever it will; the depth is capped to fit.
            alsa::Direction::Playback => {
                let period = hwp.get_period_size()?;
                if hwp
                    .set_buffer_size_near(period * PLAYBACK_BUFFER_PERIODS as i64)
                    .is_err()
                {
                    hwp.set_periods(2, alsa::ValueOr::Nearest)?;
                }
            }
        }

        let got_channels = hwp.get_channels()?;
        if got_channels != device_channels {
            bail!(
                "{alsa_name}: needs {device_channels} channels open (the route's \
                 {} channels starting at offset {}), device offers {got_channels}. \
                 Move the window down, or narrow it — unlike sample format, \
                 channel count cannot be substituted transparently, because it \
                 defines what a frame is.",
                spec.channels,
                device_channels as i64 - spec.channels as i64
            );
        }
        let got_rate = hwp.get_rate()?;
        if got_rate != spec.rate {
            bail!(
                "{alsa_name}: asked for {} Hz, device offers {got_rate} Hz. \
                 Set the route to {got_rate} Hz — sending samples labelled with \
                 a rate they were not captured at shifts the pitch and leaves \
                 roc's clock tuner chasing an offset it cannot close.",
                spec.rate
            );
        }
        // The period is a different matter: it only sets how much audio moves
        // per syscall, so a device that rounds it changes the latency and
        // nothing else. Worth saying, not worth refusing.
        if let Ok(got_period) = hwp.get_period_size() {
            if got_period != spec.frames_per_period as i64 {
                tracing::info!(
                    "{what} {alsa_name}: period {} not available, using {got_period}",
                    spec.frames_per_period
                );
            }
        }

        pcm.hw_params(&hwp)?;
        format
    };

    // Playback's software params (start threshold, wakeup level) are set by
    // the caller through `set_playback_depth`, which is also what raises the
    // depth later. Nothing may rely on alsa-lib's defaults for them — see
    // that function for what the defaults did.

    Ok((pcm, format))
}

/// How many periods of room a playback buffer is allocated, and so the
/// deepest [`set_playback_depth`] can go.
///
/// Why raising the depth has to be possible at all: on a Raspberry Pi's own
/// outputs (the headphone jack and HDMI) the driver hands audio to the
/// firmware in large chunks rather than as the DAC consumes it. The buffer
/// empties in steps, and at two periods one step can take everything that is
/// queued. Every step then risks an underrun — and a larger period does not
/// help, because the chunk grows with it. Operators saw `xr` climb steadily
/// with the period at its maximum. What does help is more periods queued,
/// which is what the extra room is for.
pub const PLAYBACK_BUFFER_PERIODS: u32 = 8;

/// Set how many periods of `write_frames` each a running playback stream
/// keeps queued, and return the depth actually applied (capped to what the
/// buffer the driver gave can hold, never below 2).
///
/// Everything is expressed through two software params, which is what lets
/// this be changed on a stream that is already open:
///
/// * `avail_min` decides when `snd_pcm_wait` wakes the loop: once `buffer -
///   (depth - 1) * period` frames are free, i.e. once the queue is down to
///   `depth - 1` periods. The loop then writes one period, bringing it back
///   to `depth`. With the buffer allocated exactly two periods deep and
///   depth 2, that is alsa-lib's default of one period — the behaviour this
///   replaced, unchanged.
/// * `start_threshold` is `depth` periods: a stream (re)starts only once
///   that much is queued.
///
/// The start threshold must be said out loud, because the default is 1 —
/// measured, not assumed. A playback stream would then begin the instant the
/// first frame is written, so the very first period of 256 frames starts a
/// device whose queue should hold 512. The hardware drains those 256 frames
/// in 5.3 ms while the thread goes back around to fetch and write the next
/// period, so the steady state runs the queue half empty with *no* margin at
/// all: any late wakeup underruns. Worse, `snd_pcm_recover` leaves the
/// buffer empty again, so the restart reproduces the same shallow start and
/// the next period underruns too. That is why the journal once showed xruns
/// in bursts of ten, spaced exactly one period apart, starting the moment a
/// route opened — one hiccup was being amplified into a run of them.
///
/// Starting only once `depth` periods are queued gives the depth its
/// meaning: one period playing while the others wait. At depth 2 that costs
/// one period of latency compared to the accidental behaviour above — and
/// that is the honest framing, because the lower figure was never a working
/// configuration, just a permanently starving one. To get the number back,
/// halve the period: 2 x 128 frames queues the same 5.3 ms as the old 1 x
/// 256, with a real 2.7 ms of slack instead of none.
///
/// Capture has no equivalent. `start_threshold` governs when a stream starts
/// on its own, and the capture path starts explicitly via
/// `ensure_capture_running`; there is no shallow-start problem when the
/// device is the one producing.
pub fn set_playback_depth(pcm: &alsa::PCM, write_frames: usize, depth: u32) -> Result<u32> {
    let buffer = pcm.hw_params_current()?.get_buffer_size()?;
    let write = (write_frames as i64).max(1);
    let depth = depth.clamp(2, ((buffer / write) as u32).max(2));
    let avail_min = (buffer - (depth as i64 - 1) * write).max(1);
    let start = (depth as i64 * write).min(buffer);
    let swp = pcm.sw_params_current()?;
    swp.set_avail_min(avail_min)
        .context("set playback wakeup level")?;
    swp.set_start_threshold(start)
        .context("set playback start threshold")?;
    pcm.sw_params(&swp)
        .context("apply playback software params")?;
    Ok(depth)
}

/// Decides when a playback stream's queue gets deeper.
///
/// One xrun on its own changes nothing — a stray scheduling hiccup is not a
/// reason to add latency for the rest of the show. Two within
/// [`DEEPEN_WINDOW`] mean the device is underrunning as a pattern, and each
/// further one inside the window adds another period, up to the cap. Depth
/// never comes back down on its own: at an event a route that has become
/// stable should stay stable, and restarting the route (changing any of its
/// settings) starts again from two.
pub struct DepthGovernor {
    depth: u32,
    last_xrun: Option<std::time::Instant>,
}

/// See [`DepthGovernor`].
pub const DEEPEN_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

impl DepthGovernor {
    pub fn new() -> Self {
        Self {
            depth: 2,
            last_xrun: None,
        }
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Record an xrun at `now`. Returns the new depth when it should grow.
    pub fn on_xrun(&mut self, now: std::time::Instant) -> Option<u32> {
        let repeated = self
            .last_xrun
            .is_some_and(|t| now.duration_since(t) < DEEPEN_WINDOW);
        self.last_xrun = Some(now);
        if repeated && self.depth < PLAYBACK_BUFFER_PERIODS {
            self.depth += 1;
            Some(self.depth)
        } else {
            None
        }
    }

    /// The device could not go as deep as asked; remember what it took, so
    /// the next xrun does not keep asking for the same impossible step.
    pub fn settle(&mut self, applied: u32) {
        self.depth = applied;
    }
}

impl Default for DepthGovernor {
    fn default() -> Self {
        Self::new()
    }
}

/// Start a capture stream if it is sitting PREPARED rather than running.
///
/// This is load-bearing now, and it was not before. `snd_pcm_wait` *observes*
/// a device; it never drives one. `snd_pcm_readi` does both — handed a
/// prepared stream it starts it implicitly — which is why the capture loop
/// used to get away with never thinking about this: after `snd_pcm_recover`
/// left the stream prepared, the next read started it again for free.
///
/// Now that the loop blocks in `snd_pcm_wait` first, a prepared capture
/// stream is a trap: no data will ever arrive, so every wait times out, and
/// the route goes permanently silent without ever raising an error. Hence
/// this call after every recovery, and again on a timeout as a backstop for
/// any path into that state we haven't thought of.
///
/// **Playback must not get the same treatment.** A playback stream starts
/// itself once `start_threshold` frames have been written; starting one with
/// an empty buffer would just underrun it immediately.
pub fn ensure_capture_running(pcm: &alsa::PCM) {
    if pcm.state() == alsa::pcm::State::Prepared {
        if let Err(err) = pcm.start() {
            tracing::warn!("capture start failed: {err}");
        }
    }
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
    use super::{metrics_every, DepthGovernor, DEEPEN_WINDOW, PLAYBACK_BUFFER_PERIODS};
    use std::time::{Duration, Instant};

    #[test]
    fn a_single_xrun_does_not_add_latency() {
        let mut g = DepthGovernor::new();
        let t = Instant::now();
        assert_eq!(g.on_xrun(t), None);
        // The next one long after: still isolated.
        assert_eq!(g.on_xrun(t + DEEPEN_WINDOW + Duration::from_secs(1)), None);
        assert_eq!(g.depth(), 2);
    }

    #[test]
    fn repeated_xruns_deepen_one_period_at_a_time_up_to_the_cap() {
        let mut g = DepthGovernor::new();
        let mut t = Instant::now();
        assert_eq!(g.on_xrun(t), None);
        for want in 3..=PLAYBACK_BUFFER_PERIODS {
            t += Duration::from_secs(5);
            assert_eq!(g.on_xrun(t), Some(want));
        }
        t += Duration::from_secs(5);
        assert_eq!(g.on_xrun(t), None, "capped");
        assert_eq!(g.depth(), PLAYBACK_BUFFER_PERIODS);
    }

    #[test]
    fn a_device_that_cannot_go_deeper_stops_the_climb() {
        let mut g = DepthGovernor::new();
        let t = Instant::now();
        g.on_xrun(t);
        assert_eq!(g.on_xrun(t + Duration::from_secs(1)), Some(3));
        g.settle(2);
        assert_eq!(g.depth(), 2);
    }

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

/// Against a real ALSA stack, where one exists: alsa-lib's `null` device
/// negotiates hardware and software params like a sound card but discards
/// what it is given. Skipped where alsa-lib itself is missing.
#[cfg(test)]
mod null_device_tests {
    use super::*;
    use soundnet_protocol::StreamSpec;

    fn spec(period: u32) -> StreamSpec {
        StreamSpec {
            frames_per_period: period,
            ..StreamSpec::default()
        }
    }

    #[test]
    fn playback_depth_is_raised_on_an_open_stream_and_capped_by_the_buffer() {
        let Ok((pcm, _)) = open("null", alsa::Direction::Playback, &spec(256), 2) else {
            eprintln!("no ALSA null device here; skipping");
            return;
        };
        let buffer = pcm.hw_params_current().unwrap().get_buffer_size().unwrap();
        let period = pcm.hw_params_current().unwrap().get_period_size().unwrap();
        assert!(
            buffer >= 4 * period,
            "room to deepen: {buffer} frames for {period}-frame periods"
        );

        assert_eq!(set_playback_depth(&pcm, 256, 2).unwrap(), 2);
        let swp = pcm.sw_params_current().unwrap();
        assert_eq!(swp.get_start_threshold().unwrap(), 512);
        assert_eq!(swp.get_avail_min().unwrap(), buffer - 256);

        assert_eq!(set_playback_depth(&pcm, 256, 4).unwrap(), 4);
        let swp = pcm.sw_params_current().unwrap();
        assert_eq!(swp.get_start_threshold().unwrap(), 1024);
        assert_eq!(swp.get_avail_min().unwrap(), buffer - 3 * 256);

        let max = (buffer / 256) as u32;
        assert_eq!(set_playback_depth(&pcm, 256, 1000).unwrap(), max);
    }
}
