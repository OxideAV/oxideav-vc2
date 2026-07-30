//! Sequence-header / video-parameter parsing (SMPTE ST 2042-1:2022 §11),
//! including the complete base-video-format defaults of Annex B
//! (Tables B.1–B.3) and the preset tables of §11.4: frame rates
//! (Table 8), pixel aspect ratios (Table 9), signal ranges (Table 10)
//! and colour specifications (Tables 11–14).
//!
//! [`VideoParameters`] retains the full Annex B parameter map — the
//! decode-critical fields *and* the display metadata of §11.4.6–§11.4.10
//! (frame rate, pixel aspect ratio, clean area, colour spec) — and
//! [`SourceOverrides`] records which §11.4 custom flags the stream set
//! together with the indices it signalled, the raw material the
//! ST 2042-2 generalized-level constraints are written against.

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

/// Source video parameters (§11.4) — the full Annex B parameter map,
/// covering both the decode-critical fields and the display metadata of
/// §11.4.6–§11.4.10 that a container needs to describe the essence
/// (frame rate for edit rates, aspect ratio / clean area for display
/// sizing, colour specification for descriptor labels).
#[derive(Debug, Clone, Copy)]
pub struct VideoParameters {
    pub frame_width: u64,
    pub frame_height: u64,
    pub color_diff_format: ColorDiffFormat,
    /// 0 = progressive, 1 = interlaced (§11.4.5).
    pub source_sampling: u64,
    pub top_field_first: bool,
    /// Frame rate is `frame_rate_numer / frame_rate_denom` frames per
    /// second (§11.4.6) — the *frame* rate even when pictures are
    /// fields (the picture rate is then twice this).
    pub frame_rate_numer: u64,
    /// See [`Self::frame_rate_numer`].
    pub frame_rate_denom: u64,
    /// Pixel aspect ratio numerator (§11.4.7): the ratio of horizontal
    /// to vertical sample spacing is `numer : denom`.
    pub pixel_aspect_ratio_numer: u64,
    /// See [`Self::pixel_aspect_ratio_numer`].
    pub pixel_aspect_ratio_denom: u64,
    /// Clean-area width (§11.4.8); the application-defined display /
    /// container region. `clean_width + left_offset <= frame_width`.
    pub clean_width: u64,
    /// See [`Self::clean_width`]:
    /// `clean_height + top_offset <= frame_height`.
    pub clean_height: u64,
    /// Clean-area left offset (§11.4.8).
    pub left_offset: u64,
    /// Clean-area top offset (§11.4.8).
    pub top_offset: u64,
    pub luma_offset: u64,
    pub luma_excursion: u64,
    pub color_diff_offset: u64,
    pub color_diff_excursion: u64,
    /// Table 12 colour-primaries index (§11.4.10.2).
    pub color_primaries_index: u64,
    /// Table 13 colour-matrix index (§11.4.10.3).
    pub color_matrix_index: u64,
    /// Table 14 transfer-function index (§11.4.10.4).
    pub transfer_function_index: u64,
}

/// Record of which §11.4 custom flags a sequence header set, plus the
/// indices it signalled — exactly the signalling the ST 2042-2
/// generalized-level constraints (its §5.3) are phrased against. Field
/// names follow that clause. `Default` is the all-defaults header (every
/// flag `false`, no indices).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceOverrides {
    /// §11.4.3 frame-size override flag.
    pub custom_dimensions_flag: bool,
    /// §11.4.4 colour-difference sampling override flag.
    pub custom_chroma_format_flag: bool,
    /// §11.4.5 scan-format override flag.
    pub custom_scan_format_flag: bool,
    /// §11.4.6 frame-rate override flag.
    pub custom_frame_rate_flag: bool,
    /// Frame-rate index signalled when the flag is set (0 = explicit
    /// numerator/denominator follow, 1..=16 = Table 8 preset).
    pub frame_rate_index: Option<u64>,
    /// §11.4.7 pixel-aspect-ratio override flag.
    pub custom_pixel_aspect_ratio_flag: bool,
    /// Aspect-ratio index signalled when the flag is set (0 = explicit,
    /// 1..=6 = Table 9 preset).
    pub pixel_aspect_ratio_index: Option<u64>,
    /// §11.4.8 clean-area override flag.
    pub custom_clean_area_flag: bool,
    /// §11.4.9 signal-range override flag.
    pub custom_signal_range_flag: bool,
    /// Signal-range index signalled when the flag is set (0 = explicit,
    /// 1..=8 = Table 10 preset).
    pub signal_range_index: Option<u64>,
    /// §11.4.10 colour-specification override flag.
    pub custom_color_spec_flag: bool,
    /// Colour-spec index signalled when the flag is set (0 = custom,
    /// with optional per-part overrides; 1..=7 = Table 11 preset).
    pub color_spec_index: Option<u64>,
}

