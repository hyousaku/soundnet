//! Sample-format conversion helpers.
//!
//! The wire format is always interleaved f32 (roc 0.3 supports only that).
//! ALSA-side buffers can be S16/S24/S32/F32 to match hardware.

use soundnet_protocol::SampleFormat;

pub fn to_alsa_format(fmt: SampleFormat) -> alsa::pcm::Format {
    match fmt {
        SampleFormat::S16Le => alsa::pcm::Format::s16(),
        SampleFormat::S24Le3 => alsa::pcm::Format::S243LE,
        SampleFormat::S24Le => alsa::pcm::Format::S24LE,
        SampleFormat::S32Le => alsa::pcm::Format::s32(),
        SampleFormat::F32Le => alsa::pcm::Format::float(),
    }
}

/// Fallback order to try when a device rejects the format a route asked for.
///
/// Ordered by how much of the signal survives, because a substitution the
/// operator did not ask for should not also be a downgrade they cannot see.
/// The wire is always f32, so every entry here is a lossless carrier of what
/// we have except the last one.
///
/// **S16 is last, and that is the whole point of this ordering.** It used to
/// sit second, which meant a route asking for 24-bit on a device that offers
/// S16_LE and S32_LE — an extremely ordinary pair — was quietly opened at 16
/// bits, throwing away eight bits while a container that would have held all
/// 24 was sitting right there in the list. The log said
/// "requested format S24Le3 unsupported, using S16Le instead", which is true
/// and completely fails to convey that a better answer was available.
///
/// Note what a 24-bit interface usually offers. ALSA has three separate
/// formats for 24-bit audio and hardware rarely offers all of them:
/// `S24_3LE` packs 24 bits into 3 bytes (common on USB with 3-byte
/// subslots), `S32_LE` carries them left-justified in a 32-bit word with the
/// low 8 ignored (common everywhere else, and *not* a sign that the device is
/// only doing 16 or 32 bit work), and `S24_LE` right-justifies them in 4
/// bytes. All three are here now. `S24_LE` used to be missing, which meant a
/// device offering only that one looked 24-bit-incapable: it never appeared
/// in the port's format list, and a 24-bit route on it got substituted down
/// to whatever else it happened to offer.
///
/// This is the same *set* as `devices::CANDIDATE_FORMATS`, which is what the
/// UI lists as a port's supported formats — so substitution can never land on
/// something the UI said the port could not do. Only the order differs:
/// probing wants the likeliest native format first, substituting wants the
/// least destructive one.
const FALLBACK_FORMATS: &[SampleFormat] = &[
    // The likeliest exact match for a 24-bit device, and no loss for anything
    // else in the list.
    SampleFormat::S24Le3,
    // The other 24-bit-in-4-bytes layout. Same resolution as S24_3LE, so it
    // ranks with it and above the 32-bit container only because a device
    // offering it is telling us 24 bits is what it actually has.
    SampleFormat::S24Le,
    // 32-bit container: holds everything f32 can represent.
    SampleFormat::S32Le,
    // Exactly what the wire carries, so no conversion at all — but rarely
    // offered by raw `hw:` devices, hence not first.
    SampleFormat::F32Le,
    // Genuinely lossy. Only when the device will take nothing else.
    SampleFormat::S16Le,
];

/// Pick the ALSA format to actually open a device with. `requested` is what
/// the route's spec asks for; `supported` reports whether a given format is
/// usable (backed by `HwParams::test_format` at the call sites — kept as a
/// closure here so the selection logic is testable without a live ALSA
/// device). The requested format wins if the hardware takes it; otherwise
/// this falls through `FALLBACK_FORMATS` and returns the first alternative
/// that works. The wire format is always f32 (see module docs), so
/// substituting the ALSA-side format is transparent to everything except
/// the conversion routines below — callers must thread the *returned*
/// format into `alsa_to_f32`/`f32_to_alsa` and buffer-size arithmetic, not
/// the originally requested one. Returns `None` only if the device accepts
/// none of the candidates, which the caller should treat as fatal.
pub fn pick_format(
    requested: SampleFormat,
    supported: impl Fn(SampleFormat) -> bool,
) -> Option<SampleFormat> {
    if supported(requested) {
        return Some(requested);
    }
    FALLBACK_FORMATS
        .iter()
        .copied()
        .find(|&fmt| fmt != requested && supported(fmt))
}

