//! Sample-format conversion helpers.
//!
//! The wire format is always interleaved f32 (roc 0.3 supports only that).
//! ALSA-side buffers can be S16/S24/S32/F32 to match hardware.

use soundnet_protocol::SampleFormat;

pub fn to_alsa_format(fmt: SampleFormat) -> alsa::pcm::Format {
    match fmt {
        SampleFormat::S16Le => alsa::pcm::Format::s16(),
        SampleFormat::S24Le3 => alsa::pcm::Format::S243LE,
        SampleFormat::S32Le => alsa::pcm::Format::s32(),
        SampleFormat::F32Le => alsa::pcm::Format::float(),
    }
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
                let raw = (chunk[0] as u32)
                    | ((chunk[1] as u32) << 8)
                    | ((chunk[2] as u32) << 16);
                let signed = if raw & 0x00_80_00_00 != 0 {
                    (raw | 0xFF_00_00_00) as i32
                } else {
                    raw as i32
                };
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
