//! SMPTE ST 2042-2 generalized-level and ST 2042-1 Annex C profile
//! conformance checks: level/base-format coverage, the §5.3 custom-flag
//! rules and their carve-outs, the §5.4 picture constraints, the §5.5
//! structure rule, and the profile parse-code tables — via the opt-in
//! `conformance` module.

mod common;

use common::{
    build_units, fragment_setup_body, header_body, hq_slice_bytes, picture_body, HeaderSpec,
    PicParams,
};
use oxideav_vc2::bitio::BitReader;
use oxideav_vc2::conformance::{
    check_sequence_header, check_stream, check_transform_parameters, level_base_video_formats,
    Violation,
};
use oxideav_vc2::params::sequence_header;

fn parse(spec: &HeaderSpec) -> oxideav_vc2::params::SequenceHeader {
    let body = header_body(spec);
    sequence_header(&mut BitReader::new(&body)).expect("header parses")
}

#[test]
fn level_format_sets_match_clause_5_2_2() {
    assert_eq!(level_base_video_formats(1), Some(&[1, 2, 3, 4, 5, 6][..]));
    assert_eq!(level_base_video_formats(2), Some(&[7, 8, 22][..]));
    assert_eq!(
        level_base_video_formats(3),
        Some(&[9, 10, 11, 12, 13, 14, 21][..])
    );
    assert_eq!(level_base_video_formats(4), Some(&[15][..]));
    assert_eq!(level_base_video_formats(5), Some(&[16][..]));
    assert_eq!(level_base_video_formats(6), Some(&[17, 18][..]));
    assert_eq!(level_base_video_formats(7), Some(&[19, 20][..]));
    for level in [0, 8, 63, 64, 65] {
        assert_eq!(level_base_video_formats(level), None);
    }
}

#[test]
fn conforming_default_headers_grade_clean() {
    // One representative per level, defaults only, with the
    // picture_coding_mode matching the format's source sampling.
    for (level, format, mode) in [
        (1, 4, 0),
        (2, 7, 1),
        (2, 22, 1),
        (3, 13, 0),
        (3, 11, 1),
        (4, 15, 0),
        (5, 16, 0),
        (6, 17, 0),
        (7, 19, 0),
    ] {
        let seq = parse(&HeaderSpec {
            level,
            base_video_format: format,
            picture_coding_mode: mode,
            ..Default::default()
        });
        assert_eq!(
            check_sequence_header(&seq),
            vec![],
            "level {level} format {format}"
        );
    }
    // Level 0 conforms to nothing and constrains nothing.
    let seq = parse(&HeaderSpec {
        level: 0,
        base_video_format: 0,
        frame_size: Some((64, 48)),
        signal_range_preset: Some(8),
        ..Default::default()
    });
    assert_eq!(check_sequence_header(&seq), vec![]);
    // Specialized levels (>= 64) are constrained elsewhere; nothing to
    // grade here.
    let seq = parse(&HeaderSpec {
        level: 65,
        base_video_format: 13,
        ..Default::default()
    });
    assert_eq!(check_sequence_header(&seq), vec![]);
}

#[test]
fn reserved_levels_and_profiles_are_flagged() {
    let seq = parse(&HeaderSpec {
        level: 8,
        base_video_format: 13,
        ..Default::default()
    });
    assert_eq!(
        check_sequence_header(&seq),
        vec![Violation::ReservedLevel { level: 8 }]
    );

    // Profile 3 (high quality) is defined; 5 is reserved; 1 and 2 are
    // earlier-version profiles tolerated only below major version 3.
    for (profile, major, ok) in [(3, 2, true), (1, 2, true), (1, 3, false), (5, 2, false)] {
        let seq = parse(&HeaderSpec {
            major_version: major,
            profile,
            ..Default::default()
        });
        let violations = check_sequence_header(&seq);
        assert_eq!(
            violations.is_empty(),
            ok,
            "profile {profile} major {major}: {violations:?}"
        );
    }
}

#[test]
fn level_base_format_coverage_is_enforced() {
    // Format 7 belongs to level 2, not level 3.
    let seq = parse(&HeaderSpec {
        level: 3,
        base_video_format: 7,
        picture_coding_mode: 1,
        ..Default::default()
    });
    assert_eq!(
        check_sequence_header(&seq),
        vec![Violation::LevelBaseFormat {
            level: 3,
            base_video_format: 7
        }]
    );
}

