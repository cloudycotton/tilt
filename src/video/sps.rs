//! Annex B NAL iteration and SPS parsing: the WebCodecs codec string and stream checks.

use anyhow::{bail, ensure, Context};

pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// Profiles whose SPS carries chroma_format_idc, bit depths and scaling lists (7.3.2.1.1).
const HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// aspect_ratio_idc that is followed by an explicit sar_width and sar_height.
const EXTENDED_SAR: u8 = 255;

/// The NAL units of an Annex B buffer, start codes stripped, emulation prevention bytes kept.
pub fn nal_units(data: &[u8]) -> NalUnits<'_> {
    NalUnits { rest: data }
}

pub struct NalUnits<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        while let Some(start) = find_start_code(self.rest) {
            let unit = &self.rest[start + 3..];
            let end = find_start_code(unit).unwrap_or(unit.len());
            self.rest = &unit[end..];
            // A NAL unit never ends in a zero byte, so zeros before the next start code are
            // trailing_zero_8bits or the first byte of a 4-byte start code.
            let len = unit[..end]
                .iter()
                .rposition(|&b| b != 0)
                .map_or(0, |last| last + 1);
            if len > 0 {
                return Some(&unit[..len]);
            }
        }
        self.rest = &[];
        None
    }
}

fn find_start_code(data: &[u8]) -> Option<usize> {
    data.windows(3).position(|w| w == [0, 0, 1])
}

/// nal_unit_type: the low 5 bits of the NAL header byte.
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpsInfo {
    pub profile_idc: u8,
    /// constraint_set0..5 flags and the reserved bits: the byte between profile and level.
    pub constraint_flags: u8,
    pub level_idc: u8,
    /// Cropped picture size.
    pub width: u32,
    pub height: u32,
    pub vui_parameters_present: bool,
    /// From video_signal_type, when present.
    pub video_full_range: Option<bool>,
    /// From colour_description, when present.
    pub colour_primaries: Option<u8>,
    pub transfer_characteristics: Option<u8>,
    pub matrix_coefficients: Option<u8>,
    /// From bitstream_restriction, when present.
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
}

