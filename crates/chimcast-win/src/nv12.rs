//! BGRA canvas -> NV12 conversion on the GPU's video engine (ID3D11VideoProcessor).

use std::collections::HashMap;
use std::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};

use crate::d3d::Device;
use crate::Result;

/// Encoders may hold on to a few input frames, so output textures rotate.
const POOL: usize = 8;

pub struct Nv12Converter {
    device: Device,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    /// Input views per canvas texture (there are only two canvases).
    inputs: HashMap<usize, ID3D11VideoProcessorInputView>,
    staging: Option<ID3D11Texture2D>,
    pub width: u32,
    pub height: u32,
}

unsafe impl Send for Nv12Converter {}

impl Nv12Converter {
    pub fn new(device: &Device, width: u32, height: u32, fps: u32) -> Result<Self> {
        let video_device: ID3D11VideoDevice = device.device.cast()?;
        let video_context: ID3D11VideoContext = device.context.cast()?;
        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        unsafe {
            let enumerator = video_device.CreateVideoProcessorEnumerator(&desc)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;
            // Input: full-range RGB. Output: BT.709 limited range, what every player assumes for HD H.264.
            let input_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let output_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (1 << 2) | (1 << 4),
            };
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &input_cs);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &output_cs);
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);

            let mut outputs = Vec::with_capacity(POOL);
            for _ in 0..POOL {
                let tex =
                    device.texture(width, height, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET)?;
                let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut view = None;
                video_device.CreateVideoProcessorOutputView(
                    &tex,
                    &enumerator,
                    &view_desc,
                    Some(&mut view),
                )?;
                outputs.push((tex, view.unwrap()));
            }
            Ok(Self {
                device: device.clone(),
                video_device,
                video_context,
                enumerator,
                processor,
                outputs,
                next: 0,
                inputs: HashMap::new(),
                staging: None,
                width,
                height,
            })
        }
    }

    /// Converts a BGRA texture (same size as the converter) and returns the NV12 texture.
    pub fn convert(&mut self, bgra: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let key = bgra.as_raw() as usize;
        if !self.inputs.contains_key(&key) {
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut view = None;
            unsafe {
                self.video_device.CreateVideoProcessorInputView(
                    bgra,
                    &self.enumerator,
                    &desc,
                    Some(&mut view),
                )?
            };
            self.inputs.insert(key, view.unwrap());
        }
        let input = self.inputs[&key].clone();
        let (tex, view) = self.outputs[self.next].clone();
        self.next = (self.next + 1) % self.outputs.len();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(input)),
            ..Default::default()
        };
        let streams = [stream];
        let result = {
            let _lock = self.device.lock();
            unsafe {
                self.video_context
                    .VideoProcessorBlt(&self.processor, &view, 0, &streams)
            }
        };
        let [mut stream] = streams;
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result?;
        Ok(tex)
    }

    /// Reads an NV12 texture back into tightly packed bytes (for software encoders).
    pub fn read_back(&mut self, nv12: &ID3D11Texture2D) -> Result<Vec<u8>> {
        let (w, h) = (self.width as usize, self.height as usize);
        if self.staging.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex = None;
            unsafe {
                self.device
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut tex))?
            };
            self.staging = tex;
        }
        let staging = self.staging.clone().unwrap();
        let ctx = &self.device.context;
        let _lock = self.device.lock();
        let mut out = vec![0u8; w * h * 3 / 2];
        unsafe {
            ctx.CopyResource(&staging, nv12);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            // Luma rows, then interleaved chroma rows, which follow `height` rows of luma.
            for y in 0..h + h / 2 {
                std::ptr::copy_nonoverlapping(base.add(y * pitch), out[y * w..].as_mut_ptr(), w);
            }
            ctx.Unmap(&staging, 0);
        }
        Ok(out)
    }
}
