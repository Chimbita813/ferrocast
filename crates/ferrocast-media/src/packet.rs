/// Which elementary stream a packet belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    Video,
    Audio,
}

/// One encoded access unit.
///
/// Video data is H.264 in Annex B form (start codes); audio data is one raw
/// AAC-LC frame without an ADTS header. Timestamps are microseconds on the
/// engine clock and may start anywhere; muxers rebase them.
#[derive(Clone, Debug)]
pub struct Packet {
    pub track: Track,
    pub data: Vec<u8>,
    pub pts_us: i64,
    pub dts_us: i64,
    pub keyframe: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bitrate_kbps: u32,
}

/// What an output is going to receive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub video: VideoInfo,
    pub audio: Option<AudioInfo>,
}
