//! SMPTE ST 2042-4:2018 — mapping a VC-2 stream into the MXF generic
//! container: the identifier ULs (essence container / compression
//! labels, picture-element key, VC-2 sub-descriptor), the Annex B CDCI
//! picture-essence-descriptor mappings from the parsed sequence header,
//! and a wrapped-stream scanner that derives the VC-2 sub-descriptor
//! values (versions, profile, level, distinct wavelet filters,
//! sequence-header identity).
//!
//! Everything here is plain data — no `oxideav-core` dependency — so an
//! MXF container crate can consume it without pulling the registry
//! feature. The 16-byte SMPTE ULs are on-wire values quoted from the
//! staged ST 2042-4 PDF's normative clauses; they are *not*
//! `CodecTag`-shaped identifiers, which is why they live here rather
//! than in the registry tag claims.

use crate::bitio::BitReader;
use crate::params::{self, SequenceHeader};
use crate::sequence::{self, DataUnit};
use crate::transform::{self, TransformParameters};
use crate::{Error, Result};

/// MXF generic-container wrapping mode for VC-2 picture essence
/// (ST 2042-4 §8.1 Table 1 / §9 Table 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrapping {
    /// Frame wrapping: each Picture Element KLV value is one edit unit.
    Frame,
    /// Clip wrapping: one Picture Element KLV holds the whole wrapped
    /// stream.
    Clip,
}

/// Picture Element Key (§8.1):
/// `06.0E.2B.34.01.02.01.01.0D.01.03.01.15.xx.yy.zz` with byte 14
/// (`xx`) the count of Picture Elements in the Picture Item, byte 15
/// (`yy`) per Table 1 (`10h` frame-wrapped, `11h` clip-wrapped) and
/// byte 16 (`zz`) the Essence Element Number.
pub fn picture_element_key(element_count: u8, wrapping: Wrapping, element_number: u8) -> [u8; 16] {
    let wrap = match wrapping {
        Wrapping::Frame => 0x10,
        Wrapping::Clip => 0x11,
    };
    [
        0x06,
        0x0E,
        0x2B,
        0x34,
        0x01,
        0x02,
        0x01,
        0x01,
        0x0D,
        0x01,
        0x03,
        0x01,
        0x15,
        element_count,
        wrap,
        element_number,
    ]
}

/// VC-2 Essence Container Label (§9):
/// `06.0E.2B.34.04.01.01.0D.0D.01.03.01.02.15.xx.00` with byte 15 per
/// Table 2 (`01h` frame-wrapped, `02h` clip-wrapped).
pub fn essence_container_label(wrapping: Wrapping) -> [u8; 16] {
    let wrap = match wrapping {
        Wrapping::Frame => 0x01,
        Wrapping::Clip => 0x02,
    };
    [
        0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, 0x0D, 0x0D, 0x01, 0x03, 0x01, 0x02, 0x15, wrap,
        0x00,
    ]
}

/// VC-2 Picture Essence Compression Label (§10):
/// `06.0E.2B.34.04.01.01.0D.04.01.02.02.03.03.01.00`.
pub const PICTURE_ESSENCE_COMPRESSION_LABEL: [u8; 16] = [
    0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, 0x0D, 0x04, 0x01, 0x02, 0x02, 0x03, 0x03, 0x01, 0x00,
];

/// VC-2 Sub-Descriptor set key (§11.1):
/// `06.0E.2B.34.02.xx.01.01.0D.01.01.01.01.01.74.00`, byte 6 per
/// ST 377-1 — the clause's note fixes it at `53h` for the element
/// lengths this set uses.
pub const SUB_DESCRIPTOR_KEY: [u8; 16] = [
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x53, 0x01, 0x01, 0x0D, 0x01, 0x01, 0x01, 0x01, 0x01, 0x74, 0x00,
];

/// The additional elements the VC-2 Sub-Descriptor defines (§11.1
/// Table 3), each identified by an item UL
/// `06.0E.2B.34.01.01.01.0E.04.01.06.07.NN.00.00.00`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubDescriptorItem {
    /// UInt8, required.
    MajorVersion,
    /// UInt8, required.
    MinorVersion,
    /// UInt8, required.
    Profile,
    /// UInt8, required.
    Level,
    /// Array of UInt8, optional: the distinct `state[wavelet_index]`
    /// values used anywhere in the wrapped stream, each once.
    WaveletFilters,
    /// Boolean, optional: all sequence headers in the wrapped stream
    /// are byte-for-byte identical.
    SequenceHeadersIdentical,
    /// Boolean, optional: every edit unit is a single complete VC-2
    /// sequence (Operating Mode A signals both booleans True, §13).
    EditUnitsAreCompleteSequences,
}

