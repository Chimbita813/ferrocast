//! Webcam capture through the Media Foundation source reader.
//!
//! The reader converts whatever the camera produces (usually NV12 or MJPEG)
//! to RGB32, and each frame is uploaded into a texture the compositor samples.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::core::HSTRING;
use windows::Win32::Graphics::Direct3D11::D3D11_BOX;
use windows::Win32::Media::MediaFoundation::*;

use crate::capture::{Frame, FrameSlot};
use crate::d3d::Device;
use crate::{mf, Result};

pub struct Webcam {
    latest: FrameSlot,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Picks the native mode with the most pixels up to 1080p, preferring >= 30 fps.
fn best_native_type(reader: &IMFSourceReader) -> Option<IMFMediaType> {
    let mut best: Option<(u64, IMFMediaType)> = None;
    for i in 0.. {
        let Ok(t) = (unsafe { reader.GetNativeMediaType(STREAM, i) }) else {
            break;
        };
        let Ok((w, h)) = mf::get_pair(&t.clone().into(), &MF_MT_FRAME_SIZE) else {
            continue;
        };
        let (num, den) = mf::get_pair(&t.clone().into(), &MF_MT_FRAME_RATE).unwrap_or((30, 1));
        if w * h > 1920 * 1080 {
            continue;
        }
        let fps = num / den.max(1);
        let score = ((fps >= 30) as u64) << 40 | (w as u64 * h as u64) << 8 | fps.min(255) as u64;
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, t));
        }
    }
    best.map(|(_, t)| t)
}

fn open(device_id: &str) -> Result<(IMFSourceReader, u32, u32)> {
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 2)?;
        let attrs = attrs.unwrap();
        attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
        attrs.SetString(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            &HSTRING::from(device_id),
        )?;
        let source = MFCreateDeviceSource(&attrs)?;

        let mut reader_attrs = None;
        MFCreateAttributes(&mut reader_attrs, 1)?;
        let reader_attrs = reader_attrs.unwrap();
        reader_attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        let reader = MFCreateSourceReaderFromMediaSource(&source, &reader_attrs)?;

        if let Some(native) = best_native_type(&reader) {
            reader.SetCurrentMediaType(STREAM, None, &native)?;
        }
        let current = reader.GetCurrentMediaType(STREAM)?;
        let (w, h) = mf::get_pair(&current.clone().into(), &MF_MT_FRAME_SIZE)?;
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        mf::set_pair(&out.clone().into(), &MF_MT_FRAME_SIZE, w, h)?;
        reader.SetCurrentMediaType(STREAM, None, &out)?;
        Ok((reader, w, h))
    }
}

impl Webcam {
    pub fn start(device: &Device, device_id: &str) -> Result<Self> {
        let latest: FrameSlot = Default::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = {
            let device = device.clone();
            let device_id = device_id.to_string();
            let latest = latest.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("webcam".into())
                .spawn(move || {
                    let _ = mf::startup();
                    let (reader, w, h) = match open(&device_id) {
                        Ok(r) => {
                            let _ = ready_tx.send(Ok(()));
                            r
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    let frame = match Frame::new(&device, w, h) {
                        Ok(f) => f,
                        Err(e) => {
                            log::error!("webcam texture: {e}");
                            return;
                        }
                    };
                    let mut rows = vec![0u8; (w * h * 4) as usize];
                    while !stop.load(Ordering::Relaxed) {
                        if let Err(e) = read_one(&reader, &device, &frame, &latest, &mut rows, w, h)
                        {
                            log::warn!("webcam read failed: {e}");
                            break;
                        }
                    }
                })
                .map_err(|e| {
                    windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string())
                })?
        };
        ready_rx.recv().unwrap_or_else(|_| {
            Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "webcam thread died",
            ))
        })?;
        Ok(Self {
            latest,
            stop,
            thread: Some(thread),
        })
    }

    pub fn frames(&self) -> FrameSlot {
        self.latest.clone()
    }
}

fn read_one(
    reader: &IMFSourceReader,
    device: &Device,
    frame: &Frame,
    latest: &FrameSlot,
    rows: &mut [u8],
    w: u32,
    h: u32,
) -> Result<()> {
    unsafe {
        let mut flags = 0u32;
        let mut sample = None;
        reader.ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample))?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "camera stopped",
            ));
        }
        let Some(sample) = sample else { return Ok(()) };
        let buffer = sample.GetBufferByIndex(0)?;
        let row_bytes = (w * 4) as usize;
        // IMF2DBuffer gives the real pitch (RGB32 is often bottom-up, i.e. negative pitch).
        if let Ok(b2d) = windows::core::Interface::cast::<IMF2DBuffer>(&buffer) {
            let mut scan0 = std::ptr::null_mut();
            let mut pitch = 0i32;
            b2d.Lock2D(&mut scan0, &mut pitch)?;
            for y in 0..h as usize {
                let src = scan0.offset(y as isize * pitch as isize);
                std::ptr::copy_nonoverlapping(src, rows[y * row_bytes..].as_mut_ptr(), row_bytes);
            }
            b2d.Unlock2D()?;
        } else {
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let n = (len as usize).min(rows.len());
            std::ptr::copy_nonoverlapping(ptr, rows.as_mut_ptr(), n);
            buffer.Unlock()?;
        }
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: w,
            bottom: h,
            back: 1,
        };
        device.context.UpdateSubresource(
            &frame.texture,
            0,
            Some(&region),
            rows.as_ptr() as *const _,
            row_bytes as u32,
            0,
        );
        let mut slot = latest.lock().unwrap();
        if slot.is_none() {
            *slot = Some(frame.clone());
        }
        Ok(())
    }
}

impl Drop for Webcam {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
