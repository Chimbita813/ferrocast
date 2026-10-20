//! The engine ties sources, compositor, encoders and outputs together.
//!
//! Threads:
//! - video: ticks at the canvas frame rate, composites, converts to NV12 and
//!   hands frames to the encoder while an output is active.
//! - audio: every 10 ms mixes all sources and, while encoding, feeds AAC.
//! - h264: owns the Media Foundation encoder.
//! - router: fans encoded packets out to the recording/streaming workers.
//! - one thread per capture source (WGC callbacks, webcam reader, WASAPI).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ferrocast_core::{AudioSourceKind, Id, Project, Rect, SceneItem, VideoSourceKind};
use ferrocast_media::audio::{self, Mixer};
use ferrocast_media::output::{Mp4Sink, OutputStatus, OutputWorker, PacketSink};
use ferrocast_media::rtmp::RtmpPublisher;
use ferrocast_media::{AudioInfo, Packet, StreamInfo, VideoInfo};
use windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView;

use crate::aac_encoder::AacEncoder;
use crate::capture::{Frame, FrameSlot, ScreenCapture};
use crate::compositor::{Compositor, DrawItem, Fill};
use crate::d3d::Device;
use crate::h264_encoder::{EncoderMsg, H264Encoder, VideoEncoderSettings};
use crate::nv12::Nv12Converter;
use crate::wasapi::{AudioCapture, Direction};
use crate::{devices, image, mf, sysinfo, webcam, Result};

enum Source {
    Screen(ScreenCapture),
    Webcam(webcam::Webcam),
    Image(Frame),
    Color([f32; 4]),
    /// Couldn't be opened (window not found, camera busy); retried later.
    Missing {
        since: Instant,
        error: String,
    },
}

impl Source {
    fn open(device: &Device, kind: &VideoSourceKind) -> Source {
        let result = match kind {
            VideoSourceKind::Monitor { index } => devices::monitors()
                .get(*index as usize)
                .ok_or_else(|| {
                    windows::core::Error::new(
                        windows::Win32::Foundation::E_INVALIDARG,
                        "monitor not connected",
                    )
                })
                .and_then(|m| ScreenCapture::monitor(device, m.handle))
                .map(Source::Screen),
            VideoSourceKind::Window { title, exe } => devices::find_window(title, exe.as_deref())
                .ok_or_else(|| {
                    windows::core::Error::new(
                        windows::Win32::Foundation::E_INVALIDARG,
                        "window not found",
                    )
                })
                .and_then(|h| ScreenCapture::window(device, h))
                .map(Source::Screen),
            VideoSourceKind::Webcam { device_id } => {
                webcam::Webcam::start(device, device_id).map(Source::Webcam)
            }
            VideoSourceKind::Image { path } => image::load(device, path).map(Source::Image),
            VideoSourceKind::Color { rgba } => Ok(Source::Color(rgba.map(|c| c as f32 / 255.0))),
        };
        result.unwrap_or_else(|e| {
            log::warn!("{} source unavailable: {e}", kind.label());
            Source::Missing {
                since: Instant::now(),
                error: e.message(),
            }
        })
    }

    fn slot(&self) -> Option<FrameSlot> {
        match self {
            Source::Screen(c) => Some(c.frames()),
            Source::Webcam(w) => Some(w.frames()),
            _ => None,
        }
    }

    fn fill(&self) -> Option<(Fill, (u32, u32))> {
        match self {
            Source::Image(f) => Some((Fill::Texture(f.srv.clone()), (f.width, f.height))),
            Source::Color(c) => Some((Fill::Solid(*c), (0, 0))),
            Source::Missing { .. } => None,
            _ => {
                let slot = self.slot()?;
                let guard = slot.lock().unwrap();
                let f = guard.as_ref()?;
                Some((Fill::Opaque(f.srv.clone()), (f.width, f.height)))
            }
        }
    }