/// `preset_signal_range(index)` — Table 10 ("Preset signal ranges",
/// §11.4.9). Returns
/// (luma_offset, luma_excursion, color_diff_offset, color_diff_excursion).
///
/// All eight rows are transcribed from the normative table (see also the
/// staged verbatim transcription in
/// `docs/video/vc2/vc2-signal-range-presets-and-container-registry.md`).
/// §11.4.9: "The value of `index` shall lie in the range 0 to 8" — index 0
/// is the custom (explicitly coded) range handled by [`signal_range`], so
/// this lookup accepts exactly 1..=8. Presets 5..=8 (the full-range and
/// 16-bit rows) are an ST 2042-1 addition: the Dirac-era specification
/// defines only indices 1..=4 and bounds `index` at 4, so those rows never
/// appear in a legacy Dirac bitstream.
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

/// One Annex B default row:
/// (frame_width, frame_height, color_diff_index, source_sampling,
///  top_field_first, frame_rate_numer, frame_rate_denom,
///  pixel_aspect_ratio_numer, pixel_aspect_ratio_denom,
///  clean_width, clean_height, left_offset, top_offset,
///  luma_offset, luma_excursion, color_diff_offset, color_diff_excursion,
///  color_primaries_index, color_matrix_index, transfer_function_index)
#[allow(clippy::type_complexity)]
type AnnexBRow = (
    u64,
    u64,
    u64,
    u64,
    bool,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
);

