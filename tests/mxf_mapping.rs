//! SMPTE ST 2042-4 MXF mapping tests: the Annex B CDCI descriptor
//! mappings computed from parsed sequence headers, and the
//! sub-descriptor scanner over whole wrapped streams.

mod common;

use common::{
    build_units, header_body, hq_slice_bytes, picture_body, HeaderSpec, PicParams, SignalRange,
};
use oxideav_vc2::bitio::BitReader;
use oxideav_vc2::mxf::{
    self, frame_layout, recommended_video_line_map, stored_dimensions, FrameLayout,
};
use oxideav_vc2::params::sequence_header;

fn parse(spec: &HeaderSpec) -> oxideav_vc2::params::SequenceHeader {
    let body = header_body(spec);
    sequence_header(&mut BitReader::new(&body)).expect("header parses")
}

#[test]
fn annex_b_descriptor_mappings() {
    // HD 1080i-60 (format 11): interlaced, field-coded, top field
    // first, 10-bit video levels, 4:2:2.
    let seq = parse(&HeaderSpec {
        base_video_format: 11,
        picture_coding_mode: 1,
        ..Default::default()
    });
    assert_eq!(frame_layout(&seq), FrameLayout::SeparateFields);
    assert_eq!(stored_dimensions(&seq), (1920, 540));
    assert_eq!(mxf::field_dominance(&seq), 1);
    assert_eq!(mxf::sample_rate(&seq), (30000, 1001));
    assert_eq!(mxf::component_depth(&seq), 10);
    assert_eq!(mxf::horizontal_subsampling(&seq), 2);
    assert_eq!(mxf::vertical_subsampling(&seq), 1);
    assert_eq!(mxf::black_ref_level(&seq), 64);
    assert_eq!(mxf::white_ref_level(&seq), 64 + 876);
    assert_eq!(mxf::color_range(&seq), 897);
    assert_eq!(recommended_video_line_map(&seq), [1, 541]);

    // The same source relabelled frame-coded: MIXED_FIELDS, full
    // stored height.
    let seq = parse(&HeaderSpec {
        base_video_format: 11,
        picture_coding_mode: 0,
        ..Default::default()
    });
    assert_eq!(frame_layout(&seq), FrameLayout::MixedFields);
    assert_eq!(stored_dimensions(&seq), (1920, 1080));

    // Progressive frame-coded (format 13): FULL_FRAME, line map {1, 0}.
    let seq = parse(&HeaderSpec {
        base_video_format: 13,
        ..Default::default()
    });
    assert_eq!(frame_layout(&seq), FrameLayout::FullFrame);
    assert_eq!(recommended_video_line_map(&seq), [1, 0]);

    // Progressive field-coded: SEGMENTED_FRAME, stored height halved
    // (the clause overrides the contradictory ST 377-1 G.2.7 table).
    let seq = parse(&HeaderSpec {
        base_video_format: 13,
        picture_coding_mode: 1,
        ..Default::default()
    });
    assert_eq!(frame_layout(&seq), FrameLayout::SegmentedFrame);
    assert_eq!(stored_dimensions(&seq), (1920, 540));

    // SD 480i-60 (format 7): top_field_first False -> dominance 2; 4K
    // D-Cinema (format 16): 4:4:4 -> subsampling 1/1, 12-bit.
    let seq = parse(&HeaderSpec {
        base_video_format: 7,
        picture_coding_mode: 1,
        ..Default::default()
    });
    assert_eq!(mxf::field_dominance(&seq), 2);
    let seq = parse(&HeaderSpec {
        base_video_format: 16,
        ..Default::default()
    });
    assert_eq!(mxf::horizontal_subsampling(&seq), 1);
    assert_eq!(mxf::vertical_subsampling(&seq), 1);
    assert_eq!(mxf::component_depth(&seq), 12);

    // 4:2:0 needs the vertical-subsampling element present (value 2).
    let seq = parse(&HeaderSpec {
        base_video_format: 1,
        ..Default::default()
    });
    assert_eq!(mxf::horizontal_subsampling(&seq), 2);
    assert_eq!(mxf::vertical_subsampling(&seq), 2);
}