    fn needs_reopen(&self) -> bool {
        match self {
            Source::Missing { since, .. } => since.elapsed() > Duration::from_secs(3),
            Source::Screen(c) => c.is_closed(),
            _ => false,
        }
    }
}

struct VideoSlot {
    kind: VideoSourceKind,
    source: Source,
}

/// What the video thread draws: the active scene's visible items, bottom first.
#[derive(Default)]
struct SceneState {
    items: Vec<(Id, Rect)>,
}

#[derive(Clone, Debug, Default)]
pub struct EngineStats {
    pub fps: f32,
    pub render_ms: f32,
    pub skipped_frames: u64,
    pub encoder_dropped_frames: u64,
    pub video_encoder: Option<String>,
    pub private_bytes: u64,
    pub working_set: u64,
    /// Peak level per audio source, dBFS.
    pub audio_peaks: HashMap<u64, f32>,
    pub last_error: Option<String>,
}

#[derive(Default)]
pub struct Outputs {
    pub recording: Option<(OutputWorker, PathBuf)>,
    pub streaming: Option<OutputWorker>,
}

impl Outputs {
    fn active(&self) -> bool {
        self.recording.is_some() || self.streaming.is_some()
    }
}

/// Encoders exist only while at least one output is running.
struct EncodingSession {
    frames: SyncSender<EncoderMsg>,
    audio: Arc<Mutex<Option<AacEncoder>>>,
    video_thread: JoinHandle<()>,
    router_thread: JoinHandle<()>,
}

/// The AAC encoder and the channel its packets go out on.
type AudioRoute = (Arc<Mutex<Option<AacEncoder>>>, Sender<Packet>);

struct Shared {
    device: Device,
    compositor: Compositor,
    scene: Mutex<SceneState>,
    video: Mutex<HashMap<Id, VideoSlot>>,
    mixer: Arc<Mutex<Mixer>>,
    outputs: Mutex<Outputs>,
    /// Video encoder input; `None` while not encoding.
    encoder_input: Mutex<Option<SyncSender<EncoderMsg>>>,
    /// Audio encoder + packet route; `None` while not encoding.
    audio_encoder: Mutex<Option<AudioRoute>>,
    stats: Mutex<EngineStats>,
    epoch: Instant,
    fps: u32,
    stop: AtomicBool,
}

pub struct Engine {
    shared: Arc<Shared>,
    audio_sources: HashMap<Id, (AudioSourceKind, AudioCapture)>,
    /// Audio sources that failed to open, and when; retried every few seconds.
    audio_failed: HashMap<Id, Instant>,
    session: Option<EncodingSession>,
    threads: Vec<JoinHandle<()>>,
    width: u32,
    height: u32,
    fps: u32,
}

impl Engine {
    /// Starts the engine on `device`, which the UI shares for the preview.
    pub fn new(project: &Project, device: Device) -> Result<Engine> {
        mf::startup()?;
        unsafe {
            windows::Win32::Media::timeBeginPeriod(1);
        }
        let v = project.settings.video;
        let compositor = Compositor::new(&device, v.width, v.height)?;
        let shared = Arc::new(Shared {
            device,
            compositor,
            scene: Mutex::default(),
            video: Mutex::default(),
            mixer: Arc::new(Mutex::new(Mixer::new())),
            outputs: Mutex::default(),
            encoder_input: Mutex::new(None),
            audio_encoder: Mutex::new(None),
            stats: Mutex::default(),
            epoch: Instant::now(),
            fps: v.fps.max(1),
            stop: AtomicBool::new(false),
        });
        let mut threads = Vec::new();
        let s = shared.clone();
        threads.push(spawn("video", move || video_loop(&s)));
        let s = shared.clone();
        threads.push(spawn("audio-mix", move || audio_loop(&s)));
        let mut engine = Engine {
            shared,
            audio_sources: HashMap::new(),
            audio_failed: HashMap::new(),
            session: None,
            threads,
            width: v.width,
            height: v.height,
            fps: v.fps,
        };
        engine.sync(project);
        Ok(engine)
    }

