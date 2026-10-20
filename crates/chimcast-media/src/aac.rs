//! AAC helpers: AudioSpecificConfig and ADTS parsing.

use crate::{Error, Result};

const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Samples per AAC-LC frame.
pub const FRAME_SAMPLES: u32 = 1024;

fn rate_index(sample_rate: u32) -> u8 {
    SAMPLE_RATES
        .iter()
        .position(|&r| r == sample_rate)
        .unwrap_or(3) as u8
}

/// Two-byte AudioSpecificConfig for AAC-LC (ISO 14496-3 1.6.2.1).
pub fn audio_specific_config(sample_rate: u32, channels: u16) -> [u8; 2] {
    let v: u16 = (2 << 11) | ((rate_index(sample_rate) as u16) << 7) | ((channels & 0xf) << 3);
    v.to_be_bytes()
}

/// One frame from an ADTS stream.
pub struct AdtsFrame<'a> {
    pub sample_rate: u32,
    pub channels: u16,
    pub payload: &'a [u8],
}

/// Splits an ADTS stream into raw AAC frames.
pub fn parse_adts(mut data: &[u8]) -> Result<Vec<AdtsFrame<'_>>> {
    let mut frames = Vec::new();
    while data.len() >= 7 {
        if data[0] != 0xff || data[1] & 0xf0 != 0xf0 {
            return Err(Error::Bitstream("lost ADTS sync"));
        }
        let protection_absent = data[1] & 1 == 1;
        let rate = SAMPLE_RATES
            .get(((data[2] >> 2) & 0xf) as usize)
            .copied()
            .ok_or(Error::Bitstream("bad ADTS sample rate"))?;
        let channels = (((data[2] & 1) << 2) | (data[3] >> 6)) as u16;
        let len = (((data[3] & 3) as usize) << 11)
            | ((data[4] as usize) << 3)
            | ((data[5] >> 5) as usize);
        let header = if protection_absent { 7 } else { 9 };
        if len < header || len > data.len() {
            return Err(Error::Bitstream("truncated ADTS frame"));
        }
        frames.push(AdtsFrame {
            sample_rate: rate,
            channels,
            payload: &data[header..len],
        });
        data = &data[len..];
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_48k_stereo() {
        assert_eq!(audio_specific_config(48000, 2), [0x11, 0x90]);
        assert_eq!(audio_specific_config(44100, 2), [0x12, 0x10]);
    }

    #[test]
    fn parses_fixture() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/audio_48k_stereo.aac"
        ))
        .unwrap();
        let frames = parse_adts(&data).unwrap();
        // 2 s at 48 kHz is ~94 frames (plus encoder priming).
        assert!((90..100).contains(&frames.len()), "{} frames", frames.len());
        assert!(frames
            .iter()
            .all(|f| f.sample_rate == 48000 && f.channels == 2));
    }
}
