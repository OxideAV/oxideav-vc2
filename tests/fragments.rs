//! §14 fragmented-picture reassembly tests over hand-assembled streams.
//!
//! The normative promise (§14.4 NOTE 1) is that fragmented pictures carry
//! the same slice data, in the same order, as non-fragmented pictures —
//! only the data-unit framing differs. The core tests here therefore build
//! one set of encoded slices and wrap it both ways, asserting the decodes
//! are identical; the remaining tests pin the §14.1/§14.2 stream
//! constraints (setup-before-data, matching picture numbers, raster order,
//! no interleaved pictures, no end-of-sequence mid-picture) and the
//! chunked-push API that keeps fragment state across calls.

mod common;

use common::{
    build_units, fragment_data_body, fragment_setup_body, hq_slice_bytes, ld_slice_bytes,
    picture_body, sequence_header_body, PicParams,
};
use oxideav_vc2::{decode_sequence, DecodedPicture, Error, SequenceDecoder};

/// 4x4 HQ picture, depth 0 (identity IDWT), 2x2 slices, qindex 0. Each
/// slice covers a 2x2 quadrant of every component. Returns the picture
/// params and the four encoded slices in raster order, with distinct
/// luma values 1..=16 laid out so quadrant boundaries are visible.
fn hq_4x4_quadrants() -> (PicParams, Vec<Vec<u8>>, Vec<u16>) {
    let p = PicParams {
        major_version: 3,
        slices_x: 2,
        slices_y: 2,
        ..PicParams::hq_depth0()
    };
    // Luma plane values (row-major, pre-offset): 1..=16.
    // Slice (sx, sy) covers columns 2sx..2sx+2 of rows 2sy..2sy+2.
    let luma: Vec<i64> = (1..=16).collect();
    let quadrant = |sx: usize, sy: usize| -> Vec<i64> {
        let mut v = Vec::new();
        for row in 2 * sy..2 * sy + 2 {
            for col in 2 * sx..2 * sx + 2 {
                v.push(luma[row * 4 + col]);
            }
        }
        v
    };
    let c = [0i64; 4];
    let mut slices = Vec::new();
    for sy in 0..2 {
        for sx in 0..2 {
            slices.push(hq_slice_bytes(p.qindex, &quadrant(sx, sy), &c, &c));
        }
    }
    let expect: Vec<u16> = luma.iter().map(|&v| (v + 128) as u16).collect();
    (p, slices, expect)
}

fn assert_pic(pic: &DecodedPicture, picture_number: u32, y: &[u16]) {
    assert_eq!(pic.picture_number, picture_number);
    assert_eq!(pic.y, y);
    assert!(pic.c1.iter().all(|&v| v == 128));
    assert!(pic.c2.iter().all(|&v| v == 128));
}

#[test]
fn hq_fragmented_matches_unfragmented() {
    let (p, slices, expect) = hq_4x4_quadrants();
    let seq = sequence_header_body(4, 4, p.major_version);

    // Non-fragmented reference.
    let plain = build_units(&[(0x00, seq.clone()), (0xE8, picture_body(&p, 7, &slices))]);
    let plain_pics = decode_sequence(&plain).expect("plain decode");
    assert_eq!(plain_pics.len(), 1);
    assert_pic(&plain_pics[0], 7, &expect);

    // Same slices as setup + two data fragments (2 slices each).
    let fragmented = build_units(&[
        (0x00, seq),
        (0xEC, fragment_setup_body(&p, 7)),
        (0xEC, fragment_data_body(7, 0, 0, &slices[0..2])),
        (0xEC, fragment_data_body(7, 0, 1, &slices[2..4])),
    ]);
    let frag_pics = decode_sequence(&fragmented).expect("fragmented decode");
    assert_eq!(frag_pics.len(), 1);
    assert_eq!(frag_pics[0].y, plain_pics[0].y);
    assert_eq!(frag_pics[0].c1, plain_pics[0].c1);
    assert_eq!(frag_pics[0].picture_number, plain_pics[0].picture_number);
}

#[test]
fn hq_fragmented_one_slice_per_fragment() {
    // Finest allowed granularity: four data fragments of one slice each,
    // with the mid-row fragment exercising a non-zero x offset.
    let (p, slices, expect) = hq_4x4_quadrants();
    let seq = sequence_header_body(4, 4, p.major_version);
    let stream = build_units(&[
        (0x00, seq),
        (0xEC, fragment_setup_body(&p, 3)),
        (0xEC, fragment_data_body(3, 0, 0, &slices[0..1])),
        (0xEC, fragment_data_body(3, 1, 0, &slices[1..2])),
        (0xEC, fragment_data_body(3, 0, 1, &slices[2..3])),
        (0xEC, fragment_data_body(3, 1, 1, &slices[3..4])),
    ]);
    let pics = decode_sequence(&stream).expect("decode");
    assert_eq!(pics.len(), 1);
    assert_pic(&pics[0], 3, &expect);
}