    pub fn is_live(&self) -> bool {
        self.shared.outputs.lock().unwrap().active()
    }

    pub fn canvas_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// True if the engine must be rebuilt for these settings (only allowed while idle).
    pub fn needs_rebuild(&self, project: &Project) -> bool {
        let v = project.settings.video;
        (v.width, v.height, v.fps) != (self.width, self.height, self.fps)
    }

    /// The latest composited frame, for the preview.
    pub fn preview(&self) -> ID3D11ShaderResourceView {
        self.shared.compositor.front().srv
    }

    /// Native size of a source's content, if known (for "fit to source").
    pub fn source_size(&self, item: Id) -> Option<(u32, u32)> {
        let video = self.shared.video.lock().unwrap();
        let (_, size) = video.get(&item)?.source.fill()?;
        (size.0 > 0).then_some(size)
    }

    pub fn source_error(&self, item: Id) -> Option<String> {
        match &self.shared.video.lock().unwrap().get(&item)?.source {
            Source::Missing { error, .. } => Some(error.clone()),
            _ => None,
        }
    }

    /// Brings sources in line with the project. Cheap when nothing changed.
    pub fn sync(&mut self, project: &Project) {
        let scene = project.active();
        self.sync_video(&scene.items);
        self.shared.scene.lock().unwrap().items = scene
            .items
            .iter()
            .filter(|i| i.visible)
            .map(|i| (i.id, i.rect))
            .collect();

        let mut mixer = self.shared.mixer.lock().unwrap();
        self.audio_sources.retain(|id, (kind, _)| {
            let keep = project.audio.iter().any(|a| a.id == *id && a.kind == *kind);
            if !keep {
                mixer.remove(id.0);
            }
            keep
        });
        drop(mixer);
        self.audio_failed.retain(|id, at| {
            project.audio.iter().any(|a| a.id == *id) && at.elapsed() < Duration::from_secs(5)
        });
        for a in &project.audio {
            if !self.audio_sources.contains_key(&a.id) && !self.audio_failed.contains_key(&a.id) {
                let (direction, device_id) = match &a.kind {
                    AudioSourceKind::Microphone { device_id } => {
                        (Direction::Input, device_id.clone())
                    }
                    AudioSourceKind::Desktop { device_id } => {
                        (Direction::Loopback, device_id.clone())
                    }
                };
                match AudioCapture::start(direction, device_id, a.id.0, self.shared.mixer.clone()) {
                    Ok(c) => {
                        self.audio_sources.insert(a.id, (a.kind.clone(), c));
                    }
                    Err(e) => {
                        log::warn!("audio source {} unavailable: {e}", a.name);
                        self.audio_failed.insert(a.id, Instant::now());
                    }
                }
            }
            self.shared
                .mixer
                .lock()
                .unwrap()
                .set_gain(a.id.0, if a.muted { 0.0 } else { a.volume });
        }
    }

    fn sync_video(&self, items: &[SceneItem]) {
        let mut video = self.shared.video.lock().unwrap();
        video.retain(|id, slot| items.iter().any(|i| i.id == *id && i.source == slot.kind));
        for item in items {
            let reopen = video.get(&item.id).is_none_or(|s| s.source.needs_reopen());
            if reopen {
                let source = Source::open(&self.shared.device, &item.source);
                video.insert(
                    item.id,
                    VideoSlot {
                        kind: item.source.clone(),
                        source,
                    },
                );
            }
        }
    }

    pub fn stats(&self) -> EngineStats {
        let mut s = self.shared.stats.lock().unwrap().clone();
        s.private_bytes = sysinfo::private_bytes();
        s.working_set = sysinfo::working_set();
        s
    }

    pub fn recording_status(&self) -> Option<(OutputStatus, PathBuf)> {
        let o = self.shared.outputs.lock().unwrap();
        o.recording.as_ref().map(|(w, p)| (w.status(), p.clone()))
    }

