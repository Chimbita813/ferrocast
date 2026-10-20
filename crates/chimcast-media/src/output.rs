//! Outputs (recording, streaming) run on their own threads behind bounded queues,
//! so a slow disk or network never stalls the encoder.

use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::mp4::Mp4Writer;
use crate::rtmp::RtmpPublisher;
use crate::{Packet, Result, Track};

/// Something that consumes encoded packets.
pub trait PacketSink: Send {
    fn write(&mut self, p: &Packet) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<()>;
    fn bytes_written(&self) -> u64;
}

/// MP4 writer that tracks how much it wrote.
pub struct Mp4Sink<W: Write + Seek + Send> {
    writer: Mp4Writer<W>,
    bytes: u64,
}

impl Mp4Sink<BufWriter<File>> {
    pub fn create(path: &std::path::Path, info: crate::StreamInfo) -> Result<Self> {
        let file = BufWriter::with_capacity(1 << 20, File::create(path)?);
        Ok(Self {
            writer: Mp4Writer::new(file, info)?,
            bytes: 0,
        })
    }
}

impl<W: Write + Seek + Send> PacketSink for Mp4Sink<W> {
    fn write(&mut self, p: &Packet) -> Result<()> {
        self.bytes += p.data.len() as u64;
        self.writer.write(p)
    }

    fn finish(self: Box<Self>) -> Result<()> {
        self.writer.finish()?;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.bytes
    }
}

impl PacketSink for RtmpPublisher {
    fn write(&mut self, p: &Packet) -> Result<()> {
        RtmpPublisher::write(self, p)
    }

    fn finish(self: Box<Self>) -> Result<()> {
        self.close();
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_sent()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum OutputState {
    Starting,
    Running,
    Failed(String),
    Stopped,
}

#[derive(Clone, Debug)]
pub struct OutputStatus {
    pub state: OutputState,
    pub bytes: u64,
    pub dropped_packets: u64,
}

/// Handle to a running output thread.
pub struct OutputWorker {
    tx: Option<SyncSender<Packet>>,
    handle: Option<JoinHandle<()>>,
    status: Arc<Mutex<OutputStatus>>,
    /// After a dropped video packet, video is skipped until the next keyframe
    /// so the decoder never sees a broken reference chain.
    waiting_for_keyframe: bool,
}

impl OutputWorker {
    /// Starts a worker. `open` runs on the worker thread (connecting to an RTMP
    /// server can take seconds); packets sent meanwhile are queued.
    pub fn spawn<F>(name: &str, queue_len: usize, open: F) -> std::io::Result<Self>
    where
        F: FnOnce() -> Result<Box<dyn PacketSink>> + Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel(queue_len);
        let status = Arc::new(Mutex::new(OutputStatus {
            state: OutputState::Starting,
            bytes: 0,
            dropped_packets: 0,
        }));
        let st = status.clone();
        let handle = std::thread::Builder::new()
            .name(format!("output-{name}"))
            .spawn(move || run(open, rx, st))?;
        Ok(Self {
            tx: Some(tx),
            handle: Some(handle),
            status,
            waiting_for_keyframe: false,
        })
    }

    /// Queues a packet. Never blocks; drops on overflow.
    pub fn send(&mut self, p: &Packet) {
        let Some(tx) = &self.tx else { return };
        if p.track == Track::Video {
            if self.waiting_for_keyframe && !p.keyframe {
                self.count_drop();
                return;
            }
            self.waiting_for_keyframe = false;
        }
        match tx.try_send(p.clone()) {
            Ok(()) => {}
            Err(TrySendError::Full(p)) => {
                if p.track == Track::Video {
                    self.waiting_for_keyframe = true;
                }
                self.count_drop();
            }
            Err(TrySendError::Disconnected(_)) => self.tx = None,
        }
    }

    fn count_drop(&self) {
        self.status.lock().unwrap().dropped_packets += 1;
    }

    pub fn status(&self) -> OutputStatus {
        self.status.lock().unwrap().clone()
    }

    /// Flushes queued packets, finalizes the sink and waits for the thread.
    pub fn stop(mut self) -> OutputStatus {
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.status()
    }
}

fn run<F>(open: F, rx: Receiver<Packet>, status: Arc<Mutex<OutputStatus>>)
where
    F: FnOnce() -> Result<Box<dyn PacketSink>>,
{
    let set = |state: OutputState| status.lock().unwrap().state = state;
    let mut sink = match open() {
        Ok(s) => s,
        Err(e) => {
            log::error!("output failed to start: {e}");
            set(OutputState::Failed(e.to_string()));
            return;
        }
    };
    set(OutputState::Running);
    for p in rx.iter() {
        if let Err(e) = sink.write(&p) {
            log::error!("output failed: {e}");
            set(OutputState::Failed(e.to_string()));
            let _ = sink.finish();
            return;
        }
        status.lock().unwrap().bytes = sink.bytes_written();
    }
    match sink.finish() {
        Ok(()) => set(OutputState::Stopped),
        Err(e) => set(OutputState::Failed(e.to_string())),
    }
}
