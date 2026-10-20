//! H.264 encoding with Media Foundation.
//!
//! Prefers the GPU vendor's hardware encoder (NVENC, AMF or Quick Sync, all
//! exposed as async MFTs) fed directly with NV12 textures. Falls back to the
//! Microsoft software encoder, fed with frames read back to system memory.

use std::sync::mpsc::{Receiver, Sender};

use chimcast_media::{h264, Packet, Track};
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;

use crate::d3d::Device;
use crate::mf::{self, Transform};
use crate::nv12::Nv12Converter;
use crate::Result;

#[derive(Clone, Copy, Debug)]
pub struct VideoEncoderSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub keyframe_interval_secs: u32,
}

pub enum EncoderMsg {
    /// An NV12 frame; timestamp in microseconds on the engine clock.
    Frame(ID3D11Texture2D, i64),
    ForceKeyframe,
}

unsafe impl Send for EncoderMsg {}

pub struct H264Encoder {
    transform: Transform,
    codec: Option<ICodecAPI>,
    gpu_input: bool,
    settings: VideoEncoderSettings,
    /// Annex B SPS/PPS from the output type, for encoders that don't repeat them in-band.
    sequence_header: Vec<u8>,
    readback: Option<Nv12Converter>,
    _manager: Option<IMFDXGIDeviceManager>,
}

unsafe impl Send for H264Encoder {}

fn video_type(subtype: &GUID, s: &VideoEncoderSettings) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        let a: IMFAttributes = t.cast()?;
        mf::set_pair(&a, &MF_MT_FRAME_SIZE, s.width, s.height)?;
        mf::set_pair(&a, &MF_MT_FRAME_RATE, s.fps, 1)?;
        mf::set_pair(&a, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        Ok(t)
    }
}

impl H264Encoder {
    /// Lists available H.264 encoders, hardware first.
    pub fn available() -> Vec<String> {
        let mut names: Vec<String> = Self::candidates(true).into_iter().map(|(_, n)| n).collect();
        names.extend(Self::candidates(false).into_iter().map(|(_, n)| n));
        names
    }

    fn candidates(hardware: bool) -> Vec<(IMFActivate, String)> {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let flags = if hardware {
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER
        } else {
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
        };
        mf::find_transforms(MFT_CATEGORY_VIDEO_ENCODER, flags, input, output).unwrap_or_default()
    }

    /// Creates the best encoder that accepts these settings.
    pub fn new(device: &Device, settings: VideoEncoderSettings) -> Result<Self> {
        let mut last_err = None;
        for (activate, name) in Self::candidates(true)
            .into_iter()
            .chain(Self::candidates(false))
        {
            match Self::open(device, settings, activate, name.clone()) {
                Ok(e) => {
                    log::info!("video encoder: {name} (gpu input: {})", e.gpu_input);
                    return Ok(e);
                }
                Err(e) => {
                    log::warn!("encoder {name} unusable: {e}");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            windows::core::Error::new(MF_E_TOPO_CODEC_NOT_FOUND, "no H.264 encoder found")
        }))
    }