#[test]
fn ld_fragmented_matches_unfragmented_with_dc_prediction() {
    // Low-delay 4x2 picture, depth 0, 2x1 slices, 12 bytes per slice.
    // LD pictures run dc_prediction over the level-0 band once ALL slices
    // are present (§14.4) — encoded values are residuals, so equality with
    // the non-fragmented decode also proves prediction ran exactly once.
    let p = PicParams {
        major_version: 3,
        slices_x: 2,
        slices_y: 1,
        low_delay: true,
        slice_bytes_numerator: 12,
        slice_bytes_denominator: 1,
        ..PicParams::hq_depth0()
    };
    let c = [0i64; 4];
    // Residuals: left slice covers columns 0..2, right slice columns 2..4.
    let left = [1i64, 1, 1, 1];
    let right = [2i64, -1, 0, 3];
    let slices = vec![
        ld_slice_bytes(p.qindex, p.ld_slice_bytes_len(0), &left, &c, &c),
        ld_slice_bytes(p.qindex, p.ld_slice_bytes_len(1), &right, &c, &c),
    ];
    let seq = sequence_header_body(4, 2, p.major_version);

    let plain = build_units(&[(0x00, seq.clone()), (0xC8, picture_body(&p, 5, &slices))]);
    let plain_pics = decode_sequence(&plain).expect("plain LD decode");
    assert_eq!(plain_pics.len(), 1);
    // Spot-check dc_prediction on the top-left corner: (0,0) has no
    // neighbours (residual 1 -> 1), (0,1) predicts from its left (1 + 1 = 2).
    assert_eq!(plain_pics[0].y[0], 128 + 1);
    assert_eq!(plain_pics[0].y[1], 128 + 2);

    let fragmented = build_units(&[
        (0x00, seq),
        (0xCC, fragment_setup_body(&p, 5)),
        (0xCC, fragment_data_body(5, 0, 0, &slices[0..1])),
        (0xCC, fragment_data_body(5, 1, 0, &slices[1..2])),
    ]);
    let frag_pics = decode_sequence(&fragmented).expect("fragmented LD decode");
    assert_eq!(frag_pics.len(), 1);
    assert_eq!(frag_pics[0].y, plain_pics[0].y);
    assert_eq!(frag_pics[0].c1, plain_pics[0].c1);
    assert_eq!(frag_pics[0].c2, plain_pics[0].c2);
}

#[test]
fn padding_and_repeated_header_interleave_with_fragments() {
    // Padding data units and a byte-identical repeated sequence header may
    // appear between the fragments of one picture (§14.1 only forbids setup
    // fragments and non-fragmented picture data units).
    let (p, slices, expect) = hq_4x4_quadrants();
    let seq = sequence_header_body(4, 4, p.major_version);
    let stream = build_units(&[
        (0x00, seq.clone()),
        (0xEC, fragment_setup_body(&p, 11)),
        (0x30, vec![0xAA; 5]), // padding
        (0xEC, fragment_data_body(11, 0, 0, &slices[0..2])),
        (0x00, seq),           // repeated identical sequence header
        (0x20, vec![0xBB; 3]), // auxiliary data
        (0xEC, fragment_data_body(11, 0, 1, &slices[2..4])),
    ]);
    let pics = decode_sequence(&stream).expect("decode");
    assert_eq!(pics.len(), 1);
    assert_pic(&pics[0], 11, &expect);
}

