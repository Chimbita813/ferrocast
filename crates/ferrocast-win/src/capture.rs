//! Monitor and window capture through Windows.Graphics.Capture.
//!
//! Frames arrive as D3D11 textures on a free-threaded frame pool and are
//! copied (GPU to GPU) into a texture the compositor samples. Nothing is
//! read back to system memory.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::d3d::{texture_size, Device};
use crate::Result;

/// The most recent image of a video source, ready to be sampled.
#[derive(Clone)]
pub struct Frame {
    pub texture: ID3D11Texture2D,
    pub srv: ID3D11ShaderResourceView,
    pub width: u32,
    pub height: u32,
}

unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

impl Frame {
    pub fn new(device: &Device, width: u32, height: u32) -> Result<Frame> {
        let texture = device.texture(
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_SHADER_RESOURCE,
        )?;
        let srv = device.srv(&texture)?;
        Ok(Frame {
            texture,
            srv,
            width,
            height,
        })
    }
}

/// Latest frame slot shared between a source's producer thread and the compositor.
pub type FrameSlot = Arc<Mutex<Option<Frame>>>;

pub struct ScreenCapture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    latest: FrameSlot,
    closed: Arc<AtomicBool>,
}

unsafe impl Send for ScreenCapture {}

impl ScreenCapture {
    pub fn monitor(device: &Device, monitor: HMONITOR) -> Result<Self> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor)? };
        Self::start(device, item)
    }

    pub fn window(device: &Device, window: HWND) -> Result<Self> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(window)? };
        Self::start(device, item)
    }

    fn start(device: &Device, item: GraphicsCaptureItem) -> Result<Self> {
        let format = DirectXPixelFormat::B8G8R8A8UIntNormalized;
        let size = item.Size()?;
        // Two buffers: one being written by DWM, one being copied by us.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&device.winrt, format, 2, size)?;
        let session = pool.CreateCaptureSession(&item)?;
        let latest: FrameSlot = Arc::new(Mutex::new(None));
        let closed = Arc::new(AtomicBool::new(false));

        let pool_size = Arc::new(Mutex::new((size.Width, size.Height)));
        let dev = device.clone();
        let slot = latest.clone();
        pool.FrameArrived(
            &TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                let pool = pool.ok()?;
                let frame = pool.TryGetNextFrame()?;
                let content = frame.ContentSize()?;
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                let src: ID3D11Texture2D = unsafe { access.GetInterface()? };
                let (sw, sh) = texture_size(&src);
                let w = (content.Width.max(1) as u32).min(sw);
                let h = (content.Height.max(1) as u32).min(sh);

                let mut slot = slot.lock().unwrap();
                let reuse = slot.as_ref().is_some_and(|f| f.width == w && f.height == h);
                if !reuse {
                    *slot = Some(Frame::new(&dev, w, h)?);
                }
                let dst = slot.as_ref().unwrap();
                let region = D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: w,
                    bottom: h,
                    back: 1,
                };
                unsafe {
                    dev.context.CopySubresourceRegion(
                        &dst.texture,
                        0,
                        0,
                        0,
                        0,
                        &src,
                        0,
                        Some(&region),
                    )
                };
                drop(slot);
                drop(frame);

                // The window/monitor changed size: resize the pool so the next frame fits.
                let mut ps = pool_size.lock().unwrap();
                if (content.Width, content.Height) != *ps && content.Width > 0 && content.Height > 0
                {
                    *ps = (content.Width, content.Height);
                    pool.Recreate(
                        &dev.winrt,
                        format,
                        2,
                        SizeInt32 {
                            Width: content.Width,
                            Height: content.Height,
                        },
                    )?;
                }
                Ok(())
            }),
        )?;
        let closed_flag = closed.clone();
        item.Closed(
            &TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                closed_flag.store(true, Ordering::Relaxed);
                Ok(())
            }),
        )?;

        let _ = session.SetIsCursorCaptureEnabled(true);
        // Windows 11 only: hide the yellow capture border. Older builds just ignore it.
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;
        Ok(Self {
            session,
            pool,
            latest,
            closed,
        })
    }

    pub fn frames(&self) -> FrameSlot {
        self.latest.clone()
    }

    /// True once the captured window was closed (or the monitor unplugged).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