#[test]
fn custom_flags_forbidden_outside_carve_outs() {
    // Signal-range override at level 3.
    let seq = parse(&HeaderSpec {
        level: 3,
        base_video_format: 13,
        signal_range_preset: Some(3),
        ..Default::default()
    });
    assert_eq!(
        check_sequence_header(&seq),
        vec![Violation::CustomFlagForbidden {
            level: 3,
            flag: "custom_signal_range_flag"
        }]
    );
    // Chroma, aspect, clean-area and colour-spec overrides likewise.
    let seq = parse(&HeaderSpec {
        level: 3,
        base_video_format: 13,
        color_diff_index: Some(2),
        pixel_aspect_preset: Some(1),
        clean_area: Some((1920, 1080, 0, 0)),
        color_spec_preset: Some(3),
        ..Default::default()
    });
    let v = check_sequence_header(&seq);
    for flag in [
        "custom_chroma_format_flag",
        "custom_pixel_aspect_ratio_flag",
        "custom_clean_area_flag",
        "custom_color_spec_flag",
    ] {
        assert!(
            v.contains(&Violation::CustomFlagForbidden { level: 3, flag }),
            "missing {flag} in {v:?}"
        );
    }
}

#[test]
fn format7_dimension_carve_out() {
    // 720 x 486 within the envelope: clean.
    let seq = parse(&HeaderSpec {
        level: 2,
        base_video_format: 7,
        frame_size: Some((720, 486)),
        picture_coding_mode: 1,
        ..Default::default()
    });
    assert_eq!(check_sequence_header(&seq), vec![]);
    // 704 x 480 is outside the permitted override envelope.
    let seq = parse(&HeaderSpec {
        level: 2,
        base_video_format: 7,
        frame_size: Some((704, 480)),
        picture_coding_mode: 1,
        ..Default::default()
    });
    assert_eq!(
        check_sequence_header(&seq),
        vec![Violation::DimensionsOutsideFormat7Envelope {
            width: 704,
            height: 480
        }]
    );
    // Any dimension override off format 7 is a plain forbidden flag.
    let seq = parse(&HeaderSpec {
        level: 3,
        base_video_format: 13,
        frame_size: Some((1920, 1080)),
        ..Default::default()
    });
    assert_eq!(
        check_sequence_header(&seq),
        vec![Violation::CustomFlagForbidden {
            level: 3,
            flag: "custom_dimensions_flag"
        }]
    );
}

#[test]
fn progressive_relabel_carve_out() {
    // Format 11 (1080i) relabelled progressive at level 3: permitted,
    // and picture_coding_mode then tracks the overridden sampling.
    let seq = parse(&HeaderSpec {
        level: 3,
        base_video_format: 11,
        scan_format: Some(0),
        picture_coding_mode: 0,
        ..Default::default()
    });
    assert_eq!(check_sequence_header(&seq), vec![]);
    // Format 13 is already progressive — no relabel carve-out.
    let seq = parse(&HeaderSpec {
        level: 3,
        base_video_format: 13,
        scan_format: Some(0),
        ..Default::default()
    });
    assert_eq!(
        check_sequence_header(&seq),
        vec![Violation::ScanFormatOverrideOutsideCarveOut {
            base_video_format: 13
        }]
    );
}

#[test]
fn level4_frame_rate_carve_out() {
    // DC 2K at 48 fps (Table 8 index 11): the one permitted frame-rate
    // override.
    let seq = parse(&HeaderSpec {
        level: 4,
        base_video_format: 15,
        frame_rate_preset: Some(11),
        ..Default::default()
    });
    assert_eq!(check_sequence_header(&seq), vec![]);
    // The same override at level 5 is not permitted.
    let seq = parse(&HeaderSpec {
        level: 5,
        base_video_format: 16,
        frame_rate_preset: Some(11),
        ..Default::default()
    });
    assert_eq!(
        check_sequence_header(&seq),
        vec![Violation::FrameRateOverrideOutsideLevel4Exception { level: 5 }]
    );
    // A different rate at level 4 is not permitted either.
    let seq = parse(&HeaderSpec {
        level: 4,
        base_video_format: 15,
        frame_rate_preset: Some(3),
        ..Default::default()
    });
    assert_eq!(
        check_sequence_header(&seq),
        vec![Violation::FrameRateOverrideOutsideLevel4Exception { level: 4 }]
    );
}

#[test]
fn picture_coding_mode_must_track_source_sampling() {
    // Format 11 is interlaced; frames coding mismatches.
    let seq = parse(&HeaderSpec {
        level: 3,
        base_video_format: 11,
        picture_coding_mode: 0,
        ..Default::default()
    });
    assert_eq!(
        check_sequence_header(&seq),
        vec![Violation::PictureCodingModeMismatch {
            source_sampling: 1,
            picture_coding_mode: 0
        }]
    );
}

