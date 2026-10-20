//! Progressive MP4 writer (H.264 + AAC).
//!
//! Samples are streamed into a single `mdat` as they arrive; only the sample
//! tables (a few bytes per frame) are kept in memory and written as `moov`
//! on [`Mp4Writer::finish`]. A recording that is never finished (crash,
//! power loss) has no `moov` and needs repair, which is the usual trade-off
//! for plain MP4.

use std::io::{Seek, SeekFrom, Write};

use crate::h264::{self, AvcConfig};
use crate::{aac, Packet, Result, StreamInfo, Track};

const MOVIE_TIMESCALE: u32 = 1000;
const VIDEO_TIMESCALE: u32 = 90_000;

struct Sample {
    offset: u64,
    size: u32,
    dts: i64,
    cts: i64,
    sync: bool,
}

struct TrackState {
    timescale: u32,
    default_duration: u32,
    samples: Vec<Sample>,
}

impl TrackState {
    fn new(timescale: u32, default_duration: u32) -> Self {
        Self {
            timescale,
            default_duration,
            samples: Vec::new(),
        }
    }

    fn ticks(&self, us: i64) -> i64 {
        (us as i128 * self.timescale as i128 / 1_000_000) as i64
    }

    fn push(&mut self, offset: u64, size: u32, dts_us: i64, pts_us: i64, sync: bool) {
        let mut dts = self.ticks(dts_us);
        if let Some(last) = self.samples.last() {
            dts = dts.max(last.dts + 1);
        }
        let cts = (self.ticks(pts_us) - self.ticks(dts_us)).max(0);
        self.samples.push(Sample {
            offset,
            size,
            dts,
            cts,
            sync,
        });
    }

    fn durations(&self) -> Vec<u32> {
        let mut d: Vec<u32> = self
            .samples
            .windows(2)
            .map(|w| (w[1].dts - w[0].dts) as u32)
            .collect();
        if !self.samples.is_empty() {
            d.push(d.last().copied().unwrap_or(self.default_duration));
        }
        d
    }

    fn start(&self) -> i64 {
        self.samples.first().map_or(0, |s| s.dts)
    }

    fn media_duration(&self) -> u64 {
        self.durations().iter().map(|&d| d as u64).sum()
    }

    fn to_movie(&self, ticks: i64) -> u64 {
        (ticks.max(0) as u128 * MOVIE_TIMESCALE as u128 / self.timescale as u128) as u64
    }
}

pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    info: StreamInfo,
    mdat_start: u64,
    offset: u64,
    base_us: Option<i64>,
    avc: Option<AvcConfig>,
    size: (u32, u32),
    video: TrackState,
    audio: Option<TrackState>,
}

