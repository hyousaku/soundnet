//! The gain ramp applied whenever audio starts flowing again.
//!
//! This exists because of an incident, not a theory. A laptop holding the
//! capture end of a route was closed by accident; on reboot the service
//! restored its routes and started streaming immediately, and the machine at
//! the other end — the one with the speakers — put out full-scale noise with
//! nobody touching anything.
//!
//! The goal is to keep that automatic recovery, because a system that needs a
//! human to press a button after every power cut is not much of an unattended
//! system. What has to change is the level it comes back at.
//!
//! ## The curve is cubic, and that is the whole point
//!
//! A linear ramp is only half a second's protection at best: halfway along it
//! is already at -6 dB, which is not quiet. `gain = (t/T)^3` spends most of
//! its length genuinely low and then arrives:
//!
//! | elapsed | linear | cubic |
//! |---------|--------|-------|
//! | 25% | -12 dB | -36 dB |
//! | 50% | -6 dB | -18 dB |
//! | 75% | -2.5 dB | -7.5 dB |
//! | 100% | 0 dB | 0 dB |
//!
//! So a two-second cubic ramp holds the output at or below -18 dB for the
//! first full second. A burst arriving into that is quiet enough to be
//! startling rather than damaging, and there is a second in hand to reach a
//! fader before it is loud.
//!
//! ## Two lengths, because two situations
//!
//! How cautious the return should be depends on how long the audio was away,
//! and the pipelines pick the length accordingly (see `pipeline/mod.rs`):
//!
//! * **A brief gap** — a dropped packet run, a peer restarting a worker — is
//!   audio whose level was fine a moment ago and will be fine now. It needs
//!   declicking, not caution: a short ramp.
//! * **A long absence, or a route that has only just opened** — the machine
//!   at the other end rebooted, or this is the first audio of the session.
//!   Nothing is known about what is about to arrive, including its level.
//!   That gets the long one.
//!
//! ## What this does not do
//!
//! It does not cap the level. If the source is genuinely producing full-scale
//! noise, that is what plays once the ramp completes. Nothing here is a
//! limiter, and calling it one would be a claim an operator might rely on
//! while standing in front of a loudspeaker.
//!
//! Cheap enough for the audio thread: a multiply per sample, and only while a
//! ramp is actually running.

/// A cubic ramp from silence to unity.
#[derive(Debug)]
pub struct Fade {
    /// Frames still to ramp. Zero means "not fading", and `apply` is then a
    /// no-op that does not even touch the buffer.
    remaining: u32,
    /// Length of the ramp currently running, in frames.
    length: u32,
    rate: u32,
}

impl Fade {
    /// A fade for a stream at `rate`, initially inactive.
    ///
    /// Inactive is the right default even for a pipeline that arms it
    /// immediately: a `Fade` nobody ever arms can then only be a no-op, so
    /// forgetting to arm one silences nothing.
    pub fn new(rate: u32) -> Self {
        Self {
            remaining: 0,
            length: 0,
            rate,
        }
    }

    /// Start a ramp lasting `ms`, from silence.
    ///
    /// Re-arming mid-ramp restarts from silence rather than from wherever the
    /// old one had reached. That is the safe direction: the reason to re-arm
    /// is that something happened, and something having happened is not a
    /// reason to be further along.
    pub fn arm(&mut self, ms: u32) {
        self.length = (self.rate as u64 * ms as u64 / 1000) as u32;
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
            let linear = (position as f32 / self.length as f32).min(1.0);
            let gain = linear * linear * linear;
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

    const RATE: u32 = 48_000;

    fn db(gain: f32) -> f32 {
        20.0 * gain.log10()
    }

    /// Push `ms` worth of full-scale audio through and return the gain applied
    /// to each frame. Full scale in means each output sample *is* the gain,
    /// which is what makes the assertions below readable.
    fn gains_over(f: &mut Fade, ms: u32, channels: usize) -> Vec<f32> {
        let frames = (RATE as usize) * ms as usize / 1000;
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
        let mut f = Fade::new(RATE);
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
        let mut f = Fade::new(RATE);
        f.arm(2_000);
        let gains = gains_over(&mut f, 2_000, 2);

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
    }

    /// The property the safety argument actually rests on: a two-second ramp
    /// keeps the output at or below -18 dB for the first full second. A linear
    /// ramp would be at -6 dB by then, which is not a level anyone would call
    /// safe to be surprised by.
    #[test]
    fn a_two_second_ramp_stays_quiet_for_the_first_second() {
        let mut f = Fade::new(RATE);
        f.arm(2_000);
        let gains = gains_over(&mut f, 2_000, 2);

        let first_second = &gains[..RATE as usize];
        let loudest = first_second.iter().copied().fold(0.0_f32, f32::max);
        assert!(
            db(loudest) <= -18.0,
            "first second peaked at {:.1} dB, expected -18 dB or quieter",
            db(loudest)
        );
        // And it does arrive — quiet for a second is protection, quiet for
        // two would just be a broken route.
        assert!(
            gains[gains.len() - 1] > 0.99,
            "the ramp has to finish, not merely start softly"
        );
    }

    /// Both samples of a frame get the same gain — a ramp that advanced
    /// per-sample rather than per-frame would swing the stereo image while it
    /// ran.
    #[test]
    fn every_channel_of_a_frame_is_scaled_alike() {
        let mut f = Fade::new(RATE);
        f.arm(200);
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
        let mut f = Fade::new(RATE);
        f.arm(200);
        gains_over(&mut f, 200, 2);
        let mut buf = vec![0.25_f32, -0.75];
        let before = buf.clone();
        f.apply(&mut buf, 2);
        assert_eq!(buf, before);
    }

    /// Re-arming restarts from silence. The reason to re-arm is that
    /// something happened, and that is not a reason to be further along the
    /// ramp than before.
    #[test]
    fn re_arming_restarts_from_silence() {
        let mut f = Fade::new(RATE);
        f.arm(200);
        gains_over(&mut f, 100, 2);
        f.arm(2_000);
        let mut buf = vec![1.0_f32; 2];
        f.apply(&mut buf, 2);
        assert_eq!(buf[0], 0.0, "a re-armed ramp must start over at silence");
    }

    /// A zero-length ramp (a nonsense rate, or 0 ms) must be a no-op rather
    /// than a division by zero that silences the route forever.
    #[test]
    fn a_zero_length_ramp_is_harmless() {
        for (rate, ms) in [(0, 200), (RATE, 0)] {
            let mut f = Fade::new(rate);
            f.arm(ms);
            let mut buf = vec![1.0_f32, 1.0];
            f.apply(&mut buf, 2);
            assert_eq!(buf, vec![1.0, 1.0], "rate={rate} ms={ms}");
        }
    }
}
