//! RTMP publisher: handshake, chunk stream, `connect`/`publish`, and FLV-tag media.
//!
//! Only plain `rtmp://` is supported. The client sends the simple (version 3)
//! handshake, which Twitch, YouTube, nginx-rtmp and FFmpeg all accept.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::amf0::{self, Value};
use crate::flv::{self, Packetizer};
use crate::{Error, Packet, Result, StreamInfo};

const OUT_CHUNK_SIZE: usize = 4096;
const CSID_CONTROL: u8 = 2;
const CSID_COMMAND: u8 = 3;
const CSID_AUDIO: u8 = 4;
const CSID_DATA: u8 = 5;
const CSID_VIDEO: u8 = 6;
const CSID_STREAM_COMMAND: u8 = 8;

const MSG_SET_CHUNK_SIZE: u8 = 1;
const MSG_ACK: u8 = 3;
const MSG_USER_CONTROL: u8 = 4;
const MSG_WINDOW_ACK_SIZE: u8 = 5;
const MSG_SET_PEER_BANDWIDTH: u8 = 6;
const MSG_COMMAND_AMF0: u8 = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtmpUrl {
    pub host: String,
    pub port: u16,
    pub app: String,
    pub tc_url: String,
}

/// Parses `rtmp://host[:port]/app[/...]`. Everything after the host is the app name.
pub fn parse_url(server: &str) -> Result<RtmpUrl> {
    let server = server.trim();
    let rest = if let Some(r) = server.strip_prefix("rtmp://") {
        r
    } else if server.starts_with("rtmps://") {
        return Err(Error::Rtmp(
            "rtmps:// is not supported yet; use the rtmp:// ingest URL".into(),
        ));
    } else {
        return Err(Error::Rtmp(format!("not an rtmp:// URL: {server}")));
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse()
                .map_err(|_| Error::Rtmp(format!("bad port in {server}")))?,
        ),
        None => (authority, 1935),
    };
    if host.is_empty() {
        return Err(Error::Rtmp(format!("missing host in {server}")));
    }
    let app = path.trim_matches('/').to_string();
    Ok(RtmpUrl {
        host: host.to_string(),
        port,
        tc_url: format!("rtmp://{host}:{port}/{app}"),
        app,
    })
}

/// Serializes one RTMP message into chunks.
fn chunk_message(
    csid: u8,
    msg_type: u8,
    stream_id: u32,
    timestamp: u32,
    payload: &[u8],
    chunk_size: usize,
) -> Vec<u8> {
    let extended = timestamp >= 0x00ff_ffff;
    let mut out = Vec::with_capacity(payload.len() + 16 + payload.len() / chunk_size * 5);
    out.push(csid & 0x3f);
    let ts_field = if extended { 0x00ff_ffff } else { timestamp };
    out.extend_from_slice(&ts_field.to_be_bytes()[1..]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    out.push(msg_type);
    out.extend_from_slice(&stream_id.to_le_bytes());
    if extended {
        out.extend_from_slice(&timestamp.to_be_bytes());
    }
    for (i, chunk) in payload.chunks(chunk_size).enumerate() {
        if i > 0 {
            out.push(0xc0 | (csid & 0x3f));
            if extended {
                out.extend_from_slice(&timestamp.to_be_bytes());
            }
        }
        out.extend_from_slice(chunk);
    }
    out
}

#[derive(Default, Clone)]
struct ChunkState {
    timestamp: u32,
    delta: u32,
    length: usize,
    msg_type: u8,
    stream_id: u32,
    extended: bool,
    payload: Vec<u8>,
}

pub struct Message {
    pub msg_type: u8,
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

/// Incoming chunk stream parser.
struct ChunkReader<R: Read> {
    inner: R,
    chunk_size: usize,
    streams: HashMap<u32, ChunkState>,
    bytes_read: u64,
}

impl<R: Read> ChunkReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            chunk_size: 128,
            streams: HashMap::new(),
            bytes_read: 0,
        }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.inner.read_exact(buf)?;
        self.bytes_read += buf.len() as u64;
        Ok(())
    }

    fn u24(&mut self) -> Result<u32> {
        let mut b = [0u8; 3];
        self.read_exact(&mut b)?;
        Ok(u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }

    fn u32_be(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_exact(&mut b)?;
        Ok(u32::from_be_bytes(b))
    }

    /// Reads chunks until one message is complete.
    fn read_message(&mut self) -> Result<Message> {
        loop {
            let mut b = [0u8; 1];
            self.read_exact(&mut b)?;
            let fmt = b[0] >> 6;
            let csid = match b[0] & 0x3f {
                0 => {
                    self.read_exact(&mut b)?;
                    b[0] as u32 + 64
                }
                1 => {
                    let mut two = [0u8; 2];
                    self.read_exact(&mut two)?;
                    two[0] as u32 + 64 + two[1] as u32 * 256
                }
                n => n as u32,
            };
            let mut st = self.streams.get(&csid).cloned().unwrap_or_default();
            let starting = st.payload.is_empty();
            match fmt {
                0 => {
                    let ts = self.u24()?;
                    st.length = self.u24()? as usize;
                    self.read_exact(&mut b)?;
                    st.msg_type = b[0];
                    let mut sid = [0u8; 4];
                    self.read_exact(&mut sid)?;
                    st.stream_id = u32::from_le_bytes(sid);
                    st.extended = ts == 0x00ff_ffff;
                    st.timestamp = if st.extended { self.u32_be()? } else { ts };
                    st.delta = 0;
                }
                1 | 2 => {
                    let delta = self.u24()?;
                    if fmt == 1 {
                        st.length = self.u24()? as usize;
                        self.read_exact(&mut b)?;
                        st.msg_type = b[0];
                    }
                    st.extended = delta == 0x00ff_ffff;
                    st.delta = if st.extended { self.u32_be()? } else { delta };
                    st.timestamp = st.timestamp.wrapping_add(st.delta);
                }
                _ => {
                    if st.extended {
                        self.u32_be()?;
                    }
                    if starting {
                        st.timestamp = st.timestamp.wrapping_add(st.delta);
                    }
                }
            }
            let want = (st.length - st.payload.len()).min(self.chunk_size);
            let mut buf = vec![0u8; want];
            self.read_exact(&mut buf)?;
            st.payload.extend_from_slice(&buf);
            if st.payload.len() >= st.length {
                let payload = std::mem::take(&mut st.payload);
                let msg = Message {
                    msg_type: st.msg_type,
                    timestamp: st.timestamp,
                    payload,
                };
                self.streams.insert(csid, st);
                if msg.msg_type == MSG_SET_CHUNK_SIZE && msg.payload.len() >= 4 {
                    let size =
                        u32::from_be_bytes(msg.payload[..4].try_into().unwrap()) & 0x7fff_ffff;
                    self.chunk_size = size.max(1) as usize;
                }
                return Ok(msg);
            }
            self.streams.insert(csid, st);
        }
    }
}

