//! Audio mixing: per-source resampling to the output format, gain, metering.
//!
//! Capture threads [`Mixer::push`] whatever the device delivers; a clocked
//! thread calls [`Mixer::mix`] for a fixed number of frames. A source that
//! has nothing buffered (e.g. desktop loopback during silence, which delivers
//! no packets at all) simply contributes silence.

use std::collections::{HashMap, VecDeque};

/// Mixer output rate. AAC at 48 kHz is what every platform expects.
pub const SAMPLE_RATE: u32 = 48_000;
/// Mixer output channel count (interleaved stereo).
pub const CHANNELS: usize = 2;

/// Linear-interpolating resampler with channel up/down-mixing to stereo.
#[derive(Clone, Debug)]
pub struct Resampler {
    in_rate: u32,
    in_channels: usize,
    /// Position of the next output frame, in input frames, relative to `prev`.
    pos: f64,
    /// Last frame of the previous call; `None` before the first call.
    prev: Option<[f32; 2]>,
}

impl Resampler {
    pub fn new(in_rate: u32, in_channels: usize) -> Self {
        Self {
            in_rate,
            in_channels: in_channels.max(1),
            pos: 0.0,
            prev: None,
        }
    }

    fn frame(&self, input: &[f32], i: usize) -> [f32; 2] {
        let f = &input[i * self.in_channels..(i + 1) * self.in_channels];
        match self.in_channels {
            1 => [f[0], f[0]],
            _ => [f[0], f[1]],
        }
    }

    /// Converts interleaved input frames into interleaved stereo at [`SAMPLE_RATE`].
    pub fn process(&mut self, input: &[f32], out: &mut VecDeque<f32>) {
        let frames = input.len() / self.in_channels;
        if frames == 0 {
            return;
        }
        if self.in_rate == SAMPLE_RATE {
            for i in 0..frames {
                out.extend(self.frame(input, i));
            }
            return;
        }
        let step = self.in_rate as f64 / SAMPLE_RATE as f64;
        let prev = self.prev.unwrap_or_else(|| self.frame(input, 0));
        // Input index -1 is `prev` (last frame of the previous call).
        while self.pos < frames as f64 {
            let idx = self.pos.floor();
            let t = (self.pos - idx) as f32;
            let i = idx as isize - 1;
            let a = if i < 0 {
                prev
            } else {
                self.frame(input, i as usize)
            };
            let b = self.frame(input, (i + 1) as usize);
            out.push_back(a[0] + (b[0] - a[0]) * t);
            out.push_back(a[1] + (b[1] - a[1]) * t);
            self.pos += step;
        }
        self.pos -= frames as f64;
        self.prev = Some(self.frame(input, frames - 1));
    }
}

struct SourceState {
    resampler: Resampler,
    buffer: VecDeque<f32>,
    gain: f32,
    peak: f32,
}

/// Mixes any number of sources into 48 kHz stereo.
pub struct Mixer {
    sources: HashMap<u64, SourceState>,
    /// Upper bound on buffered audio per source, in frames. Older audio is dropped.
    max_latency_frames: usize,
}

impl Default for Mixer {
    fn default() -> Self {
        Self {
            sources: HashMap::new(),
            max_latency_frames: SAMPLE_RATE as usize / 5,
        }
    }
}

impl Mixer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds captured audio. Resets the source's resampler if its format changed.
    pub fn push(&mut self, source: u64, rate: u32, channels: usize, samples: &[f32]) {
        let st = self.sources.entry(source).or_insert_with(|| SourceState {
            resampler: Resampler::new(rate, channels),
            buffer: VecDeque::new(),
            gain: 1.0,
            peak: 0.0,
        });
        if st.resampler.in_rate != rate || st.resampler.in_channels != channels.max(1) {
            st.resampler = Resampler::new(rate, channels);
        }
        st.resampler.process(samples, &mut st.buffer);
        let max = self.max_latency_frames * CHANNELS;
        if st.buffer.len() > max {
            let excess = st.buffer.len() - max;
            st.buffer.drain(..excess);
        }
    }

    /// Sets linear gain (0 mutes).
    pub fn set_gain(&mut self, source: u64, gain: f32) {
        if let Some(st) = self.sources.get_mut(&source) {
            st.gain = gain;
        } else {
            self.sources.insert(
                source,
                SourceState {
                    resampler: Resampler::new(SAMPLE_RATE, CHANNELS),
                    buffer: VecDeque::new(),
                    gain,
                    peak: 0.0,
                },
            );
        }
    }

    pub fn remove(&mut self, source: u64) {
        self.sources.remove(&source);
    }

    /// Produces `frames` interleaved stereo frames, consuming buffered input.
    pub fn mix(&mut self, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; frames * CHANNELS];
        for st in self.sources.values_mut() {
            let n = st.buffer.len().min(out.len());
            let mut peak = 0.0f32;
            for (o, s) in out.iter_mut().zip(st.buffer.drain(..n)) {
                let v = s * st.gain;
                peak = peak.max(v.abs());
                *o += v;
            }
            st.peak = st.peak.max(peak);
        }
        for s in &mut out {
            *s = s.clamp(-1.0, 1.0);
        }
        out
    }

    /// Peak level per source since the last call, in dBFS (-inf for silence).
    pub fn take_peaks(&mut self) -> Vec<(u64, f32)> {
        self.sources
            .iter_mut()
            .map(|(&id, st)| (id, 20.0 * std::mem::take(&mut st.peak).log10()))
            .collect()
    }
}

/// Converts float samples to signed 16-bit PCM bytes (little endian).
pub fn to_i16_le(samples: &[f32], out: &mut Vec<u8>) {
    out.reserve(samples.len() * 2);
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_and_mono_upmix() {
        let mut out = VecDeque::new();
        Resampler::new(48_000, 1).process(&[0.5, -0.5], &mut out);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn resampling_preserves_duration_across_calls() {
        let mut r = Resampler::new(44_100, 2);
        let mut out = VecDeque::new();
        // One second of 44.1 kHz audio in 10 ms chunks.
        let chunk = vec![0.25f32; 441 * 2];
        for _ in 0..100 {
            r.process(&chunk, &mut out);
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames} frames");
        assert!(out.iter().all(|&s| (s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn mixes_with_gain_and_pads_silence() {
        let mut m = Mixer::new();
        m.push(1, 48_000, 2, &[0.5, 0.5, 0.5, 0.5]);
        m.push(2, 48_000, 2, &[0.25, 0.25]);
        m.set_gain(2, 2.0);
        let out = m.mix(3);
        assert_eq!(out, vec![1.0, 1.0, 0.5, 0.5, 0.0, 0.0]);
        let peaks: HashMap<u64, f32> = m.take_peaks().into_iter().collect();
        assert!((peaks[&1] - 20.0 * 0.5f32.log10()).abs() < 1e-4);
        assert_eq!(peaks[&2], 20.0 * 0.5f32.log10());
        assert!(m.take_peaks().iter().all(|(_, p)| *p == f32::NEG_INFINITY));
    }

    #[test]
    fn bounds_latency() {
        let mut m = Mixer::new();
        m.push(1, 48_000, 2, &vec![0.1; 48_000 * 2]);
        assert_eq!(
            m.sources[&1].buffer.len(),
            (SAMPLE_RATE as usize / 5) * CHANNELS
        );
    }

    #[test]
    fn i16_conversion_clamps() {
        let mut out = Vec::new();
        to_i16_le(&[1.5, -1.0, 0.0], &mut out);
        assert_eq!(out, [0xff, 0x7f, 0x01, 0x80, 0, 0]);
    }
}
