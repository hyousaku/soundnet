//! Real-time scheduling helpers for the audio-path worker threads.
//!
//! Pro-audio on Linux wants two things the default process/thread setup
//! doesn't give you: the process's memory must never be paged out under
//! pressure (`mlockall`), and the threads on the period-by-period hot path
//! must run under `SCHED_FIFO` so the CFS scheduler can't preempt them for
//! an arbitrary slice behind unrelated load. The systemd unit
//! (`packaging/soundnet-engine.service`) grants the rlimits these need
//! (`LimitRTPRIO=95`, `LimitMEMLOCK=infinity`, `LimitNICE=-19`), but plenty
//! of ways to run this binary don't — `cargo run` as a normal user, this
//! dev sandbox, a non-systemd install. Both syscalls fail with `EPERM` in
//! that case, and that must never be fatal: it's strictly better to run at
//! normal priority than to refuse to start because the box isn't tuned yet.

use std::sync::Once;

/// Priorities for the four audio-path worker threads. Chosen in the 70s
/// rather than up against the `LimitRTPRIO=95` ceiling the unit grants —
/// pro-audio convention on Linux is to leave headroom above your own
/// threads for IRQ handling, not crowd right up against the rlimit.
///
/// The send pipeline sits one point above the receive pipeline. Send feeds
/// the network, where a late frame becomes a gap every downstream listener
/// hears; receive feeds a physical DAC through a path that already degrades
/// gracefully (roc's jitter buffer absorbs a late read). If both contend for
/// the CPU at once, losing the scheduling race should hit the side with the
/// softer landing. The gap is a single point — this isn't a real priority
/// band, just a tiebreaker.
pub const PRIO_SEND: i32 = 72;
pub const PRIO_RECV: i32 = 71;

static WARN_ONCE: Once = Once::new();

/// Lock all current and future process memory into RAM so audio buffers
/// never get paged out. Call exactly once, at process startup — `mlockall`
/// is a whole-process setting, not a per-thread one, so calling it again
/// per audio thread would be redundant and would make the "warn once" logic
/// below fire spuriously.
///
/// # Safety-in-spirit
/// This wraps `libc::mlockall`, which is `unsafe` only because it's an FFI
/// call, not because it has any preconditions we need to uphold: it takes
/// no pointers and can't violate Rust's aliasing rules, it just flips a
/// kernel bookkeeping flag for the calling process. Failure is reported via
/// `errno` (typically `EPERM` without `CAP_IPC_LOCK`/`RLIMIT_MEMLOCK`, or
/// `ENOMEM`) and is handled below without touching anything the return
/// value doesn't already tell us.
pub fn lock_memory() {
    let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if rc != 0 {
        warn_once_degraded();
    }
}

/// Raise the *calling* thread to `SCHED_FIFO` at `priority`. Must be called
/// from inside the audio thread itself — Linux scheduling policy is set
/// per-thread, and `pid = 0` in `sched_setscheduler` means "the calling
/// thread", not "the process".
///
/// Degrades to a single process-wide warning (see [`lock_memory`]) rather
/// than a panic, a startup failure, or a message per thread/per call: a
/// deployment missing the rlimit fails this the same way on all four audio
/// threads, and one clear line explaining why is more useful than four
/// repeats of the same fact — or worse, one per audio period.
///
/// # Safety-in-spirit
/// This wraps `libc::sched_setscheduler`, `unsafe` only for being FFI. The
/// `sched_param` is a plain stack value we own and pass by reference for the
/// duration of the call; no pointers escape and nothing aliases. Failure is
/// reported via `errno` (`EPERM` without `RLIMIT_RTPRIO`) and handled below.
pub fn raise_thread_priority(label: &str, priority: i32) {
    let param = libc::sched_param {
        sched_priority: priority,
    };
    let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if rc != 0 {
        warn_once_degraded();
    } else {
        tracing::debug!("{label}: raised to SCHED_FIFO priority {priority}");
    }
}

fn warn_once_degraded() {
    WARN_ONCE.call_once(|| {
        tracing::warn!(
            "could not acquire real-time scheduling / locked memory for audio threads \
             (EPERM — no RTPRIO/MEMLOCK rlimit?); continuing at normal priority. This is \
             expected when not running under packaging/soundnet-engine.service (e.g. `cargo \
             run` as a normal user), but means xruns are more likely under CPU load."
        );
    });
}