/// The §11.1 Table 3 item UL for a sub-descriptor element.
pub fn sub_descriptor_item_ul(item: SubDescriptorItem) -> [u8; 16] {
    let nn = match item {
        SubDescriptorItem::MajorVersion => 0x01,
        SubDescriptorItem::MinorVersion => 0x02,
        SubDescriptorItem::Profile => 0x03,
        SubDescriptorItem::Level => 0x04,
        SubDescriptorItem::WaveletFilters => 0x05,
        SubDescriptorItem::SequenceHeadersIdentical => 0x06,
        SubDescriptorItem::EditUnitsAreCompleteSequences => 0x07,
    };
    [
        0x06, 0x0E, 0x2B, 0x34, 0x01, 0x01, 0x01, 0x0E, 0x04, 0x01, 0x06, 0x07, nn, 0x00, 0x00,
        0x00,
    ]
}

/// MXF Frame Layout derived per Annex B from
/// `(source_sampling, picture_coding_mode)`. The numeric encoding of
/// these names in a CDCI descriptor comes from ST 377-1 (not staged),
/// so only the names are modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLayout {
    /// Progressive source coded as frames (0, 0).
    FullFrame,
    /// Interlaced source coded as frames (1, 0).
    MixedFields,
    /// Progressive source coded as fields (0, 1).
    SegmentedFrame,
    /// Interlaced source coded as fields (1, 1).
    SeparateFields,
}

/// Annex B Frame Layout mapping.
pub fn frame_layout(seq: &SequenceHeader) -> FrameLayout {
    match (
        seq.video_parameters.source_sampling,
        seq.picture_coding_mode,
    ) {
        (0, 0) => FrameLayout::FullFrame,
        (_, 0) => FrameLayout::MixedFields,
        (0, _) => FrameLayout::SegmentedFrame,
        _ => FrameLayout::SeparateFields,
    }
}

/// Annex B Stored Width / Stored Height: the frame width, and the frame
/// height halved when pictures are fields (`picture_coding_mode` 1) —
/// deliberately including the SEGMENTED_FRAME case; the clause's note
/// records that its table in ST 377-1 G.2.7 contradicts the Stored
/// Height definition and is ignored.
pub fn stored_dimensions(seq: &SequenceHeader) -> (u64, u64) {
    let vp = &seq.video_parameters;
    let height = if seq.picture_coding_mode == 1 {
        vp.frame_height / 2
    } else {
        vp.frame_height
    };
    (vp.frame_width, height)
}

/// Annex B Field Dominance: 1 when `top_field_first`, else 2.
pub fn field_dominance(seq: &SequenceHeader) -> u8 {
    if seq.video_parameters.top_field_first {
        1
    } else {
        2
    }
}

/// Annex B Sample Rate:
/// `{frame_rate_numer, frame_rate_denom}`.
pub fn sample_rate(seq: &SequenceHeader) -> (u64, u64) {
    (
        seq.video_parameters.frame_rate_numer,
        seq.video_parameters.frame_rate_denom,
    )
}

/// Annex B Component Depth: `state[luma_depth]` — always the luma depth
/// even when the colour-difference depth differs (the clause's note).
pub fn component_depth(seq: &SequenceHeader) -> u32 {
    seq.coding_parameters.luma_depth
}

/// Annex B Horizontal Subsampling from
/// `color_diff_format_index` (0 → 1, 1 → 2, 2 → 2).
pub fn horizontal_subsampling(seq: &SequenceHeader) -> u64 {
    match seq.video_parameters.color_diff_format.index() {
        0 => 1,
        _ => 2,
    }
}

/// Annex B Vertical Subsampling (0 → 1, 1 → 1, 2 → 2; the element is
/// mandatory in the descriptor only for 4:2:0, where it departs from
/// the default of 1).
pub fn vertical_subsampling(seq: &SequenceHeader) -> u64 {
    match seq.video_parameters.color_diff_format.index() {
        2 => 2,
        _ => 1,
    }
}

/// Annex B Black Ref Level: `luma_offset`.
pub fn black_ref_level(seq: &SequenceHeader) -> u64 {
    seq.video_parameters.luma_offset
}