impl<W: Write + Seek> Mp4Writer<W> {
    pub fn new(mut out: W, info: StreamInfo) -> Result<Self> {
        let ftyp = mp4_box(b"ftyp", |b| {
            b.extend_from_slice(b"isom");
            b.extend_from_slice(&512u32.to_be_bytes());
            for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
                b.extend_from_slice(brand);
            }
        });
        out.write_all(&ftyp)?;
        let mdat_start = ftyp.len() as u64;
        // 64-bit "largesize" mdat so recordings can exceed 4 GiB; size patched in finish().
        out.write_all(&[0, 0, 0, 1, b'm', b'd', b'a', b't', 0, 0, 0, 0, 0, 0, 0, 0])?;
        let fps = info.video.fps.max(1);
        Ok(Self {
            out,
            mdat_start,
            offset: mdat_start + 16,
            base_us: None,
            avc: None,
            size: (info.video.width, info.video.height),
            video: TrackState::new(VIDEO_TIMESCALE, VIDEO_TIMESCALE / fps),
            audio: info
                .audio
                .map(|a| TrackState::new(a.sample_rate, aac::FRAME_SAMPLES)),
            info,
        })
    }

    /// Appends one packet. Everything before the first video keyframe is skipped.
    pub fn write(&mut self, p: &Packet) -> Result<()> {
        if self.base_us.is_none() {
            if p.track != Track::Video || !p.keyframe {
                return Ok(());
            }
            let Some(cfg) = AvcConfig::from_annexb(&p.data) else {
                return Ok(());
            };
            if let Ok(dims) = h264::sps_dimensions(&cfg.sps) {
                self.size = dims;
            }
            self.avc = Some(cfg);
            self.base_us = Some(p.dts_us);
        }
        let base = self.base_us.unwrap_or(0);
        if p.dts_us < base {
            return Ok(());
        }
        let (track, data) = match p.track {
            Track::Video => (&mut self.video, h264::annexb_to_avcc(&p.data)),
            Track::Audio => match self.audio.as_mut() {
                Some(a) => (a, p.data.clone()),
                None => return Ok(()),
            },
        };
        if data.is_empty() {
            return Ok(());
        }
        let sync = p.track == Track::Audio || p.keyframe;
        track.push(
            self.offset,
            data.len() as u32,
            p.dts_us - base,
            p.pts_us - base,
            sync,
        );
        self.out.write_all(&data)?;
        self.offset += data.len() as u64;
        Ok(())
    }

    /// Patches the `mdat` size and appends the `moov` index.
    pub fn finish(mut self) -> Result<W> {
        self.out.seek(SeekFrom::Start(self.mdat_start + 8))?;
        self.out
            .write_all(&(self.offset - self.mdat_start).to_be_bytes())?;
        self.out.seek(SeekFrom::Start(self.offset))?;
        if self.avc.is_some() {
            let moov = self.moov();
            self.out.write_all(&moov)?;
        }
        self.out.flush()?;
        Ok(self.out)
    }

    fn moov(&self) -> Vec<u8> {
        let mut traks = vec![self.trak(1, &self.video, true)];
        if let Some(a) = &self.audio {
            if !a.samples.is_empty() {
                traks.push(self.trak(2, a, false));
            }
        }
        let duration = std::iter::once(&self.video)
            .chain(self.audio.as_ref())
            .map(|t| t.to_movie(t.start() + t.media_duration() as i64))
            .max()
            .unwrap_or(0);
        mp4_box(b"moov", |b| {
            b.extend(full_box(b"mvhd", 1, 0, |b| {
                b.extend_from_slice(&[0; 16]); // creation + modification time
                b.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
                b.extend_from_slice(&duration.to_be_bytes());
                b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
                b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
                b.extend_from_slice(&[0; 10]);
                matrix(b);
                b.extend_from_slice(&[0; 24]);
                b.extend_from_slice(&3u32.to_be_bytes()); // next_track_ID
            }));
            for t in &traks {
                b.extend_from_slice(t);
            }
        })
    }

    fn trak(&self, id: u32, t: &TrackState, video: bool) -> Vec<u8> {
        let media_duration = t.media_duration();
        let start = t.start();
        let first_cts = t.samples.first().map_or(0, |s| s.cts);
        let movie_duration = t.to_movie(media_duration as i64);
        mp4_box(b"trak", |b| {
            b.extend(full_box(b"tkhd", 1, 3, |b| {
                b.extend_from_slice(&[0; 16]);
                b.extend_from_slice(&id.to_be_bytes());
                b.extend_from_slice(&[0; 4]);
                b.extend_from_slice(&(t.to_movie(start) + movie_duration).to_be_bytes());
                b.extend_from_slice(&[0; 8]);
                b.extend_from_slice(&[0; 4]); // layer, alternate_group
                b.extend_from_slice(&(if video { 0u16 } else { 0x0100 }).to_be_bytes());
                b.extend_from_slice(&[0; 2]);
                matrix(b);
                let (w, h) = if video { self.size } else { (0, 0) };
                b.extend_from_slice(&(w << 16).to_be_bytes());
                b.extend_from_slice(&(h << 16).to_be_bytes());
            }));
            // Edit list: delay a track that starts late, and skip composition offset.
            b.extend(mp4_box(b"edts", |b| {
                let mut entries: Vec<(u32, i32)> = Vec::new();
                if start > 0 {
                    entries.push((t.to_movie(start) as u32, -1));
                }
                entries.push((movie_duration as u32, first_cts as i32));
                b.extend(full_box(b"elst", 0, 0, |b| {
                    b.extend_from_slice(&(entries.len() as u32).to_be_bytes());
                    for (dur, media_time) in entries {
                        b.extend_from_slice(&dur.to_be_bytes());
                        b.extend_from_slice(&media_time.to_be_bytes());
                        b.extend_from_slice(&0x0001_0000u32.to_be_bytes());
                    }
                }));
            }));
            b.extend(mp4_box(b"mdia", |b| {
                b.extend(full_box(b"mdhd", 1, 0, |b| {
                    b.extend_from_slice(&[0; 16]);
                    b.extend_from_slice(&t.timescale.to_be_bytes());
                    b.extend_from_slice(&media_duration.to_be_bytes());
                    b.extend_from_slice(&0x55c4u16.to_be_bytes()); // "und"
                    b.extend_from_slice(&[0; 2]);
                }));
                b.extend(full_box(b"hdlr", 0, 0, |b| {
                    b.extend_from_slice(&[0; 4]);
                    b.extend_from_slice(if video { b"vide" } else { b"soun" });
                    b.extend_from_slice(&[0; 12]);
                    b.extend_from_slice(if video {
                        b"VideoHandler\0"
                    } else {
                        b"SoundHandler\0"
                    });
                }));
                b.extend(mp4_box(b"minf", |b| {
                    if video {
                        b.extend(full_box(b"vmhd", 0, 1, |b| b.extend_from_slice(&[0; 8])));
                    } else {
                        b.extend(full_box(b"smhd", 0, 0, |b| b.extend_from_slice(&[0; 4])));
                    }
                    b.extend(mp4_box(b"dinf", |b| {
                        b.extend(full_box(b"dref", 0, 0, |b| {
                            b.extend_from_slice(&1u32.to_be_bytes());
                            b.extend(full_box(b"url ", 0, 1, |_| {}));
                        }));
                    }));
                    b.extend(self.stbl(t, video));
                }));
            }));
        })
    }

    fn stbl(&self, t: &TrackState, video: bool) -> Vec<u8> {
        let durations = t.durations();
        mp4_box(b"stbl", |b| {
            b.extend(full_box(b"stsd", 0, 0, |b| {
                b.extend_from_slice(&1u32.to_be_bytes());
                if video {
                    b.extend(self.avc1());
                } else {
                    b.extend(self.mp4a());
                }
            }));
            b.extend(full_box(b"stts", 0, 0, |b| {
                let runs = run_lengths(durations.iter().map(|&d| d as i64));
                b.extend_from_slice(&(runs.len() as u32).to_be_bytes());
                for (count, delta) in runs {
                    b.extend_from_slice(&count.to_be_bytes());
                    b.extend_from_slice(&(delta as u32).to_be_bytes());
                }
            }));
            if t.samples.iter().any(|s| s.cts != 0) {
                b.extend(full_box(b"ctts", 0, 0, |b| {
                    let runs = run_lengths(t.samples.iter().map(|s| s.cts));
                    b.extend_from_slice(&(runs.len() as u32).to_be_bytes());
                    for (count, cts) in runs {
                        b.extend_from_slice(&count.to_be_bytes());
                        b.extend_from_slice(&(cts as u32).to_be_bytes());
                    }
                }));
            }
            if video {
                b.extend(full_box(b"stss", 0, 0, |b| {
                    let sync: Vec<u32> = t
                        .samples
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.sync)
                        .map(|(i, _)| i as u32 + 1)
                        .collect();
                    b.extend_from_slice(&(sync.len() as u32).to_be_bytes());
                    for n in sync {
                        b.extend_from_slice(&n.to_be_bytes());
                    }
                }));
            }
            // One sample per chunk keeps the tables trivial; co64 gives each its offset.
            b.extend(full_box(b"stsc", 0, 0, |b| {
                for v in [1u32, 1, 1, 1] {
                    b.extend_from_slice(&v.to_be_bytes());
                }
            }));
            b.extend(full_box(b"stsz", 0, 0, |b| {
                b.extend_from_slice(&0u32.to_be_bytes());
                b.extend_from_slice(&(t.samples.len() as u32).to_be_bytes());
                for s in &t.samples {
                    b.extend_from_slice(&s.size.to_be_bytes());
                }
            }));
            b.extend(full_box(b"co64", 0, 0, |b| {
                b.extend_from_slice(&(t.samples.len() as u32).to_be_bytes());
                for s in &t.samples {
                    b.extend_from_slice(&s.offset.to_be_bytes());
                }
            }));
        })
    }

    fn avc1(&self) -> Vec<u8> {
        let (w, h) = self.size;
        mp4_box(b"avc1", |b| {
            b.extend_from_slice(&[0; 6]);
            b.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
            b.extend_from_slice(&[0; 16]);
            b.extend_from_slice(&(w as u16).to_be_bytes());
            b.extend_from_slice(&(h as u16).to_be_bytes());
            b.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi
            b.extend_from_slice(&0x0048_0000u32.to_be_bytes());
            b.extend_from_slice(&[0; 4]);
            b.extend_from_slice(&1u16.to_be_bytes()); // frame_count
            b.extend_from_slice(&[0; 32]); // compressorname
            b.extend_from_slice(&0x0018u16.to_be_bytes());
            b.extend_from_slice(&0xffffu16.to_be_bytes());
            if let Some(cfg) = &self.avc {
                b.extend(mp4_box(b"avcC", |b| b.extend_from_slice(&cfg.record())));
            }
        })
    }

    fn mp4a(&self) -> Vec<u8> {
        let a = self.info.audio.expect("audio track without audio info");
        let asc = aac::audio_specific_config(a.sample_rate, a.channels);
        let bitrate = a.bitrate_kbps * 1000;
        mp4_box(b"mp4a", |b| {
            b.extend_from_slice(&[0; 6]);
            b.extend_from_slice(&1u16.to_be_bytes());
            b.extend_from_slice(&[0; 8]);
            b.extend_from_slice(&a.channels.to_be_bytes());
            b.extend_from_slice(&16u16.to_be_bytes());
            b.extend_from_slice(&[0; 4]);
            b.extend_from_slice(&(a.sample_rate << 16).to_be_bytes());
            b.extend(full_box(b"esds", 0, 0, |b| {
                let mut dsi = vec![0x05, asc.len() as u8];
                dsi.extend_from_slice(&asc);
                let mut dcd = vec![0x40, 0x15, 0, 0x18, 0]; // AAC, audio stream, 6 KiB buffer
                dcd.extend_from_slice(&bitrate.to_be_bytes());
                dcd.extend_from_slice(&bitrate.to_be_bytes());
                dcd.extend_from_slice(&dsi);
                let mut es = vec![0, 2, 0]; // ES_ID, flags
                es.push(0x04);
                es.push(dcd.len() as u8);
                es.extend_from_slice(&dcd);
                es.extend_from_slice(&[0x06, 1, 2]); // SLConfigDescriptor
                b.push(0x03);
                b.push(es.len() as u8);
                b.extend_from_slice(&es);
            }));
        })
    }
}

fn mp4_box(kind: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut b = vec![0, 0, 0, 0];
    b.extend_from_slice(kind);
    body(&mut b);
    let len = b.len() as u32;
    b[..4].copy_from_slice(&len.to_be_bytes());
    b
}

fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    mp4_box(kind, |b| {
        b.push(version);
        b.extend_from_slice(&flags.to_be_bytes()[1..]);
        body(b);
    })
}

fn matrix(b: &mut Vec<u8>) {
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        b.extend_from_slice(&v.to_be_bytes());
    }
}

fn run_lengths(values: impl Iterator<Item = i64>) -> Vec<(u32, i64)> {
    let mut runs: Vec<(u32, i64)> = Vec::new();
    for v in values {
        match runs.last_mut() {
            Some((count, last)) if *last == v => *count += 1,
            _ => runs.push((1, v)),
        }
    }
    runs
}