#[test]
fn fragments_reassemble_across_chunked_pushes() {
    // The stateful walker keeps the partially assembled picture across
    // push() calls — the shape a packetized consumer feeds.
    let (p, slices, expect) = hq_4x4_quadrants();
    let seq = sequence_header_body(4, 4, p.major_version);

    let mut dec = SequenceDecoder::new();
    // Chunk 1: sequence header + setup fragment (units only, no EOS).
    let mut chunk1 = Vec::new();
    common::parse_info(&mut chunk1, 0x00, (13 + seq.len()) as u32, 0);
    chunk1.extend_from_slice(&seq);
    let setup = fragment_setup_body(&p, 21);
    common::parse_info(
        &mut chunk1,
        0xEC,
        (13 + setup.len()) as u32,
        (13 + seq.len()) as u32,
    );
    chunk1.extend_from_slice(&setup);
    assert!(dec.push(&chunk1).unwrap().is_empty());
    assert!(dec.has_incomplete_picture());

    // Chunk 2: first data fragment.
    let mut chunk2 = Vec::new();
    let d1 = fragment_data_body(21, 0, 0, &slices[0..2]);
    common::parse_info(&mut chunk2, 0xEC, (13 + d1.len()) as u32, 0);
    chunk2.extend_from_slice(&d1);
    assert!(dec.push(&chunk2).unwrap().is_empty());
    assert!(dec.has_incomplete_picture());

    // Chunk 3: final data fragment completes the picture.
    let mut chunk3 = Vec::new();
    let d2 = fragment_data_body(21, 0, 1, &slices[2..4]);
    common::parse_info(&mut chunk3, 0xEC, (13 + d2.len()) as u32, 0);
    chunk3.extend_from_slice(&d2);
    let pics = dec.push(&chunk3).unwrap();
    assert_eq!(pics.len(), 1);
    assert!(!dec.has_incomplete_picture());
    assert_pic(&pics[0], 21, &expect);
}

// ───────────────────────── §14 constraint violations ─────────────────────

#[test]
fn eos_mid_picture_is_rejected() {
    let (p, slices, _) = hq_4x4_quadrants();
    let seq = sequence_header_body(4, 4, p.major_version);
    let stream = build_units(&[
        (0x00, seq),
        (0xEC, fragment_setup_body(&p, 7)),
        (0xEC, fragment_data_body(7, 0, 0, &slices[0..2])),
        // build_units appends EOS while two slices are still missing.
    ]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn second_setup_mid_picture_is_rejected() {
    let (p, slices, _) = hq_4x4_quadrants();
    let seq = sequence_header_body(4, 4, p.major_version);
    let stream = build_units(&[
        (0x00, seq),
        (0xEC, fragment_setup_body(&p, 7)),
        (0xEC, fragment_data_body(7, 0, 0, &slices[0..2])),
        (0xEC, fragment_setup_body(&p, 8)),
    ]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn plain_picture_mid_fragmented_picture_is_rejected() {
    let (p, slices, _) = hq_4x4_quadrants();
    let seq = sequence_header_body(4, 4, p.major_version);
    let stream = build_units(&[
        (0x00, seq),
        (0xEC, fragment_setup_body(&p, 7)),
        (0xE8, picture_body(&p, 8, &slices)),
    ]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn mismatched_picture_number_is_rejected() {
    let (p, slices, _) = hq_4x4_quadrants();
    let seq = sequence_header_body(4, 4, p.major_version);
    let stream = build_units(&[
        (0x00, seq),
        (0xEC, fragment_setup_body(&p, 7)),
        (0xEC, fragment_data_body(9, 0, 0, &slices[0..2])),
    ]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn out_of_raster_order_slices_are_rejected() {
    // Second data fragment repeats the first two slices instead of
    // continuing at (0, 1): §14.2 slices shall not be omitted or repeated.
    let (p, slices, _) = hq_4x4_quadrants();
    let seq = sequence_header_body(4, 4, p.major_version);
    let stream = build_units(&[
        (0x00, seq),
        (0xEC, fragment_setup_body(&p, 7)),
        (0xEC, fragment_data_body(7, 0, 0, &slices[0..2])),
        (0xEC, fragment_data_body(7, 0, 0, &slices[0..2])),
    ]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn slice_count_overflowing_picture_is_rejected() {
    // A data fragment claiming 6 slices on a 4-slice picture.
    let (p, slices, _) = hq_4x4_quadrants();
    let seq = sequence_header_body(4, 4, p.major_version);
    let mut six = slices.clone();
    six.extend_from_slice(&slices[0..2]);
    let stream = build_units(&[
        (0x00, seq),
        (0xEC, fragment_setup_body(&p, 7)),
        (0xEC, fragment_data_body(7, 0, 0, &six)),
    ]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn data_fragment_kind_must_match_setup() {
    // HQ setup fragment (0xEC) followed by an LD data fragment (0xCC).
    let (p, slices, _) = hq_4x4_quadrants();
    let seq = sequence_header_body(4, 4, p.major_version);
    let stream = build_units(&[
        (0x00, seq),
        (0xEC, fragment_setup_body(&p, 7)),
        (0xCC, fragment_data_body(7, 0, 0, &slices[0..2])),
    ]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn setup_fragment_requires_sequence_header() {
    let (p, _, _) = hq_4x4_quadrants();
    let stream = build_units(&[(0xEC, fragment_setup_body(&p, 7))]);
    assert!(matches!(
        decode_sequence(&stream),
        Err(Error::MissingSequenceHeader)
    ));
}