/// Annex B White Ref Level: `luma_offset + luma_excursion`.
pub fn white_ref_level(seq: &SequenceHeader) -> u64 {
    seq.video_parameters.luma_offset + seq.video_parameters.luma_excursion
}

/// Annex B Color Range: `color_diff_excursion + 1`.
pub fn color_range(seq: &SequenceHeader) -> u64 {
    seq.video_parameters.color_diff_excursion + 1
}

/// Annex B Video Line Map recommendation for when the video signal
/// standard cannot be determined: `{1, 0}` for FULL_FRAME, else
/// `{1, frame_height / 2 + 1}`.
pub fn recommended_video_line_map(seq: &SequenceHeader) -> [u64; 2] {
    match frame_layout(seq) {
        FrameLayout::FullFrame => [1, 0],
        _ => [1, seq.video_parameters.frame_height / 2 + 1],
    }
}

/// Annex B Transfer Characteristic label for a
/// `transfer_function_index` (Table 14 values 0..=5).
pub fn transfer_characteristic_label(transfer_function_index: u64) -> Option<[u8; 16]> {
    let (b8, b14) = match transfer_function_index {
        0 => (0x0E, 0x09),
        1 => (0x06, 0x05),
        2 => (0x06, 0x06),
        3 => (0x08, 0x07),
        4 => (0x0D, 0x0A),
        5 => (0x0D, 0x0B),
        _ => return None,
    };
    Some([
        0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, b8, 0x04, 0x01, 0x01, 0x01, 0x01, b14, 0x00, 0x00,
    ])
}

/// Annex B Coding Equations label for a `color_matrix_index`
/// (Table 13 values 0..=4).
pub fn coding_equations_label(color_matrix_index: u64) -> Option<[u8; 16]> {
    let (b8, b14) = match color_matrix_index {
        0 => (0x01, 0x02),
        1 => (0x01, 0x01),
        2 => (0x0D, 0x04),
        3 => (0x0D, 0x05),
        4 => (0x0D, 0x06),
        _ => return None,
    };
    Some([
        0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, b8, 0x04, 0x01, 0x01, 0x01, 0x02, b14, 0x00, 0x00,
    ])
}

/// Annex B Color Primaries label for a `color_primaries_index`
/// (Table 12 values 0..=4).
pub fn color_primaries_label(color_primaries_index: u64) -> Option<[u8; 16]> {
    let (b8, b14) = match color_primaries_index {
        0 => (0x06, 0x03),
        1 => (0x06, 0x01),
        2 => (0x06, 0x02),
        3 => (0x0D, 0x05),
        4 => (0x0D, 0x04),
        _ => return None,
    };
    Some([
        0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, b8, 0x04, 0x01, 0x01, 0x01, 0x03, b14, 0x00, 0x00,
    ])
}

/// VC-2 Sub-Descriptor values (§11.1 Table 3) derived by scanning a
/// wrapped VC-2 stream. `EditUnitsAreCompleteSequences` needs the edit
/// unit boundaries — information the elementary stream does not carry —
/// so it is not derived here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubDescriptorValues {
    pub major_version: u64,
    pub minor_version: u64,
    pub profile: u64,
    pub level: u64,
    /// Distinct `state[wavelet_index]` values used anywhere in the
    /// stream, ascending, each once.
    pub wavelet_filters: Vec<u8>,
    /// Whether every sequence-header data unit in the stream is
    /// byte-for-byte identical.
    pub sequence_headers_identical: bool,
}

