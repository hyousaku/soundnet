//! The two audio pipelines, one per direction of a route.
//!
//! Each is a single thread that owns both its sound device and its roc
//! endpoint, so a route this engine is the source of costs one thread, and a
//! route it is the destination of costs one more. (It used to be two each,
//! with a ring buffer in between; see the module docs on `send.rs` for why
//! that ring had to go.)

pub mod fade;
pub mod recv;
pub mod send;

use std::sync::atomic::{AtomicU32, Ordering};

/// Fold this period's peak into the rolling level the UI draws.
///
/// Rises instantly and falls slowly: a meter that tracked the raw per-period
/// peak would flicker to the floor between syllables and be unreadable, while
/// one that only rose would never come back down. Both pipelines publish
/// through here so the two ends of a route decay at the same rate — a meter
/// that behaved differently depending on which engine's UI you had open would
/// be worse than no meter.
///
/// Stored as the bits of an f32 in an `AtomicU32`: the audio thread must not
/// take a lock, and there is no atomic float.
pub fn publish_level(level_bits: &AtomicU32, peak: f32) {
    let prev = f32::from_bits(level_bits.load(Ordering::Relaxed));
    let smoothed = if peak > prev {
        peak
    } else {
        prev * 0.7 + peak * 0.3
    };
    level_bits.store(smoothed.to_bits(), Ordering::Relaxed);
}

/// How many consecutive failing iterations a pipeline tolerates before it
/// gives up and lets the route supervisor restart it with backoff.
///
/// The counter resets on any successful iteration, so reaching this means
/// the device or endpoint has failed outright — an unplugged interface, a
/// driver that recovers from every xrun and then immediately xruns again.
/// A bound is not optional: these threads run `SCHED_FIFO`, and the error
/// paths are the one place where a loop can go around without hitting its
/// blocking call, so an unbounded retry would pin a core at real-time
/// priority and starve everything else on it — including the control plane
/// that would let an operator fix the problem.
pub const MAX_CONSECUTIVE_ERRORS: u32 = 64;

/// How long a pipeline blocks in `snd_pcm_wait` before coming back around to
/// look at its stop flag.
///
/// This exists to put a ceiling on teardown. The loops used to block
/// directly in `snd_pcm_readi`/`snd_pcm_writei`, which return when the device
/// says so and not before — so how long a route took to stop was entirely up
/// to the hardware, and a device that stopped answering (USB pulled
/// mid-stream, driver wedged) kept its thread forever. Waiting with a timeout
/// first makes the answer "at most this long", always.
///
/// 100 ms, chosen from both directions:
///
/// * **Floor.** It must comfortably exceed the longest *legitimate* wait,
///   which is one period. The coarsest setting the UI offers — 512 frames at
///   44.1 kHz — is 11.6 ms; the recommended 128 at 48 kHz is 2.7 ms. At 100 ms
///   there is ~9x headroom over the worst case, so a healthy device returns
///   with data long before the timeout and this number never touches the
///   audio path at all.
/// * **Ceiling.** It is exactly how long a stop request can go unnoticed. Now
///   that `routing::shutdown_all` flags every route before joining any of
///   them, it is also roughly the whole engine's teardown time rather than
///   per route.
///
/// The cost is one extra `poll()` per period — ~750/s at 128 frames/48 kHz —
/// and is independent of the value chosen here, because in normal operation
/// the wait ends on data arriving, not on the clock.
pub const DEVICE_WAIT_TIMEOUT_MS: u32 = 100;

/// Consecutive device timeouts (so, `STALL_WARN_AFTER * DEVICE_WAIT_TIMEOUT_MS`
/// of nothing at all) before the pipeline says so in the log.
///
/// A stalled device is not an error the pipeline can act on — see the timeout
/// arms in `send.rs`/`recv.rs` for why it deliberately does not give up — but
/// silence about it is worse. One line per stall episode, not per timeout:
/// the counter resets on the next successful wait, so a device that stalls
/// repeatedly produces one line each time rather than ten a second forever.
pub const STALL_WARN_AFTER: u32 = 10;

/// How long audio takes to come back up to full level after a gap.
///
/// Applied by both pipelines through `fade::Fade` — see that module for what
/// a ramp does and does not protect against, and for the incident that put it
/// there. 200 ms is a compromise: long enough that a burst arrives as a swell
/// somebody can react to and short enough that a stream resuming on purpose
/// does not feel broken.
pub const RESUME_FADE_MS: u32 = 200;

/// How much unbroken digital silence counts as "the stream was gone", after
/// which the receiver ramps the audio back in rather than resuming at full
/// level.
///
/// The test is exact zeros, which is a proxy for "roc has no session": with
/// no sender connected `roc_receiver_read` zero-fills the frame, while a real
/// converter's idea of silence always carries some noise in the low bits. The
/// proxy is not perfect — a digitally muted source does produce exact zeros —
/// but the consequence of a false positive is a 200 ms fade-in on a passage
/// that was already silent, which nobody can hear.
///
/// Half a second is well past any musical gap and well short of the time it
/// takes to notice a machine has dropped off.
pub const SILENCE_BEFORE_FADE_MS: u32 = 500;

#[cfg(test)]
mod tests {
    use super::{DEVICE_WAIT_TIMEOUT_MS, STALL_WARN_AFTER};

    /// The wait timeout has to stay well clear of the longest wait a
    /// *healthy* device can legitimately impose, which is one period.
    ///
    /// This is the assumption the whole change rests on: because a working
    /// device always returns data first, the timeout never touches the audio
    /// path and only bounds teardown. If someone later offers a coarser
    /// period or a lower rate in the UI and that stops holding, every single
    /// period starts looking like a stall — the journal fills with warnings,
    /// and worse, the loop starts going around without reading. Rather than
    /// leave that to be discovered by ear, fail here.
    ///
    /// The lists mirror `RATES` and `PERIODS` in `web/src/RouteEditor.tsx`;
    /// they are the menu, so widening the menu is exactly the event that
    /// should make someone revisit `DEVICE_WAIT_TIMEOUT_MS`.
    #[test]
    fn wait_timeout_clears_the_longest_period_the_ui_offers() {
        let rates = [44100u32, 48000, 88200, 96000];
        let periods = [32u32, 64, 128, 256, 512];
        let worst_ms = periods
            .iter()
            .flat_map(|p| rates.iter().map(move |r| 1000.0 * *p as f64 / *r as f64))
            .fold(0.0f64, f64::max);

        // 512 frames at 44.1 kHz.
        assert!(
            (worst_ms - 11.61).abs() < 0.05,
            "worst-case period is now {worst_ms:.2}ms — the UI's rate/period menu changed"
        );
        assert!(
            f64::from(DEVICE_WAIT_TIMEOUT_MS) >= worst_ms * 4.0,
            "DEVICE_WAIT_TIMEOUT_MS ({DEVICE_WAIT_TIMEOUT_MS}ms) leaves too little headroom \
             over a legitimate {worst_ms:.2}ms period wait"
        );
    }

    /// A stall warning that fires in well under a second would cry wolf over
    /// an ordinary hiccup; one that takes half a minute is no better than
    /// silence when an operator is standing there wondering why it went
    /// quiet.
    #[test]
    fn stall_warning_lands_in_the_useful_range() {
        let after_ms = STALL_WARN_AFTER * DEVICE_WAIT_TIMEOUT_MS;
        assert!(
            (500..=5_000).contains(&after_ms),
            "stall warning after {after_ms}ms is outside the useful range"
        );
    }
}
