//! WASAPI capture: microphones, and desktop audio via loopback on a playback device.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use chimcast_media::audio::Mixer;
use windows::core::HSTRING;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

use crate::{mf, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// A capture endpoint (microphone, line in).
    Input,
    /// Loopback of a render endpoint: what the speakers play.
    Loopback,
}

pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
enum SampleFormat {
    F32,
    I16,
    I24,
    I32,
}

struct Format {
    rate: u32,
    channels: usize,
    sample: SampleFormat,
    block_align: usize,
}

unsafe fn parse_format(ptr: *const WAVEFORMATEX) -> Format {
    let fmt = unsafe { std::ptr::read_unaligned(ptr) };
    let tag = fmt.wFormatTag as u32;
    let bits = fmt.wBitsPerSample;
    let float = if tag == WAVE_FORMAT_EXTENSIBLE {
        let ext = unsafe { std::ptr::read_unaligned(ptr as *const WAVEFORMATEXTENSIBLE) };
        let sub = ext.SubFormat;
        sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        tag == WAVE_FORMAT_IEEE_FLOAT
    };
    let sample = match (float, bits) {
        (true, _) => SampleFormat::F32,
        (false, 16) => SampleFormat::I16,
        (false, 24) => SampleFormat::I24,
        _ => SampleFormat::I32,
    };
    Format {
        rate: fmt.nSamplesPerSec,
        channels: fmt.nChannels as usize,
        sample,
        block_align: fmt.nBlockAlign as usize,
    }
}

fn convert(data: &[u8], fmt: &Format, out: &mut Vec<f32>) {
    out.clear();
    match fmt.sample {
        SampleFormat::F32 => out.extend(
            data.as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b)),
        ),
        SampleFormat::I16 => out.extend(
            data.as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0),
        ),
        SampleFormat::I24 => out.extend(
            data.as_chunks::<3>()
                .0
                .iter()
                .map(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0),
        ),
        SampleFormat::I32 => out.extend(
            data.as_chunks::<4>()
                .0
                .iter()
                .map(|b| i32::from_le_bytes(*b) as f32 / 2_147_483_648.0),
        ),
    }
}

fn open(
    direction: Direction,
    device_id: Option<&str>,
) -> Result<(IAudioClient, IAudioCaptureClient, Format)> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = match device_id {
            Some(id) => enumerator.GetDevice(&HSTRING::from(id))?,
            None => {
                let flow = if direction == Direction::Input {
                    eCapture
                } else {
                    eRender
                };
                enumerator.GetDefaultAudioEndpoint(flow, eConsole)?
            }
        };
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let mix = client.GetMixFormat()?;
        let format = parse_format(mix);
        let flags = if direction == Direction::Loopback {
            AUDCLNT_STREAMFLAGS_LOOPBACK
        } else {
            0
        };
        // 100 ms shared-mode buffer; we poll every 5 ms.
        let result = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, mix, None);
        CoTaskMemFree(Some(mix as *const _));
        result?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok((client, capture, format))
    }
}

impl AudioCapture {
    /// Starts capturing into `mixer` under `source_id`.
    pub fn start(
        direction: Direction,
        device_id: Option<String>,
        source_id: u64,
        mixer: Arc<Mutex<Mixer>>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name(format!("audio-{source_id}"))
                .spawn(move || {
                    let _ = mf::startup();
                    let (client, capture, fmt) = match open(direction, device_id.as_deref()) {
                        Ok(v) => {
                            let _ = ready_tx.send(Ok(()));
                            v
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    let mut samples = Vec::new();
                    while !stop.load(Ordering::Relaxed) {
                        if let Err(e) = drain(&capture, &fmt, &mut samples, source_id, &mixer) {
                            log::warn!("audio source {source_id} stopped: {e}");
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    unsafe {
                        let _ = client.Stop();
                    }
                })
                .map_err(|e| {
                    windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string())
                })?
        };
        ready_rx.recv().unwrap_or_else(|_| {
            Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "audio thread died",
            ))
        })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

fn drain(
    capture: &IAudioCaptureClient,
    fmt: &Format,
    samples: &mut Vec<f32>,
    source_id: u64,
    mixer: &Mutex<Mixer>,
) -> Result<()> {
    unsafe {
        while capture.GetNextPacketSize()? > 0 {
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
            let bytes = frames as usize * fmt.block_align;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                samples.clear();
                samples.resize(frames as usize * fmt.channels, 0.0);
            } else {
                convert(std::slice::from_raw_parts(data, bytes), fmt, samples);
            }
            capture.ReleaseBuffer(frames)?;
            mixer
                .lock()
                .unwrap()
                .push(source_id, fmt.rate, fmt.channels, samples);
        }
    }
    Ok(())
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
