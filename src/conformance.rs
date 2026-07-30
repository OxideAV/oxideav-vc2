//! Profile and level conformance checking: the SMPTE ST 2042-1:2022
//! Annex C profile constraints and the SMPTE ST 2042-2:2017
//! generalized-level definitions (its clause 5).
//!
//! The decode path stays permissive — a stream whose profile/level
//! signalling is wrong may still decode fine — so these checks are
//! opt-in: [`check_sequence_header`] and [`check_transform_parameters`]
//! grade individual parsed structures, and [`check_stream`] walks a
//! whole stream (data-unit headers, sequence headers and per-picture
//! transform parameters, skipping coefficient payloads) collecting
//! every [`Violation`].
//!
//! Level semantics (ST 2042-2 clause 4): generalized level values 0..=63
//! constrain streams against the Annex B predefined video formats —
//! level 0 means "conforms to no other level" and carries no
//! constraints, levels 1..=7 are defined, and the rest of 8..=63 are
//! reserved. Values >= 64 name *specialized* application levels whose
//! constraints live in their own SMPTE documents (its informative
//! Annex A catalogues 64..=66), so nothing can be checked here and
//! they are reported violation-free.

use crate::bitio::BitReader;
use crate::params::{self, SequenceHeader};
use crate::sequence::{self, DataUnit};
use crate::transform::{self, TransformParameters};
use crate::{Error, Result};

