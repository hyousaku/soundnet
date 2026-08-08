//! Preview tone generators — synthetic capture ports that produce a sine wave.
//!
//! Registered as `LocalPort`s with `kind = Tone` and `alsa_name = "tone:<freq>"`.
//! The capture worker sees the `tone:` prefix and short-circuits to the sine
//! generator instead of opening ALSA.

use soundnet_protocol::{LocalPort, PortKind, SampleFormat};
use std::sync::Arc;

use crate::state::EngineState;

pub const TONE_PREFIX: &str = "tone:";

pub fn register_default_tones(state: &Arc<EngineState>) {
    for freq in [440, 1000] {
        let id = format!("tone_{freq}");
        state.local_ports.insert(
            id.clone(),
            LocalPort {
                node_id: state.identity.node_id.clone(),
                id,
                kind: PortKind::Tone,
                alsa_name: format!("{TONE_PREFIX}{freq}"),
                label: format!("Test tone {freq} Hz"),
                max_channels: 2,
                supported_formats: vec![SampleFormat::F32Le, SampleFormat::S16Le, SampleFormat::S24Le3],
                supported_rates: vec![44_100, 48_000, 96_000],
            },
        );
    }
}

/// Fill `out` with `frames` frames of interleaved sine at `freq`, advancing
/// `phase` (radians, 0..2π) in-place. Amplitude is fixed at -12 dBFS.
pub fn generate(freq: f32, rate: u32, channels: u8, frames: usize, phase: &mut f32, out: &mut Vec<f32>) {
    out.clear();
    out.reserve(frames * channels as usize);
    let two_pi = std::f32::consts::TAU;
    let step = two_pi * freq / rate as f32;
    let amp = 0.25_f32; // ~ -12 dBFS
    for _ in 0..frames {
        let s = phase.sin() * amp;
        for _ in 0..channels {
            out.push(s);
        }
        *phase += step;
        if *phase > two_pi {
            *phase -= two_pi;
        }
    }
}