    pub fn streaming_status(&self) -> Option<OutputStatus> {
        self.shared
            .outputs
            .lock()
            .unwrap()
            .streaming
            .as_ref()
            .map(|w| w.status())
    }

    fn stream_info(project: &Project) -> StreamInfo {
        let s = &project.settings;
        StreamInfo {
            video: VideoInfo {
                width: s.video.width,
                height: s.video.height,
                fps: s.video.fps,
                bitrate_kbps: s.encoder.video_bitrate_kbps,
            },
            audio: Some(AudioInfo {
                sample_rate: audio::SAMPLE_RATE,
                channels: audio::CHANNELS as u16,
                bitrate_kbps: s.encoder.audio_bitrate_kbps,
            }),
        }
    }

    fn ensure_encoding(&mut self, project: &Project) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let s = &project.settings;
        let settings = VideoEncoderSettings {
            width: s.video.width,
            height: s.video.height,
            fps: s.video.fps,
            bitrate_kbps: s.encoder.video_bitrate_kbps,
            keyframe_interval_secs: s.encoder.keyframe_interval_secs,
        };
        let (packet_tx, packet_rx) = mpsc::channel::<Packet>();
        let (frame_tx, frame_rx) = mpsc::sync_channel::<EncoderMsg>(3);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<String>>();

        // The encoder is created on its own (MTA) thread and lives there.
        let device = self.shared.device.clone();
        let video_packets = packet_tx.clone();
        let video_thread = spawn("h264", move || {
            let _ = mf::startup();
            match H264Encoder::new(&device, settings) {
                Ok(enc) => {
                    let _ = ready_tx.send(Ok(enc.name().to_string()));
                    if let Err(e) = enc.run(frame_rx, video_packets) {
                        log::error!("video encoder failed: {e}");
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        });
        let name = ready_rx.recv().unwrap_or_else(|_| {
            Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "encoder thread died",
            ))
        })?;
        self.shared.stats.lock().unwrap().video_encoder = Some(name);

        let aac = Arc::new(Mutex::new(Some(AacEncoder::new(
            s.encoder.audio_bitrate_kbps,
        )?)));
        let shared = self.shared.clone();
        let router_thread = spawn("router", move || {
            for p in packet_rx.iter() {
                let mut o = shared.outputs.lock().unwrap();
                if let Some((w, _)) = o.recording.as_mut() {
                    w.send(&p);
                }
                if let Some(w) = o.streaming.as_mut() {
                    w.send(&p);
                }
            }
        });
        *self.shared.encoder_input.lock().unwrap() = Some(frame_tx.clone());
        *self.shared.audio_encoder.lock().unwrap() = Some((aac.clone(), packet_tx));
        self.session = Some(EncodingSession {
            frames: frame_tx,
            audio: aac,
            video_thread,
            router_thread,
        });
        Ok(())
    }

    fn request_keyframe(&self) {
        if let Some(s) = &self.session {
            let _ = s.frames.try_send(EncoderMsg::ForceKeyframe);
        }
    }

    pub fn start_recording(&mut self, project: &Project) -> std::result::Result<PathBuf, String> {
        if self.shared.outputs.lock().unwrap().recording.is_some() {
            return Err("already recording".into());
        }
        self.ensure_encoding(project)
            .map_err(|e| format!("encoder: {}", e.message()))?;
        let dir = project.settings.recording.directory.clone();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(format!("Ferrocast {}.mp4", timestamp()));
        let info = Self::stream_info(project);
        let p = path.clone();
        let worker = OutputWorker::spawn("record", 600, move || {
            Ok(Box::new(Mp4Sink::create(&p, info)?) as Box<dyn PacketSink>)
        })
        .map_err(|e| e.to_string())?;
        self.shared.outputs.lock().unwrap().recording = Some((worker, path.clone()));
        self.request_keyframe();
        Ok(path)
    }