#[test]
fn picture_constraints_of_clause_5_4() {
    let seq = parse(&HeaderSpec {
        level: 3,
        base_video_format: 13,
        ..Default::default()
    });
    let base_tp = {
        // A conforming parameter set: LeGall depth 2, slice grid
        // dividing the DC bands (1920x1080 4:2:2, depth 2 -> luma DC
        // 480x270, chroma DC 240x270).
        let body = picture_body(
            &PicParams {
                wavelet_index: 1,
                dwt_depth: 2,
                slices_x: 120,
                slices_y: 27,
                ..PicParams::hq_depth0()
            },
            0,
            &[],
        );
        let mut r = BitReader::new(&body[4..]); // skip picture_number
        oxideav_vc2::transform::transform_parameters(
            &mut r,
            &seq,
            oxideav_vc2::PictureKind::HighQuality,
        )
        .expect("params parse")
    };
    assert_eq!(check_transform_parameters(&seq, &base_tp), vec![]);

    // Wavelet index above 4 (Fidelity = 5 parses fine, but the level
    // bound excludes it).
    let mut tp = base_tp.clone();
    tp.wavelet_index = 5;
    assert_eq!(
        check_transform_parameters(&seq, &tp),
        vec![Violation::WaveletIndexAboveLevelBound { wavelet_index: 5 }]
    );

    // Transform depth above 4.
    let mut tp = base_tp.clone();
    tp.dwt_depth = 5;
    let v = check_transform_parameters(&seq, &tp);
    assert!(v.contains(&Violation::TransformDepthAboveLevelBound { dwt_depth: 5 }));

    // A slice grid that splits the DC band unevenly (7 does not divide
    // 480).
    let mut tp = base_tp.clone();
    tp.slices_x = 7;
    assert_eq!(
        check_transform_parameters(&seq, &tp),
        vec![Violation::UnevenDcCoefficientsPerSlice {
            slices_x: 7,
            slices_y: 27
        }]
    );

    // Quantization-matrix values must stay in 0..=127.
    let mut tp = base_tp.clone();
    tp.quant_matrix = vec![oxideav_vc2::quant::MatrixLevel::Ll(128)];
    assert_eq!(
        check_transform_parameters(&seq, &tp),
        vec![Violation::QuantMatrixValueOutOfRange { value: 128 }]
    );

    // Asymmetric-transform flags in a major-version-3 sequence.
    let seq3 = parse(&HeaderSpec {
        major_version: 3,
        level: 3,
        base_video_format: 13,
        ..Default::default()
    });
    let mut tp = base_tp.clone();
    tp.asym_transform_flag = true;
    assert_eq!(
        check_transform_parameters(&seq3, &tp),
        vec![Violation::AsymmetricTransformSignalled]
    );

    // Levels without picture constraints grade clean.
    let seq0 = parse(&HeaderSpec {
        level: 0,
        ..Default::default()
    });
    let mut tp = base_tp.clone();
    tp.wavelet_index = 6;
    tp.dwt_depth = 6;
    assert_eq!(check_transform_parameters(&seq0, &tp), vec![]);
}

/// Whole-stream walking: profile parse-code tables and the §5.5
/// no-mixed-units rule.
#[test]
fn stream_checks_profiles_and_structure() {
    // A high-quality picture in a profile-0 (low delay) sequence
    // violates Table C.1.
    let p = PicParams::hq_depth0();
    let c = [0i64; 4];
    let seq_body = header_body(&HeaderSpec {
        profile: 0,
        base_video_format: 0,
        frame_size: Some((2, 2)),
        ..Default::default()
    });
    let pic = picture_body(&p, 0, &[hq_slice_bytes(p.qindex, &c, &c, &c)]);
    let stream = build_units(&[(0x00, seq_body), (0xE8, pic.clone())]);
    assert_eq!(
        check_stream(&stream).expect("walk"),
        vec![Violation::ProfileParseCode {
            profile: 0,
            parse_code: 0xE8
        }]
    );

    // The same stream under profile 3 grades clean (level 0: no level
    // constraints; 2x2 frame with a 1x1 slice grid).
    let seq_body = header_body(&HeaderSpec {
        profile: 3,
        base_video_format: 0,
        frame_size: Some((2, 2)),
        ..Default::default()
    });
    let stream = build_units(&[(0x00, seq_body.clone()), (0xE8, pic.clone())]);
    assert_eq!(check_stream(&stream).expect("walk"), vec![]);

    // Mixing picture and fragment units in one level-2 sequence trips
    // clause 5.5. (Fragments need major version 3; profile 3 keeps the
    // parse codes legal, and format 7 needs field coding.)
    let p3 = PicParams {
        major_version: 3,
        ..PicParams::hq_depth0()
    };
    let seq_body = header_body(&HeaderSpec {
        major_version: 3,
        profile: 3,
        level: 2,
        base_video_format: 7,
        picture_coding_mode: 1,
        ..Default::default()
    });
    let pic3 = picture_body(&p3, 0, &[hq_slice_bytes(p3.qindex, &[0], &[0], &[0])]);
    let setup = fragment_setup_body(&p3, 1);
    let stream = build_units(&[(0x00, seq_body), (0xE8, pic3), (0xEC, setup)]);
    let violations = check_stream(&stream).expect("walk");
    assert!(
        violations.contains(&Violation::MixedPictureAndFragmentUnits { level: 2 }),
        "{violations:?}"
    );
}