/// A profile or level conformance violation, each tied to the clause it
/// breaks. `Display` spells out the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// ST 2042-1 Annex C.2.1: the profile value is not one this version
    /// defines (0 = low delay, 3 = high quality). Values 1 and 2 name
    /// earlier-version profiles and are tolerated only below major
    /// version 3; everything else is reserved.
    ReservedProfile { profile: u64, major_version: u64 },
    /// ST 2042-1 Annex C.2.2 / C.2.3: a data unit whose parse code the
    /// signalled profile's table (C.1 for low delay, C.2 for high
    /// quality) does not list.
    ProfileParseCode { profile: u64, parse_code: u8 },
    /// ST 2042-2 clause 4: generalized level values 8..=63 are
    /// reserved.
    ReservedLevel { level: u64 },
    /// ST 2042-2 §5.2.2: the base video format is outside the set the
    /// claimed level covers.
    LevelBaseFormat { level: u64, base_video_format: u64 },
    /// ST 2042-2 §5.1 / §5.3: a custom flag that must be False for
    /// levels 1..=7 was set (outside its documented carve-out, where
    /// one exists).
    CustomFlagForbidden { level: u64, flag: &'static str },
    /// ST 2042-2 §5.3: base-format-7 dimension override outside the
    /// permitted 720 x 480..=486 envelope.
    DimensionsOutsideFormat7Envelope { width: u64, height: u64 },
    /// ST 2042-2 §5.3: scan-format override that is not the
    /// interlaced-to-progressive relabelling permitted for base formats
    /// 7, 8, 11, 12 and 22.
    ScanFormatOverrideOutsideCarveOut { base_video_format: u64 },
    /// ST 2042-2 §5.2.2 / §5.3: frame-rate override that is not the
    /// Level 4 exception (base format 15 at Table 8 index 11, 48 fps).
    FrameRateOverrideOutsideLevel4Exception { level: u64 },
    /// ST 2042-2 §5.3: `picture_coding_mode` does not correspond to the
    /// signalled source sampling (progressive -> frames, interlaced ->
    /// fields).
    PictureCodingModeMismatch {
        source_sampling: u64,
        picture_coding_mode: u64,
    },
    /// ST 2042-2 §5.4: `wavelet_index` above 4.
    WaveletIndexAboveLevelBound { wavelet_index: u64 },
    /// ST 2042-2 §5.4: `dwt_depth` outside 0..=4.
    TransformDepthAboveLevelBound { dwt_depth: u64 },
    /// ST 2042-2 §5.4: an asymmetric-transform flag set in a
    /// major-version-3 sequence.
    AsymmetricTransformSignalled,
    /// ST 2042-2 §5.4: `slices_x` / `slices_y` give unequal DC (0-LL)
    /// coefficient counts per slice for some component.
    UnevenDcCoefficientsPerSlice { slices_x: u64, slices_y: u64 },
    /// ST 2042-2 §5.4: a quantization-matrix value outside 0..=127.
    QuantMatrixValueOutOfRange { value: i64 },
    /// ST 2042-2 §5.5: the sequence mixes picture data units and
    /// picture fragment data units.
    MixedPictureAndFragmentUnits { level: u64 },
    /// ST 2042-1 §12.2: picture numbers within a sequence shall
    /// increment by one for each successive picture (wrapping past
    /// 2^32 - 1 back to zero).
    PictureNumberDiscontinuity { expected: u32, found: u32 },
    /// ST 2042-1 §12.2 / §11.5: with field coding, the earliest field
    /// of each frame shall have an even picture number — so a
    /// field-coded sequence must not open on an odd one.
    FirstFieldOddPictureNumber { picture_number: u32 },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::ReservedProfile {
                profile,
                major_version,
            } => write!(
                f,
                "profile {profile} is not defined by this version (Annex C.2.1 \
                 defines 0 = low delay and 3 = high quality; 1 and 2 are \
                 earlier-version profiles, not permitted at major version \
                 {major_version})"
            ),
            Violation::ProfileParseCode {
                profile,
                parse_code,
            } => write!(
                f,
                "parse code 0x{parse_code:02X} is not permitted in a profile-{profile} \
                 sequence (ST 2042-1 Tables C.1/C.2)"
            ),
            Violation::ReservedLevel { level } => {
                write!(
                    f,
                    "generalized level {level} is reserved (ST 2042-2 clause 4)"
                )
            }
            Violation::LevelBaseFormat {
                level,
                base_video_format,
            } => write!(
                f,
                "base video format {base_video_format} is outside level {level}'s \
                 coverage (ST 2042-2 clause 5.2.2)"
            ),
            Violation::CustomFlagForbidden { level, flag } => write!(
                f,
                "{flag} must be False for level {level} (ST 2042-2 clause 5.3)"
            ),
            Violation::DimensionsOutsideFormat7Envelope { width, height } => write!(
                f,
                "base-format-7 dimension override {width}x{height} is outside the \
                 permitted 720 x 480..=486 envelope (ST 2042-2 clause 5.3)"
            ),
            Violation::ScanFormatOverrideOutsideCarveOut { base_video_format } => write!(
                f,
                "scan-format override on base format {base_video_format} is not the \
                 progressive relabelling permitted for formats 7/8/11/12/22 \
                 (ST 2042-2 clause 5.3)"
            ),
            Violation::FrameRateOverrideOutsideLevel4Exception { level } => write!(
                f,
                "frame-rate override at level {level} is not the Level 4 exception \
                 (base format 15 at 48 fps; ST 2042-2 clauses 5.2.2/5.3)"
            ),
            Violation::PictureCodingModeMismatch {
                source_sampling,
                picture_coding_mode,
            } => write!(
                f,
                "picture_coding_mode {picture_coding_mode} does not correspond to \
                 source_sampling {source_sampling} (ST 2042-2 clause 5.3)"
            ),
            Violation::WaveletIndexAboveLevelBound { wavelet_index } => write!(
                f,
                "wavelet_index {wavelet_index} exceeds the level bound of 4 \
                 (ST 2042-2 clause 5.4)"
            ),
            Violation::TransformDepthAboveLevelBound { dwt_depth } => write!(
                f,
                "dwt_depth {dwt_depth} exceeds the level bound of 4 (ST 2042-2 \
                 clause 5.4)"
            ),
            Violation::AsymmetricTransformSignalled => write!(
                f,
                "asymmetric-transform flags must be False in major-version-3 \
                 sequences at levels 1..=7 (ST 2042-2 clause 5.4)"
            ),
            Violation::UnevenDcCoefficientsPerSlice { slices_x, slices_y } => write!(
                f,
                "slices_x/slices_y {slices_x}x{slices_y} give unequal DC-coefficient \
                 counts per slice (ST 2042-2 clause 5.4)"
            ),
            Violation::QuantMatrixValueOutOfRange { value } => write!(
                f,
                "quantization-matrix value {value} is outside 0..=127 (ST 2042-2 \
                 clause 5.4)"
            ),
            Violation::MixedPictureAndFragmentUnits { level } => write!(
                f,
                "level-{level} sequence contains both picture and picture-fragment \
                 data units (ST 2042-2 clause 5.5)"
            ),
            Violation::PictureNumberDiscontinuity { expected, found } => write!(
                f,
                "picture number {found} where {expected} was expected — numbers \
                 increment by one within a sequence, wrapping at 2^32 - 1 \
                 (ST 2042-1 clause 12.2)"
            ),
            Violation::FirstFieldOddPictureNumber { picture_number } => write!(
                f,
                "field-coded sequence opens on odd picture number {picture_number} — \
                 the earliest field of each frame has an even picture number \
                 (ST 2042-1 clauses 11.5/12.2)"
            ),
        }
    }
}