    pub fn start_streaming(&mut self, project: &Project) -> std::result::Result<(), String> {
        if self.shared.outputs.lock().unwrap().streaming.is_some() {
            return Err("already streaming".into());
        }
        let st = &project.settings.stream;
        if st.key.trim().is_empty() {
            return Err("set a stream key in Settings first".into());
        }
        ferrocast_media::rtmp::parse_url(&st.server).map_err(|e| e.to_string())?;
        self.ensure_encoding(project)
            .map_err(|e| format!("encoder: {}", e.message()))?;
        let (server, key) = (st.server.clone(), st.key.trim().to_string());
        let info = Self::stream_info(project);
        // ~2 s of packets at 60 fps; beyond that the network is too slow and frames drop.
        let worker = OutputWorker::spawn("stream", 240, move || {
            Ok(Box::new(RtmpPublisher::connect(&server, &key, info)?) as Box<dyn PacketSink>)
        })
        .map_err(|e| e.to_string())?;
        self.shared.outputs.lock().unwrap().streaming = Some(worker);
        self.request_keyframe();
        Ok(())
    }

    /// Stops recording; the file is finalized on a background thread.
    pub fn stop_recording(&mut self) {
        let taken = self.shared.outputs.lock().unwrap().recording.take();
        if let Some((w, path)) = taken {
            spawn("finish-recording", move || {
                let status = w.stop();
                log::info!("recording {} finished: {:?}", path.display(), status.state);
            });
        }
        self.end_encoding_if_idle();
    }

    pub fn stop_streaming(&mut self) {
        let taken = self.shared.outputs.lock().unwrap().streaming.take();
        if let Some(w) = taken {
            spawn("finish-stream", move || {
                w.stop();
            });
        }
        self.end_encoding_if_idle();
    }

