//! A short gain ramp applied whenever audio starts flowing again.
//!
//! This exists because of an incident, not a theory. A laptop holding the
//! capture end of a route was closed by accident; on reboot the service
//! restored its routes and started streaming immediately, and the machine at
//! the other end — the one with the speakers — emitted full-scale noise with
//! nobody touching anything.
//!
//! What a ramp does and does not fix is worth being exact about, because it
//! is easy to mistake for a safety feature it is not:
//!
//! * It **removes the step.** Going from silence to full scale in one sample
//!   is the part that is hard on drivers, and it is the part that makes a
//!   burst feel like a bang rather than a swell.
//! * It **buys reaction time.** A couple of hundred milliseconds is enough
//!   for a person to reach a fader or pull a plug.
//! * It **does not cap the level.** If the source is genuinely producing
//!   full-scale noise, that is what plays once the ramp completes. Nothing
//!   here is a limiter, and calling it one would be a lie an operator might
//!   rely on.
//!
//! Cheap enough for the audio thread: one multiply per sample, and only while
//! a ramp is actually running.

/// A linear ramp from silence to unity over a fixed number of frames.
#[derive(Debug)]
pub struct Fade {
    /// Frames still to ramp. Zero means "not fading", and `apply` is then a
    /// no-op that does not even touch the buffer.
    remaining: u32,
    /// Full length of the ramp, in frames.
    length: u32,
}

impl Fade {
    /// A ramp lasting `ms` at `rate`, initially inactive.
    ///
    /// Inactive is the right default even for a pipeline that arms it
    /// immediately: a `Fade` nobody ever arms can then only be a no-op, so
    /// forgetting to arm one silences nothing.
    pub fn new(rate: u32, ms: u32) -> Self {
        Self {
            remaining: 0,
            length: (rate as u64 * ms as u64 / 1000) as u32,
        }
    }

    /// Start (or restart) the ramp from silence.
    pub fn arm(&mut self) {
        self.remaining = self.length;
    }

    /// Scale one period in place, advancing the ramp by however many frames
    /// the period holds.
    ///
    /// `channels` is the interleave width: every sample of a frame gets the
    /// same gain, so the ramp cannot pull a stereo image around while it
    /// runs.
    pub fn apply(&mut self, samples: &mut [f32], channels: usize) {
        if self.remaining == 0 || self.length == 0 {
            return;
        }
        let channels = channels.max(1);
        let already_done = self.length.saturating_sub(self.remaining);
        for (i, frame) in samples.chunks_mut(channels).enumerate() {
            let position = already_done as u64 + i as u64;
            let gain = (position as f32 / self.length as f32).min(1.0);
            for s in frame.iter_mut() {
                *s *= gain;
            }
        }
        let frames = (samples.len() / channels) as u32;
        self.remaining = self.remaining.saturating_sub(frames);
    }
}

#[cfg(test)]
mod tests {
    use super::Fade;

    /// 48 kHz, 200 ms — the shape the pipelines use.
    const RAMP_FRAMES: usize = 48_000 * 200 / 1000;

    fn fade() -> Fade {
        Fade::new(48_000, 200)
    }

    /// Push `frames` of full-scale audio through and return the gain applied
    /// to each frame. Full scale in means each output sample *is* the gain,
    /// which is what makes the assertions below readable.
    fn gains_over(f: &mut Fade, frames: usize, channels: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames);
        let mut done = 0;
        while done < frames {
            let n = 256.min(frames - done);
            let mut period = vec![1.0_f32; n * channels];
            f.apply(&mut period, channels);
            out.extend(period.chunks(channels).map(|fr| fr[0]));
            done += n;
        }
        out
    }

    /// An unarmed fade must not touch the audio at all. Anything else would
    /// mean the safety net was quietly attenuating every route that never
    /// needed it.
    #[test]
    fn does_nothing_until_armed() {
        let mut f = fade();
        let mut buf = vec![0.5_f32, -0.5, 1.0, -1.0];
        let before = buf.clone();
        f.apply(&mut buf, 2);
        assert_eq!(buf, before);
        // Still inert next period: an unarmed fade never starts on its own.
        f.apply(&mut buf, 2);
        assert_eq!(buf, before);
    }

    /// Starts at silence, ends at unity, never goes backwards. The first
    /// sample being exactly zero is the point — that is the step this exists
    /// to remove.
    #[test]
    fn ramps_from_silence_to_unity_without_ever_dipping() {
        let mut f = fade();
        f.arm();
        let gains = gains_over(&mut f, RAMP_FRAMES, 2);

        assert_eq!(gains[0], 0.0, "the ramp must begin at silence");
        assert!(
            gains.windows(2).all(|w| w[1] >= w[0]),
            "a ramp that dips would be audible as a wobble"
        );
        assert!(
            (gains.last().copied().unwrap() - 1.0).abs() < 0.01,
            "the ramp must reach unity by the end of its length: {:?}",
            gains.last()
        );
        // Halfway along it should be halfway up. A ramp that spent most of
        // its length near full scale would satisfy the checks above while
        // still delivering very nearly a step.
        let middle = gains[RAMP_FRAMES / 2];
        assert!(
            (middle - 0.5).abs() < 0.01,
            "expected roughly half gain at the midpoint, got {middle}"
        );
    }

    /// Both samples of a frame get the same gain — a ramp that advanced
    /// per-sample rather than per-frame would swing the stereo image while it
    /// ran.
    #[test]
    fn every_channel_of_a_frame_is_scaled_alike() {
        let mut f = fade();
        f.arm();
        let mut period = vec![1.0_f32; 64 * 4];
        f.apply(&mut period, 4);
        for frame in period.chunks(4) {
            assert!(
                frame.windows(2).all(|w| w[0] == w[1]),
                "channels within one frame diverged: {frame:?}"
            );
        }
    }

    /// Once the ramp is spent the audio passes through untouched, so a long
    /// stream is not permanently scaled by a stale ramp.
    #[test]
    fn passes_audio_through_once_spent() {
        let mut f = fade();
        f.arm();
        gains_over(&mut f, RAMP_FRAMES, 2);
        let mut buf = vec![0.25_f32, -0.75];
        let before = buf.clone();
        f.apply(&mut buf, 2);
        assert_eq!(buf, before);
    }

    /// A zero-length ramp (a nonsense rate, say) must be a no-op rather than
    /// a division by zero that silences the route forever.
    #[test]
    fn a_zero_length_ramp_is_harmless() {
        let mut f = Fade::new(0, 200);
        f.arm();
        let mut buf = vec![1.0_f32, 1.0];
        f.apply(&mut buf, 2);
        assert_eq!(buf, vec![1.0, 1.0]);
    }
}