/// The base video formats a generalized level covers (ST 2042-2
/// §5.2.2). `None` for level values that carry no format set — 0 (no
/// constraints), 8..=63 (reserved) and >= 64 (specialized).
pub fn level_base_video_formats(level: u64) -> Option<&'static [u64]> {
    Some(match level {
        1 => &[1, 2, 3, 4, 5, 6],
        2 => &[7, 8, 22],
        3 => &[9, 10, 11, 12, 13, 14, 21],
        4 => &[15],
        5 => &[16],
        6 => &[17, 18],
        7 => &[19, 20],
        _ => return None,
    })
}

/// Parse codes permitted per profile (ST 2042-1 Tables C.1 / C.2).
/// `None` when the profile has no table in this version.
fn profile_allows(profile: u64, parse_code: u8) -> Option<bool> {
    let picture_codes: [u8; 2] = match profile {
        0 => [0xC8, 0xCC], // low delay picture / fragment
        3 => [0xE8, 0xEC], // high quality picture / fragment
        _ => return None,
    };
    Some(matches!(parse_code, 0x00 | 0x10 | 0x20 | 0x30) || picture_codes.contains(&parse_code))
}

/// Grade a parsed sequence header: the Annex C profile value and every
/// ST 2042-2 §5.2/§5.3 sequence-header constraint of its claimed level.
pub fn check_sequence_header(seq: &SequenceHeader) -> Vec<Violation> {
    let mut v = Vec::new();
    let profile = seq.parse_parameters.profile;
    let major_version = seq.parse_parameters.major_version;
    match profile {
        0 | 3 => {}
        1 | 2 if major_version < 3 => {}
        _ => v.push(Violation::ReservedProfile {
            profile,
            major_version,
        }),
    }
    let level = seq.parse_parameters.level;
    match level {
        0 => {}
        1..=7 => check_level_header(seq, level, &mut v),
        8..=63 => v.push(Violation::ReservedLevel { level }),
        // Specialized levels: constrained by their own application
        // specifications, unknowable here.
        _ => {}
    }
    v
}

/// The §5.2.2 / §5.3 sequence-header constraints for levels 1..=7.
fn check_level_header(seq: &SequenceHeader, level: u64, v: &mut Vec<Violation>) {
    let formats = level_base_video_formats(level).expect("levels 1..=7 have format sets");
    if !formats.contains(&seq.base_video_format) {
        v.push(Violation::LevelBaseFormat {
            level,
            base_video_format: seq.base_video_format,
        });
    }
    let ov = &seq.source_overrides;
    let vp = &seq.video_parameters;

    // custom_dimensions_flag: False except for base format 7, where the
    // override must stay inside 720 x 480..=486. (The §5.3 bullet's
    // inner sentence names custom_scan_format_flag, but it sits under —
    // and describes the exception to — the custom_dimensions_flag rule;
    // the enclosing bullet is followed here.)
    if ov.custom_dimensions_flag {
        if seq.base_video_format != 7 {
            v.push(Violation::CustomFlagForbidden {
                level,
                flag: "custom_dimensions_flag",
            });
        } else if vp.frame_width != 720 || !(480..=486).contains(&vp.frame_height) {
            v.push(Violation::DimensionsOutsideFormat7Envelope {
                width: vp.frame_width,
                height: vp.frame_height,
            });
        }
    }
    if ov.custom_chroma_format_flag {
        v.push(Violation::CustomFlagForbidden {
            level,
            flag: "custom_chroma_format_flag",
        });
    }
    // custom_scan_format_flag: False except relabelling formats
    // 7/8/11/12/22 as progressive.
    if ov.custom_scan_format_flag
        && !(matches!(seq.base_video_format, 7 | 8 | 11 | 12 | 22) && vp.source_sampling == 0)
    {
        v.push(Violation::ScanFormatOverrideOutsideCarveOut {
            base_video_format: seq.base_video_format,
        });
    }
    // custom_frame_rate_flag: False except the Level 4 exception —
    // base format 15 overridden to Table 8 index 11 (48 fps).
    if ov.custom_frame_rate_flag
        && !(level == 4 && seq.base_video_format == 15 && ov.frame_rate_index == Some(11))
    {
        v.push(Violation::FrameRateOverrideOutsideLevel4Exception { level });
    }
    if ov.custom_pixel_aspect_ratio_flag {
        v.push(Violation::CustomFlagForbidden {
            level,
            flag: "custom_pixel_aspect_ratio_flag",
        });
    }
    if ov.custom_clean_area_flag {
        v.push(Violation::CustomFlagForbidden {
            level,
            flag: "custom_clean_area_flag",
        });
    }
    if ov.custom_signal_range_flag {
        v.push(Violation::CustomFlagForbidden {
            level,
            flag: "custom_signal_range_flag",
        });
    }
    if ov.custom_color_spec_flag {
        v.push(Violation::CustomFlagForbidden {
            level,
            flag: "custom_color_spec_flag",
        });
    }
    // picture_coding_mode shall correspond to the signalled source
    // sampling: progressive -> frames (0), interlaced -> fields (1).
    if seq.picture_coding_mode != vp.source_sampling {
        v.push(Violation::PictureCodingModeMismatch {
            source_sampling: vp.source_sampling,
            picture_coding_mode: seq.picture_coding_mode,
        });
    }
}

