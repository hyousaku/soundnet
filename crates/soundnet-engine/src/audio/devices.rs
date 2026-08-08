//! Enumerate ALSA capture/playback devices and stash them in EngineState.
//!
//! Uses `snd_device_name_hint` (via `alsa::device_name::HintIter`) which is
//! ALSA's recommended device discovery entry point — it returns everything the
//! user's `~/.asoundrc` and the system configuration knows about
//! (`hw:*,*`, `plughw:*,*`, `default`, PipeWire, etc.).

use anyhow::Result;
use soundnet_protocol::{LocalPort, PortKind, SampleFormat};
use std::sync::Arc;

use crate::state::EngineState;

const CANDIDATE_FORMATS: &[(alsa::pcm::Format, SampleFormat)] = &[
    (alsa::pcm::Format::S243LE, SampleFormat::S24Le3),
    (alsa::pcm::Format::s16(), SampleFormat::S16Le),
    (alsa::pcm::Format::s32(), SampleFormat::S32Le),
    (alsa::pcm::Format::float(), SampleFormat::F32Le),
];

const CANDIDATE_RATES: &[u32] = &[44_100, 48_000, 88_200, 96_000];

pub fn refresh(state: &Arc<EngineState>) -> Result<()> {
    // Drop non-virtual entries so re-scans stay clean.
    state
        .local_ports
        .retain(|_, port| matches!(port.kind, PortKind::Tone));

    let hints = alsa::device_name::HintIter::new_str(None, "pcm")?;
    for hint in hints {
        let Some(name) = hint.name else { continue };
        if name == "null" {
            continue;
        }
        // Skip most `plughw:` duplicates — expose the plain hw entry only.
        // We keep `default` since it's the friendly per-user route.
        if name.starts_with("plughw:") || name.starts_with("sysdefault") {
            continue;
        }

        let desc = hint
            .desc
            .as_ref()
            .and_then(|d| d.lines().next().map(|s| s.to_string()))
            .unwrap_or_else(|| name.clone());

        match hint.direction {
            Some(alsa::Direction::Capture) => add_port(state, &name, &desc, PortKind::Capture),
            Some(alsa::Direction::Playback) => add_port(state, &name, &desc, PortKind::Playback),
            None => {
                // Duplex — expose both.
                add_port(state, &name, &desc, PortKind::Capture);
                add_port(state, &name, &desc, PortKind::Playback);
            }
        }
    }

    tracing::info!("enumerated {} local ports", state.local_ports.len());
    Ok(())
}

fn add_port(state: &Arc<EngineState>, alsa_name: &str, desc: &str, kind: PortKind) {
    let dir = match kind {
        PortKind::Capture => alsa::Direction::Capture,
        PortKind::Playback => alsa::Direction::Playback,
        PortKind::Tone => return,
    };

    let (max_channels, formats, rates) = probe(alsa_name, dir);
    let id = format!(
        "{}_{}",
        alsa_name.replace([':', ',', '/', ' '], "_"),
        if matches!(kind, PortKind::Capture) { "in" } else { "out" }
    );
    let label = format!(
        "{desc} ({})",
        if matches!(kind, PortKind::Capture) { "in" } else { "out" }
    );
    state.local_ports.insert(
        id.clone(),
        LocalPort {
            node_id: state.identity.node_id.clone(),
            id,
            kind,
            alsa_name: alsa_name.to_string(),
            label,
            max_channels,
            supported_formats: formats,
            supported_rates: rates,
        },
    );
}

fn probe(alsa_name: &str, dir: alsa::Direction) -> (u8, Vec<SampleFormat>, Vec<u32>) {
    let pcm = match alsa::PCM::new(alsa_name, dir, true) {
        Ok(pcm) => pcm,
        Err(err) => {
            tracing::debug!("probe open {alsa_name}: {err}");
            return (2, vec![SampleFormat::S16Le], vec![48_000]);
        }
    };
    let hwp = match alsa::pcm::HwParams::any(&pcm) {
        Ok(h) => h,
        Err(_) => return (2, vec![SampleFormat::S16Le], vec![48_000]),
    };

    let max_channels = hwp.get_channels_max().unwrap_or(2).min(32) as u8;

    let mut formats = Vec::new();
    for (afmt, sfmt) in CANDIDATE_FORMATS {
        if hwp.test_format(*afmt).is_ok() {
            formats.push(*sfmt);
        }
    }
    if formats.is_empty() {
        formats.push(SampleFormat::S16Le);
    }

    let mut rates = Vec::new();
    for &rate in CANDIDATE_RATES {
        if hwp.test_rate(rate).is_ok() {
            rates.push(rate);
        }
    }
    if rates.is_empty() {
        rates.push(48_000);
    }

    (max_channels, formats, rates)
}