type SharedWriter = Arc<Mutex<TcpStream>>;

fn send(
    w: &SharedWriter,
    csid: u8,
    msg_type: u8,
    stream_id: u32,
    ts: u32,
    payload: &[u8],
) -> Result<()> {
    let bytes = chunk_message(csid, msg_type, stream_id, ts, payload, OUT_CHUNK_SIZE);
    w.lock().unwrap().write_all(&bytes)?;
    Ok(())
}

/// Answers pings and acknowledgements. Returns the command messages it didn't consume.
fn handle_control<R: Read>(
    reader: &mut ChunkReader<R>,
    w: &SharedWriter,
    msg: &Message,
    ack_window: &mut u32,
    last_ack: &mut u64,
) -> Result<()> {
    match msg.msg_type {
        MSG_WINDOW_ACK_SIZE if msg.payload.len() >= 4 => {
            *ack_window = u32::from_be_bytes(msg.payload[..4].try_into().unwrap());
        }
        MSG_SET_PEER_BANDWIDTH if msg.payload.len() >= 4 => {
            send(
                w,
                CSID_CONTROL,
                MSG_WINDOW_ACK_SIZE,
                0,
                0,
                &msg.payload[..4],
            )?;
        }
        MSG_USER_CONTROL if msg.payload.len() >= 6 && msg.payload[..2] == [0, 6] => {
            // PingRequest -> PingResponse with the same timestamp.
            let mut pong = vec![0, 7];
            pong.extend_from_slice(&msg.payload[2..6]);
            send(w, CSID_CONTROL, MSG_USER_CONTROL, 0, 0, &pong)?;
        }
        _ => {}
    }
    if *ack_window > 0 && reader.bytes_read - *last_ack >= *ack_window as u64 {
        *last_ack = reader.bytes_read;
        send(
            w,
            CSID_CONTROL,
            MSG_ACK,
            0,
            0,
            &(reader.bytes_read as u32).to_be_bytes(),
        )?;
    }
    Ok(())
}

/// A live RTMP publish session.
pub struct RtmpPublisher {
    writer: SharedWriter,
    stream_id: u32,
    key: String,
    info: StreamInfo,
    packetizer: Packetizer,
    closed: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    bytes_sent: u64,
}