/// Parses an SPS NAL unit, header byte included.
pub fn parse_sps(nal: &[u8]) -> anyhow::Result<SpsInfo> {
    ensure!(nal_type(nal) == Some(NAL_SPS), "not an SPS NAL unit");
    let rbsp = unescape(&nal[1..]);
    let mut r = BitReader {
        data: &rbsp,
        pos: 0,
    };

    let profile_idc = r.u8()?;
    let constraint_flags = r.u8()?;
    let level_idc = r.u8()?;
    ensure!(r.ue()? <= 31, "seq_parameter_set_id out of range");

    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    if HIGH_PROFILES.contains(&profile_idc) {
        chroma_format_idc = r.ue()?;
        ensure!(
            chroma_format_idc <= 3,
            "chroma_format_idc {chroma_format_idc} out of range"
        );
        if chroma_format_idc == 3 {
            separate_colour_plane = r.flag()?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.flag()?; // qpprime_y_zero_transform_bypass_flag
        if r.flag()? {
            // seq_scaling_matrix_present_flag: six 4x4 lists, then two or six 8x8 ones.
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.flag()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
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
            r.flag()?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            let cycle = r.ue()?;
            ensure!(
                cycle <= 255,
                "num_ref_frames_in_pic_order_cnt_cycle {cycle} out of range"
            );
            for _ in 0..cycle {
                r.se()?; // offset_for_ref_frame
            }
        }
        2 => {}
        other => bail!("pic_order_cnt_type {other} out of range"),
    }
    r.ue()?; // max_num_ref_frames
    r.flag()?; // gaps_in_frame_num_value_allowed_flag

    let width_mbs = u64::from(r.ue()?) + 1;
    let height_map_units = u64::from(r.ue()?) + 1;
    let frame_mbs_only = r.flag()?;
    if !frame_mbs_only {
        r.flag()?; // mb_adaptive_frame_field_flag
    }
    r.flag()?; // direct_8x8_inference_flag
               // Field coding counts map units in pairs of macroblock rows (7-18).
    let field_factor = if frame_mbs_only { 1 } else { 2 };
    let mut width = width_mbs * 16;
    let mut height = height_map_units * field_factor * 16;
    if r.flag()? {
        // frame_cropping_flag: offsets are in crop units, set by ChromaArrayType (7-19..7-22).
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let chroma_array_type = if separate_colour_plane {
            0
        } else {
            chroma_format_idc
        };
        let (unit_x, unit_y) = match chroma_array_type {
            1 => (2, 2 * field_factor),
            2 => (2, field_factor),
            _ => (1, field_factor),
        };
        let crop = |size: u64, unit: u64, a: u32, b: u32| {
            size.checked_sub(unit * (u64::from(a) + u64::from(b)))
                .filter(|&s| s > 0)
        };
        width = crop(width, unit_x, left, right).context("frame cropping exceeds the width")?;
        height = crop(height, unit_y, top, bottom).context("frame cropping exceeds the height")?;
    }
    let vui_parameters_present = r.flag()?;

    let mut info = SpsInfo {
        profile_idc,
        constraint_flags,
        level_idc,
        width: u32::try_from(width).context("SPS width out of range")?,
        height: u32::try_from(height).context("SPS height out of range")?,
        vui_parameters_present,
        video_full_range: None,
        colour_primaries: None,
        transfer_characteristics: None,
        matrix_coefficients: None,
        max_num_reorder_frames: None,
        max_dec_frame_buffering: None,
    };
    if vui_parameters_present {
        parse_vui(&mut r, &mut info)?;
    }
    Ok(info)
}

/// `avc1.PPCCLL` from the first SPS in an Annex B buffer.
pub fn codec_string(annexb: &[u8]) -> Option<String> {
    let sps = nal_units(annexb).find(|nal| nal_type(nal) == Some(NAL_SPS))?;
    match unescape(&sps[1..]).as_slice() {
        [profile, constraints, level, ..] => {
            Some(format!("avc1.{profile:02X}{constraints:02X}{level:02X}"))
        }
        _ => None,
    }
}

/// vui_parameters() (E.1.1), keeping what WebCodecs and the stream checks look at.
fn parse_vui(r: &mut BitReader, info: &mut SpsInfo) -> anyhow::Result<()> {
    if r.flag()? {
        // aspect_ratio_info_present_flag
        if r.u8()? == EXTENDED_SAR {
            r.skip(32)?; // sar_width, sar_height
        }
    }
    if r.flag()? {
        r.flag()?; // overscan_appropriate_flag
    }
    if r.flag()? {
        // video_signal_type_present_flag
        r.skip(3)?; // video_format
        info.video_full_range = Some(r.flag()?);
        if r.flag()? {
            // colour_description_present_flag
            info.colour_primaries = Some(r.u8()?);
            info.transfer_characteristics = Some(r.u8()?);
            info.matrix_coefficients = Some(r.u8()?);
        }
    }
    if r.flag()? {
        // chroma_loc_info_present_flag
        r.ue()?;
        r.ue()?;
    }
    if r.flag()? {
        // timing_info_present_flag: num_units_in_tick, time_scale, fixed_frame_rate_flag
        r.skip(65)?;
    }
    let nal_hrd = r.flag()?;
    if nal_hrd {
        skip_hrd_parameters(r)?;
    }
    let vcl_hrd = r.flag()?;
    if vcl_hrd {
        skip_hrd_parameters(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.flag()?; // low_delay_hrd_flag
    }
    r.flag()?; // pic_struct_present_flag
    if r.flag()? {
        // bitstream_restriction_flag
        r.flag()?; // motion_vectors_over_pic_boundaries_flag
        for _ in 0..4 {
            // max_bytes_per_pic_denom, max_bits_per_mb_denom and both log2_max_mv_length_*
            r.ue()?;
        }
        info.max_num_reorder_frames = Some(r.ue()?);
        info.max_dec_frame_buffering = Some(r.ue()?);
    }
    Ok(())
}

/// hrd_parameters() (E.1.2), consumed only.
fn skip_hrd_parameters(r: &mut BitReader) -> anyhow::Result<()> {
    let cpb_cnt_minus1 = r.ue()?;
    ensure!(
        cpb_cnt_minus1 <= 31,
        "cpb_cnt_minus1 {cpb_cnt_minus1} out of range"
    );
    r.skip(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..=cpb_cnt_minus1 {
        r.ue()?; // bit_rate_value_minus1
        r.ue()?; // cpb_size_value_minus1
        r.flag()?; // cbr_flag
    }
    // initial_cpb_removal_delay_length_minus1, cpb_removal_delay_length_minus1,
    // dpb_output_delay_length_minus1, time_offset_length
    r.skip(20)
}

/// scaling_list() (7.3.2.1.1.1), consumed only: a zero next scale ends the coded deltas.
fn skip_scaling_list(r: &mut BitReader, size: usize) -> anyhow::Result<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + i64::from(r.se()?)).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

/// The RBSP of a NAL payload: every 00 00 03 becomes 00 00 (7.4.1).
fn unescape(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0;
    for &b in ebsp {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Reads an RBSP most significant bit first; running out of data is an error, never a panic.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn bit(&mut self) -> anyhow::Result<u32> {
        let byte = *self.data.get(self.pos / 8).context("SPS ends early")?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(u32::from(bit))
    }

    fn flag(&mut self) -> anyhow::Result<bool> {
        Ok(self.bit()? == 1)
    }

    /// u(n) for n <= 32.
    fn bits(&mut self, n: u32) -> anyhow::Result<u32> {
        (0..n).try_fold(0u32, |acc, _| Ok((acc << 1) | self.bit()?))
    }

    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.bits(8)? as u8)
    }

    fn skip(&mut self, n: usize) -> anyhow::Result<()> {
        self.pos = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.data.len() * 8)
            .context("SPS ends early")?;
        Ok(())
    }

    /// ue(v) (9.1). At most 31 leading zeros keeps the value within u32.
    fn ue(&mut self) -> anyhow::Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            ensure!(zeros < 32, "exp-Golomb code longer than 32 bits");
        }
        let suffix = self.bits(zeros)?;
        Ok(((1u64 << zeros) - 1 + u64::from(suffix)) as u32)
    }

    /// se(v) (9.1.1): 0, 1, -1, 2, -2, ...
    fn se(&mut self) -> anyhow::Result<i32> {
        let k = i64::from(self.ue()?);
        Ok((if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) }) as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds SPS NAL units bit by bit, the inverse of the parser, for syntax it must accept.
    #[derive(Default)]
    struct BitWriter {
        bits: Vec<bool>,
    }

    impl BitWriter {
        fn u(&mut self, n: u32, value: u64) -> &mut Self {
            self.bits
                .extend((0..n).rev().map(|i| (value >> i) & 1 == 1));
            self
        }

        fn flag(&mut self, b: bool) -> &mut Self {
            self.u(1, u64::from(b))
        }

        fn ue(&mut self, value: u32) -> &mut Self {
            let x = u64::from(value) + 1;
            let len = 64 - x.leading_zeros();
            self.u(len - 1, 0).u(len, x)
        }

        fn se(&mut self, value: i32) -> &mut Self {
            let v = i64::from(value);
            let k = if v > 0 { 2 * v - 1 } else { -2 * v };
            self.ue(k as u32)
        }

        /// Header 0x67, the bits, rbsp_trailing_bits, then emulation prevention.
        fn sps_nal(&self) -> Vec<u8> {
            let mut bits = self.bits.clone();
            bits.push(true);
            while !bits.len().is_multiple_of(8) {
                bits.push(false);
            }
            let mut nal = vec![0x67];
            let mut zeros = 0;
            for chunk in bits.chunks(8) {
                let byte = chunk.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b));
                if zeros >= 2 && byte <= 3 {
                    nal.push(3);
                    zeros = 0;
                }
                zeros = if byte == 0 { zeros + 1 } else { 0 };
                nal.push(byte);
            }
            nal
        }
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn reader(bytes: &[u8]) -> BitReader<'_> {
        BitReader {
            data: bytes,
            pos: 0,
        }
    }

    /// Main profile, POC type 0 and timing info, from a stream with B-frames (1280x720).
    const MAIN_BFRAMES_720: &str = "674d4020f602802dd80880000003008000003c078c189c";
    /// OpenH264 Baseline 512x256 rewritten with BT.709 limited-range colour.
    const BASELINE_BT709_TV: &str = "6742c015da0200869a808080a0000003002000000791e2c5d4";
    /// The same with BT.601 full-range colour.
    const BASELINE_BT601_PC: &str = "6742c015da0200869b83030320000003002000000791e2c5d4";
    /// OpenH264 Baseline 1366x768: 86x48 macroblocks cropped on the right.
    const BASELINE_1366X768: &str = "6742c02ada01581879bc0440000003004000001e23c60ca8";
    /// OpenH264 Baseline 640x360: 40x23 macroblocks cropped at the bottom.
    const BASELINE_640X360: &str = "6742c01fda0280bfe5c044000003000400000301e23c60ca80";

    #[test]
    fn splits_annex_b_into_nal_units() {
        let stream = [
            0xff, 0xee, // junk before the first start code is ignored
            0, 0, 0, 1, 0x67, 1, 2, // 4-byte start code
            0, 0, 1, 0x68, 3, // 3-byte start code
            0, 0, 1, 0, 0, 1, // an empty unit
            0, 0, 0, 1, 0x06, 0, 0, 3, 1, 0x80, // emulation prevention stays in place
            0, 0, 0, 1, 0x65, 4, 0, 0, // trailing_zero_8bits
        ];
        let units: Vec<&[u8]> = nal_units(&stream).collect();
        assert_eq!(
            units,
            [
                &[0x67, 1, 2][..],
                &[0x68, 3],
                &[0x06, 0, 0, 3, 1, 0x80],
                &[0x65, 4]
            ]
        );
        assert_eq!(
            units.iter().map(|u| nal_type(u)).collect::<Vec<_>>(),
            [Some(NAL_SPS), Some(NAL_PPS), Some(NAL_SEI), Some(NAL_IDR)]
        );
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[0x67, 1, 2, 0, 0]).count(), 0);
        assert_eq!(nal_units(&[0, 0, 1]).count(), 0);
        assert_eq!(nal_type(&[]), None);
    }

    #[test]
    fn reads_exp_golomb_codes() {
        let mut w = BitWriter::default();
        for v in [0, 1, 2, 3, 7, 255, 65_535, u32::MAX - 1] {
            w.ue(v);
        }
        for v in [0, 1, -1, 2, -2, 1000, -1000, i32::MAX, -i32::MAX] {
            w.se(v);
        }
        let bytes = unescape(&w.sps_nal()[1..]);
        let mut r = reader(&bytes);
        for v in [0, 1, 2, 3, 7, 255, 65_535, u32::MAX - 1] {
            assert_eq!(r.ue().unwrap(), v);
        }
        for v in [0, 1, -1, 2, -2, 1000, -1000, i32::MAX, -i32::MAX] {
            assert_eq!(r.se().unwrap(), v);
        }

        // 32 leading zeros would overflow u32; running out of data is an error too.
        assert!(reader(&[0, 0, 0, 0, 0xff]).ue().is_err());
        assert!(reader(&[0, 0]).ue().is_err());
        assert!(reader(&[0b0000_0001]).ue().is_err(), "suffix cut short");
        let mut r = reader(&[0xab]);
        assert_eq!(r.bits(8).unwrap(), 0xab);
        assert!(r.bit().is_err() && r.skip(1).is_err());
    }

    #[test]
    fn removes_emulation_prevention() {
        assert_eq!(
            unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]),
            [0, 0, 1, 0, 0, 0, 0]
        );
        assert_eq!(unescape(&[0, 3, 0, 0, 0, 3, 3]), [0, 3, 0, 0, 0, 3]);
    }

    #[test]
    fn parses_real_sps_units() {
        let main = parse_sps(&hex(MAIN_BFRAMES_720)).unwrap();
        assert_eq!(
            main,
            SpsInfo {
                profile_idc: 77,
                constraint_flags: 0x40,
                level_idc: 32,
                width: 1280,
                height: 720,
                vui_parameters_present: true,
                video_full_range: None,
                colour_primaries: None,
                transfer_characteristics: None,
                matrix_coefficients: None,
                max_num_reorder_frames: Some(1),
                max_dec_frame_buffering: Some(2),
            }
        );

        let tv = parse_sps(&hex(BASELINE_BT709_TV)).unwrap();
        assert_eq!(
            (tv.profile_idc, tv.constraint_flags, tv.level_idc),
            (66, 0xc0, 21)
        );
        assert_eq!((tv.width, tv.height), (512, 256));
        assert_eq!(tv.video_full_range, Some(false));
        assert_eq!(
            (
                tv.colour_primaries,
                tv.transfer_characteristics,
                tv.matrix_coefficients
            ),
            (Some(1), Some(1), Some(1))
        );
        assert_eq!(
            (tv.max_num_reorder_frames, tv.max_dec_frame_buffering),
            (Some(0), Some(1))
        );

        let pc = parse_sps(&hex(BASELINE_BT601_PC)).unwrap();
        assert_eq!(pc.video_full_range, Some(true));
        assert_eq!(
            (
                pc.colour_primaries,
                pc.transfer_characteristics,
                pc.matrix_coefficients
            ),
            (Some(6), Some(6), Some(6))
        );

        let cropped = parse_sps(&hex(BASELINE_1366X768)).unwrap();
        assert_eq!(
            (cropped.width, cropped.height, cropped.level_idc),
            (1366, 768, 42)
        );
        let cropped = parse_sps(&hex(BASELINE_640X360)).unwrap();
        assert_eq!((cropped.width, cropped.height), (640, 360));
    }

    #[test]
    fn parses_high_profile_fields_scaling_lists_and_full_vui() {
        let mut w = BitWriter::default();
        w.u(8, 100).u(8, 0).u(8, 40).ue(0); // profile, constraints, level, sps id
        w.ue(1).ue(0).ue(0).flag(false); // 4:2:0, 8-bit, no transform bypass
        w.flag(true); // seq_scaling_matrix_present_flag
        for i in 0..8 {
            match i {
                // A 4x4 list with every delta coded.
                0 => {
                    w.flag(true);
                    for j in 0..16 {
                        w.se(if j % 2 == 0 { 3 } else { -2 });
                    }
                }
                // An 8x8 list that ends early: next scale 0 after three deltas (8+4+4-16).
                6 => {
                    w.flag(true).se(4).se(4).se(-16);
                }
                _ => {
                    w.flag(false);
                }
            }
        }
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(1).flag(false).se(-2).se(1).ue(3).se(1).se(-1).se(5); // POC type 1 with a cycle
        w.ue(4).flag(false); // max_num_ref_frames, gaps
        w.ue(119).ue(33).flag(false).flag(true).flag(true); // 1920 x 34 field-pair map units
        w.flag(true).ue(0).ue(0).ue(0).ue(2); // crop 2 units of 4 rows: 1088 -> 1080
        w.flag(true); // vui_parameters_present_flag
        w.flag(true).u(8, 255).u(16, 4).u(16, 3); // Extended_SAR 4:3
        w.flag(true).flag(false); // overscan
        w.flag(true)
            .u(3, 5)
            .flag(true)
            .flag(true)
            .u(8, 9)
            .u(8, 16)
            .u(8, 9); // BT.2020 PQ, full
        w.flag(true).ue(1).ue(1); // chroma location
        w.flag(true).u(32, 1001).u(32, 60_000).flag(true); // timing
        w.flag(true).ue(1).u(4, 2).u(4, 3); // NAL HRD with two CPB specs
        w.ue(10_000)
            .ue(20_000)
            .flag(false)
            .ue(30_000)
            .ue(40_000)
            .flag(true);
        w.u(5, 23).u(5, 23).u(5, 23).u(5, 24);
        w.flag(false).flag(false).flag(true); // no VCL HRD, low_delay_hrd, pic_struct_present
        w.flag(true)
            .flag(true)
            .ue(2)
            .ue(1)
            .ue(16)
            .ue(16)
            .ue(2)
            .ue(4); // bitstream_restriction

        let info = parse_sps(&w.sps_nal()).unwrap();
        assert_eq!(
            info,
            SpsInfo {
                profile_idc: 100,
                constraint_flags: 0,
                level_idc: 40,
                width: 1920,
                height: 1080,
                vui_parameters_present: true,
                video_full_range: Some(true),
                colour_primaries: Some(9),
                transfer_characteristics: Some(16),
                matrix_coefficients: Some(9),
                max_num_reorder_frames: Some(2),
                max_dec_frame_buffering: Some(4),
            }
        );
    }

    /// High profile prefix through direct_8x8_inference_flag, without scaling lists.
    fn high_sps(chroma: u32, separate_planes: bool, frame_mbs_only: bool) -> BitWriter {
        let mut w = BitWriter::default();
        w.u(8, 244).u(8, 0).u(8, 51).ue(3).ue(chroma);
        if chroma == 3 {
            w.flag(separate_planes);
        }
        w.ue(2).ue(2).flag(false).flag(false); // 10-bit, no scaling matrix
        w.ue(12).ue(0).ue(9); // log2_max_frame_num_minus4, POC type 0, lsb
        w.ue(1).flag(true).ue(39).ue(29).flag(frame_mbs_only);
        if !frame_mbs_only {
            w.flag(false);
        }
        w.flag(true);
        w
    }

    #[test]
    fn crop_units_follow_chroma_array_type() {
        // 640 wide; 480 high as frames, 960 as field pairs. Crops 3 units across, 5 down.
        let cases = [
            (0, false, true, (637, 475)),  // monochrome: 1 x 1
            (1, false, true, (634, 470)),  // 4:2:0: 2 x 2
            (2, false, true, (634, 475)),  // 4:2:2: 2 x 1
            (3, false, true, (637, 475)),  // 4:4:4: 1 x 1
            (3, true, true, (637, 475)),   // separate planes: ChromaArrayType 0
            (1, false, false, (634, 940)), // 4:2:0 fields: 2 x 4
            (0, false, false, (637, 950)), // monochrome fields: 1 x 2
        ];
        for (chroma, separate, frames, size) in cases {
            let mut w = high_sps(chroma, separate, frames);
            w.flag(true).ue(1).ue(2).ue(2).ue(3).flag(false);
            let info = parse_sps(&w.sps_nal()).unwrap();
            assert_eq!(
                (info.width, info.height),
                size,
                "chroma {chroma} separate {separate}"
            );
            assert!(!info.vui_parameters_present && info.max_num_reorder_frames.is_none());
        }

        let mut w = high_sps(1, false, true);
        w.flag(true).ue(160).ue(160).ue(0).ue(0).flag(false);
        assert!(
            parse_sps(&w.sps_nal()).is_err(),
            "cropping away the whole width"
        );
    }

    #[test]
    fn rejects_malformed_sps_without_panicking() {
        assert!(parse_sps(&[]).is_err());
        assert!(
            parse_sps(&[0x68, 0x42, 0xc0, 0x1f]).is_err(),
            "a PPS is not an SPS"
        );
        assert!(parse_sps(&[0x67, 0x42, 0xc0]).is_err());

        let mut w = high_sps(1, false, true);
        w.flag(false).flag(false);
        let good = w.sps_nal();
        assert!(parse_sps(&good).is_ok());
        // Every truncation either parses or fails cleanly.
        for vector in [good, hex(MAIN_BFRAMES_720), hex(BASELINE_1366X768)] {
            for len in 0..vector.len() {
                let _ = parse_sps(&vector[..len]);
            }
        }

        let mut bad_poc = BitWriter::default();
        bad_poc.u(8, 66).u(8, 0).u(8, 30).ue(0).ue(0).ue(3);
        assert!(parse_sps(&bad_poc.sps_nal()).is_err());
        let mut bad_chroma = BitWriter::default();
        bad_chroma.u(8, 100).u(8, 0).u(8, 30).ue(0).ue(4);
        assert!(parse_sps(&bad_chroma.sps_nal()).is_err());

        // Arbitrary bytes behind an SPS header must never panic.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..20_000 {
            let len = (state % 40) as usize;
            let mut nal = vec![0x67];
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                nal.push((state >> 24) as u8);
            }
            let _ = parse_sps(&nal);
            let _ = codec_string(&[&[0, 0, 1][..], &nal].concat());
        }
    }

    #[test]
    fn codec_string_comes_from_the_first_sps() {
        let aud = [0, 0, 0, 1, 0x09, 0xf0];
        let stream = [
            &aud[..],
            &[0, 0, 0, 1],
            &hex(MAIN_BFRAMES_720),
            &[0, 0, 0, 1],
            &hex(BASELINE_1366X768),
        ]
        .concat();
        assert_eq!(codec_string(&stream).as_deref(), Some("avc1.4D4020"));
        assert_eq!(
            codec_string(&[&[0, 0, 1][..], &hex(BASELINE_1366X768)].concat()).as_deref(),
            Some("avc1.42C02A")
        );
        // The three bytes are read from the RBSP, past any emulation prevention byte.
        assert_eq!(
            codec_string(&[0, 0, 1, 0x67, 0, 0, 3, 1, 0x80]).as_deref(),
            Some("avc1.000001")
        );
        assert_eq!(codec_string(&aud), None);
        assert_eq!(codec_string(&[0, 0, 1, 0x67, 0x64, 0x00]), None);
    }
}
