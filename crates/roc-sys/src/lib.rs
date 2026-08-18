//! FFI bindings for libroc **0.4.x**, generated at build time by bindgen from
//! the headers actually installed on the machine doing the building.
//!
//! They used to be written out by hand. That worked, and the layout happened
//! to be right — but "happened to be" was the whole problem: a struct
//! transcribed by eye compiles just as cleanly when it is wrong, and a single
//! field added upstream would have shifted every field after it while the
//! build stayed green and the reads went to the wrong offsets at runtime.
//! Generating from the headers moves that from a thing to be careful about to
//! a thing that cannot happen. bindgen also emits `const _` layout assertions
//! for every struct, so a disagreement about size, alignment or field offset
//! is a compile error rather than a bug report about audio.
//!
//! See `build.rs` for which libroc gets bound, how the same decision drives
//! the linker, and why three enums are deliberately generated as plain
//! constants rather than Rust enums.
//!
//! ### What this does not check
//!
//! bindgen guarantees agreement with the headers *at build time*. It says
//! nothing about the shared library that gets loaded at run time, which on
//! these machines is a real distinction and not a theoretical one — see
//! [`check_runtime_version`].
//!
//! ### Notes on the 0.3 → 0.4 jump
//!
//! Kept because they are the record of what actually went wrong, and because
//! anyone who meets a 0.3 in the wild will need them again:
//!
//! * `roc_clock_source` enum values shifted (0.4 added `DEFAULT = 0`).
//! * `roc_clock_sync_backend` / `roc_clock_sync_profile` were renamed to
//!   `roc_latency_tuner_backend` / `roc_latency_tuner_profile`.
//! * `roc_sender_config` gained `latency_tuner_backend`,
//!   `latency_tuner_profile`, `target_latency`, `latency_tolerance` at the end.
//! * `roc_receiver_config` fields renamed (same layout).
//! * `metrics.h` was rewritten: no more `roc_session_metrics {niq_latency,
//!   e2e_latency}` / receiver-metrics-with-sessions-pointer. Now
//!   `roc_connection_metrics {e2e_latency}` (only) is an out-array, and
//!   `roc_receiver_metrics {connection_count}` /
//!   `roc_sender_metrics {connection_count}` are the slot-level structs. The
//!   dropped `niq_latency` is why SoundNet no longer reports a jitter figure
//!   at all — the `StreamStats::jitter_ms` field that used to carry it was
//!   removed rather than left showing a permanent 0.00 ms.
//! * `roc_receiver_query` / `roc_sender_query` grew a fourth and fifth
//!   argument (slot metrics, connection metrics array, count).
//!
//! ### Lifetimes the C API does not express
//!
//! Every `*const c_char` in `roc_log_message` is valid only for the duration
//! of the log handler call — libroc reuses the buffers afterwards, so anything
//! to be kept must be copied out before returning.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::useless_transmute)]
#![allow(dead_code)]

use core::ffi::c_uint;

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// The libroc ABI these bindings describe. 0.3 is not merely older — several
/// `roc_sender_config` / `roc_receiver_config` fields were renamed and
/// reordered between the two, so a 0.3 library reads our structs at the wrong
/// offsets. It does not crash; it rejects the call with a message about a
/// field name that does not exist in this crate
/// ("invalid roc_receiver_config.clock_sync_profile"), which is a genuinely
/// baffling thing to be told, and then every route fails to start with no
/// hint that the library is the problem.
pub const EXPECTED_MAJOR: c_uint = 0;
pub const EXPECTED_MINOR: c_uint = 4;

/// Ask the *loaded* library what it is, and complain if it is not what these
/// bindings were built against.
///
/// Generating the bindings from headers does not make this redundant, and the
/// distinction is the one this project actually got caught by: the header in
/// /usr/local can say 0.4 — so bindgen faithfully describes 0.4 — while the
/// dynamic linker resolves `libroc.so.0.3` from /usr/lib. Everything about
/// that build is internally consistent. It is consistent with the wrong
/// library.
pub fn check_runtime_version() -> Result<(u32, u32, u32), String> {
    let mut v = roc_version::default();
    unsafe { roc_version_load(&mut v) };
    if v.major == EXPECTED_MAJOR && v.minor == EXPECTED_MINOR {
        Ok((v.major, v.minor, v.patch))
    } else {
        Err(format!(
            "libroc {}.{}.{} is loaded, but this build requires {}.{}.x — the two \
             are not ABI compatible (config struct fields were renamed and \
             reordered), so every route would fail to start with a confusing \
             error about a field that does not exist in this version",
            v.major, v.minor, v.patch, EXPECTED_MAJOR, EXPECTED_MINOR
        ))
    }
}
