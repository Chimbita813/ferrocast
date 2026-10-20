//! End-to-end checks of the muxers and the RTMP client against FFmpeg.
//!
//! The fixtures are 2 s of 320x240@30 H.264 (with AUDs) and 48 kHz stereo AAC.
//! Tests are skipped when `ffmpeg`/`ffprobe` aren't installed.

use std::io::Cursor;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ferrocast_media::flv::FlvWriter;
use ferrocast_media::mp4::Mp4Writer;
use ferrocast_media::rtmp::RtmpPublisher;
use ferrocast_media::{aac, h264, AudioInfo, Packet, StreamInfo, Track, VideoInfo};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn have_ffmpeg() -> bool {
    let ok = |bin: &str| {
        Command::new(bin)
            .arg("-version")
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if ok("ffmpeg") && ok("ffprobe") {
        true
    } else {
        eprintln!("ffmpeg/ffprobe not found; skipping");
        false
    }
}

fn info() -> StreamInfo {
    StreamInfo {
        video: VideoInfo {
            width: 320,
            height: 240,
            fps: 30,
            bitrate_kbps: 150,
        },
        audio: Some(AudioInfo {
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 128,
        }),
    }
}

/// Fixture packets interleaved by DTS, with timestamps offset like a live engine clock.
fn packets() -> Vec<Packet> {
    let offset_us = 5_000_000;
    let video = std::fs::read(format!("{FIXTURES}/video_320x240_30fps.h264")).unwrap();
    let mut aus: Vec<Vec<u8>> = Vec::new();
    for nal in h264::split_annexb(&video) {
        if h264::nal_type(nal) == h264::NAL_AUD || aus.is_empty() {
            aus.push(Vec::new());
        }
        let au = aus.last_mut().unwrap();
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(nal);
    }
    let mut out: Vec<Packet> = aus
        .into_iter()
        .enumerate()
        .map(|(i, data)| {
            let ts = offset_us + i as i64 * 1_000_000 / 30;
            Packet {
                track: Track::Video,
                keyframe: h264::is_keyframe(&data),
                data,
                pts_us: ts,
                dts_us: ts,
            }
        })
        .collect();
    let audio = std::fs::read(format!("{FIXTURES}/audio_48k_stereo.aac")).unwrap();
    for (i, f) in aac::parse_adts(&audio).unwrap().into_iter().enumerate() {
        let ts = offset_us + i as i64 * 1024 * 1_000_000 / 48_000;
        out.push(Packet {
            track: Track::Audio,
            data: f.payload.to_vec(),
            pts_us: ts,
            dts_us: ts,
            keyframe: true,
        });
    }
    out.sort_by_key(|p| p.dts_us);
    out
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferrocast-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

struct Probe {
    streams: Vec<(String, u32)>,
    duration: f64,
}

/// Lists (codec, decoded frame count) per stream, and fails on any decode error.
fn probe(path: &Path) -> Probe {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-show_entries",
            "stream=codec_name,nb_read_frames:format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "ffprobe errors: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let mut streams = Vec::new();
    let mut duration = 0.0;
    for line in text.lines().filter(|l| !l.is_empty()) {
        let parts: Vec<&str> = line.split(',').collect();
        match parts.as_slice() {
            [codec, frames] => streams.push((codec.to_string(), frames.parse().unwrap())),
            [d] => duration = d.parse().unwrap(),
            _ => panic!("unexpected ffprobe line {line}"),
        }
    }
    Probe { streams, duration }
}

fn assert_full_av(p: &Probe) {
    assert_eq!(p.streams.len(), 2, "{:?}", p.streams);
    assert!(
        p.streams.contains(&("h264".to_string(), 60)),
        "{:?}",
        p.streams
    );
    let (codec, audio_frames) = &p.streams.iter().find(|s| s.0 == "aac").unwrap();
    assert_eq!(codec, "aac");
    assert!(*audio_frames >= 90, "{:?}", p.streams);
    assert!((1.9..2.2).contains(&p.duration), "duration {}", p.duration);
}

#[test]
fn mp4_plays_in_ffmpeg() {
    if !have_ffmpeg() {
        return;
    }
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), info()).unwrap();
    for p in packets() {
        w.write(&p).unwrap();
    }
    let bytes = w.finish().unwrap().into_inner();
    let path = tmp("out.mp4");
    std::fs::write(&path, bytes).unwrap();
    assert_full_av(&probe(&path));
}

#[test]
fn mp4_skips_until_first_keyframe() {
    if !have_ffmpeg() {
        return;
    }
    // Start mid-GOP: the first 10 video frames are not decodable on their own.
    let pk: Vec<Packet> = packets()
        .into_iter()
        .filter(|p| p.track == Track::Audio || p.dts_us >= 5_000_000 + 10 * 33_333)
        .collect();
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), info()).unwrap();
    for p in &pk {
        w.write(p).unwrap();
    }
    let path = tmp("midgop.mp4");
    std::fs::write(&path, w.finish().unwrap().into_inner()).unwrap();
    let p = probe(&path);
    // GOP is 30 frames, so recording starts at frame 30.
    assert!(
        p.streams.contains(&("h264".to_string(), 30)),
        "{:?}",
        p.streams
    );
}

#[test]
fn flv_plays_in_ffmpeg() {
    if !have_ffmpeg() {
        return;
    }
    let mut w = FlvWriter::new(Vec::new(), info()).unwrap();
    for p in packets() {
        w.write(&p).unwrap();
    }
    let path = tmp("out.flv");
    std::fs::write(&path, w.finish().unwrap()).unwrap();
    assert_full_av(&probe(&path));
}

#[test]
fn rtmp_publish_to_ffmpeg_server() {
    if !have_ffmpeg() {
        return;
    }
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let path = tmp("rtmp.flv");
    let mut server = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-listen", "1", "-f", "flv", "-i"])
        .arg(format!("rtmp://127.0.0.1:{port}/live/test-key"))
        .args(["-c", "copy", "-f", "flv"])
        .arg(&path)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut publisher = loop {
        match RtmpPublisher::connect(&format!("rtmp://127.0.0.1:{port}/live"), "test-key", info()) {
            Ok(p) => break p,
            Err(e) if Instant::now() < deadline => {
                eprintln!("waiting for server: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => panic!("could not connect: {e}"),
        }
    };
    for p in packets() {
        publisher.write(&p).unwrap();
    }
    assert!(publisher.bytes_sent() > 30_000);
    publisher.close();

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = server.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            server.kill().ok();
            panic!("ffmpeg server did not exit");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "ffmpeg server exited with {status}");
    assert_full_av(&probe(&path));
}
