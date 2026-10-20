//! FLV tag packaging, shared by the RTMP publisher and the `.flv` file writer.

use std::io::Write;

use crate::amf0::{self, Value};
use crate::h264::{self, AvcConfig};
use crate::{aac, Packet, Result, StreamInfo, Track};

pub const TAG_AUDIO: u8 = 8;
pub const TAG_VIDEO: u8 = 9;
pub const TAG_SCRIPT: u8 = 18;

/// AAC, 44 kHz flag, 16-bit, stereo: the FLV spec says AAC always uses 0xAF.
const AAC_SOUND_FLAGS: u8 = 0xAF;

/// An FLV tag body; RTMP sends the same bytes as a message payload.
#[derive(Clone, Debug, PartialEq)]
pub struct Tag {
    pub kind: u8,
    pub timestamp_ms: u32,
    pub data: Vec<u8>,
}

/// `onMetaData` properties describing the stream.
pub fn metadata(info: &StreamInfo) -> Vec<(String, Value)> {
    let mut props = vec![
        ("duration".to_string(), Value::Number(0.0)),
        ("width".to_string(), Value::Number(info.video.width as f64)),
        (
            "height".to_string(),
            Value::Number(info.video.height as f64),
        ),
        (
            "framerate".to_string(),
            Value::Number(info.video.fps as f64),
        ),
        ("videocodecid".to_string(), Value::Number(7.0)),
        (
            "videodatarate".to_string(),
            Value::Number(info.video.bitrate_kbps as f64),
        ),
    ];
    if let Some(a) = info.audio {
        props.extend([
            ("audiocodecid".to_string(), Value::Number(10.0)),
            (
                "audiodatarate".to_string(),
                Value::Number(a.bitrate_kbps as f64),
            ),
            (
                "audiosamplerate".to_string(),
                Value::Number(a.sample_rate as f64),
            ),
            ("audiosamplesize".to_string(), Value::Number(16.0)),
            ("stereo".to_string(), Value::Boolean(a.channels == 2)),
        ]);
    }
    props.push((
        "encoder".to_string(),
        Value::str(concat!("Chimcast ", env!("CARGO_PKG_VERSION"))),
    ));
    props
}

/// Turns encoder packets into FLV tags.
///
/// Output starts at the first video keyframe carrying SPS/PPS; everything
/// before it is dropped. Timestamps are rebased so that keyframe is at 0 ms.
pub struct Packetizer {
    info: StreamInfo,
    base_us: Option<i64>,
}

impl Packetizer {
    pub fn new(info: StreamInfo) -> Self {
        Self {
            info,
            base_us: None,
        }
    }

    pub fn started(&self) -> bool {
        self.base_us.is_some()
    }

    fn ts(&self, us: i64) -> u32 {
        ((us - self.base_us.unwrap_or(us)).max(0) / 1000) as u32
    }

    pub fn push(&mut self, p: &Packet) -> Vec<Tag> {
        let mut tags = Vec::new();
        if self.base_us.is_none() {
            if p.track != Track::Video || !p.keyframe {
                return tags;
            }
            let Some(cfg) = AvcConfig::from_annexb(&p.data) else {
                log::warn!("keyframe without SPS/PPS; waiting for the next one");
                return tags;
            };
            self.base_us = Some(p.dts_us);
            let mut seq = vec![0x17, 0, 0, 0, 0];
            seq.extend_from_slice(&cfg.record());
            tags.push(Tag {
                kind: TAG_VIDEO,
                timestamp_ms: 0,
                data: seq,
            });
            if let Some(a) = self.info.audio {
                let mut seq = vec![AAC_SOUND_FLAGS, 0];
                seq.extend_from_slice(&aac::audio_specific_config(a.sample_rate, a.channels));
                tags.push(Tag {
                    kind: TAG_AUDIO,
                    timestamp_ms: 0,
                    data: seq,
                });
            }
        }
        match p.track {
            Track::Video => {
                let cts = ((p.pts_us - p.dts_us).max(0) / 1000) as u32;
                let mut data = Vec::with_capacity(p.data.len() + 8);
                data.push(if p.keyframe { 0x17 } else { 0x27 });
                data.push(1);
                data.extend_from_slice(&cts.to_be_bytes()[1..]);
                data.extend_from_slice(&h264::annexb_to_avcc(&p.data));
                tags.push(Tag {
                    kind: TAG_VIDEO,
                    timestamp_ms: self.ts(p.dts_us),
                    data,
                });
            }
            Track::Audio => {
                if self.info.audio.is_none() || p.dts_us < self.base_us.unwrap_or(0) {
                    return tags;
                }
                let mut data = Vec::with_capacity(p.data.len() + 2);
                data.extend_from_slice(&[AAC_SOUND_FLAGS, 1]);
                data.extend_from_slice(&p.data);
                tags.push(Tag {
                    kind: TAG_AUDIO,
                    timestamp_ms: self.ts(p.dts_us),
                    data,
                });
            }
        }
        tags
    }
}

/// `@setDataFrame onMetaData` body for RTMP, or `onMetaData` for files.
pub fn script_tag(info: &StreamInfo, set_data_frame: bool) -> Tag {
    let mut values = Vec::new();
    if set_data_frame {
        values.push(Value::str("@setDataFrame"));
    }
    values.push(Value::str("onMetaData"));
    values.push(Value::EcmaArray(metadata(info)));
    Tag {
        kind: TAG_SCRIPT,
        timestamp_ms: 0,
        data: amf0::encode(&values),
    }
}

/// Writes a `.flv` file.
pub struct FlvWriter<W: Write> {
    out: W,
    packetizer: Packetizer,
}

impl<W: Write> FlvWriter<W> {
    pub fn new(mut out: W, info: StreamInfo) -> Result<Self> {
        let flags = if info.audio.is_some() { 0x05 } else { 0x01 };
        out.write_all(&[b'F', b'L', b'V', 1, flags, 0, 0, 0, 9, 0, 0, 0, 0])?;
        let mut w = Self {
            out,
            packetizer: Packetizer::new(info),
        };
        w.write_tag(&script_tag(&info, false))?;
        Ok(w)
    }

    fn write_tag(&mut self, tag: &Tag) -> Result<()> {
        let len = tag.data.len() as u32;
        let ts = tag.timestamp_ms;
        let mut header = [0u8; 11];
        header[0] = tag.kind;
        header[1..4].copy_from_slice(&len.to_be_bytes()[1..]);
        header[4..7].copy_from_slice(&ts.to_be_bytes()[1..]);
        header[7] = (ts >> 24) as u8;
        self.out.write_all(&header)?;
        self.out.write_all(&tag.data)?;
        self.out.write_all(&(len + 11).to_be_bytes())?;
        Ok(())
    }

    pub fn write(&mut self, p: &Packet) -> Result<()> {
        for tag in self.packetizer.push(p) {
            self.write_tag(&tag)?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<W> {
        self.out.flush()?;
        Ok(self.out)
    }
}
