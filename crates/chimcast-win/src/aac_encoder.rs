//! AAC-LC encoding with the Media Foundation AAC encoder (ships with Windows).

use chimcast_media::{audio, Packet, Track};
use windows::Win32::Media::MediaFoundation::*;

use crate::mf::{self, Transform};
use crate::Result;

pub struct AacEncoder {
    transform: Transform,
}

unsafe impl Send for AacEncoder {}

impl AacEncoder {
    /// `bitrate_kbps` must be one of 96, 128, 160, 192.
    pub fn new(bitrate_kbps: u32) -> Result<Self> {
        let rate = audio::SAMPLE_RATE;
        let channels = audio::CHANNELS as u32;
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_PCM,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_AAC,
        };
        let (activate, name) = mf::find_transforms(
            MFT_CATEGORY_AUDIO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            input,
            output,
        )?
        .into_iter()
        .next()
        .ok_or_else(|| {
            windows::core::Error::new(MF_E_TOPO_CODEC_NOT_FOUND, "no AAC encoder found")
        })?;
        let mut transform = Transform::activate(activate, name)?;
        unsafe {
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bitrate_kbps * 1000 / 8)?;
            out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw AAC, no ADTS
            out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
            transform.set_output_type(&out)?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
            inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2)?;
            inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * channels * 2)?;
            transform.set_input_type(&inp)?;
        }
        transform.begin()?;
        Ok(Self { transform })
    }

    /// Encodes interleaved 16-bit stereo PCM starting at `ts_us`.
    pub fn encode(&mut self, pcm: &[u8], ts_us: i64, out: &mut Vec<Packet>) -> Result<()> {
        let frames = pcm.len() as i64 / (audio::CHANNELS as i64 * 2);
        let duration = frames * 10_000_000 / audio::SAMPLE_RATE as i64;
        let sample = mf::memory_sample(pcm, ts_us * 10, duration)?;
        self.transform.process_input(&sample)?;
        while let Some(s) = self.transform.process_output()? {
            let data = mf::sample_bytes(&s)?;
            if data.is_empty() {
                continue;
            }
            let ts = unsafe { s.GetSampleTime()? } / 10;
            out.push(Packet {
                track: Track::Audio,
                data,
                pts_us: ts,
                dts_us: ts,
                keyframe: true,
            });
        }
        Ok(())
    }
}
