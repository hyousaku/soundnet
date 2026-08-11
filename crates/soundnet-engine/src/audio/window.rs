//! Channel windows: moving a slice of an interleaved buffer's channels.
//!
//! A route carries `channels` channels, but the device it is attached to may
//! have more. `PortRef::channel_offset` says which of the device's channels
//! the window starts at, so "this 8-channel interface's input 5 to that one's
//! output 5" is one route with a width of 1 and an offset of 4 on each side.
//!
//! Both functions are pure and tested, deliberately: interleaving arithmetic
//! that is off by one does not fail, it quietly plays the wrong channel or
//! shifts every sample by one slot, and the result is audio that is present
//! and wrong. That is far harder to notice than silence.

/// Copy `channels` channels starting at `offset` out of a `device_channels`-wide
/// interleaved buffer, into a `channels`-wide interleaved buffer.
///
/// `out` is cleared first. Frames that would read past the end of `src` are
/// skipped rather than panicking — a short read from ALSA is normal.
pub fn extract(
    src: &[f32],
    device_channels: usize,
    offset: usize,
    channels: usize,
    out: &mut Vec<f32>,
) {
    out.clear();
    if device_channels == 0 || channels == 0 || offset + channels > device_channels {
        return;
    }
    let frames = src.len() / device_channels;
    out.reserve(frames * channels);
    for f in 0..frames {
        let base = f * device_channels + offset;
        out.extend_from_slice(&src[base..base + channels]);
    }
}

/// The inverse: place a `channels`-wide interleaved buffer into a
/// `device_channels`-wide one at `offset`, leaving every other channel silent.
///
/// Silence rather than untouched, because the destination buffer is reused
/// every period: whatever the previous period left in the channels this route
/// does not drive would otherwise be played again, as a stuck fragment of
/// audio on the channels next to the one you patched.
pub fn scatter(
    src: &[f32],
    channels: usize,
    device_channels: usize,
    offset: usize,
    out: &mut Vec<f32>,
) {
    out.clear();
    if device_channels == 0 || channels == 0 || offset + channels > device_channels {
        return;
    }
    let frames = src.len() / channels;
    out.resize(frames * device_channels, 0.0);
    for f in 0..frames {
        let from = f * channels;
        let to = f * device_channels + offset;
        out[to..to + channels].copy_from_slice(&src[from..from + channels]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two frames of a 4-channel device, channels numbered so a misplaced
    /// sample is obvious: frame 0 is [10, 11, 12, 13], frame 1 is [20, ...].
    fn device_buffer() -> Vec<f32> {
        vec![10.0, 11.0, 12.0, 13.0, 20.0, 21.0, 22.0, 23.0]
    }

    #[test]
    fn extract_takes_the_right_channels_from_every_frame() {
        let mut out = Vec::new();
        extract(&device_buffer(), 4, 1, 2, &mut out);
        assert_eq!(out, vec![11.0, 12.0, 21.0, 22.0]);
    }

    #[test]
    fn extract_of_one_channel_is_the_mono_case() {
        // "input 5" on an 8-channel device: offset 4, width 1.
        let mut out = Vec::new();
        extract(&device_buffer(), 4, 3, 1, &mut out);
        assert_eq!(out, vec![13.0, 23.0]);
    }

    #[test]
    fn extract_of_the_whole_device_is_a_copy() {
        let mut out = Vec::new();
        extract(&device_buffer(), 4, 0, 4, &mut out);
        assert_eq!(out, device_buffer());
    }

    #[test]
    fn extract_rejects_a_window_past_the_end() {
        let mut out = vec![99.0];
        extract(&device_buffer(), 4, 3, 2, &mut out);
        assert!(out.is_empty(), "must not read past the frame");
    }

    #[test]
    fn extract_ignores_a_trailing_partial_frame() {
        // ALSA can return fewer frames than asked for; the tail must not be
        // read as if it were a whole frame.
        let short = vec![10.0, 11.0, 12.0, 13.0, 20.0, 21.0];
        let mut out = Vec::new();
        extract(&short, 4, 0, 2, &mut out);
        assert_eq!(out, vec![10.0, 11.0]);
    }

    #[test]
    fn scatter_places_the_window_and_silences_the_rest() {
        let mut out = Vec::new();
        scatter(&[1.0, 2.0, 3.0, 4.0], 2, 4, 2, &mut out);
        assert_eq!(out, vec![0.0, 0.0, 1.0, 2.0, 0.0, 0.0, 3.0, 4.0]);
    }

    #[test]
    fn scatter_clears_the_previous_period() {
        // The same buffer is reused every period. If a stale sample survives
        // in a channel this route doesn't drive, it plays as a stuck fragment
        // on the neighbouring output.
        let mut out = vec![7.0; 8];
        scatter(&[1.0, 2.0], 1, 4, 0, &mut out);
        assert_eq!(out, vec![1.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn scatter_rejects_a_window_past_the_end() {
        let mut out = vec![99.0];
        scatter(&[1.0, 2.0], 2, 2, 1, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn extract_and_scatter_round_trip() {
        let mut window = Vec::new();
        extract(&device_buffer(), 4, 1, 2, &mut window);
        let mut back = Vec::new();
        scatter(&window, 2, 4, 1, &mut back);
        // Channels 1..3 survive; 0 and 3 come back silent, which is correct —
        // this route never carried them.
        assert_eq!(back, vec![0.0, 11.0, 12.0, 0.0, 0.0, 21.0, 22.0, 0.0]);
    }
}