/// Grade a picture's transform parameters against the ST 2042-2 §5.4
/// constraints of the sequence's claimed level. Levels without picture
/// constraints (0, reserved, specialized) grade violation-free.
pub fn check_transform_parameters(
    seq: &SequenceHeader,
    tp: &TransformParameters,
) -> Vec<Violation> {
    let level = seq.parse_parameters.level;
    if !(1..=7).contains(&level) {
        return Vec::new();
    }
    let mut v = Vec::new();
    if tp.wavelet_index > 4 {
        v.push(Violation::WaveletIndexAboveLevelBound {
            wavelet_index: tp.wavelet_index,
        });
    }
    if tp.dwt_depth > 4 {
        v.push(Violation::TransformDepthAboveLevelBound {
            dwt_depth: tp.dwt_depth,
        });
    }
    if seq.parse_parameters.major_version >= 3
        && (tp.asym_transform_index_flag || tp.asym_transform_flag)
    {
        v.push(Violation::AsymmetricTransformSignalled);
    }
    // Equal DC (0-LL) coefficients per slice: the §13.5.6 slice ranges
    // split a subband of extent `e` into floor-spaced pieces, which are
    // all equal exactly when the slice count divides `e` — so check
    // divisibility on the DC band of every component (§13.2.3 padded
    // dimensions).
    let cp = &seq.coding_parameters;
    let scale_w = 1u64 << tp.total_levels();
    let scale_h = 1u64 << tp.dwt_depth;
    let mut uneven = false;
    for (w, h) in [
        (cp.luma_width, cp.luma_height),
        (cp.color_diff_width, cp.color_diff_height),
    ] {
        // DC-band extents: padded dims divided by the per-axis scale,
        // i.e. exactly ceil(dim / scale).
        let dc_w = w.div_ceil(scale_w);
        let dc_h = h.div_ceil(scale_h);
        if dc_w % tp.slices_x != 0 || dc_h % tp.slices_y != 0 {
            uneven = true;
        }
    }
    if uneven {
        v.push(Violation::UnevenDcCoefficientsPerSlice {
            slices_x: tp.slices_x,
            slices_y: tp.slices_y,
        });
    }
    for level_entry in &tp.quant_matrix {
        let values: &[i64] = match level_entry {
            crate::quant::MatrixLevel::Ll(dc) => &[*dc],
            crate::quant::MatrixLevel::H(hv) => &[*hv],
            crate::quant::MatrixLevel::Ac { hl, lh, hh } => &[*hl, *lh, *hh],
        };
        if let Some(&bad) = values.iter().find(|&&x| !(0..=127).contains(&x)) {
            v.push(Violation::QuantMatrixValueOutOfRange { value: bad });
            break;
        }
    }
    v
}

