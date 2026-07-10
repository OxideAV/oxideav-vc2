//! Sequence-header / video-parameter parsing (SMPTE ST 2042-1:2022 §11),
//! including the decode-critical base-video-format defaults of Annex B and
//! the preset signal ranges of Table 10.
//!
//! Only the parameters required to decode pixel data are stored on
//! [`VideoParameters`]; the display-only metadata of §11.4.6–§11.4.10
//! (frame rate, aspect ratio, clean area, colour spec) is parsed and
//! discarded so the bitstream position stays correct, but is not retained.

use crate::bitio::BitReader;
use crate::{Error, Result};

/// Colour-difference subsampling (Table 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDiffFormat {
    /// 4:4:4 — chroma at full resolution.
    Yuv444,
    /// 4:2:2 — chroma horizontally halved.
    Yuv422,
    /// 4:2:0 — chroma halved in both dimensions.
    Yuv420,
}

impl ColorDiffFormat {
    fn from_index(idx: u64) -> Result<Self> {
        match idx {
            0 => Ok(ColorDiffFormat::Yuv444),
            1 => Ok(ColorDiffFormat::Yuv422),
            2 => Ok(ColorDiffFormat::Yuv420),
            _ => Err(Error::InvalidValue("color_diff_format_index out of 0..=2")),
        }
    }

    /// Index value as stored in `state[color_diff_format_index]`.
    pub fn index(self) -> u64 {
        match self {
            ColorDiffFormat::Yuv444 => 0,
            ColorDiffFormat::Yuv422 => 1,
            ColorDiffFormat::Yuv420 => 2,
        }
    }
}

/// Decode-critical source video parameters (§11.4).
#[derive(Debug, Clone, Copy)]
pub struct VideoParameters {
    pub frame_width: u64,
    pub frame_height: u64,
    pub color_diff_format: ColorDiffFormat,
    /// 0 = progressive, 1 = interlaced (§11.4.5).
    pub source_sampling: u64,
    pub top_field_first: bool,
    pub luma_offset: u64,
    pub luma_excursion: u64,
    pub color_diff_offset: u64,
    pub color_diff_excursion: u64,
}

/// `preset_signal_range(index)` — Table 10. Returns
/// (luma_offset, luma_excursion, color_diff_offset, color_diff_excursion).
fn preset_signal_range(index: u64) -> Result<(u64, u64, u64, u64)> {
    Ok(match index {
        1 => (0, 255, 128, 255),
        2 => (16, 219, 128, 224),
        3 => (64, 876, 512, 896),
        4 => (256, 3504, 2048, 3584),
        5 => (0, 1023, 512, 1023),
        6 => (0, 4095, 2048, 4095),
        7 => (4096, 56064, 32768, 57344),
        8 => (0, 65535, 32768, 65535),
        _ => {
            return Err(Error::InvalidValue(
                "signal range preset index out of 1..=8",
            ))
        }
    })
}

/// `set_source_defaults(base_video_format)` — decode-critical subset of
/// Annex B Tables B.1–B.3. Display-only fields are filled with token values.
pub fn set_source_defaults(base_video_format: u64) -> Result<VideoParameters> {
    // (frame_width, frame_height, color_diff_index, source_sampling,
    //  top_field_first, luma_offset, luma_excursion,
    //  color_diff_offset, color_diff_excursion)
    let row: (u64, u64, u64, u64, bool, u64, u64, u64, u64) = match base_video_format {
        0 => (640, 480, 2, 0, false, 0, 255, 128, 255), // Custom
        1 => (176, 120, 2, 0, false, 0, 255, 128, 255),
        2 => (176, 144, 2, 0, true, 0, 255, 128, 255),
        3 => (352, 240, 2, 0, false, 0, 255, 128, 255),
        4 => (352, 288, 2, 0, true, 0, 255, 128, 255),
        5 => (704, 480, 2, 0, false, 0, 255, 128, 255),
        6 => (704, 576, 2, 0, true, 0, 255, 128, 255),
        7 => (720, 480, 1, 1, false, 64, 876, 512, 896),
        8 => (720, 576, 1, 1, true, 64, 876, 512, 896),
        9 => (1280, 720, 1, 0, true, 64, 876, 512, 896),
        10 => (1280, 720, 1, 0, true, 64, 876, 512, 896),
        11 => (1920, 1080, 1, 1, true, 64, 876, 512, 896),
        12 => (1920, 1080, 1, 1, true, 64, 876, 512, 896),
        13 => (1920, 1080, 1, 0, true, 64, 876, 512, 896),
        14 => (1920, 1080, 1, 0, true, 64, 876, 512, 896),
        15 => (2048, 1080, 0, 0, true, 256, 3504, 2048, 3584),
        16 => (4096, 2160, 0, 0, true, 256, 3504, 2048, 3584),
        17 => (3840, 2160, 1, 0, true, 64, 876, 512, 896),
        18 => (3840, 2160, 1, 0, true, 64, 876, 512, 896),
        19 => (7680, 4320, 1, 0, true, 64, 876, 512, 896),
        20 => (7680, 4320, 1, 0, true, 64, 876, 512, 896),
        21 => (1920, 1080, 1, 0, true, 64, 876, 512, 896),
        22 => (720, 486, 1, 1, false, 64, 876, 512, 896),
        _ => return Err(Error::InvalidValue("base_video_format out of 0..=22")),
    };
    Ok(VideoParameters {
        frame_width: row.0,
        frame_height: row.1,
        color_diff_format: ColorDiffFormat::from_index(row.2)?,
        source_sampling: row.3,
        top_field_first: row.4,
        luma_offset: row.5,
        luma_excursion: row.6,
        color_diff_offset: row.7,
        color_diff_excursion: row.8,
    })
}