/// Convert a raw ALSA byte buffer (little-endian, interleaved) into f32.
pub fn alsa_to_f32(fmt: SampleFormat, bytes: &[u8], out: &mut Vec<f32>) {
    out.clear();
    match fmt {
        SampleFormat::S16Le => {
            out.reserve(bytes.len() / 2);
            for chunk in bytes.chunks_exact(2) {
                let v = i16::from_le_bytes([chunk[0], chunk[1]]);
                out.push(v as f32 / i16::MAX as f32);
            }
        }
        SampleFormat::S24Le3 => {
            out.reserve(bytes.len() / 3);
            for chunk in bytes.chunks_exact(3) {
                // sign-extend 24 → 32
                let raw = (chunk[0] as u32) | ((chunk[1] as u32) << 8) | ((chunk[2] as u32) << 16);
                let signed = if raw & 0x00_80_00_00 != 0 {
                    (raw | 0xFF_00_00_00) as i32
                } else {
                    raw as i32
                };
                out.push(signed as f32 / 8_388_607.0);
            }
        }
        SampleFormat::S24Le => {
            out.reserve(bytes.len() / 4);
            for chunk in bytes.chunks_exact(4) {
                // Right-justified: the value occupies the low 24 bits and the
                // top byte is padding, so it has to be sign-extended from bit
                // 23 rather than read as an i32. Reading it as one would be
                // wrong by a factor of 256 for a device that zero-pads, and
                // would turn every negative sample into a large positive one
                // for a device that sign-extends.
                let raw = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let signed = ((raw << 8) as i32) >> 8;
                out.push(signed as f32 / 8_388_607.0);
            }
        }
        SampleFormat::S32Le => {
            out.reserve(bytes.len() / 4);
            for chunk in bytes.chunks_exact(4) {
                let v = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                out.push(v as f32 / i32::MAX as f32);
            }
        }
        SampleFormat::F32Le => {
            out.reserve(bytes.len() / 4);
            for chunk in bytes.chunks_exact(4) {
                out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
        }
    }
}

/// Convert an f32 buffer into little-endian ALSA bytes of the requested format.
pub fn f32_to_alsa(fmt: SampleFormat, samples: &[f32], out: &mut Vec<u8>) {
    out.clear();
    match fmt {
        SampleFormat::S16Le => {
            out.reserve(samples.len() * 2);
            for &s in samples {
                let clamped = s.clamp(-1.0, 1.0);
                let v = (clamped * i16::MAX as f32) as i16;
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::S24Le3 => {
            out.reserve(samples.len() * 3);
            for &s in samples {
                let clamped = s.clamp(-1.0, 1.0);
                let v = (clamped * 8_388_607.0) as i32;
                out.push((v & 0xFF) as u8);
                out.push(((v >> 8) & 0xFF) as u8);
                out.push(((v >> 16) & 0xFF) as u8);
            }
        }
        SampleFormat::S24Le => {
            out.reserve(samples.len() * 4);
            for &s in samples {
                let clamped = s.clamp(-1.0, 1.0);
                let v = (clamped * 8_388_607.0) as i32;
                // Sign-extend into the top byte rather than zeroing it. ALSA
                // specifies the padding as "don't care", but drivers that do
                // look at it expect a sign-extended word, and writing zeroes
                // there makes every negative sample read as a large positive
                // one on those. Sign-extension is correct for both.
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::S32Le => {
            out.reserve(samples.len() * 4);
            for &s in samples {
                let clamped = s.clamp(-1.0, 1.0);
                let v = (clamped * i32::MAX as f32) as i32;
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::F32Le => {
            out.reserve(samples.len() * 4);
            for &s in samples {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_s16() {
        let src = vec![0.0, 0.5, -0.5, 1.0, -1.0];
        let mut bytes = Vec::new();
        f32_to_alsa(SampleFormat::S16Le, &src, &mut bytes);
        let mut back = Vec::new();
        alsa_to_f32(SampleFormat::S16Le, &bytes, &mut back);
        for (a, b) in src.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1.0 / i16::MAX as f32);
        }
    }

    #[test]
    fn pick_format_falls_back_when_requested_unsupported() {
        // AG06MK2-shaped case from the bug report: route wants S16LE, the
        // raw device only does S24_3LE.
        let chosen = pick_format(SampleFormat::S16Le, |f| f == SampleFormat::S24Le3);
        assert_eq!(chosen, Some(SampleFormat::S24Le3));
    }

    #[test]
    fn pick_format_keeps_requested_when_supported() {
        let chosen = pick_format(SampleFormat::S16Le, |f| {
            matches!(f, SampleFormat::S16Le | SampleFormat::S24Le3)
        });
        assert_eq!(chosen, Some(SampleFormat::S16Le));
    }

    #[test]
    fn pick_format_fails_when_nothing_matches() {
        let chosen = pick_format(SampleFormat::S16Le, |_| false);
        assert_eq!(chosen, None);
    }

    /// The case this ordering exists for. A 24-bit interface that exposes
    /// S16_LE and S32_LE but not S24_3LE is completely ordinary, and the old
    /// order handed it 16 bits.
    #[test]
    fn a_24_bit_request_prefers_a_wider_container_over_16_bit() {
        let chosen = pick_format(SampleFormat::S24Le3, |f| {
            matches!(f, SampleFormat::S16Le | SampleFormat::S32Le)
        });
        assert_eq!(
            chosen,
            Some(SampleFormat::S32Le),
            "asked for 24 bits and a 32-bit container was on offer; dropping to \
             16 discards resolution the device was willing to carry"
        );
    }

    /// The invariant behind the ordering, checked over every combination
    /// rather than the one case above: substituting *down* to 16 bits is only
    /// ever allowed when the device will accept nothing else.
    #[test]
    fn s16_is_only_substituted_when_nothing_else_works() {
        let all = all_formats();
        for &requested in &all {
            for mask in 0..(1u8 << all.len()) {
                let offered: Vec<SampleFormat> = all
                    .iter()
                    .copied()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, f)| f)
                    .collect();
                let chosen = pick_format(requested, |f| offered.contains(&f));
                if chosen == Some(SampleFormat::S16Le) && requested != SampleFormat::S16Le {
                    assert_eq!(
                        offered,
                        vec![SampleFormat::S16Le],
                        "requested {requested:?}, device offered {offered:?}, and S16 was \
                         chosen anyway"
                    );
                }
            }
        }
    }

    /// A format the engine can convert but never substitutes in is a format
    /// that silently does not exist for anyone whose device only offers that
    /// one. `all_formats` fails to compile when a variant is added, so this
    /// catches the omission at the moment it is introduced rather than on the
    /// first machine that needs it.
    #[test]
    fn every_sample_format_is_available_as_a_substitute() {
        let all = all_formats();
        assert_eq!(
            FALLBACK_FORMATS.len(),
            all.len(),
            "FALLBACK_FORMATS has drifted from the set of formats that exist"
        );
        for f in all {
            assert!(
                FALLBACK_FORMATS.contains(&f),
                "{f:?} can be converted but would never be substituted in"
            );
        }
    }

    /// Every `SampleFormat`. The `match` is not decoration: it has no `_` arm,
    /// so adding a variant to the enum stops this compiling until the new one
    /// is listed here too — and then `every_sample_format_is_available_as_a_
    /// substitute` immediately fails until it is wired into the fallback list
    /// as well.
    fn all_formats() -> Vec<SampleFormat> {
        let all = vec![
            SampleFormat::S16Le,
            SampleFormat::S24Le3,
            SampleFormat::S24Le,
            SampleFormat::S32Le,
            SampleFormat::F32Le,
        ];
        for f in &all {
            match f {
                SampleFormat::S16Le
                | SampleFormat::S24Le3
                | SampleFormat::S24Le
                | SampleFormat::S32Le
                | SampleFormat::F32Le => {}
            }
        }
        all
    }

    /// The format this variant exists for. `S24_LE` and `S32_LE` share a
    /// container and differ only in where the 24 bits sit inside it, so the
    /// one thing that must not happen is reading one as the other.
    #[test]
    fn s24_le_is_not_s32_le_with_extra_steps() {
        let src = vec![0.5_f32];
        let mut as_s24 = Vec::new();
        let mut as_s32 = Vec::new();
        f32_to_alsa(SampleFormat::S24Le, &src, &mut as_s24);
        f32_to_alsa(SampleFormat::S32Le, &src, &mut as_s32);
        assert_eq!(as_s24.len(), 4);
        assert_eq!(as_s32.len(), 4);
        assert_ne!(
            as_s24, as_s32,
            "24 bits right-justified and 24 bits left-justified must not \
             produce the same word — mixing them up is a 256x error"
        );

        // Read back the wrong way round to pin down which way the mistake
        // goes, so a future change that "fixes" one by breaking the other
        // gets caught.
        let mut misread = Vec::new();
        alsa_to_f32(SampleFormat::S32Le, &as_s24, &mut misread);
        assert!(
            misread[0].abs() < 0.01,
            "reading right-justified 24-bit as 32-bit should collapse to near \
             silence, got {}",
            misread[0]
        );
    }

    /// Negative samples are where the padding byte matters: a device that
    /// reads the top byte expects it sign-extended, and zeroing it turns
    /// every negative sample into a large positive one.
    #[test]
    fn roundtrip_s24_le_including_negatives() {
        let src = vec![0.0_f32, 0.25, -0.25, 0.75, -0.9, 1.0, -1.0];
        let mut bytes = Vec::new();
        f32_to_alsa(SampleFormat::S24Le, &src, &mut bytes);
        assert_eq!(bytes.len(), src.len() * 4);
        let mut back = Vec::new();
        alsa_to_f32(SampleFormat::S24Le, &bytes, &mut back);
        assert_eq!(back.len(), src.len());
        for (a, b) in src.iter().zip(back.iter()) {
            assert!(
                (a - b).abs() < 1.0 / 8_388_607.0 * 2.0,
                "{a} came back as {b}"
            );
        }

        // The padding really is sign-extended, not zeroed.
        let negative = &bytes[2 * 4..3 * 4];
        assert_eq!(
            negative[3], 0xFF,
            "the pad byte of a negative sample should be sign-extended, got {:#04x}",
            negative[3]
        );
    }

    /// A device that only offers `S24_LE` used to look 24-bit-incapable: the
    /// format was in neither the fallback list nor the probe candidates, so a
    /// 24-bit route on it was substituted down to whatever else it had.
    #[test]
    fn a_device_offering_only_s24_le_is_not_treated_as_16_bit() {
        let chosen = pick_format(SampleFormat::S24Le3, |f| {
            matches!(f, SampleFormat::S16Le | SampleFormat::S24Le)
        });
        assert_eq!(
            chosen,
            Some(SampleFormat::S24Le),
            "asked for 24 bits, the device offered 24 bits in a different \
             layout, and 16 was chosen instead"
        );
    }

    #[test]
    fn roundtrip_s24() {
        let src = vec![0.0_f32, 0.25, -0.25, 0.75, -0.9];
        let mut bytes = Vec::new();
        f32_to_alsa(SampleFormat::S24Le3, &src, &mut bytes);
        assert_eq!(bytes.len(), src.len() * 3);
        let mut back = Vec::new();
        alsa_to_f32(SampleFormat::S24Le3, &bytes, &mut back);
        for (a, b) in src.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1.0 / 8_388_607.0 * 2.0);
        }
    }
}