/// Scan a wrapped VC-2 stream (a concatenation of edit units, which is
/// itself a VC-2 stream, §7.1) and derive the sub-descriptor values.
///
/// The four required scalar elements map from the parse parameters; the
/// wrapped stream must hold them constant to be describable by the
/// single sub-descriptor ST 2042-4 §11 requires, so a stream whose
/// sequence headers disagree on any of them is rejected (§7.3: VC-2
/// properties mapped to mandatory elements have to be fixed across the
/// wrapped stream). Populating the wavelet-filter array requires
/// reading the whole stream (§11.1.1) — every picture's and setup
/// fragment's transform parameters are visited, with coefficient
/// payloads skipped via `next_parse_offset`. Stops at a non-terminal
/// data unit whose `next_parse_offset` is zero, judging the stream on
/// what was walked.
pub fn sub_descriptor_values(data: &[u8]) -> Result<SubDescriptorValues> {
    let mut seq: Option<SequenceHeader> = None;
    let mut scalars: Option<(u64, u64, u64, u64)> = None;
    let mut first_header_bytes: Option<&[u8]> = None;
    let mut identical = true;
    let mut filters: Vec<u8> = Vec::new();
    let mut record = |tp: &TransformParameters| {
        let w = tp.wavelet_index as u8;
        if let Err(at) = filters.binary_search(&w) {
            filters.insert(at, w);
        }
    };
    let mut pos = 0usize;
    while pos < data.len() {
        let mut r = BitReader::new(&data[pos..]);
        let pi = sequence::parse_info(&mut r)?;
        let unit = sequence::classify(pi.parse_code);
        match unit {
            DataUnit::SequenceHeader => {
                let s = params::sequence_header(&mut r)?;
                let pp = &s.parse_parameters;
                let sc = (pp.major_version, pp.minor_version, pp.profile, pp.level);
                match scalars {
                    None => scalars = Some(sc),
                    Some(prev) if prev != sc => {
                        return Err(Error::InvalidValue(
                            "wrapped stream varies version/profile/level across sequence headers",
                        ))
                    }
                    Some(_) => {}
                }
                // Byte-for-byte identity across the whole wrapped
                // stream (the body spans up to the next parse-info
                // header when the offset is known).
                if pi.next_parse_offset >= 13 {
                    let end = (pos + pi.next_parse_offset as usize).min(data.len());
                    let body = &data[pos + 13..end];
                    match first_header_bytes {
                        None => first_header_bytes = Some(body),
                        Some(first) if first != body => identical = false,
                        Some(_) => {}
                    }
                }
                seq = Some(s);
            }
            DataUnit::EndOfSequence => {
                seq = None;
            }
            DataUnit::Picture(kind) => {
                let s = seq.as_ref().ok_or(Error::MissingSequenceHeader)?;
                r.byte_align();
                let _picture_number = r.read_uint_lit(4);
                r.byte_align();
                record(&transform::transform_parameters(&mut r, s, kind)?);
            }
            DataUnit::Fragment(kind) => {
                let s = seq.as_ref().ok_or(Error::MissingSequenceHeader)?;
                let fh = sequence::fragment_header(&mut r)?;
                if fh.fragment_slice_count == 0 {
                    record(&transform::transform_parameters(&mut r, s, kind)?);
                }
            }
            DataUnit::AuxiliaryData | DataUnit::Padding | DataUnit::Reserved => {}
        }
        if pi.next_parse_offset == 0 {
            if matches!(unit, DataUnit::EndOfSequence) {
                pos += 13;
                continue;
            }
            break;
        }
        pos += pi.next_parse_offset as usize;
    }
    let (major_version, minor_version, profile, level) =
        scalars.ok_or(Error::MissingSequenceHeader)?;
    Ok(SubDescriptorValues {
        major_version,
        minor_version,
        profile,
        level,
        wavelet_filters: filters,
        sequence_headers_identical: identical,
    })
}

/// Structural check that one edit unit is a *single complete VC-2
/// sequence in its entirety* — the per-edit-unit property behind the
/// `EditUnitsAreCompleteSequences` sub-descriptor boolean and Operating
/// Mode A (§13: each edit unit shall comprise a single valid VC-2
/// sequence in its entirety).
///
/// Checked structurally, without decoding pictures: the unit must open
/// with a parse-info header carrying the sequence-header parse code
/// (§13.1: each Operating-Mode-A edit unit begins with a parse-info
/// header followed by a sequence header), every walked parse-info
/// header must be intact, exactly one end-of-sequence unit may appear
/// and it must terminate the edit unit's bytes. A unit whose data-unit
/// payload cannot be skipped (zero `next_parse_offset` on a
/// non-terminal unit) is not verifiable and reports `false`.
pub fn edit_unit_is_complete_sequence(unit: &[u8]) -> bool {
    let mut pos = 0usize;
    let mut first = true;
    while pos < unit.len() {
        if unit.len() < pos + 13 || unit[pos..pos + 4] != crate::PARSE_INFO_PREFIX {
            return false;
        }
        let parse_code = unit[pos + 4];
        let kind = sequence::classify(parse_code);
        if first {
            if kind != DataUnit::SequenceHeader {
                return false;
            }
            first = false;
        }
        if kind == DataUnit::EndOfSequence {
            // Must be the terminal unit: exactly the last 13 bytes.
            return pos + 13 == unit.len();
        }
        let next = u32::from_be_bytes([unit[pos + 5], unit[pos + 6], unit[pos + 7], unit[pos + 8]])
            as usize;
        if next < 13 {
            return false;
        }
        pos += next;
    }
    // Ran out of bytes without an end-of-sequence unit.
    false
}