/// Build a one-picture sequence with the given wavelet index on the
/// shared 8x8 harness shape.
fn one_sequence(wavelet_index: u64, range: SignalRange) -> Vec<(u8, Vec<u8>)> {
    let p = PicParams {
        wavelet_index,
        dwt_depth: if wavelet_index == 0 { 2 } else { 1 },
        ..PicParams::hq_depth0()
    };
    let seq = common::sequence_header_body_full(8, 8, p.major_version, 0, range);
    let y = [0i64; 64];
    let c = [0i64; 64];
    let pic = picture_body(&p, 0, &[hq_slice_bytes(p.qindex, &y, &c, &c)]);
    vec![(0x00, seq), (0xE8, pic)]
}

#[test]
fn sub_descriptor_scan_collects_filters_and_header_identity() {
    // Two concatenated sequences with identical headers but different
    // wavelet filters (Haar-with-shift 4, then LeGall 1).
    let mut units = one_sequence(4, SignalRange::Preset(3));
    units.extend(one_sequence(1, SignalRange::Preset(3)));
    let stream: Vec<u8> = units.chunks(2).flat_map(build_units).collect();
    let values = mxf::sub_descriptor_values(&stream).expect("scan");
    assert_eq!(values.major_version, 2);
    assert_eq!(values.minor_version, 0);
    assert_eq!(values.profile, 0);
    assert_eq!(values.level, 0);
    assert_eq!(values.wavelet_filters, vec![1, 4]);
    assert!(values.sequence_headers_identical);

    // Different signal ranges make the headers differ byte-for-byte.
    let mut units = one_sequence(4, SignalRange::Preset(3));
    units.extend(one_sequence(4, SignalRange::Preset(1)));
    let stream: Vec<u8> = units.chunks(2).flat_map(build_units).collect();
    let values = mxf::sub_descriptor_values(&stream).expect("scan");
    assert_eq!(values.wavelet_filters, vec![4]);
    assert!(!values.sequence_headers_identical);
}

#[test]
fn sub_descriptor_scan_rejects_varying_scalars() {
    // Two sequences disagreeing on the level: not describable by the
    // single required sub-descriptor.
    let p = PicParams::hq_depth0();
    let y = [0i64; 4];
    let seq_a = header_body(&HeaderSpec {
        level: 0,
        base_video_format: 0,
        frame_size: Some((2, 2)),
        ..Default::default()
    });
    let seq_b = header_body(&HeaderSpec {
        level: 1,
        base_video_format: 0,
        frame_size: Some((2, 2)),
        ..Default::default()
    });
    let pic = picture_body(&p, 0, &[hq_slice_bytes(p.qindex, &y, &y, &y)]);
    let mut stream = build_units(&[(0x00, seq_a), (0xE8, pic.clone())]);
    stream.extend(build_units(&[(0x00, seq_b), (0xE8, pic)]));
    assert!(mxf::sub_descriptor_values(&stream).is_err());
}

#[test]
fn edit_unit_completeness_is_structural() {
    use mxf::{edit_unit_is_complete_sequence, edit_units_are_complete_sequences};

    // A whole sequence (header + picture + EOS) is a complete edit
    // unit; Operating Mode A wraps exactly these.
    let units = one_sequence(4, SignalRange::Preset(3));
    let complete = build_units(&units);
    assert!(edit_unit_is_complete_sequence(&complete));

    // Missing EOS: not complete.
    let mut no_eos = complete.clone();
    no_eos.truncate(no_eos.len() - 13);
    assert!(!edit_unit_is_complete_sequence(&no_eos));

    // Trailing bytes after the EOS: not a *single* sequence in its
    // entirety.
    let mut trailing = complete.clone();
    trailing.extend_from_slice(&complete);
    assert!(!edit_unit_is_complete_sequence(
        &trailing[..complete.len() + 4]
    ));
    // Two whole sequences in one edit unit are likewise rejected
    // (the first EOS is not terminal).
    assert!(!edit_unit_is_complete_sequence(&trailing));

    // Not opening on a sequence header: rejected.
    let p = PicParams::hq_depth0();
    let y = [0i64; 64];
    let pic = picture_body(&p, 0, &[hq_slice_bytes(p.qindex, &y, &y, &y)]);
    let headerless = build_units(&[(0xE8, pic)]);
    assert!(!edit_unit_is_complete_sequence(&headerless));

    // The iterator form: all units complete -> true; any incomplete ->
    // false; no units -> false.
    assert!(edit_units_are_complete_sequences(
        [complete.as_slice(), complete.as_slice()].into_iter()
    ));
    assert!(!edit_units_are_complete_sequences(
        [complete.as_slice(), no_eos.as_slice()].into_iter()
    ));
    assert!(!edit_units_are_complete_sequences(std::iter::empty()));
}