    fn open(
        device: &Device,
        s: VideoEncoderSettings,
        activate: IMFActivate,
        name: String,
    ) -> Result<Self> {
        let mut transform = Transform::activate(activate, name)?;
        let attrs = transform.attributes();
        let d3d_aware = attrs
            .as_ref()
            .is_some_and(|a| unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 1);
        let mut manager = None;
        if d3d_aware {
            unsafe {
                let mut token = 0u32;
                let mut m = None;
                MFCreateDXGIDeviceManager(&mut token, &mut m)?;
                let m = m.unwrap();
                m.ResetDevice(&device.device, token)?;
                transform.message(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize)?;
                manager = Some(m);
            }
        }
        if let Some(a) = &attrs {
            unsafe {
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
        }

        let codec: Option<ICodecAPI> = transform.mft.cast().ok();
        if let Some(c) = &codec {
            let set = |api: &GUID, v: windows::Win32::System::Variant::VARIANT| unsafe {
                if let Err(e) = c.SetValue(api, &v) {
                    log::debug!("codec api {api:?}: {e}");
                }
            };
            set(
                &CODECAPI_AVEncCommonRateControlMode,
                mf::variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            set(
                &CODECAPI_AVEncCommonMeanBitRate,
                mf::variant_u32(s.bitrate_kbps * 1000),
            );
            set(
                &CODECAPI_AVEncMPVGOPSize,
                mf::variant_u32(s.fps * s.keyframe_interval_secs.max(1)),
            );
            set(&CODECAPI_AVEncMPVDefaultBPictureCount, mf::variant_u32(0));
            set(&CODECAPI_AVLowLatencyMode, mf::variant_bool(true));
        }

        let out = video_type(&MFVideoFormat_H264, &s)?;
        unsafe {
            out.SetUINT32(&MF_MT_AVG_BITRATE, s.bitrate_kbps * 1000)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        }
        transform.set_output_type(&out)?;
        transform.set_input_type(&video_type(&MFVideoFormat_NV12, &s)?)?;
        transform.begin()?;

        let sequence_header = transform
            .output_type()
            .ok()
            .and_then(|t| unsafe {
                let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
                let mut buf = vec![0u8; size as usize];
                t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None)
                    .ok()?;
                Some(buf)
            })
            .unwrap_or_default();

        let readback = if d3d_aware {
            None
        } else {
            Some(Nv12Converter::new(device, s.width, s.height, s.fps)?)
        };
        Ok(Self {
            transform,
            codec,
            gpu_input: d3d_aware,
            settings: s,
            sequence_header,
            readback,
            _manager: manager,
        })
    }

    pub fn name(&self) -> &str {
        &self.transform.name
    }

    fn input_sample(&mut self, tex: &ID3D11Texture2D, ts_us: i64) -> Result<IMFSample> {
        let time = ts_us * 10;
        let duration = 10_000_000 / self.settings.fps as i64;
        if self.gpu_input {
            unsafe {
                let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
                let len = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
                buffer.SetCurrentLength(len)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(time)?;
                sample.SetSampleDuration(duration)?;
                Ok(sample)
            }
        } else {
            let bytes = self.readback.as_mut().unwrap().read_back(tex)?;
            mf::memory_sample(&bytes, time, duration)
        }
    }

    fn to_packet(&self, sample: &IMFSample) -> Result<Packet> {
        let mut data = mf::sample_bytes(sample)?;
        let pts_us = unsafe { sample.GetSampleTime()? } / 10;
        let dts_us = unsafe { sample.GetUINT64(&MFSampleExtension_DecodeTimestamp) }
            .map(|d| d as i64 / 10)
            .unwrap_or(pts_us);
        let keyframe = h264::is_keyframe(&data);
        if keyframe
            && h264::AvcConfig::from_annexb(&data).is_none()
            && !self.sequence_header.is_empty()
        {
            let mut with_header = self.sequence_header.clone();
            with_header.extend_from_slice(&data);
            data = with_header;
        }
        Ok(Packet {
            track: Track::Video,
            data,
            pts_us,
            dts_us,
            keyframe,
        })
    }

    fn force_keyframe(&self) {
        if let Some(c) = &self.codec {
            unsafe {
                let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &mf::variant_u32(1));
            }
        }
    }

    fn drain_sync(&mut self, packets: &Sender<Packet>) -> Result<()> {
        while let Some(sample) = self.transform.process_output()? {
            let _ = packets.send(self.to_packet(&sample)?);
        }
        Ok(())
    }

    /// Runs until `frames` disconnects, then drains. Call on a dedicated thread.
    pub fn run(mut self, frames: Receiver<EncoderMsg>, packets: Sender<Packet>) -> Result<()> {
        let next_frame = |this: &mut Self| -> Option<(ID3D11Texture2D, i64)> {
            loop {
                match frames.recv().ok()? {
                    EncoderMsg::Frame(tex, ts) => return Some((tex, ts)),
                    EncoderMsg::ForceKeyframe => this.force_keyframe(),
                }
            }
        };
        if !self.transform.is_async() {
            while let Some((tex, ts)) = next_frame(&mut self) {
                let sample = self.input_sample(&tex, ts)?;
                self.transform.process_input(&sample)?;
                self.drain_sync(&packets)?;
            }
            self.transform
                .message(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.transform.message(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
            return self.drain_sync(&packets);
        }

        let mut draining = false;
        loop {
            let event = self.transform.next_event()?;
            if event == METransformNeedInput {
                if draining {
                    continue;
                }
                match next_frame(&mut self) {
                    Some((tex, ts)) => {
                        let sample = self.input_sample(&tex, ts)?;
                        self.transform.process_input(&sample)?;
                    }
                    None => {
                        draining = true;
                        self.transform
                            .message(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                        self.transform.message(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                    }
                }
            } else if event == METransformHaveOutput {
                if let Some(sample) = self.transform.process_output()? {
                    let _ = packets.send(self.to_packet(&sample)?);
                }
            } else if event == METransformDrainComplete {
                return Ok(());
            }
        }
    }
}
