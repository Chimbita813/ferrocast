//! H.264 bitstream helpers: Annex B splitting, AVCC conversion, SPS parsing.

use crate::{Error, Result};

pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// Splits an Annex B buffer into NAL units (without start codes).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                // A 4-byte start code leaves a trailing zero on the previous NAL.
                let mut end = i;
                while end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                nals.push(&data[s..end]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    match start {
        Some(s) if s < data.len() => nals.push(&data[s..]),
        None if !data.is_empty() => nals.push(data),
        _ => {}
    }
    nals.retain(|n| !n.is_empty());
    nals
}

/// Parameter sets needed for `avcC` / the FLV sequence header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvcConfig {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

impl AvcConfig {
    /// Pulls SPS and PPS out of an Annex B access unit, if both are present.
    pub fn from_annexb(data: &[u8]) -> Option<AvcConfig> {
        let mut sps = None;
        let mut pps = None;
        for nal in split_annexb(data) {
            match nal_type(nal) {
                NAL_SPS if sps.is_none() => sps = Some(nal.to_vec()),
                NAL_PPS if pps.is_none() => pps = Some(nal.to_vec()),
                _ => {}
            }
        }
        Some(AvcConfig {
            sps: sps?,
            pps: pps?,
        })
    }

    /// AVCDecoderConfigurationRecord (ISO 14496-15 5.2.4.1).
    pub fn record(&self) -> Vec<u8> {
        let mut r = Vec::with_capacity(11 + self.sps.len() + self.pps.len());
        r.push(1);
        r.push(self.sps.get(1).copied().unwrap_or(0x64)); // profile
        r.push(self.sps.get(2).copied().unwrap_or(0)); // constraint flags
        r.push(self.sps.get(3).copied().unwrap_or(0x28)); // level
        r.push(0xFC | 3); // 4-byte NAL lengths
        r.push(0xE0 | 1); // one SPS
        r.extend_from_slice(&(self.sps.len() as u16).to_be_bytes());
        r.extend_from_slice(&self.sps);
        r.push(1); // one PPS
        r.extend_from_slice(&(self.pps.len() as u16).to_be_bytes());
        r.extend_from_slice(&self.pps);
        r
    }
}

/// Converts an Annex B access unit to 4-byte length-prefixed NAL units,
/// dropping access unit delimiters and parameter sets (they live in `avcC`).
pub fn annexb_to_avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for nal in split_annexb(data) {
        if matches!(nal_type(nal), NAL_AUD | NAL_SPS | NAL_PPS) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// True if the access unit contains an IDR slice.
pub fn is_keyframe(data: &[u8]) -> bool {
    split_annexb(data).iter().any(|n| nal_type(n) == NAL_IDR)
}

/// Exp-Golomb bit reader over an RBSP (emulation prevention bytes removed).
struct BitReader {
    data: Vec<u8>,
    pos: usize,
}

impl BitReader {
    fn new(nal_payload: &[u8]) -> Self {
        let mut data = Vec::with_capacity(nal_payload.len());
        let mut zeros = 0;
        for &b in nal_payload {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            data.push(b);
        }
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Result<u32> {
        let byte = *self
            .data
            .get(self.pos / 8)
            .ok_or(Error::Bitstream("truncated SPS"))?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(bit as u32)
    }

    fn bits(&mut self, n: u32) -> Result<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::Bitstream("bad exp-golomb code"));
            }
        }
        Ok((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    fn se(&mut self) -> Result<i32> {
        let v = self.ue()?;
        Ok(if v & 1 == 1 {
            v.div_ceil(2) as i32
        } else {
            -((v / 2) as i32)
        })
    }
}

/// Frame dimensions from an SPS NAL unit (including the NAL header byte).
pub fn sps_dimensions(sps: &[u8]) -> Result<(u32, u32)> {
    if nal_type(sps) != NAL_SPS {
        return Err(Error::Bitstream("not an SPS"));
    }
    let mut r = BitReader::new(&sps[1..]);
    let profile_idc = r.bits(8)?;
    r.bits(16)?; // constraint flags + level
    r.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.bit()?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()? + 1;
    let height_map_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    let mut width = width_mbs * 16;
    let mut height = height_map_units * 16 * (2 - frame_mbs_only);
    if r.bit()? == 1 {
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let (crop_x, crop_y) = match chroma_format_idc {
            0 => (1, 2 - frame_mbs_only),
            1 => (2, 2 * (2 - frame_mbs_only)),
            2 => (2, 2 - frame_mbs_only),
            _ => (1, 2 - frame_mbs_only),
        };
        width = width.saturating_sub((left + right) * crop_x);
        height = height.saturating_sub((top + bottom) * crop_y);
    }
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    // SPS from x264, High profile, 1920x1080 (1088 coded, cropped by 8).
    const SPS_1080P: &[u8] = &[
        0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x60, 0xc6, 0x58,
    ];

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x65, 9, 9,
        ];
        let nals = split_annexb(&data);
        assert_eq!(
            nals,
            vec![&[0x09, 0xf0][..], &[0x67, 1, 2][..], &[0x65, 9, 9][..]]
        );
        assert!(is_keyframe(&data));
    }

    #[test]
    fn avcc_drops_parameter_sets() {
        let data = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 9,
            9,
        ];
        assert_eq!(annexb_to_avcc(&data), vec![0, 0, 0, 3, 0x65, 9, 9]);
        let cfg = AvcConfig::from_annexb(&data).unwrap();
        assert_eq!(
            cfg,
            AvcConfig {
                sps: vec![0x67, 1],
                pps: vec![0x68, 2]
            }
        );
    }

    #[test]
    fn parses_1080p_sps() {
        assert_eq!(sps_dimensions(SPS_1080P).unwrap(), (1920, 1080));
    }
}