/// Whether *every* edit unit of a wrapped stream is a single complete
/// VC-2 sequence — the `EditUnitsAreCompleteSequences` sub-descriptor
/// value (`true` also being one of the two Operating Mode A signals,
/// §13.1, alongside identical sequence headers). Empty input reports
/// `false`: there is nothing to assert completeness of.
pub fn edit_units_are_complete_sequences<'a>(mut units: impl Iterator<Item = &'a [u8]>) -> bool {
    let mut any = false;
    units.all(|u| {
        any = true;
        edit_unit_is_complete_sequence(u)
    }) && any
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_uls_match_the_staged_clauses() {
        assert_eq!(
            picture_element_key(1, Wrapping::Frame, 2),
            [
                0x06, 0x0E, 0x2B, 0x34, 0x01, 0x02, 0x01, 0x01, 0x0D, 0x01, 0x03, 0x01, 0x15, 0x01,
                0x10, 0x02
            ]
        );
        assert_eq!(picture_element_key(1, Wrapping::Clip, 1)[14], 0x11);
        assert_eq!(
            essence_container_label(Wrapping::Frame),
            [
                0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, 0x0D, 0x0D, 0x01, 0x03, 0x01, 0x02, 0x15,
                0x01, 0x00
            ]
        );
        assert_eq!(essence_container_label(Wrapping::Clip)[14], 0x02);
        assert_eq!(
            PICTURE_ESSENCE_COMPRESSION_LABEL,
            [
                0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, 0x0D, 0x04, 0x01, 0x02, 0x02, 0x03, 0x03,
                0x01, 0x00
            ]
        );
        assert_eq!(SUB_DESCRIPTOR_KEY[5], 0x53);
        assert_eq!(SUB_DESCRIPTOR_KEY[14], 0x74);
        // Table 3 item ULs share the prefix and differ at byte 13.
        for (item, nn) in [
            (SubDescriptorItem::MajorVersion, 0x01),
            (SubDescriptorItem::MinorVersion, 0x02),
            (SubDescriptorItem::Profile, 0x03),
            (SubDescriptorItem::Level, 0x04),
            (SubDescriptorItem::WaveletFilters, 0x05),
            (SubDescriptorItem::SequenceHeadersIdentical, 0x06),
            (SubDescriptorItem::EditUnitsAreCompleteSequences, 0x07),
        ] {
            let ul = sub_descriptor_item_ul(item);
            assert_eq!(
                &ul[..12],
                &[0x06, 0x0E, 0x2B, 0x34, 0x01, 0x01, 0x01, 0x0E, 0x04, 0x01, 0x06, 0x07]
            );
            assert_eq!(ul[12], nn);
            assert_eq!(&ul[13..], &[0x00, 0x00, 0x00]);
        }
    }

    #[test]
    fn annex_b_colour_labels() {
        // Transfer characteristic 0 (TV Gamma per Table 14) and the
        // HLG row 5.
        assert_eq!(
            transfer_characteristic_label(0).unwrap(),
            [
                0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, 0x0E, 0x04, 0x01, 0x01, 0x01, 0x01, 0x09,
                0x00, 0x00
            ]
        );
        assert_eq!(
            transfer_characteristic_label(5).unwrap(),
            [
                0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, 0x0D, 0x04, 0x01, 0x01, 0x01, 0x01, 0x0B,
                0x00, 0x00
            ]
        );
        assert_eq!(transfer_characteristic_label(6), None);
        assert_eq!(
            coding_equations_label(4).unwrap(),
            [
                0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, 0x0D, 0x04, 0x01, 0x01, 0x01, 0x02, 0x06,
                0x00, 0x00
            ]
        );
        assert_eq!(coding_equations_label(5), None);
        assert_eq!(
            color_primaries_label(1).unwrap(),
            [
                0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, 0x06, 0x04, 0x01, 0x01, 0x01, 0x03, 0x01,
                0x00, 0x00
            ]
        );
        assert_eq!(color_primaries_label(5), None);
    }
}
