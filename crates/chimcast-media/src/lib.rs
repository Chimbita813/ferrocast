//! Platform-independent media plumbing.
//!
//! The Windows engine produces encoded H.264 (Annex B) and raw AAC packets;
//! this crate turns them into MP4 files and RTMP streams, and mixes the PCM
//! audio that feeds the AAC encoder. Nothing here touches the OS beyond
//! files and TCP sockets, so it is tested on any platform.

pub mod aac;
pub mod amf0;
pub mod audio;
pub mod flv;
pub mod h264;
pub mod mp4;
pub mod output;
pub mod packet;
pub mod rtmp;

pub use packet::{AudioInfo, Packet, StreamInfo, Track, VideoInfo};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid bitstream: {0}")]
    Bitstream(&'static str),
    #[error("rtmp: {0}")]
    Rtmp(String),
}

pub type Result<T> = std::result::Result<T, Error>;