/// Parsed parse-parameters block (§11.2).
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseParameters {
    pub major_version: u64,
    pub minor_version: u64,
    pub profile: u64,
    pub level: u64,
}

/// `parse_parameters()` (§11.2.1).
fn parse_parameters(r: &mut BitReader) -> ParseParameters {
    ParseParameters {
        major_version: r.read_uint(),
        minor_version: r.read_uint(),
        profile: r.read_uint(),
        level: r.read_uint(),
    }
}

/// `frame_size()` (§11.4.3).
fn frame_size(r: &mut BitReader, vp: &mut VideoParameters) {
    if r.read_bool() {
        vp.frame_width = r.read_uint();
        vp.frame_height = r.read_uint();
    }
}

/// `color_diff_sampling_format()` (§11.4.4).
fn color_diff_sampling_format(r: &mut BitReader, vp: &mut VideoParameters) -> Result<()> {
    if r.read_bool() {
        vp.color_diff_format = ColorDiffFormat::from_index(r.read_uint())?;
    }
    Ok(())
}

/// `scan_format()` (§11.4.5). `top_field_first` cannot be overridden here.
fn scan_format(r: &mut BitReader, vp: &mut VideoParameters) {
    if r.read_bool() {
        vp.source_sampling = r.read_uint();
    }
}

/// `frame_rate()` (§11.4.6) — parsed for bitstream position only.
fn frame_rate(r: &mut BitReader) {
    if r.read_bool() {
        let index = r.read_uint();
        if index == 0 {
            let _numer = r.read_uint();
            let _denom = r.read_uint();
        }
    }
}

/// `pixel_aspect_ratio()` (§11.4.7) — parsed for position only.
fn pixel_aspect_ratio(r: &mut BitReader) {
    if r.read_bool() {
        let index = r.read_uint();
        if index == 0 {
            let _numer = r.read_uint();
            let _denom = r.read_uint();
        }
    }
}

/// `clean_area()` (§11.4.8) — parsed for position only.
fn clean_area(r: &mut BitReader) {
    if r.read_bool() {
        let _clean_width = r.read_uint();
        let _clean_height = r.read_uint();
        let _left_offset = r.read_uint();
        let _top_offset = r.read_uint();
    }
}

/// `signal_range()` (§11.4.9).
fn signal_range(r: &mut BitReader, vp: &mut VideoParameters) -> Result<()> {
    if r.read_bool() {
        let index = r.read_uint();
        if index == 0 {
            vp.luma_offset = r.read_uint();
            vp.luma_excursion = r.read_uint();
            vp.color_diff_offset = r.read_uint();
            vp.color_diff_excursion = r.read_uint();
        } else {
            let (lo, le, co, ce) = preset_signal_range(index)?;
            vp.luma_offset = lo;
            vp.luma_excursion = le;
            vp.color_diff_offset = co;
            vp.color_diff_excursion = ce;
        }
    }
    Ok(())
}

/// `color_spec()` (§11.4.10) — parsed for position only.
fn color_spec(r: &mut BitReader) {
    if r.read_bool() {
        let index = r.read_uint();
        if index == 0 {
            // color_primaries
            if r.read_bool() {
                let _ = r.read_uint();
            }
            // color_matrix
            if r.read_bool() {
                let _ = r.read_uint();
            }
            // transfer_function
            if r.read_bool() {
                let _ = r.read_uint();
            }
        }
    }
}

/// `source_parameters()` (§11.4.1).
fn source_parameters(r: &mut BitReader, base_video_format: u64) -> Result<VideoParameters> {
    let mut vp = set_source_defaults(base_video_format)?;
    frame_size(r, &mut vp);
    color_diff_sampling_format(r, &mut vp)?;
    scan_format(r, &mut vp);
    frame_rate(r);
    pixel_aspect_ratio(r);
    clean_area(r);
    signal_range(r, &mut vp)?;
    color_spec(r);
    Ok(vp)
}

/// Decoder coding parameters derived by `set_coding_parameters()` (§11.6).
#[derive(Debug, Clone, Copy)]
pub struct CodingParameters {
    pub luma_width: u64,
    pub luma_height: u64,
    pub color_diff_width: u64,
    pub color_diff_height: u64,
    pub luma_depth: u32,
    pub color_diff_depth: u32,
}