impl RtmpPublisher {
    /// Connects, publishes `key` and sends stream metadata.
    pub fn connect(server: &str, key: &str, info: StreamInfo) -> Result<Self> {
        let url = parse_url(server)?;
        let addr = (url.host.as_str(), url.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| Error::Rtmp(format!("could not resolve {}", url.host)))?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;

        let mut read_half = stream.try_clone()?;
        let writer: SharedWriter = Arc::new(Mutex::new(stream));
        handshake(&mut read_half, &writer)?;
        let mut reader = ChunkReader::new(read_half);

        send(
            &writer,
            CSID_CONTROL,
            MSG_SET_CHUNK_SIZE,
            0,
            0,
            &(OUT_CHUNK_SIZE as u32).to_be_bytes(),
        )?;
        let mut ack_window = 0u32;
        let mut last_ack = 0u64;
        let command = |name: &str, txn: f64, args: Vec<Value>, csid: u8, sid: u32| -> Result<()> {
            let mut values = vec![Value::str(name), Value::Number(txn)];
            values.extend(args);
            send(
                &writer,
                csid,
                MSG_COMMAND_AMF0,
                sid,
                0,
                &amf0::encode(&values),
            )
        };
        command(
            "connect",
            1.0,
            vec![Value::Object(vec![
                ("app".into(), Value::str(&url.app)),
                ("type".into(), Value::str("nonprivate")),
                (
                    "flashVer".into(),
                    Value::str("FMLE/3.0 (compatible; FMSc/1.0)"),
                ),
                ("swfUrl".into(), Value::str(&url.tc_url)),
                ("tcUrl".into(), Value::str(&url.tc_url)),
            ])],
            CSID_COMMAND,
            0,
        )?;
        let mut wait_for = |reader: &mut ChunkReader<TcpStream>,
                            done: &dyn Fn(&[Value]) -> Option<Result<f64>>|
         -> Result<f64> {
            loop {
                let msg = reader.read_message()?;
                handle_control(reader, &writer, &msg, &mut ack_window, &mut last_ack)?;
                if msg.msg_type == MSG_COMMAND_AMF0 {
                    let values = amf0::decode_all(&msg.payload)?;
                    if let Some(r) = done(&values) {
                        return r;
                    }
                }
            }
        };
        let result_for = |txn: f64| {
            move |v: &[Value]| -> Option<Result<f64>> {
                let name = v.first()?.as_str()?;
                if v.get(1)?.as_number()? != txn {
                    return None;
                }
                match name {
                    "_result" => Some(Ok(v.get(3).and_then(Value::as_number).unwrap_or(0.0))),
                    "_error" => {
                        let desc = v
                            .get(3)
                            .and_then(|i| i.get("description"))
                            .and_then(Value::as_str)
                            .unwrap_or("rejected");
                        Some(Err(Error::Rtmp(format!("server refused connect: {desc}"))))
                    }
                    _ => None,
                }
            }
        };
        wait_for(&mut reader, &result_for(1.0))?;

        command(
            "releaseStream",
            2.0,
            vec![Value::Null, Value::str(key)],
            CSID_COMMAND,
            0,
        )?;
        command(
            "FCPublish",
            3.0,
            vec![Value::Null, Value::str(key)],
            CSID_COMMAND,
            0,
        )?;
        command("createStream", 4.0, vec![Value::Null], CSID_COMMAND, 0)?;
        let stream_id = wait_for(&mut reader, &result_for(4.0))? as u32;

        command(
            "publish",
            5.0,
            vec![Value::Null, Value::str(key), Value::str("live")],
            CSID_STREAM_COMMAND,
            stream_id,
        )?;
        wait_for(&mut reader, &|v: &[Value]| {
            if v.first()?.as_str()? != "onStatus" {
                return None;
            }
            let info = v.get(3)?;
            let code = info.get("code").and_then(Value::as_str).unwrap_or("");
            match code {
                "NetStream.Publish.Start" => Some(Ok(0.0)),
                c if info.get("level").and_then(Value::as_str) == Some("error")
                    || c.contains("Failed")
                    || c.contains("BadName") =>
                {
                    let desc = info.get("description").and_then(Value::as_str).unwrap_or(c);
                    Some(Err(Error::Rtmp(format!("publish rejected: {desc}"))))
                }
                _ => None,
            }
        })?;

        // From here on, a background thread drains whatever the server sends.
        let closed = Arc::new(AtomicBool::new(false));
        reader.inner.set_read_timeout(None)?;
        let reader_thread = {
            let writer = writer.clone();
            let closed = closed.clone();
            std::thread::Builder::new()
                .name("rtmp-reader".into())
                .spawn(move || {
                    while !closed.load(Ordering::Relaxed) {
                        match reader.read_message() {
                            Ok(msg) => {
                                if handle_control(
                                    &mut reader,
                                    &writer,
                                    &msg,
                                    &mut ack_window,
                                    &mut last_ack,
                                )
                                .is_err()
                                {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    closed.store(true, Ordering::Relaxed);
                })?
        };

        let mut publisher = Self {
            writer,
            stream_id,
            key: key.to_string(),
            info,
            packetizer: Packetizer::new(info),
            closed,
            reader: Some(reader_thread),
            bytes_sent: 0,
        };
        let meta = flv::script_tag(&publisher.info, true);
        publisher.send_tag(CSID_DATA, &meta)?;
        Ok(publisher)
    }

    fn send_tag(&mut self, csid: u8, tag: &flv::Tag) -> Result<()> {
        send(
            &self.writer,
            csid,
            tag.kind,
            self.stream_id,
            tag.timestamp_ms,
            &tag.data,
        )?;
        self.bytes_sent += tag.data.len() as u64;
        Ok(())
    }

    /// Sends one encoded packet. Fails once the connection is gone.
    pub fn write(&mut self, p: &Packet) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Rtmp("connection closed by server".into()));
        }
        for tag in self.packetizer.push(p) {
            let csid = if tag.kind == flv::TAG_AUDIO {
                CSID_AUDIO
            } else {
                CSID_VIDEO
            };
            self.send_tag(csid, &tag)?;
        }
        Ok(())
    }

    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }

