//! Enumerate ALSA capture/playback devices and stash them in EngineState.
//!
//! We walk the sound cards directly with `alsa::Card::iter` and, for each
//! (card, device, direction), synthesize two ports:
//!
//! * a `plughw:C,D` entry — the friendly, always-openable one that
//!   auto-converts rate / format / channel count. Default choice for the UI.
//! * a `hw:C,D` entry — the raw device, for advanced users who want the
//!   lowest possible latency and know their sample-rate / format matches
//!   the hardware exactly.
//!
//! We deliberately skip the `default:` / `sysdefault:` / `dmix:` / `dsnoop:`
//! / `surround*:` / `front:` / `iec958:` aliases that ALSA's hint API would
//! otherwise return: most of them either duplicate the hardware or require
//! very specific channel/format combinations that our per-route spec
//! doesn't match — leaving them on the list just gave the user a bunch of
//! broken destinations.

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

    for card in alsa::card::Iter::new().flatten() {
        let card_idx = card.get_index();
        let card_name = card
            .get_name()
            .unwrap_or_else(|_| format!("card{card_idx}"));

        let ctl = match alsa::Ctl::from_card(&card, false) {
            Ok(c) => c,
            Err(err) => {
                tracing::debug!("card {card_idx} ({card_name}) ctl open: {err}");
                continue;
            }
        };

        for device_idx in alsa::ctl::DeviceIter::new(&ctl) {
            for dir in [alsa::Direction::Capture, alsa::Direction::Playback] {
                let info = match ctl.pcm_info(device_idx as u32, 0, dir) {
                    Ok(i) => i,
                    Err(_) => continue, // this direction not supported on this device
                };
                let dev_name = info.get_name().unwrap_or("").to_string();
                let kind = match dir {
                    alsa::Direction::Capture => PortKind::Capture,
                    alsa::Direction::Playback => PortKind::Playback,
                };

                // Preferred: plughw — libasound converts rate/format/channels
                // on the fly, so almost any route spec will Just Work.
                add_port(
                    state,
                    &format!("plughw:{card_idx},{device_idx}"),
                    &format!("{card_name} — {dev_name}"),
                    kind,
                    /* raw */ false,
                );

                // Also expose the raw device for advanced/low-latency use.
                add_port(
                    state,
                    &format!("hw:{card_idx},{device_idx}"),
                    &format!("{card_name} — {dev_name}"),
                    kind,
                    /* raw */ true,
                );
            }
        }
    }

    tracing::info!("enumerated {} local ports", state.local_ports.len());
    Ok(())
}

fn add_port(
    state: &Arc<EngineState>,
    alsa_name: &str,
    hardware_label: &str,
    kind: PortKind,
    raw: bool,
) {
    let dir = match kind {
        PortKind::Capture => alsa::Direction::Capture,
        PortKind::Playback => alsa::Direction::Playback,
        PortKind::Tone => return,
    };

    let probed = probe(alsa_name, dir);
    let id = alsa_name.replace([':', ',', '/', ' '], "_")
        + match kind {
            PortKind::Capture => "_in",
            PortKind::Playback => "_out",
            PortKind::Tone => "",
        };
    let dir_label = match kind {
        PortKind::Capture => "in",
        PortKind::Playback => "out",
        PortKind::Tone => "",
    };
    let mode_label = if raw { " · low-latency" } else { "" };
    let label = format!("{hardware_label} ({dir_label}{mode_label})");

    state.local_ports.insert(
        id.clone(),
        LocalPort {
            node_id: state.identity.node_id.clone(),
            id,
            kind,
            alsa_name: alsa_name.to_string(),
            label,
            max_channels: probed.max_channels,
            supported_formats: probed.formats,
            supported_rates: probed.rates,
            probe_failed: probed.failed,
        },
    );
}

struct Probe {
    max_channels: u8,
    formats: Vec<SampleFormat>,
    rates: Vec<u32>,
    failed: bool,
}

/// What we have to say when the device won't open. Marked `failed` so the UI
/// can show it as unknown: a device that is merely busy would otherwise be
/// reported as a plain stereo 48 kHz interface, which looks entirely normal
/// and hides however many channels it really has.
fn unprobed() -> Probe {
    Probe {
        max_channels: 2,
        formats: vec![SampleFormat::S16Le],
        rates: vec![48_000],
        failed: true,
    }
}

fn probe(alsa_name: &str, dir: alsa::Direction) -> Probe {
    let pcm = match alsa::PCM::new(alsa_name, dir, true) {
        Ok(pcm) => pcm,
        Err(err) => {
            // Warn, not debug. This is the difference between "my 8-channel
            // interface shows up as stereo" being a five-minute fix and an
            // afternoon.
            tracing::warn!(
                "cannot probe {alsa_name} ({err}); reporting placeholder \
                 capabilities. If something else holds the card (PipeWire on \
                 a desktop), release it and rescan."
            );
            return unprobed();
        }
    };
    let hwp = match alsa::pcm::HwParams::any(&pcm) {
        Ok(h) => h,
        Err(err) => {
            tracing::warn!("cannot read hw params for {alsa_name}: {err}");
            return unprobed();
        }
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

    Probe { max_channels, formats, rates, failed: false }
}
