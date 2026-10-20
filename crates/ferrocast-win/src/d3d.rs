//! Shared D3D11 device.

use windows::core::Interface;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;

use crate::Result;

/// One D3D11 device for the whole app. Capture, compositing, colour
/// conversion, encoding and the UI all share it, so frames never leave the GPU.
#[derive(Clone)]
pub struct Device {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    /// The same device wrapped for WinRT (Windows.Graphics.Capture).
    pub winrt: IDirect3DDevice,
    multithread: ID3D11Multithread,
}

/// Holds the device lock; released on drop.
pub struct DeviceLock<'a>(&'a ID3D11Multithread);

impl Drop for DeviceLock<'_> {
    fn drop(&mut self) {
        unsafe { self.0.Leave() };
    }
}

// D3D11 devices are free-threaded; the immediate context is made safe by
// multithread protection, which is enabled in `Device::new`.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    pub fn new() -> Result<Self> {
        let mut device = None;
        let mut context = None;
        let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        if cfg!(debug_assertions) && std::env::var_os("FERROCAST_D3D_DEBUG").is_some() {
            flags |= D3D11_CREATE_DEVICE_DEBUG;
        }
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device: ID3D11Device = device.unwrap();
        let context = context.unwrap();
        let multithread: ID3D11Multithread = context.cast()?;
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
        let dxgi: IDXGIDevice = device.cast()?;
        let winrt: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        Ok(Self {
            device,
            context,
            winrt,
            multithread,
        })
    }

    /// Locks the immediate context for a multi-call sequence (set state + draw).
    /// Single calls are already serialized by multithread protection.
    pub fn lock(&self) -> DeviceLock<'_> {
        unsafe { self.multithread.Enter() };
        DeviceLock(&self.multithread)
    }

    pub fn texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        bind: D3D11_BIND_FLAG,
    ) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width.max(1),
            Height: height.max(1),
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        Ok(tex.unwrap())
    }

    pub fn srv(&self, tex: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
        let mut srv = None;
        unsafe {
            self.device
                .CreateShaderResourceView(tex, None, Some(&mut srv))?
        };
        Ok(srv.unwrap())
    }

    pub fn rtv(&self, tex: &ID3D11Texture2D) -> Result<ID3D11RenderTargetView> {
        let mut rtv = None;
        unsafe {
            self.device
                .CreateRenderTargetView(tex, None, Some(&mut rtv))?
        };
        Ok(rtv.unwrap())
    }
}

/// Compiles HLSL source for one entry point (`vs_5_0`, `ps_5_0`, ...).
pub fn compile_shader(source: &str, entry: &str, target: &str) -> Result<Vec<u8>> {
    use windows::core::PCSTR;
    use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
    let entry = std::ffi::CString::new(entry).unwrap();
    let target = std::ffi::CString::new(target).unwrap();
    let mut code = None;
    let mut errors = None;
    let result = unsafe {
        D3DCompile(
            source.as_ptr() as *const _,
            source.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry.as_ptr() as *const u8),
            PCSTR(target.as_ptr() as *const u8),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let msg = errors
            .map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    b.GetBufferPointer() as *const u8,
                    b.GetBufferSize(),
                ))
                .into_owned()
            })
            .unwrap_or_default();
        return Err(windows::core::Error::new(
            e.code(),
            format!("shader {entry:?}: {msg}"),
        ));
    }
    let code = code.unwrap();
    Ok(unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize())
            .to_vec()
    })
}

pub fn texture_size(tex: &ID3D11Texture2D) -> (u32, u32) {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    (desc.Width, desc.Height)
}