/// §12.2 picture-number bookkeeping for [`check_stream`]: numbers
/// increment by one within a sequence (wrapping at `u32::MAX`), and a
/// field-coded sequence must open on an even number (the earliest field
/// of each frame is even). After a discontinuity the expectation
/// resyncs to the observed number.
fn track_picture_number(
    seq: &SequenceHeader,
    found: u32,
    expected: &mut Option<u32>,
    violations: &mut Vec<Violation>,
) {
    match *expected {
        None => {
            if seq.picture_coding_mode == 1 && found % 2 == 1 {
                violations.push(Violation::FirstFieldOddPictureNumber {
                    picture_number: found,
                });
            }
        }
        Some(want) if want != found => {
            violations.push(Violation::PictureNumberDiscontinuity {
                expected: want,
                found,
            });
        }
        Some(_) => {}
    }
    *expected = Some(found.wrapping_add(1));
}

/// Walk a whole stream collecting profile and level violations.
///
/// Parses data-unit headers, sequence headers and per-picture /
/// per-setup-fragment transform parameters, skipping coefficient
/// payloads via `next_parse_offset`; stops early (returning what it
/// has) if a non-terminal data unit carries a zero `next_parse_offset`,
/// since the payload cannot then be skipped without decoding it.
/// Structural stream errors (bad prefix, truncated header, picture
/// before any sequence header) surface as `Err`.
pub fn check_stream(data: &[u8]) -> Result<Vec<Violation>> {
    let mut violations = Vec::new();
    let mut seq: Option<SequenceHeader> = None;
    let mut saw_picture = false;
    let mut saw_fragment = false;
    let mut mixed_flagged = false;
    // §12.2 picture-number tracking, per sequence.
    let mut expected_number: Option<u32> = None;
    let mut pos = 0usize;
    while pos < data.len() {
        let mut r = BitReader::new(&data[pos..]);
        let pi = sequence::parse_info(&mut r)?;
        let unit = sequence::classify(pi.parse_code);

        // Annex C profile constraint on every data unit of the current
        // sequence (checkable once its header names the profile).
        if let Some(s) = &seq {
            if let Some(false) = profile_allows(s.parse_parameters.profile, pi.parse_code) {
                violations.push(Violation::ProfileParseCode {
                    profile: s.parse_parameters.profile,
                    parse_code: pi.parse_code,
                });
            }
        }

        match unit {
            DataUnit::SequenceHeader => {
                let s = params::sequence_header(&mut r)?;
                violations.extend(check_sequence_header(&s));
                seq = Some(s);
            }
            DataUnit::EndOfSequence => {
                seq = None;
                saw_picture = false;
                saw_fragment = false;
                mixed_flagged = false;
                expected_number = None;
            }
            DataUnit::Picture(kind) => {
                saw_picture = true;
                let s = seq.as_ref().ok_or(Error::MissingSequenceHeader)?;
                r.byte_align();
                let picture_number = r.read_uint_lit(4) as u32; // picture_header (§12.2)
                r.byte_align();
                track_picture_number(s, picture_number, &mut expected_number, &mut violations);
                let tp = transform::transform_parameters(&mut r, s, kind)?;
                violations.extend(check_transform_parameters(s, &tp));
            }
            DataUnit::Fragment(kind) => {
                saw_fragment = true;
                let s = seq.as_ref().ok_or(Error::MissingSequenceHeader)?;
                let fh = sequence::fragment_header(&mut r)?;
                if fh.fragment_slice_count == 0 {
                    // A setup fragment opens a new picture; data
                    // fragments continue it and carry the same number.
                    track_picture_number(
                        s,
                        fh.picture_number,
                        &mut expected_number,
                        &mut violations,
                    );
                    let tp = transform::transform_parameters(&mut r, s, kind)?;
                    violations.extend(check_transform_parameters(s, &tp));
                }
            }
            DataUnit::AuxiliaryData | DataUnit::Padding | DataUnit::Reserved => {}
        }

        if !mixed_flagged && saw_picture && saw_fragment {
            if let Some(s) = &seq {
                let level = s.parse_parameters.level;
                if (1..=7).contains(&level) {
                    violations.push(Violation::MixedPictureAndFragmentUnits { level });
                    mixed_flagged = true;
                }
            }
        }

        if pi.next_parse_offset == 0 {
            if matches!(unit, DataUnit::EndOfSequence) {
                // §10.5.1: an end-of-sequence unit has no body; a zero
                // offset just means "nothing follows to point at".
                pos += 13;
                continue;
            }
            // Cannot skip an unsized payload without decoding it.
            break;
        }
        pos += pi.next_parse_offset as usize;
    }
    Ok(violations)
}
