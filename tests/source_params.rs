//! Source-parameter retention tests (§11.4.6–§11.4.10): the sequence
//! header keeps the full Annex B parameter map — frame rate, pixel
//! aspect ratio, clean area and colour specification, preset or
//! custom — plus the `SourceOverrides` record of which custom flags the
//! stream set, and rejects the out-of-range indices and degenerate
//! values §11.4 rules out.

mod common;

use common::BitWriter;
use oxideav_vc2::bitio::BitReader;
use oxideav_vc2::params::{sequence_header, SequenceHeader};
use oxideav_vc2::Error;

/// Options for the full-control sequence-header builder.
#[derive(Clone, Copy, Default)]
struct HeaderSpec {
    base_video_format: u64,
    /// `Some((w, h))` sets `custom_dimensions_flag`.
    frame_size: Option<(u64, u64)>,
    /// `Some(index)` sets the frame-rate flag with a Table 8 preset;
    /// `Some(0)` is invalid here — use `frame_rate_custom`.
    frame_rate_preset: Option<u64>,
    /// `Some((numer, denom))` sets the flag with index 0 + values.
    frame_rate_custom: Option<(u64, u64)>,
    pixel_aspect_preset: Option<u64>,
    pixel_aspect_custom: Option<(u64, u64)>,
    /// `Some((cw, ch, left, top))` sets `custom_clean_area_flag`.
    clean_area: Option<(u64, u64, u64, u64)>,
    /// Table 11 preset index for the colour spec.
    color_spec_preset: Option<u64>,
    /// Per-part overrides riding colour-spec index 0:
    /// (primaries, matrix, transfer), each optional.
    color_spec_custom: Option<(Option<u64>, Option<u64>, Option<u64>)>,
    /// Explicit picture coding mode (default 0 = frames).
    picture_coding_mode: u64,
}

/// Build a sequence-header body per §11.1 with the given overrides.
fn header_body(spec: &HeaderSpec) -> Vec<u8> {
    let mut w = BitWriter::default();
    // parse_parameters: major 2, minor 0, profile 0, level 0.
    w.put_uint(2);
    w.put_uint(0);
    w.put_uint(0);
    w.put_uint(0);
    w.put_uint(spec.base_video_format);
    // frame_size.
    match spec.frame_size {
        Some((width, height)) => {
            w.put_bool(true);
            w.put_uint(width);
            w.put_uint(height);
        }
        None => w.put_bool(false),
    }
    // color_diff_sampling_format: default.
    w.put_bool(false);
    // scan_format: default.
    w.put_bool(false);
    // frame_rate.
    match (spec.frame_rate_preset, spec.frame_rate_custom) {
        (Some(index), _) => {
            w.put_bool(true);
            w.put_uint(index);
        }
        (None, Some((numer, denom))) => {
            w.put_bool(true);
            w.put_uint(0);
            w.put_uint(numer);
            w.put_uint(denom);
        }
        (None, None) => w.put_bool(false),
    }
    // pixel_aspect_ratio.
    match (spec.pixel_aspect_preset, spec.pixel_aspect_custom) {
        (Some(index), _) => {
            w.put_bool(true);
            w.put_uint(index);
        }
        (None, Some((numer, denom))) => {
            w.put_bool(true);
            w.put_uint(0);
            w.put_uint(numer);
            w.put_uint(denom);
        }
        (None, None) => w.put_bool(false),
    }
    // clean_area.
    match spec.clean_area {
        Some((cw, ch, left, top)) => {
            w.put_bool(true);
            w.put_uint(cw);
            w.put_uint(ch);
            w.put_uint(left);
            w.put_uint(top);
        }
        None => w.put_bool(false),
    }
    // signal_range: default.
    w.put_bool(false);
    // color_spec.
    match (spec.color_spec_preset, spec.color_spec_custom) {
        (Some(index), _) => {
            w.put_bool(true);
            w.put_uint(index);
            if index == 0 {
                // No per-part overrides.
                w.put_bool(false);
                w.put_bool(false);
                w.put_bool(false);
            }
        }
        (None, Some((primaries, matrix, transfer))) => {
            w.put_bool(true);
            w.put_uint(0);
            for part in [primaries, matrix, transfer] {
                match part {
                    Some(idx) => {
                        w.put_bool(true);
                        w.put_uint(idx);
                    }
                    None => w.put_bool(false),
                }
            }
        }
        (None, None) => w.put_bool(false),
    }
    w.put_uint(spec.picture_coding_mode);
    w.into_bytes()
}

fn parse(spec: &HeaderSpec) -> Result<SequenceHeader, Error> {
    let body = header_body(spec);
    sequence_header(&mut BitReader::new(&body))
}

