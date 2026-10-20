//! Small Media Foundation helpers shared by the encoders and the webcam source.

use std::mem::ManuallyDrop;

use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};

use crate::Result;

/// Initializes COM (MTA) and Media Foundation for the calling thread/process.
/// Safe to call repeatedly; each thread that uses COM calls it once.
pub fn startup() -> Result<()> {
    unsafe {
        // S_FALSE (already initialized) is fine; RPC_E_CHANGED_MODE means the
        // thread is STA (e.g. the UI thread), which works too.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
    }
}

/// Packs two u32 values into an MF "UINT64 pair" attribute (frame size, frame rate).
pub fn set_pair(attrs: &IMFAttributes, key: &GUID, hi: u32, lo: u32) -> Result<()> {
    unsafe { attrs.SetUINT64(key, ((hi as u64) << 32) | lo as u64) }
}

pub fn get_pair(attrs: &IMFAttributes, key: &GUID) -> Result<(u32, u32)> {
    let v = unsafe { attrs.GetUINT64(key)? };
    Ok(((v >> 32) as u32, v as u32))
}

pub fn get_string(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    unsafe {
        let mut ptr = PWSTR::null();
        let mut len = 0u32;
        attrs.GetAllocatedString(key, &mut ptr, &mut len).ok()?;
        let s = ptr.to_string().ok();
        CoTaskMemFree(Some(ptr.0 as *const _));
        s
    }
}

pub fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

pub fn variant_bool(v: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: VARIANT_BOOL(if v { -1 } else { 0 }),
                },
            }),
        },
    }
}

/// Finds and activates transforms in `category` that convert `input` to `output`.
/// Returns them in Media Foundation's preference order (hardware first when asked).
pub fn find_transforms(
    category: GUID,
    flags: MFT_ENUM_FLAG,
    input: MFT_REGISTER_TYPE_INFO,
    output: MFT_REGISTER_TYPE_INFO,
) -> Result<Vec<(IMFActivate, String)>> {
    unsafe {
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            category,
            flags,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )?;
        let mut out = Vec::new();
        if !activates.is_null() {
            for a in std::slice::from_raw_parts_mut(activates, count as usize) {
                if let Some(a) = a.take() {
                    let name = get_string(&a.cast()?, &MFT_FRIENDLY_NAME_Attribute)
                        .unwrap_or_else(|| "unknown".into());
                    out.push((a, name));
                }
            }
            CoTaskMemFree(Some(activates as *const _));
        }
        Ok(out)
    }
}

/// Copies the contents of a sample into a Vec.
pub fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buffer.Unlock()?;
        Ok(data)
    }
}

/// Wraps bytes in a new memory-backed sample.
pub fn memory_sample(data: &[u8], time_100ns: i64, duration_100ns: i64) -> Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(data.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time_100ns)?;
        sample.SetSampleDuration(duration_100ns)?;
        Ok(sample)
    }
}

/// An output buffer for `ProcessOutput` when the transform doesn't allocate its own.
pub fn output_sample(size: u32) -> Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(size.max(1))?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

/// A Media Foundation transform (encoder) with its streaming state.
pub struct Transform {
    pub mft: IMFTransform,
    pub name: String,
    activate: IMFActivate,
    events: Option<IMFMediaEventGenerator>,
    in_id: u32,
    out_id: u32,
    provides_samples: bool,
    out_size: u32,
}

impl Transform {
    /// Activates the transform; async (hardware) transforms are unlocked for use.
    pub fn activate(activate: IMFActivate, name: String) -> Result<Self> {
        unsafe {
            let mft: IMFTransform = activate.ActivateObject()?;
            let mut events = None;
            if let Ok(attrs) = mft.GetAttributes() {
                if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1 {
                    attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                    events = Some(mft.cast::<IMFMediaEventGenerator>()?);
                }
            }
            let (mut in_ids, mut out_ids) = ([0u32], [0u32]);
            let (in_id, out_id) = match mft.GetStreamIDs(&mut in_ids, &mut out_ids) {
                Ok(()) => (in_ids[0], out_ids[0]),
                Err(_) => (0, 0), // E_NOTIMPL: stream IDs are 0..n
            };
            Ok(Self {
                mft,
                name,
                activate,
                events,
                in_id,
                out_id,
                provides_samples: false,
                out_size: 0,
            })
        }
    }

    pub fn is_async(&self) -> bool {
        self.events.is_some()
    }

    pub fn attributes(&self) -> Option<IMFAttributes> {
        unsafe { self.mft.GetAttributes().ok() }
    }

    pub fn set_output_type(&self, t: &IMFMediaType) -> Result<()> {
        unsafe { self.mft.SetOutputType(self.out_id, t, 0) }
    }

    pub fn set_input_type(&self, t: &IMFMediaType) -> Result<()> {
        unsafe { self.mft.SetInputType(self.in_id, t, 0) }
    }

    pub fn output_type(&self) -> Result<IMFMediaType> {
        unsafe { self.mft.GetOutputCurrentType(self.out_id) }
    }

    /// Call once media types are set.
    pub fn begin(&mut self) -> Result<()> {
        unsafe {
            let info = self.mft.GetOutputStreamInfo(self.out_id)?;
            self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.out_size = info.cbSize;
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(())
    }

    pub fn process_input(&self, sample: &IMFSample) -> Result<()> {
        unsafe { self.mft.ProcessInput(self.in_id, sample, 0) }
    }

    /// Asks for one output sample. `Ok(None)` means the transform needs more input.
    pub fn process_output(&mut self) -> Result<Option<IMFSample>> {
        loop {
            let sample = if self.provides_samples {
                None
            } else {
                Some(output_sample(self.out_size)?)
            };
            let mut buf = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.out_id,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = unsafe { self.mft.ProcessOutput(0, &mut buf, &mut status) };
            let [mut buf] = buf;
            let sample = unsafe { ManuallyDrop::take(&mut buf.pSample) };
            unsafe { ManuallyDrop::drop(&mut buf.pEvents) };
            match result {
                Ok(()) => return Ok(sample),
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder renegotiated its output (e.g. added codec data): accept it.
                    let t = unsafe { self.mft.GetOutputAvailableType(self.out_id, 0)? };
                    self.set_output_type(&t)?;
                    let info = unsafe { self.mft.GetOutputStreamInfo(self.out_id)? };
                    self.provides_samples =
                        info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
                    self.out_size = info.cbSize;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Blocks until the next async event (hardware transforms only).
    pub fn next_event(&self) -> Result<MF_EVENT_TYPE> {
        let events = self
            .events
            .as_ref()
            .expect("next_event on a synchronous transform");
        unsafe {
            let ev = events.GetEvent(MF_EVENT_FLAG_NONE)?;
            Ok(MF_EVENT_TYPE(ev.GetType()? as i32))
        }
    }

    pub fn message(&self, msg: MFT_MESSAGE_TYPE, param: usize) -> Result<()> {
        unsafe { self.mft.ProcessMessage(msg, param) }
    }
}

impl Drop for Transform {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.mft.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}
