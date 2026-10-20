//! Windows engine for Chimcast.
//!
//! Everything that touches the OS lives here: monitor/window capture through
//! Windows.Graphics.Capture, webcams through Media Foundation, a D3D11
//! compositor, hardware H.264 and AAC encoding through Media Foundation
//! transforms, WASAPI audio capture, and the self-updater. On other platforms the crate is empty.

#![cfg(windows)]

pub mod aac_encoder;
pub mod browser;
pub mod capture;
pub mod compositor;
pub mod d3d;
pub mod devices;
pub mod engine;
pub mod h264_encoder;
pub mod http;
pub mod image;
pub mod mf;
pub mod nv12;
pub mod streamlabs;
pub mod sysinfo;
pub mod twitch_auth;
pub mod update;
pub mod wasapi;
pub mod webcam;

pub use engine::{Engine, EngineStats, Outputs};

pub type Result<T> = windows::core::Result<T>;