    fn end_encoding_if_idle(&mut self) {
        if self.shared.outputs.lock().unwrap().active() {
            return;
        }
        if let Some(session) = self.session.take() {
            *self.shared.encoder_input.lock().unwrap() = None;
            *self.shared.audio_encoder.lock().unwrap() = None;
            drop(session.frames);
            drop(session.audio);
            let _ = session.video_thread.join();
            let _ = session.router_thread.join();
            self.shared.stats.lock().unwrap().video_encoder = None;
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Finish outputs synchronously so recordings are never left without an index.
        let (rec, stream) = {
            let mut o = self.shared.outputs.lock().unwrap();
            (o.recording.take(), o.streaming.take())
        };
        if let Some((w, _)) = rec {
            w.stop();
        }
        if let Some(w) = stream {
            w.stop();
        }
        self.end_encoding_if_idle();
        self.shared.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        self.audio_sources.clear();
        self.shared.video.lock().unwrap().clear();
        unsafe {
            windows::Win32::Media::timeEndPeriod(1);
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .expect("failed to spawn thread")
}

fn timestamp() -> String {
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

fn video_loop(shared: &Shared) {
    let _ = mf::startup();
    let fps = shared.fps as u64;
    let frame_us = 1_000_000 / fps;
    let mut converter: Option<Nv12Converter> = None;
    let mut tick: u64 = (shared.epoch.elapsed().as_micros() as u64) / frame_us;
    let mut window_start = Instant::now();
    let mut window_frames = 0u32;
    let mut render_total = Duration::ZERO;

    while !shared.stop.load(Ordering::Relaxed) {
        tick += 1;
        let due = Duration::from_micros(tick * frame_us);
        let now = shared.epoch.elapsed();
        if now < due {
            std::thread::sleep(due - now);
        } else if now > due + Duration::from_micros(frame_us * 2) {
            // Fell behind (system stall): skip ahead instead of bursting frames.
            let behind = (now - due).as_micros() as u64 / frame_us;
            tick += behind;
            shared.stats.lock().unwrap().skipped_frames += behind;
        }
        let ts_us = (tick * frame_us) as i64;

        let started = Instant::now();
        let items: Vec<DrawItem> = {
            let scene = shared.scene.lock().unwrap();
            let video = shared.video.lock().unwrap();
            scene
                .items
                .iter()
                .filter_map(|(id, rect)| {
                    let (fill, _) = video.get(id)?.source.fill()?;
                    Some(DrawItem { fill, rect: *rect })
                })
                .collect()
        };
        let canvas = match shared.compositor.render(&items) {
            Ok(c) => c,
            Err(e) => {
                shared.stats.lock().unwrap().last_error = Some(format!("render: {}", e.message()));
                continue;
            }
        };

        let encoder = shared.encoder_input.lock().unwrap().clone();
        match encoder {
            Some(tx) => {
                if converter.is_none() {
                    match Nv12Converter::new(
                        &shared.device,
                        shared.compositor.width,
                        shared.compositor.height,
                        shared.fps,
                    ) {
                        Ok(c) => converter = Some(c),
                        Err(e) => {
                            shared.stats.lock().unwrap().last_error =
                                Some(format!("color conversion: {}", e.message()));
                            continue;
                        }
                    }
                }
                match converter.as_mut().unwrap().convert(&canvas.texture) {
                    Ok(nv12) => {
                        if let Err(TrySendError::Full(_)) =
                            tx.try_send(EncoderMsg::Frame(nv12, ts_us))
                        {
                            shared.stats.lock().unwrap().encoder_dropped_frames += 1;
                        }
                    }
                    Err(e) => {
                        shared.stats.lock().unwrap().last_error =
                            Some(format!("color conversion: {}", e.message()))
                    }
                }
            }
            // Free the NV12 pool while idle.
            None => converter = None,
        }

        render_total += started.elapsed();
        window_frames += 1;
        if window_start.elapsed() >= Duration::from_secs(1) {
            let secs = window_start.elapsed().as_secs_f32();
            let mut st = shared.stats.lock().unwrap();
            st.fps = window_frames as f32 / secs;
            st.render_ms = render_total.as_secs_f32() * 1000.0 / window_frames as f32;
            window_start = Instant::now();
            window_frames = 0;
            render_total = Duration::ZERO;
        }
    }
}

fn audio_loop(shared: &Shared) {
    let _ = mf::startup();
    let rate = audio::SAMPLE_RATE as u64;
    let chunk = (rate / 100) as usize; // 10 ms
    let mut produced = shared.epoch.elapsed().as_micros() as u64 * rate / 1_000_000;
    let mut pcm = Vec::new();
    let mut packets = Vec::new();
    let mut last_peaks = Instant::now();
    let mut was_encoding = false;

    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(10));
        let target = shared.epoch.elapsed().as_micros() as u64 * rate / 1_000_000;
        let encoder = shared.audio_encoder.lock().unwrap().clone();
        if encoder.is_some() && !was_encoding {
            // Start the AAC timeline fresh so it lines up with video.
            produced = target;
        }
        was_encoding = encoder.is_some();
        while produced + chunk as u64 <= target {
            let ts_us = (produced * 1_000_000 / rate) as i64;
            let mixed = shared.mixer.lock().unwrap().mix(chunk);
            produced += chunk as u64;
            if let Some((aac, route)) = &encoder {
                pcm.clear();
                audio::to_i16_le(&mixed, &mut pcm);
                let mut guard = aac.lock().unwrap();
                if let Some(enc) = guard.as_mut() {
                    packets.clear();
                    match enc.encode(&pcm, ts_us, &mut packets) {
                        Ok(()) => {
                            for p in packets.drain(..) {
                                let _ = route.send(p);
                            }
                        }
                        Err(e) => {
                            shared.stats.lock().unwrap().last_error =
                                Some(format!("audio encoder: {}", e.message()));
                            *guard = None;
                        }
                    }
                }
            }
        }
        if last_peaks.elapsed() >= Duration::from_millis(50) {
            last_peaks = Instant::now();
            let peaks = shared.mixer.lock().unwrap().take_peaks();
            shared.stats.lock().unwrap().audio_peaks = peaks.into_iter().collect();
        }
    }
}