/// `intlog2(n)` (§5.6.4): smallest `m` with `2**(m-1) < n <= 2**m`.
pub fn intlog2(n: u64) -> u32 {
    if n <= 1 {
        return 0;
    }
    let mut m = 0u32;
    let mut p = 1u64;
    while p < n {
        p <<= 1;
        m += 1;
    }
    m
}

/// `set_coding_parameters()` (§11.6) — picture dimensions (§11.6.2) and
/// video depth (§11.6.3). `picture_coding_mode` of 1 codes fields.
pub fn set_coding_parameters(vp: &VideoParameters, picture_coding_mode: u64) -> CodingParameters {
    let luma_width = vp.frame_width;
    let mut luma_height = vp.frame_height;
    let mut color_diff_width = luma_width;
    let mut color_diff_height = luma_height;
    match vp.color_diff_format {
        ColorDiffFormat::Yuv444 => {}
        ColorDiffFormat::Yuv422 => color_diff_width /= 2,
        ColorDiffFormat::Yuv420 => {
            color_diff_width /= 2;
            color_diff_height /= 2;
        }
    }
    if picture_coding_mode == 1 {
        luma_height /= 2;
        color_diff_height /= 2;
    }
    CodingParameters {
        luma_width,
        luma_height,
        color_diff_width,
        color_diff_height,
        luma_depth: intlog2(vp.luma_excursion + 1),
        color_diff_depth: intlog2(vp.color_diff_excursion + 1),
    }
}

/// Result of `sequence_header()` (§11.1).
#[derive(Debug, Clone, Copy)]
pub struct SequenceHeader {
    pub parse_parameters: ParseParameters,
    pub base_video_format: u64,
    pub video_parameters: VideoParameters,
    pub picture_coding_mode: u64,
    pub coding_parameters: CodingParameters,
}

/// `sequence_header()` (§11.1): parse parse-parameters, base format, source
/// parameters and picture coding mode, and derive the coding parameters.
pub fn sequence_header(r: &mut BitReader) -> Result<SequenceHeader> {
    let parse_parameters = parse_parameters(r);
    let base_video_format = r.read_uint();
    let video_parameters = source_parameters(r, base_video_format)?;
    let picture_coding_mode = r.read_uint();
    if r.overrun() {
        return Err(Error::UnexpectedEof);
    }
    if video_parameters.frame_width == 0 || video_parameters.frame_height == 0 {
        return Err(Error::InvalidValue("zero frame dimensions"));
    }
    // The decoded planes are 16-bit unsigned samples; excursions beyond
    // 16 bits are unrepresentable (the deepest Table 10 preset is 65535)
    // and would otherwise overflow the video-depth derivation.
    if video_parameters.luma_excursion == 0
        || video_parameters.luma_excursion > u16::MAX as u64
        || video_parameters.color_diff_excursion == 0
        || video_parameters.color_diff_excursion > u16::MAX as u64
    {
        return Err(Error::InvalidValue(
            "signal excursion outside the representable 1..=65535 range",
        ));
    }
    let coding_parameters = set_coding_parameters(&video_parameters, picture_coding_mode);
    Ok(SequenceHeader {
        parse_parameters,
        base_video_format,
        video_parameters,
        picture_coding_mode,
        coding_parameters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intlog2_examples() {
        // Spec example: intlog2(25) = intlog2(32) = 5.
        assert_eq!(intlog2(25), 5);
        assert_eq!(intlog2(32), 5);
        assert_eq!(intlog2(256), 8);
        // 8-bit excursion 255 -> depth = intlog2(256) = 8.
        assert_eq!(intlog2(255 + 1), 8);
    }

    #[test]
    fn hd_defaults_420_to_422() {
        let vp = set_source_defaults(13).unwrap(); // HD 1080p60
        assert_eq!(vp.frame_width, 1920);
        assert_eq!(vp.frame_height, 1080);
        assert_eq!(vp.color_diff_format, ColorDiffFormat::Yuv422);
        assert_eq!(vp.luma_excursion, 876);
    }

    #[test]
    fn coding_params_422_progressive() {
        let vp = set_source_defaults(13).unwrap();
        let cp = set_coding_parameters(&vp, 0);
        assert_eq!(cp.luma_width, 1920);
        assert_eq!(cp.luma_height, 1080);
        assert_eq!(cp.color_diff_width, 960); // 4:2:2 halves width
        assert_eq!(cp.color_diff_height, 1080);
        assert_eq!(cp.luma_depth, 10); // intlog2(877) = 10
    }

    #[test]
    fn coding_params_fields_halve_height() {
        let vp = set_source_defaults(11).unwrap(); // 1080i60, 4:2:2
        let cp = set_coding_parameters(&vp, 1); // coded as fields
        assert_eq!(cp.luma_height, 540);
        assert_eq!(cp.color_diff_height, 540);
    }
}