#[test]
fn all_defaults_header_retains_the_annex_b_row() {
    // Base format 13 (HD 1080p-60) untouched: every display field comes
    // from Table B.2 and no override is recorded.
    let seq = parse(&HeaderSpec {
        base_video_format: 13,
        ..Default::default()
    })
    .expect("parse");
    let vp = &seq.video_parameters;
    assert_eq!((vp.frame_rate_numer, vp.frame_rate_denom), (60000, 1001));
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
        (1920, 1080, 0, 0)
    );
    assert_eq!(
        (
            vp.color_primaries_index,
            vp.color_matrix_index,
            vp.transfer_function_index
        ),
        (0, 0, 0)
    );
    assert_eq!(seq.source_overrides, Default::default());
}

#[test]
fn preset_overrides_are_retained_with_their_indices() {
    // Format 13 pushed to 48 fps (Table 8 index 11), square-pixel
    // override via Table 9 index 1, HDR-TV HLG colour (Table 11 row 7 —
    // the row the §11.4.10.1 prose forgets but the table defines).
    let seq = parse(&HeaderSpec {
        base_video_format: 13,
        frame_rate_preset: Some(11),
        pixel_aspect_preset: Some(1),
        color_spec_preset: Some(7),
        ..Default::default()
    })
    .expect("parse");
    let vp = &seq.video_parameters;
    assert_eq!((vp.frame_rate_numer, vp.frame_rate_denom), (48, 1));
    assert_eq!(
        (vp.pixel_aspect_ratio_numer, vp.pixel_aspect_ratio_denom),
        (1, 1)
    );
    assert_eq!(
        (
            vp.color_primaries_index,
            vp.color_matrix_index,
            vp.transfer_function_index
        ),
        (4, 4, 5)
    );
    let ov = &seq.source_overrides;
    assert!(ov.custom_frame_rate_flag);
    assert_eq!(ov.frame_rate_index, Some(11));
    assert!(ov.custom_pixel_aspect_ratio_flag);
    assert_eq!(ov.pixel_aspect_ratio_index, Some(1));
    assert!(ov.custom_color_spec_flag);
    assert_eq!(ov.color_spec_index, Some(7));
    assert!(!ov.custom_dimensions_flag);
    assert!(!ov.custom_clean_area_flag);
    assert!(!ov.custom_signal_range_flag);
}

#[test]
fn fully_custom_values_are_retained() {
    let seq = parse(&HeaderSpec {
        base_video_format: 0,
        frame_size: Some((64, 48)),
        frame_rate_custom: Some((17, 3)),
        pixel_aspect_custom: Some((59, 54)),
        clean_area: Some((60, 40, 4, 8)),
        color_spec_custom: Some((Some(4), Some(4), Some(5))),
        ..Default::default()
    })
    .expect("parse");
    let vp = &seq.video_parameters;
    assert_eq!((vp.frame_width, vp.frame_height), (64, 48));
    assert_eq!((vp.frame_rate_numer, vp.frame_rate_denom), (17, 3));
    assert_eq!(
        (vp.pixel_aspect_ratio_numer, vp.pixel_aspect_ratio_denom),
        (59, 54)
    );
    assert_eq!(
        (
            vp.clean_width,
            vp.clean_height,
            vp.left_offset,
            vp.top_offset
        ),
        (60, 40, 4, 8)
    );
    assert_eq!(
        (
            vp.color_primaries_index,
            vp.color_matrix_index,
            vp.transfer_function_index
        ),
        (4, 4, 5)
    );
    let ov = &seq.source_overrides;
    assert!(ov.custom_dimensions_flag);
    assert_eq!(ov.frame_rate_index, Some(0));
    assert_eq!(ov.pixel_aspect_ratio_index, Some(0));
    assert!(ov.custom_clean_area_flag);
    assert_eq!(ov.color_spec_index, Some(0));
}

#[test]
fn color_spec_index0_starts_from_the_custom_row() {
    // Index 0 with no per-part overrides is the Table 11 "Custom" row:
    // HDTV / HDTV / TV Gamma — even when the base format's default said
    // otherwise (format 15 defaults to the D-Cinema triple).
    let seq = parse(&HeaderSpec {
        base_video_format: 15,
        color_spec_preset: Some(0),
        ..Default::default()
    })
    .expect("parse");
    let vp = &seq.video_parameters;
    assert_eq!(
        (
            vp.color_primaries_index,
            vp.color_matrix_index,
            vp.transfer_function_index
        ),
        (0, 0, 0)
    );
    // A partial override refines only the flagged part.
    let seq = parse(&HeaderSpec {
        base_video_format: 15,
        color_spec_custom: Some((None, Some(3), None)),
        ..Default::default()
    })
    .expect("parse");
    let vp = &seq.video_parameters;
    assert_eq!(
        (
            vp.color_primaries_index,
            vp.color_matrix_index,
            vp.transfer_function_index
        ),
        (0, 3, 0)
    );
}

