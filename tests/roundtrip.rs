//! End-to-end decode tests driven by hand-built VC-2 streams.
//!
//! These tests assemble a minimal but spec-valid VC-2 sequence (parse-info
//! headers, a sequence header, a high-quality picture with a single slice)
//! using the shared Annex A bit writer in `common`, then decode it through
//! the public [`oxideav_vc2::decode_sequence`] entry point and assert on the
//! recovered component planes. A successful round trip exercises the whole
//! chain: stream walk → sequence header → transform parameters → HQ slice
//! unpack → inverse quant → IDWT → clip/offset.

mod common;

use common::{
    build_units, fragment_data_body, hq_slice_bytes, picture_body, sequence_header_body, PicParams,
};

/// Assemble a full sequence: PI(seq header) seq_header PI(HQ) picture PI(EOS).
fn build_stream_with(
    p: PicParams,
    width: u64,
    height: u64,
    y: &[i64],
    c1: &[i64],
    c2: &[i64],
) -> Vec<u8> {
    let seq_body = sequence_header_body(width, height, p.major_version);
    let pic_body = picture_body(&p, 7, &[hq_slice_bytes(p.qindex, y, c1, c2)]);
    build_units(&[(0x00, seq_body), (0xE8, pic_body)])
}

/// [`build_stream_with`] using the bootstrap depth-0 defaults.
fn build_stream(width: u64, height: u64, y: &[i64], c1: &[i64], c2: &[i64]) -> Vec<u8> {
    build_stream_with(PicParams::hq_depth0(), width, height, y, c1, c2)
}

#[test]
fn hq_depth0_haar_roundtrips_samples() {
    // 2x2 picture; depth-0 IDWT is identity, qindex 0 is lossless, so the
    // decoded unsigned samples are coeff + 128 (8-bit offset).
    let width = 2;
    let height = 2;
    let y = [10i64, -20, 30, -40];
    let c1 = [1i64, 2, 3, 4];
    let c2 = [-1i64, -2, -3, -4];

    let stream = build_stream(width, height, &y, &c1, &c2);
    let pics = oxideav_vc2::decode_sequence(&stream).expect("decode");
    assert_eq!(pics.len(), 1);
    let p = &pics[0];
    assert_eq!(p.picture_number, 7);
    assert_eq!(p.luma_width, 2);
    assert_eq!(p.luma_height, 2);

    // Offset is +2**(depth-1) = +128 for 8-bit (excursion 255 -> depth 8).
    let expect = |c: &[i64]| -> Vec<u16> { c.iter().map(|&v| (v + 128) as u16).collect() };
    assert_eq!(p.y, expect(&y));
    assert_eq!(p.c1, expect(&c1));
    assert_eq!(p.c2, expect(&c2));
}

#[test]
fn hq_asymmetric_mixed_pair_uses_table_d8_defaults() {
    // 4x1 picture, dwt_depth 0 with one horizontal-only level
    // (dwt_depth_ho = 1), Haar-no-shift vertically and LeGall horizontally —
    // the Table D.8 mixed pair, so the default quantization matrix lookup
    // must succeed without a custom matrix in the stream (L = 2, H1 = 0 at
    // this depth). qindex 2 makes the two bands quantize differently:
    // qval(L) = max(2 - 2, 0) = 0 (identity), qval(H) = max(2 - 0, 0) = 2.
    //
    // Expected output is computed through the crate's own public quant /
    // wavelet primitives so the test pins the *plumbing* — stream walk,
    // extended transform parameters, Annex D lookup, per-band quantizer
    // assignment, HO synthesis stage — rather than re-deriving the filter
    // math (covered by the wavelet reversibility tests).
    use oxideav_vc2::quant::inverse_quant;
    use oxideav_vc2::wavelet::{h_synthesis, wavelet_filter, Plane};

    let p = PicParams {
        major_version: 3,
        wavelet_index: 3,
        ho: Some((1, 1)),
        qindex: 2,
        ..PicParams::hq_depth0()
    };
    // Subband layout for ho=1, depth=0 on a 4x1 component: L 2x1, H 2x1.
    // Coefficients per component in stream order: L then H.
    let y = [12i64, -6, 2, -3];
    let c = [0i64, 0, 0, 0];
    let stream = build_stream_with(p, 4, 1, &y, &c, &c);
    let pics = oxideav_vc2::decode_sequence(&stream).expect("decode");
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].luma_width, 4);
    assert_eq!(pics[0].luma_height, 1);

    // Reference path: dequantize each band with its Table D.8 quantizer,
    // run one horizontal LeGall synthesis stage, offset by +128 (8-bit).
    let legall = wavelet_filter(1).unwrap();
    let l_band = Plane {
        width: 2,
        height: 1,
        data: vec![inverse_quant(y[0], 0), inverse_quant(y[1], 0)],
    };
    let h_band = Plane {
        width: 2,
        height: 1,
        data: vec![inverse_quant(y[2], 2), inverse_quant(y[3], 2)],
    };
    let synth = h_synthesis(&l_band, &h_band, &legall);
    let expect: Vec<u16> = (0..4)
        .map(|x| (synth.get(0, x).clamp(-128, 127) + 128) as u16)
        .collect();
    assert_eq!(pics[0].y, expect);
    // With qval(H) = 2 the H coefficients are scaled up by dequantization,
    // so the decode must NOT equal the identity-quantizer reconstruction.
    let h_identity = Plane {
        width: 2,
        height: 1,
        data: vec![y[2], y[3]],
    };
    let synth_id = h_synthesis(&l_band, &h_identity, &legall);
    let expect_id: Vec<u16> = (0..4)
        .map(|x| (synth_id.get(0, x).clamp(-128, 127) + 128) as u16)
        .collect();
    assert_ne!(pics[0].y, expect_id, "H-band quantizer must differ from L");
    assert!(pics[0].c1.iter().all(|&v| v == 128));
}

