//! Robustness tests: truncated, hostile and garbage streams must produce
//! prompt, deterministic errors — never panics, hangs, or huge
//! allocations. Complements the per-clause conformance tests.

mod common;

use common::{
    build_units, hq_slice_bytes, parse_info, picture_body, sequence_header_body, BitWriter,
    PicParams,
};
use oxideav_vc2::{decode_sequence, Error};

/// A well-formed single-picture stream to truncate at every length.
fn good_stream() -> Vec<u8> {
    let p = PicParams::hq_depth0();
    let y = [10i64, -20, 30, -40];
    let c = [1i64, 2, 3, 4];
    let seq = sequence_header_body(2, 2, p.major_version);
    let pic = picture_body(&p, 7, &[hq_slice_bytes(p.qindex, &y, &c, &c)]);
    build_units(&[(0x00, seq), (0xE8, pic)])
}

#[test]
fn every_truncation_point_errors_cleanly() {
    // Cutting the stream anywhere before its end must yield an error (the
    // final EOS unit is load-bearing too: a fragmentless stream cut right
    // after the picture parses fine only if the cut lands exactly on a
    // data-unit boundary, which never happens here because the EOS header
    // begins immediately after the picture body).
    let stream = good_stream();
    assert!(decode_sequence(&stream).is_ok());
    for len in 0..stream.len() {
        let cut = &stream[..len];
        assert!(
            decode_sequence(cut).is_err(),
            "truncation at {len} of {} unexpectedly decoded",
            stream.len()
        );
    }
}

#[test]
fn truncated_sequence_header_is_unexpected_eof() {
    let stream = good_stream();
    // Cut inside the sequence-header body (byte 20 of a ~13+30 byte unit).
    assert!(matches!(
        decode_sequence(&stream[..20]),
        Err(Error::UnexpectedEof)
    ));
}