#[test]
fn out_of_range_indices_and_degenerate_values_are_rejected() {
    let base = HeaderSpec {
        base_video_format: 0,
        ..Default::default()
    };
    // Frame rate: index above Table 8, zero numerator, zero denominator.
    for spec in [
        HeaderSpec {
            frame_rate_preset: Some(17),
            ..base
        },
        HeaderSpec {
            frame_rate_custom: Some((0, 1)),
            ..base
        },
        HeaderSpec {
            frame_rate_custom: Some((30, 0)),
            ..base
        },
        // Pixel aspect ratio: index above Table 9, zero values.
        HeaderSpec {
            pixel_aspect_preset: Some(7),
            ..base
        },
        HeaderSpec {
            pixel_aspect_custom: Some((0, 1)),
            ..base
        },
        // Colour spec: index above Table 11, per-part indices above
        // Tables 12/13/14.
        HeaderSpec {
            color_spec_preset: Some(8),
            ..base
        },
        HeaderSpec {
            color_spec_custom: Some((Some(5), None, None)),
            ..base
        },
        HeaderSpec {
            color_spec_custom: Some((None, Some(5), None)),
            ..base
        },
        HeaderSpec {
            color_spec_custom: Some((None, None, Some(6))),
            ..base
        },
        // Reserved picture coding mode (§11.5).
        HeaderSpec {
            picture_coding_mode: 2,
            ..base
        },
    ] {
        assert!(
            matches!(parse(&spec), Err(Error::InvalidValue(_))),
            "spec unexpectedly parsed"
        );
    }
    // Transfer-function index 5 (Table 14's HLG row, past the stale
    // prose bound) is legal.
    assert!(parse(&HeaderSpec {
        color_spec_custom: Some((None, None, Some(5))),
        ..base
    })
    .is_ok());
}

#[test]
fn signalled_clean_area_must_fit_the_frame() {
    // 60 + 8 > 64: rejected when explicitly signalled.
    let bad = HeaderSpec {
        base_video_format: 0,
        frame_size: Some((64, 48)),
        clean_area: Some((60, 40, 8, 0)),
        ..Default::default()
    };
    assert!(matches!(parse(&bad), Err(Error::InvalidValue(_))));
    // A *default* clean area stale-exceeding an overridden smaller
    // frame is retained as parsed (Annex B defaults track the default
    // frame, and externally validated small-frame streams do this).
    let stale = HeaderSpec {
        base_video_format: 0,
        frame_size: Some((8, 8)),
        ..Default::default()
    };
    let seq = parse(&stale).expect("stale default clean area decodes");
    assert_eq!(seq.video_parameters.clean_width, 640);
    assert!(!seq.source_overrides.custom_clean_area_flag);
}

#[test]
fn sequence_decoder_exposes_the_header_mid_sequence() {
    use common::{build_units, hq_slice_bytes, picture_body, PicParams};
    use oxideav_vc2::SequenceDecoder;

    let p = PicParams::hq_depth0();
    let seq_body = common::sequence_header_body(2, 2, p.major_version);
    let c = [0i64; 4];
    let pic = picture_body(&p, 0, &[hq_slice_bytes(p.qindex, &c, &c, &c)]);

    // Header + picture without an end-of-sequence unit: the walker is
    // mid-sequence and surfaces the parsed header.
    let mut units = Vec::new();
    common::parse_info(&mut units, 0x00, (13 + seq_body.len()) as u32, 0);
    units.extend_from_slice(&seq_body);
    common::parse_info(&mut units, 0xE8, 0, (13 + seq_body.len()) as u32);
    units.extend_from_slice(&pic);

    let mut dec = SequenceDecoder::new();
    let pics = dec.push(&units).expect("push");
    assert_eq!(pics.len(), 1);
    let header = dec.sequence_header().expect("header retained");
    assert_eq!(header.video_parameters.frame_width, 2);
    // The bootstrap harness default is 24/1.001 via base format 0.
    assert_eq!(header.video_parameters.frame_rate_numer, 24000);

    // An end-of-sequence unit resets the per-sequence state (§10.4.1),
    // clearing the exposed header.
    let full = build_units(&[]);
    dec.push(&full).expect("eos");
    assert!(dec.sequence_header().is_none());
}