    /// Politely ends the publish and closes the socket.
    pub fn close(mut self) {
        let key = self.key.clone();
        let cmd = |name: &str, args: Vec<Value>| {
            let mut v = vec![Value::str(name), Value::Number(0.0), Value::Null];
            v.extend(args);
            amf0::encode(&v)
        };
        let _ = send(
            &self.writer,
            CSID_COMMAND,
            MSG_COMMAND_AMF0,
            0,
            0,
            &cmd("FCUnpublish", vec![Value::str(&key)]),
        );
        let _ = send(
            &self.writer,
            CSID_COMMAND,
            MSG_COMMAND_AMF0,
            0,
            0,
            &cmd("deleteStream", vec![Value::Number(self.stream_id as f64)]),
        );
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        if let Ok(s) = self.writer.lock() {
            let _ = s.shutdown(Shutdown::Both);
        }
        if let Some(t) = self.reader.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RtmpPublisher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn handshake(read: &mut TcpStream, w: &SharedWriter) -> Result<()> {
    let mut c0c1 = vec![0u8; 1537];
    c0c1[0] = 3;
    // C1: time (0), zero, then 1528 bytes that only need to be unpredictable-ish.
    let mut x: u32 = 0x9e37_79b9 ^ std::process::id();
    for b in c0c1[9..].iter_mut() {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *b = x as u8;
    }
    w.lock().unwrap().write_all(&c0c1)?;
    let mut s0s1 = vec![0u8; 1537];
    read.read_exact(&mut s0s1)?;
    if s0s1[0] != 3 {
        return Err(Error::Rtmp(format!("unsupported RTMP version {}", s0s1[0])));
    }
    w.lock().unwrap().write_all(&s0s1[1..])?; // C2 echoes S1
    let mut s2 = vec![0u8; 1536];
    read.read_exact(&mut s2)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = parse_url("rtmp://live.twitch.tv/app").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.app.as_str()),
            ("live.twitch.tv", 1935, "app")
        );
        assert_eq!(u.tc_url, "rtmp://live.twitch.tv:1935/app");
        let u = parse_url("rtmp://127.0.0.1:19350/live/sub/").unwrap();
        assert_eq!((u.port, u.app.as_str()), (19350, "live/sub"));
        assert!(parse_url("rtmps://x/app").is_err());
        assert!(parse_url("http://x/app").is_err());
    }

    #[test]
    fn chunking_roundtrips_through_reader() {
        let payload: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let mut bytes = chunk_message(6, 9, 1, 0x0100_0000, &payload, 4096);
        bytes.extend(chunk_message(3, 20, 0, 5, b"hi", 4096));
        let mut r = ChunkReader::new(&bytes[..]);
        r.chunk_size = 4096;
        let m = r.read_message().unwrap();
        assert_eq!(
            (m.msg_type, m.timestamp, m.payload.len()),
            (9, 0x0100_0000, 10_000)
        );
        assert_eq!(m.payload, payload);
        let m = r.read_message().unwrap();
        assert_eq!(
            (m.msg_type, m.timestamp, &m.payload[..]),
            (20, 5, &b"hi"[..])
        );
    }
}