#[test]
fn asymmetric_depths_without_default_require_custom_matrix() {
    // ho = (wavelet_index_ho 1, dwt_depth_ho 2) with dwt_depth 4: total
    // depth 6 exceeds the Annex D limit of 5, and no custom matrix is
    // present -> the decoder must reject the stream, not invent values.
    let p = PicParams {
        major_version: 3,
        wavelet_index: 1,
        dwt_depth: 4,
        ho: Some((1, 2)),
        ..PicParams::hq_depth0()
    };
    let coeffs = [0i64; 64];
    let stream = build_stream_with(p, 64, 16, &coeffs, &coeffs, &coeffs);
    assert!(matches!(
        oxideav_vc2::decode_sequence(&stream),
        Err(oxideav_vc2::Error::MissingQuantMatrix)
    ));
}

#[test]
fn writer_uint_matches_reader() {
    // Sanity-check the test's exp-Golomb writer against the crate's reader by
    // decoding a degenerate single-value high-quality stream where the only
    // coefficient is a large value, forcing multi-bit exp-Golomb codes.
    let y = [255i64; 4];
    let c = [0i64; 4];
    let stream = build_stream(2, 2, &y, &c, &c);
    let pics = oxideav_vc2::decode_sequence(&stream).unwrap();
    // 255 + 128 = 383 clipped to 8-bit signed range [-128,127] -> 127, then
    // +128 = 255.
    assert!(pics[0].y.iter().all(|&v| v == 255));
}

#[test]
fn trailing_sequence_after_eos_also_decodes() {
    // A VC-2 stream is a concatenation of sequences (§10.3): two complete
    // single-picture sequences back to back decode to two pictures.
    let y1 = [1i64, 2, 3, 4];
    let y2 = [5i64, 6, 7, 8];
    let c = [0i64; 4];
    let mut stream = build_stream(2, 2, &y1, &c, &c);
    stream.extend_from_slice(&build_stream(2, 2, &y2, &c, &c));
    let pics = oxideav_vc2::decode_sequence(&stream).unwrap();
    assert_eq!(pics.len(), 2);
    assert_eq!(pics[0].y, vec![129, 130, 131, 132]);
    assert_eq!(pics[1].y, vec![133, 134, 135, 136]);
}

#[test]
fn picture_after_eos_without_new_header_is_rejected() {
    // The end-of-sequence data unit resets all per-sequence state
    // (§10.4.1 reset_state), so a picture in the next sequence must not
    // reuse the previous sequence header.
    let y = [1i64, 2, 3, 4];
    let c = [0i64; 4];
    let mut stream = build_stream(2, 2, &y, &c, &c);
    let p = PicParams::hq_depth0();
    let orphan = picture_body(&p, 9, &[hq_slice_bytes(0, &y, &c, &c)]);
    stream.extend_from_slice(&build_units(&[(0xE8, orphan)]));
    assert!(matches!(
        oxideav_vc2::decode_sequence(&stream),
        Err(oxideav_vc2::Error::MissingSequenceHeader)
    ));
}

#[test]
fn fragment_data_unit_without_setup_is_rejected() {
    // A data fragment with no preceding setup fragment violates §14.1.
    let seq_body = sequence_header_body(2, 2, 3);
    let frag = fragment_data_body(7, 0, 0, &[hq_slice_bytes(0, &[0; 4], &[0; 4], &[0; 4])]);
    let stream = build_units(&[(0x00, seq_body), (0xEC, frag)]);
    assert!(matches!(
        oxideav_vc2::decode_sequence(&stream),
        Err(oxideav_vc2::Error::InvalidValue(_))
    ));
}