#[test]
fn absurd_transform_depth_is_rejected() {
    // dwt_depth = 60: 1 << 60 pad granularity would explode every
    // allocation. Must be rejected during transform-parameter parsing.
    let p = PicParams {
        dwt_depth: 60,
        ..PicParams::hq_depth0()
    };
    let seq = sequence_header_body(2, 2, p.major_version);
    let pic = picture_body(&p, 1, &[hq_slice_bytes(0, &[0; 4], &[0; 4], &[0; 4])]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn deep_transform_on_tiny_picture_is_area_capped() {
    // Depth 16 is within the depth cap, but pads a 2x2 picture to
    // 65536x65536 — the padded-area cap must fire before allocation.
    let p = PicParams {
        dwt_depth: 16,
        ..PicParams::hq_depth0()
    };
    let seq = sequence_header_body(2, 2, p.major_version);
    let pic = picture_body(&p, 1, &[hq_slice_bytes(0, &[0; 4], &[0; 4], &[0; 4])]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn huge_frame_dimensions_are_rejected() {
    // 1M x 1M custom frame: no allocation may be attempted.
    let p = PicParams::hq_depth0();
    let seq = sequence_header_body(1 << 20, 1 << 20, p.major_version);
    let pic = picture_body(&p, 1, &[hq_slice_bytes(0, &[0; 4], &[0; 4], &[0; 4])]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn zero_frame_dimensions_are_rejected() {
    let seq = sequence_header_body(0, 2, 2);
    let stream = build_units(&[(0x00, seq)]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn huge_slice_count_is_rejected() {
    let p = PicParams {
        slices_x: 1 << 32,
        slices_y: 1 << 32,
        ..PicParams::hq_depth0()
    };
    let seq = sequence_header_body(2, 2, p.major_version);
    let pic = picture_body(&p, 1, &[]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn oversized_excursion_is_rejected() {
    // Custom signal range with a 2^40 excursion: unrepresentable in the
    // 16-bit output planes and previously able to overflow the depth math.
    let mut w = BitWriter::default();
    w.put_uint(2); // major
    w.put_uint(0);
    w.put_uint(0);
    w.put_uint(0);
    w.put_uint(0); // custom base format
    w.put_bool(true); // frame_size
    w.put_uint(2);
    w.put_uint(2);
    w.put_bool(true); // color_diff: 4:4:4
    w.put_uint(0);
    w.put_bool(false); // scan_format
    w.put_bool(false); // frame_rate
    w.put_bool(false); // pixel_aspect_ratio
    w.put_bool(false); // clean_area
    w.put_bool(true); // signal_range: custom values (index 0)
    w.put_uint(0);
    w.put_uint(0); // luma_offset
    w.put_uint(1 << 40); // luma_excursion
    w.put_uint(0); // color_diff_offset
    w.put_uint(255); // color_diff_excursion
    w.put_bool(false); // color_spec
    w.put_uint(0); // picture_coding_mode
    let stream = build_units(&[(0x00, w.into_bytes())]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn ld_slice_smaller_than_header_is_rejected() {
    // slice_bytes 5/8 -> some slices are 0 bytes: cannot even hold the
    // 7-bit qindex. Must error, not underflow the length-field math.
    let p = PicParams {
        slices_x: 2,
        slices_y: 2,
        low_delay: true,
        slice_bytes_numerator: 5,
        slice_bytes_denominator: 8,
        ..PicParams::hq_depth0()
    };
    let seq = sequence_header_body(4, 4, p.major_version);
    let pic = picture_body(&p, 1, &[]);
    let stream = build_units(&[(0x00, seq), (0xC8, pic)]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn max_qindex_with_coefficients_does_not_panic() {
    // HQ qindex is a full byte; 255 drives quant_factor into territory
    // that overflowed i64 before the i128/saturating rework. The decode
    // must complete (values clip to the video range).
    let p = PicParams {
        qindex: 255,
        ..PicParams::hq_depth0()
    };
    let y = [3i64, -3, 7, -7];
    let c = [1i64; 4];
    let seq = sequence_header_body(2, 2, p.major_version);
    let pic = picture_body(&p, 1, &[hq_slice_bytes(p.qindex, &y, &c, &c)]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);
    let pics = decode_sequence(&stream).expect("decode");
    // Positive saturated coefficients clip to 255, negative to 0.
    assert_eq!(pics[0].y, vec![255, 0, 255, 0]);
}

#[test]
fn hq_length_overrunning_stream_is_unexpected_eof() {
    // A slice whose declared luma length runs past the end of the input.
    let p = PicParams::hq_depth0();
    let seq = sequence_header_body(2, 2, p.major_version);
    let mut slice = hq_slice_bytes(0, &[1, 2, 3, 4], &[0; 4], &[0; 4]);
    // Bump the luma length byte (index 1 after the qindex byte) to claim
    // far more payload than exists, then truncate the stream right after
    // the slice bytes.
    slice[1] = 0xFF;
    let pic = picture_body(&p, 1, &[slice]);
    let mut stream = Vec::new();
    let seq_next = (13 + seq.len()) as u32;
    parse_info(&mut stream, 0x00, seq_next, 0);
    stream.extend_from_slice(&seq);
    parse_info(&mut stream, 0xE8, (13 + pic.len()) as u32, seq_next);
    stream.extend_from_slice(&pic);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::UnexpectedEof)
    ));
}

#[test]
fn pseudo_random_garbage_never_panics() {
    // Deterministic LCG fuzz-lite: whatever the bytes, decode_sequence
    // must return promptly with *some* Result. Buffers are prefixed with
    // a valid parse-info prefix half the time so the walk gets past the
    // first gate. Kept small so the Miri CI job stays fast.
    let mut state: u64 = 0x243F_6A88_85A3_08D3;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u8
    };
    for case in 0..48 {
        let len = 16 + (case * 5) % 160;
        let mut buf: Vec<u8> = (0..len).map(|_| next()).collect();
        if case % 2 == 0 {
            buf[..4].copy_from_slice(&oxideav_vc2::PARSE_INFO_PREFIX);
            // Plausible parse code so classification passes sometimes.
            buf[4] = [0x00, 0x10, 0xE8, 0xC8, 0xEC, 0xCC, 0x30][case % 7];
        }
        let _ = decode_sequence(&buf);
    }
}
