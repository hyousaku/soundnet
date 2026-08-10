//! The two audio pipelines, one per direction of a route.
//!
//! Each is a single thread that owns both its sound device and its roc
//! endpoint, so a route this engine is the source of costs one thread, and a
//! route it is the destination of costs one more. (It used to be two each,
//! with a ring buffer in between; see the module docs on `send.rs` for why
//! that ring had to go.)

pub mod recv;
pub mod send;

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
