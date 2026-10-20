//! Still images decoded with WIC straight into a GPU texture.

use std::path::Path;

use windows::core::HSTRING;
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use crate::capture::Frame;
use crate::d3d::Device;
use crate::Result;

/// Loads an image file as a premultiplied BGRA texture. The decoded pixels
/// are freed once uploaded; only the GPU copy stays.
pub fn load(device: &Device, path: &Path) -> Result<Frame> {
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let decoder = factory.CreateDecoderFromFilename(
            &HSTRING::from(path),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;
        let bgra = WICConvertBitmapSource(&GUID_WICPixelFormat32bppPBGRA, &frame)?;
        let (mut w, mut h) = (0u32, 0u32);
        bgra.GetSize(&mut w, &mut h)?;
        let stride = w * 4;
        let mut pixels = vec![0u8; (stride * h) as usize];
        bgra.CopyPixels(std::ptr::null(), stride, &mut pixels)?;

        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr() as *const _,
            SysMemPitch: stride,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        device
            .device
            .CreateTexture2D(&desc, Some(&init), Some(&mut texture))?;
        let texture = texture.unwrap();
        let srv = device.srv(&texture)?;
        Ok(Frame {
            texture,
            srv,
            width: w,
            height: h,
        })
    }
}