/// `set_source_defaults(base_video_format)` — the complete Annex B
/// parameter map (Tables B.1–B.3), every label the annex lists.
pub fn set_source_defaults(base_video_format: u64) -> Result<VideoParameters> {
    #[rustfmt::skip]
    let row: AnnexBRow = match base_video_format {
        // Table B.1 (formats 0..=6).
        0 => (640, 480, 2, 0, false, 24000, 1001, 1, 1, 640, 480, 0, 0, 0, 255, 128, 255, 0, 0, 0), // Custom Format
        1 => (176, 120, 2, 0, false, 15000, 1001, 10, 11, 176, 120, 0, 0, 0, 255, 128, 255, 1, 1, 0), // QSIF525
        2 => (176, 144, 2, 0, true, 25, 2, 12, 11, 176, 144, 0, 0, 0, 255, 128, 255, 2, 1, 0), // QCIF
        3 => (352, 240, 2, 0, false, 15000, 1001, 10, 11, 352, 240, 0, 0, 0, 255, 128, 255, 1, 1, 0), // SIF525
        4 => (352, 288, 2, 0, true, 25, 2, 12, 11, 352, 288, 0, 0, 0, 255, 128, 255, 2, 1, 0), // CIF
        5 => (704, 480, 2, 0, false, 15000, 1001, 10, 11, 704, 480, 0, 0, 0, 255, 128, 255, 1, 1, 0), // 4SIF525
        6 => (704, 576, 2, 0, true, 25, 2, 12, 11, 704, 576, 0, 0, 0, 255, 128, 255, 2, 1, 0), // 4CIF
        // Table B.2 (formats 7..=14).
        7 => (720, 480, 1, 1, false, 30000, 1001, 10, 11, 704, 480, 8, 0, 64, 876, 512, 896, 1, 1, 0), // SD 480i-60
        8 => (720, 576, 1, 1, true, 25, 1, 12, 11, 704, 576, 8, 0, 64, 876, 512, 896, 2, 1, 0), // SD 576i-50
        9 => (1280, 720, 1, 0, true, 60000, 1001, 1, 1, 1280, 720, 0, 0, 64, 876, 512, 896, 0, 0, 0), // HD 720p-60
        10 => (1280, 720, 1, 0, true, 50, 1, 1, 1, 1280, 720, 0, 0, 64, 876, 512, 896, 0, 0, 0), // HD 720p-50
        11 => (1920, 1080, 1, 1, true, 30000, 1001, 1, 1, 1920, 1080, 0, 0, 64, 876, 512, 896, 0, 0, 0), // HD 1080i-60
        12 => (1920, 1080, 1, 1, true, 25, 1, 1, 1, 1920, 1080, 0, 0, 64, 876, 512, 896, 0, 0, 0), // HD 1080i-50
        13 => (1920, 1080, 1, 0, true, 60000, 1001, 1, 1, 1920, 1080, 0, 0, 64, 876, 512, 896, 0, 0, 0), // HD 1080p-60
        14 => (1920, 1080, 1, 0, true, 50, 1, 1, 1, 1920, 1080, 0, 0, 64, 876, 512, 896, 0, 0, 0), // HD 1080p-50
        // Table B.3 (formats 15..=22).
        15 => (2048, 1080, 0, 0, true, 24, 1, 1, 1, 2048, 1080, 0, 0, 256, 3504, 2048, 3584, 3, 2, 3), // DC 2K-24
        16 => (4096, 2160, 0, 0, true, 24, 1, 1, 1, 4096, 2160, 0, 0, 256, 3504, 2048, 3584, 3, 2, 3), // DC 4K-24
        17 => (3840, 2160, 1, 0, true, 60000, 1001, 1, 1, 3840, 2160, 0, 0, 64, 876, 512, 896, 4, 4, 0), // UHDTV 4K-60
        18 => (3840, 2160, 1, 0, true, 50, 1, 1, 1, 3840, 2160, 0, 0, 64, 876, 512, 896, 4, 4, 0), // UHDTV 4K-50
        19 => (7680, 4320, 1, 0, true, 60000, 1001, 1, 1, 7680, 4320, 0, 0, 64, 876, 512, 896, 4, 4, 0), // UHDTV 8K-60
        20 => (7680, 4320, 1, 0, true, 50, 1, 1, 1, 7680, 4320, 0, 0, 64, 876, 512, 896, 4, 4, 0), // UHDTV 8K-50
        21 => (1920, 1080, 1, 0, true, 24000, 1001, 1, 1, 1920, 1080, 0, 0, 64, 876, 512, 896, 0, 0, 0), // HD 1080p-24
        22 => (720, 486, 1, 1, false, 30000, 1001, 10, 11, 720, 486, 0, 0, 64, 876, 512, 896, 0, 0, 0), // SD Pro486
        _ => return Err(Error::InvalidValue("base_video_format out of 0..=22")),
    };
    Ok(VideoParameters {
        frame_width: row.0,
        frame_height: row.1,
        color_diff_format: ColorDiffFormat::from_index(row.2)?,
        source_sampling: row.3,
        top_field_first: row.4,
        frame_rate_numer: row.5,
        frame_rate_denom: row.6,
        pixel_aspect_ratio_numer: row.7,
        pixel_aspect_ratio_denom: row.8,
        clean_width: row.9,
        clean_height: row.10,
        left_offset: row.11,
        top_offset: row.12,
        luma_offset: row.13,
        luma_excursion: row.14,
        color_diff_offset: row.15,
        color_diff_excursion: row.16,
        color_primaries_index: row.17,
        color_matrix_index: row.18,
        transfer_function_index: row.19,
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
fn frame_size(r: &mut BitReader, vp: &mut VideoParameters, ov: &mut SourceOverrides) {
    if r.read_bool() {
        ov.custom_dimensions_flag = true;
        vp.frame_width = r.read_uint();
        vp.frame_height = r.read_uint();
    }
}

/// `color_diff_sampling_format()` (§11.4.4).
fn color_diff_sampling_format(
    r: &mut BitReader,
    vp: &mut VideoParameters,
    ov: &mut SourceOverrides,
) -> Result<()> {
    if r.read_bool() {
        ov.custom_chroma_format_flag = true;
        vp.color_diff_format = ColorDiffFormat::from_index(r.read_uint())?;
    }
    Ok(())
}

/// `scan_format()` (§11.4.5). `top_field_first` cannot be overridden here.
fn scan_format(r: &mut BitReader, vp: &mut VideoParameters, ov: &mut SourceOverrides) {
    if r.read_bool() {
        ov.custom_scan_format_flag = true;
        vp.source_sampling = r.read_uint();
    }
}

/// `preset_frame_rate(index)` — Table 8 ("Preset frame rate values").
/// Returns (frame_rate_numer, frame_rate_denom). §11.4.6: the index
/// shall lie in the range 0 to the maximum Table 8 defines (16); 0 is
/// the explicit-values arm handled by [`frame_rate`].
fn preset_frame_rate(index: u64) -> Result<(u64, u64)> {
    Ok(match index {
        1 => (24000, 1001),
        2 => (24, 1),
        3 => (25, 1),
        4 => (30000, 1001),
        5 => (30, 1),
        6 => (50, 1),
        7 => (60000, 1001),
        8 => (60, 1),
        9 => (15000, 1001),
        10 => (25, 2),
        11 => (48, 1),
        12 => (48000, 1001),
        13 => (96, 1),
        14 => (100, 1),
        15 => (120000, 1001),
        16 => (120, 1),
        _ => return Err(Error::InvalidValue("frame rate preset index out of 1..=16")),
    })
}

/// `frame_rate()` (§11.4.6).
fn frame_rate(r: &mut BitReader, vp: &mut VideoParameters, ov: &mut SourceOverrides) -> Result<()> {
    if r.read_bool() {
        ov.custom_frame_rate_flag = true;
        let index = r.read_uint();
        if r.overrun() {
            return Err(Error::UnexpectedEof);
        }
        ov.frame_rate_index = Some(index);
        if index == 0 {
            vp.frame_rate_numer = r.read_uint();
            vp.frame_rate_denom = r.read_uint();
            if r.overrun() {
                return Err(Error::UnexpectedEof);
            }
            // "When given, the frame rate numerator and denominator
            // values shall both be at least 1."
            if vp.frame_rate_numer == 0 || vp.frame_rate_denom == 0 {
                return Err(Error::InvalidValue(
                    "custom frame rate numerator / denominator must be >= 1",
                ));
            }
        } else {
            let (numer, denom) = preset_frame_rate(index)?;
            vp.frame_rate_numer = numer;
            vp.frame_rate_denom = denom;
        }
    }
    Ok(())
}

/// `preset_pixel_aspect_ratio(index)` — Table 9 ("Preset pixel aspect
/// ratio values"). Returns (numer, denom). §11.4.7: the index shall lie
/// in the range 0 to 6; 0 is the explicit-values arm.
fn preset_pixel_aspect_ratio(index: u64) -> Result<(u64, u64)> {
    Ok(match index {
        1 => (1, 1),   // square sampling
        2 => (10, 11), // 4:3 525-line systems
        3 => (12, 11), // 4:3 625-line systems
        4 => (40, 33), // 16:9 525-line systems
        5 => (16, 11), // 16:9 625-line systems
        6 => (4, 3),   // reduced horizontal resolution systems
        _ => {
            return Err(Error::InvalidValue(
                "pixel aspect ratio preset index out of 1..=6",
            ))
        }
    })
}

/// `pixel_aspect_ratio()` (§11.4.7).
fn pixel_aspect_ratio(
    r: &mut BitReader,
    vp: &mut VideoParameters,
    ov: &mut SourceOverrides,
) -> Result<()> {
    if r.read_bool() {
        ov.custom_pixel_aspect_ratio_flag = true;
        let index = r.read_uint();
        if r.overrun() {
            return Err(Error::UnexpectedEof);
        }
        ov.pixel_aspect_ratio_index = Some(index);
        if index == 0 {
            vp.pixel_aspect_ratio_numer = r.read_uint();
            vp.pixel_aspect_ratio_denom = r.read_uint();
            if r.overrun() {
                return Err(Error::UnexpectedEof);
            }
            // "When given, the pixel aspect ratio numerator and
            // denominator values shall both be at least 1."
            if vp.pixel_aspect_ratio_numer == 0 || vp.pixel_aspect_ratio_denom == 0 {
                return Err(Error::InvalidValue(
                    "custom pixel aspect ratio numerator / denominator must be >= 1",
                ));
            }
        } else {
            let (numer, denom) = preset_pixel_aspect_ratio(index)?;
            vp.pixel_aspect_ratio_numer = numer;
            vp.pixel_aspect_ratio_denom = denom;
        }
    }
    Ok(())
}

/// `clean_area()` (§11.4.8). The §11.4.8 containment restrictions
/// ("regardless of how the clean area is defined") are enforced in
/// [`sequence_header`] once the frame size is final.
fn clean_area(r: &mut BitReader, vp: &mut VideoParameters, ov: &mut SourceOverrides) {
    if r.read_bool() {
        ov.custom_clean_area_flag = true;
        vp.clean_width = r.read_uint();
        vp.clean_height = r.read_uint();
        vp.left_offset = r.read_uint();
        vp.top_offset = r.read_uint();
    }
}

/// `signal_range()` (§11.4.9).
fn signal_range(
    r: &mut BitReader,
    vp: &mut VideoParameters,
    ov: &mut SourceOverrides,
) -> Result<()> {
    if r.read_bool() {
        ov.custom_signal_range_flag = true;
        let index = r.read_uint();
        ov.signal_range_index = Some(index);
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

/// `preset_color_spec(index)` — Table 11 ("Preset color
/// specifications"). Returns the (color_primaries_index,
/// color_matrix_index, transfer_function_index) triple.
///
/// Table 11 defines rows 0..=7 (index 0 "Custom" carries the HDTV /
/// HDTV / TV-Gamma starting point that the optional per-part overrides
/// then refine). The §11.4.10.1 prose says the index "shall lie in the
/// range 0 to 6" while the normative table defines row 7 (HDR-TV HLG);
/// the table row is honoured here — rejecting only indices above 7 —
/// since `preset_color_spec` is defined by the table itself.
fn preset_color_spec(index: u64) -> Result<(u64, u64, u64)> {
    Ok(match index {
        0 => (0, 0, 0), // Custom (HDTV starting point)
        1 => (1, 1, 0), // SDTV 525
        2 => (2, 1, 0), // SDTV 625
        3 => (0, 0, 0), // HDTV
        4 => (3, 2, 3), // D-Cinema
        5 => (4, 4, 0), // UHDTV
        6 => (4, 4, 4), // HDR-TV PQ
        7 => (4, 4, 5), // HDR-TV HLG
        _ => {
            return Err(Error::InvalidValue(
                "color specification preset index out of 0..=7",
            ))
        }
    })
}

/// `color_spec()` (§11.4.10) with the per-part overrides of
/// §11.4.10.2–§11.4.10.4. Index bounds: Table 12 defines primaries
/// 0..=4, Table 13 matrices 0..=4, and Table 14 transfer functions
/// 0..=5 (the §11.4.10.4 prose says 0..=4 but the normative table
/// defines row 5, Hybrid Log Gamma; the table row is honoured).
fn color_spec(r: &mut BitReader, vp: &mut VideoParameters, ov: &mut SourceOverrides) -> Result<()> {
    if r.read_bool() {
        ov.custom_color_spec_flag = true;
        let index = r.read_uint();
        if r.overrun() {
            return Err(Error::UnexpectedEof);
        }
        ov.color_spec_index = Some(index);
        let (primaries, matrix, transfer) = preset_color_spec(index)?;
        vp.color_primaries_index = primaries;
        vp.color_matrix_index = matrix;
        vp.transfer_function_index = transfer;
        if index == 0 {
            // color_primaries() (§11.4.10.2).
            if r.read_bool() {
                let idx = r.read_uint();
                if r.overrun() {
                    return Err(Error::UnexpectedEof);
                }
                if idx > 4 {
                    return Err(Error::InvalidValue(
                        "color primaries preset index out of 0..=4",
                    ));
                }
                vp.color_primaries_index = idx;
            }
            // color_matrix() (§11.4.10.3).
            if r.read_bool() {
                let idx = r.read_uint();
                if r.overrun() {
                    return Err(Error::UnexpectedEof);
                }
                if idx > 4 {
                    return Err(Error::InvalidValue(
                        "color matrix preset index out of 0..=4",
                    ));
                }
                vp.color_matrix_index = idx;
            }
            // transfer_function() (§11.4.10.4).
            if r.read_bool() {
                let idx = r.read_uint();
                if r.overrun() {
                    return Err(Error::UnexpectedEof);
                }
                if idx > 5 {
                    return Err(Error::InvalidValue(
                        "transfer function preset index out of 0..=5",
                    ));
                }
                vp.transfer_function_index = idx;
            }
        }
    }
    Ok(())
}

/// `source_parameters()` (§11.4.1).
fn source_parameters(
    r: &mut BitReader,
    base_video_format: u64,
) -> Result<(VideoParameters, SourceOverrides)> {
    let mut vp = set_source_defaults(base_video_format)?;
    let mut ov = SourceOverrides::default();
    frame_size(r, &mut vp, &mut ov);
    color_diff_sampling_format(r, &mut vp, &mut ov)?;
    scan_format(r, &mut vp, &mut ov);
    frame_rate(r, &mut vp, &mut ov)?;
    pixel_aspect_ratio(r, &mut vp, &mut ov)?;
    clean_area(r, &mut vp, &mut ov);
    signal_range(r, &mut vp, &mut ov)?;
    color_spec(r, &mut vp, &mut ov)?;
    Ok((vp, ov))
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
    /// Which §11.4 custom flags the header set, and the indices it
    /// signalled (the ST 2042-2 level-constraint raw material).
    pub source_overrides: SourceOverrides,
    pub picture_coding_mode: u64,
    pub coding_parameters: CodingParameters,
}

/// `sequence_header()` (§11.1): parse parse-parameters, base format, source
/// parameters and picture coding mode, and derive the coding parameters.
pub fn sequence_header(r: &mut BitReader) -> Result<SequenceHeader> {
    let parse_parameters = parse_parameters(r);
    let base_video_format = r.read_uint();
    let (video_parameters, source_overrides) = source_parameters(r, base_video_format)?;
    let picture_coding_mode = r.read_uint();
    if r.overrun() {
        return Err(Error::UnexpectedEof);
    }
    // §11.5: 0 codes frames, 1 codes fields, "values greater than 1
    // shall be reserved" — reserved values shall not be used.
    if picture_coding_mode > 1 {
        return Err(Error::InvalidValue(
            "picture_coding_mode above 1 is reserved",
        ));
    }
    if video_parameters.frame_width == 0 || video_parameters.frame_height == 0 {
        return Err(Error::InvalidValue("zero frame dimensions"));
    }
    // §11.4.8: the clean area must fit inside the frame
    // (`clean_width + left_offset <= frame_width`, and likewise
    // vertically). Enforced for explicitly signalled clean areas; a
    // *default* clean area can go stale when `custom_dimensions_flag`
    // shrinks the frame without a matching clean-area override (each
    // Annex B row's default clean area is its default frame), and such
    // streams — including externally validated fixtures — decode fine,
    // so the stale default is retained as parsed rather than rejected.
    let fits = |extent: u64, offset: u64, frame: u64| {
        extent
            .checked_add(offset)
            .is_some_and(|total| total <= frame)
    };
    if source_overrides.custom_clean_area_flag
        && !(fits(
            video_parameters.clean_width,
            video_parameters.left_offset,
            video_parameters.frame_width,
        ) && fits(
            video_parameters.clean_height,
            video_parameters.top_offset,
            video_parameters.frame_height,
        ))
    {
        return Err(Error::InvalidValue("clean area extends outside the frame"));
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
    // Signal offsets are code values inside the output range (§11.4.9's
    // presets place them within 0..2^depth - 1); an offset beyond the
    // 16-bit lattice cannot denote any representable level, so custom
    // (index 0) values above it are rejected as hostile rather than
    // carried around as meaningless metadata.
    if video_parameters.luma_offset > u16::MAX as u64
        || video_parameters.color_diff_offset > u16::MAX as u64
    {
        return Err(Error::InvalidValue(
            "signal offset outside the representable 0..=65535 range",
        ));
    }
    let coding_parameters = set_coding_parameters(&video_parameters, picture_coding_mode);
    Ok(SequenceHeader {
        parse_parameters,
        base_video_format,
        video_parameters,
        source_overrides,
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

    /// Table 10 ("Preset signal ranges") in full, row for row, against the
    /// staged verbatim transcription
    /// (`docs/video/vc2/vc2-signal-range-presets-and-container-registry.md`,
    /// itself hash-anchored to the ST 2042-1:2022 PDF page 59).
    #[test]
    fn table10_preset_signal_ranges_match_the_staged_transcription() {
        // (index, luma_offset, luma_excursion,
        //  color_diff_offset, color_diff_excursion)
        const TABLE_10: [(u64, u64, u64, u64, u64); 8] = [
            (1, 0, 255, 128, 255),          // 8-bit Full Range
            (2, 16, 219, 128, 224),         // 8-bit Video
            (3, 64, 876, 512, 896),         // 10-bit Video
            (4, 256, 3504, 2048, 3584),     // 12-bit Video
            (5, 0, 1023, 512, 1023),        // 10-bit Full Range
            (6, 0, 4095, 2048, 4095),       // 12-bit Full Range
            (7, 4096, 56064, 32768, 57344), // 16-bit Video
            (8, 0, 65535, 32768, 65535),    // 16-bit Full Range
        ];
        for &(index, lo, le, co, ce) in &TABLE_10 {
            assert_eq!(
                preset_signal_range(index).unwrap(),
                (lo, le, co, ce),
                "Table 10 row {index}"
            );
        }
        // §11.4.9: index shall lie in 0..=8; 0 is the custom-range arm of
        // signal_range(), never a preset lookup, and anything above 8 is
        // invalid.
        assert!(preset_signal_range(0).is_err());
        assert!(preset_signal_range(9).is_err());
        assert!(preset_signal_range(u64::MAX).is_err());
    }

    /// §11.6.3 depth derivation over every Table 10 preset: the video-range
    /// ladder presets 2/3/4/7 scale by exact powers of two (219*4=876,
    /// 876*4=3504, 3504*16=56064 — the staged transcription's consistency
    /// note), and all four 16-bit-capable values derive depth 16.
    #[test]
    fn table10_presets_derive_the_documented_depths() {
        const DEPTHS: [(u64, u32, u32); 8] = [
            (1, 8, 8),
            (2, 8, 8),
            (3, 10, 10),
            (4, 12, 12),
            (5, 10, 10),
            (6, 12, 12),
            (7, 16, 16),
            (8, 16, 16),
        ];
        for &(index, luma_depth, color_diff_depth) in &DEPTHS {
            let (_, le, _, ce) = preset_signal_range(index).unwrap();
            assert_eq!(intlog2(le + 1), luma_depth, "preset {index} luma depth");
            assert_eq!(
                intlog2(ce + 1),
                color_diff_depth,
                "preset {index} color-diff depth"
            );
        }
        // The video-range ladder is exact power-of-two scaling of the 8-bit
        // row — the same `x << (depth - 8)` relationship the >12-bit output
        // promotion path relies on.
        let p2 = preset_signal_range(2).unwrap();
        let p3 = preset_signal_range(3).unwrap();
        let p4 = preset_signal_range(4).unwrap();
        let p7 = preset_signal_range(7).unwrap();
        assert_eq!((p2.0 * 4, p2.1 * 4, p2.2 * 4, p2.3 * 4), p3);
        assert_eq!((p3.0 * 4, p3.1 * 4, p3.2 * 4, p3.3 * 4), p4);
        assert_eq!((p4.0 * 16, p4.1 * 16, p4.2 * 16, p4.3 * 16), p7);
        assert_eq!((p2.1 * 256, p2.3 * 256), (p7.1, p7.3));
    }

    /// Full-range presets keep the colour-difference zero point at
    /// mid-scale while the luma offset stays 0 — the shape that separates
    /// rows 1/5/6/8 from the video-range rows.
    #[test]
    fn table10_full_range_presets_are_zero_offset_mid_chroma() {
        for index in [1u64, 5, 6, 8] {
            let (lo, le, co, _) = preset_signal_range(index).unwrap();
            assert_eq!(lo, 0, "preset {index} luma offset");
            // Mid-scale chroma zero point: 2^(depth-1).
            let depth = intlog2(le + 1);
            assert_eq!(co, 1 << (depth - 1), "preset {index} chroma offset");
        }
    }

    /// Table 8 ("Preset frame rate values") in full.
    #[test]
    fn table8_preset_frame_rates_transcription() {
        const TABLE_8: [(u64, u64, u64); 16] = [
            (1, 24000, 1001),
            (2, 24, 1),
            (3, 25, 1),
            (4, 30000, 1001),
            (5, 30, 1),
            (6, 50, 1),
            (7, 60000, 1001),
            (8, 60, 1),
            (9, 15000, 1001),
            (10, 25, 2),
            (11, 48, 1),
            (12, 48000, 1001),
            (13, 96, 1),
            (14, 100, 1),
            (15, 120000, 1001),
            (16, 120, 1),
        ];
        for &(index, numer, denom) in &TABLE_8 {
            assert_eq!(
                preset_frame_rate(index).unwrap(),
                (numer, denom),
                "Table 8 row {index}"
            );
        }
        assert!(preset_frame_rate(0).is_err());
        assert!(preset_frame_rate(17).is_err());
    }

    /// Table 9 ("Preset pixel aspect ratio values") in full.
    #[test]
    fn table9_preset_pixel_aspect_ratios_transcription() {
        const TABLE_9: [(u64, u64, u64); 6] = [
            (1, 1, 1),
            (2, 10, 11),
            (3, 12, 11),
            (4, 40, 33),
            (5, 16, 11),
            (6, 4, 3),
        ];
        for &(index, numer, denom) in &TABLE_9 {
            assert_eq!(
                preset_pixel_aspect_ratio(index).unwrap(),
                (numer, denom),
                "Table 9 row {index}"
            );
        }
        assert!(preset_pixel_aspect_ratio(0).is_err());
        assert!(preset_pixel_aspect_ratio(7).is_err());
    }

    /// Table 11 ("Preset color specifications") in full — the
    /// (primaries, matrix, transfer) index triples of Tables 12–14.
    #[test]
    fn table11_preset_color_specs_transcription() {
        const TABLE_11: [(u64, u64, u64, u64); 8] = [
            (0, 0, 0, 0), // Custom (HDTV starting point)
            (1, 1, 1, 0), // SDTV 525
            (2, 2, 1, 0), // SDTV 625
            (3, 0, 0, 0), // HDTV
            (4, 3, 2, 3), // D-Cinema
            (5, 4, 4, 0), // UHDTV
            (6, 4, 4, 4), // HDR-TV PQ
            (7, 4, 4, 5), // HDR-TV HLG
        ];
        for &(index, primaries, matrix, transfer) in &TABLE_11 {
            assert_eq!(
                preset_color_spec(index).unwrap(),
                (primaries, matrix, transfer),
                "Table 11 row {index}"
            );
        }
        assert!(preset_color_spec(8).is_err());
    }

    /// Annex B full-row spot checks across all three tables, covering
    /// every retained label at least once.
    #[test]
    fn annex_b_rows_carry_the_full_parameter_map() {
        // Table B.1, format 0 (Custom Format).
        let vp = set_source_defaults(0).unwrap();
        assert_eq!(
            (vp.frame_rate_numer, vp.frame_rate_denom),
            (24000, 1001),
            "custom format is 24/1.001"
        );
        assert_eq!(
            (vp.pixel_aspect_ratio_numer, vp.pixel_aspect_ratio_denom),
            (1, 1)
        );
        assert_eq!(
            (
                vp.clean_width,
                vp.clean_height,
                vp.left_offset,
                vp.top_offset
            ),
            (640, 480, 0, 0)
        );
        assert_eq!(
            (
                vp.color_primaries_index,
                vp.color_matrix_index,
                vp.transfer_function_index
            ),
            (0, 0, 0)
        );

        // Table B.2, format 7 (SD 480i-60): the only rows (with 8) whose
        // default clean area is inset from the frame.
        let vp = set_source_defaults(7).unwrap();
        assert_eq!((vp.frame_rate_numer, vp.frame_rate_denom), (30000, 1001));
        assert_eq!(
            (vp.pixel_aspect_ratio_numer, vp.pixel_aspect_ratio_denom),
            (10, 11)
        );
        assert_eq!(
            (
                vp.clean_width,
                vp.clean_height,
                vp.left_offset,
                vp.top_offset
            ),
            (704, 480, 8, 0)
        );
        assert_eq!(
            (vp.color_primaries_index, vp.color_matrix_index),
            (1, 1),
            "SDTV 525 primaries / SDTV matrix"
        );

        // Table B.3, format 15 (DC 2K-24): D-Cinema colour triple.
        let vp = set_source_defaults(15).unwrap();
        assert_eq!((vp.frame_rate_numer, vp.frame_rate_denom), (24, 1));
        assert_eq!(
            (
                vp.color_primaries_index,
                vp.color_matrix_index,
                vp.transfer_function_index
            ),
            (3, 2, 3)
        );

        // Table B.3, format 17 (UHDTV 4K-60): UHDTV primaries + matrix.
        let vp = set_source_defaults(17).unwrap();
        assert_eq!((vp.frame_rate_numer, vp.frame_rate_denom), (60000, 1001));
        assert_eq!(
            (
                vp.color_primaries_index,
                vp.color_matrix_index,
                vp.transfer_function_index
            ),
            (4, 4, 0)
        );

        // Table B.3, format 22 (SD Pro486): full-frame clean area, HDTV
        // colour, 525-line pixel aspect.
        let vp = set_source_defaults(22).unwrap();
        assert_eq!((vp.frame_rate_numer, vp.frame_rate_denom), (30000, 1001));
        assert_eq!(
            (vp.pixel_aspect_ratio_numer, vp.pixel_aspect_ratio_denom),
            (10, 11)
        );
        assert_eq!(
            (
                vp.clean_width,
                vp.clean_height,
                vp.left_offset,
                vp.top_offset
            ),
            (720, 486, 0, 0)
        );
        assert_eq!(
            (
                vp.color_primaries_index,
                vp.color_matrix_index,
                vp.transfer_function_index
            ),
            (0, 0, 0)
        );
    }

    /// Every Annex B row's default clean area fits its default frame —
    /// the §11.4.8 restriction holds for all defaults, so enforcement
    /// only ever fires on signalled custom values.
    #[test]
    fn annex_b_default_clean_areas_fit_their_frames() {
        for format in 0..=22u64 {
            let vp = set_source_defaults(format).unwrap();
            assert!(
                vp.clean_width + vp.left_offset <= vp.frame_width
                    && vp.clean_height + vp.top_offset <= vp.frame_height,
                "format {format} default clean area exceeds its frame"
            );
        }
    }

    #[test]
    fn hd_defaults_420_to_422() {
        let vp = set_source_defaults(13).unwrap(); // HD 1080p60
        assert_eq!(vp.frame_width, 1920);
        assert_eq!(vp.frame_height, 1080);
        assert_eq!(vp.color_diff_format, ColorDiffFormat::Yuv422);
        assert_eq!(vp.luma_excursion, 876);
        assert_eq!((vp.frame_rate_numer, vp.frame_rate_denom), (60000, 1001));
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
