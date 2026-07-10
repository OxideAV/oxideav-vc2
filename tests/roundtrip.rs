//! End-to-end decode tests driven by hand-built VC-2 streams.
//!
//! These tests assemble a minimal but spec-valid VC-2 sequence (parse-info
//! headers, a sequence header, a high-quality picture with a single slice)
//! using a small bit/exp-Golomb writer, then decode it through the public
//! [`oxideav_vc2::decode_sequence`] entry point and assert on the recovered
//! component planes. The writer mirrors Annex A's encodings (the inverse of
//! the reader the decoder uses), so a successful round trip exercises the
//! whole chain: stream walk → sequence header → transform parameters →
//! HQ slice unpack → inverse quant → IDWT → clip/offset.

/// A minimal MSB-first bit writer with the VC-2 variable-length encoders.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    nbits: u8,
}

impl BitWriter {
    fn put_bit(&mut self, bit: u32) {
        self.cur = (self.cur << 1) | (bit as u8 & 1);
        self.nbits += 1;
        if self.nbits == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    fn put_bool(&mut self, b: bool) {
        self.put_bit(b as u32);
    }

    fn put_nbits(&mut self, val: u64, n: u32) {
        for i in (0..n).rev() {
            self.put_bit(((val >> i) & 1) as u32);
        }
    }

    /// `byte_align`: pad the current byte with zero bits.
    fn byte_align(&mut self) {
        while self.nbits != 0 {
            self.put_bit(0);
        }
    }

    /// Unsigned interleaved exp-Golomb (inverse of A.4.3 `read_uint`).
    fn put_uint(&mut self, value: u64) {
        let n = value + 1;
        // Number of bits in n.
        let bits = 64 - n.leading_zeros();
        // Emit (bits-1) [follow=0, data] pairs from MSB-1 down, then follow=1.
        // Per A.4.3: value (n) = 0b1 x_{K-1} ... x_0; for each data bit emit
        // a leading 0 then the data bit, and terminate with a 1.
        for i in (0..bits - 1).rev() {
            self.put_bit(0);
            self.put_bit(((n >> i) & 1) as u32);
        }
        self.put_bit(1);
    }

    /// Signed interleaved exp-Golomb (inverse of A.4.4 `read_sint`).
    fn put_sint(&mut self, value: i64) {
        self.put_uint(value.unsigned_abs());
        if value != 0 {
            self.put_bit((value < 0) as u32);
        }
    }

    fn into_bytes(mut self) -> Vec<u8> {
        self.byte_align();
        self.bytes
    }
}

/// Write a 13-byte parse-info header (§10.5.1).
fn parse_info(out: &mut Vec<u8>, parse_code: u8, next_offset: u32, prev_offset: u32) {
    out.extend_from_slice(&[0x42, 0x42, 0x43, 0x44]); // "BBCD"
    out.push(parse_code);
    out.extend_from_slice(&next_offset.to_be_bytes());
    out.extend_from_slice(&prev_offset.to_be_bytes());
}

/// Build a sequence-header data-unit body (§11.1) for the custom base format
/// with explicit small frame size, 4:4:4, progressive, 8-bit full-range
/// signal. Major version 2 suffices for HQ pictures; version 3 unlocks the
/// §12.4.4 extended (asymmetric) transform parameters. Returns the body
/// bytes.
fn sequence_header_body(width: u64, height: u64, major_version: u64) -> Vec<u8> {
    let mut w = BitWriter::default();
    // parse_parameters: major, minor=0, profile=0, level=0.
    w.put_uint(major_version);
    w.put_uint(0);
    w.put_uint(0);
    w.put_uint(0);
    // base_video_format = 0 (custom).
    w.put_uint(0);
    // source_parameters:
    // frame_size: custom flag true, width, height.
    w.put_bool(true);
    w.put_uint(width);
    w.put_uint(height);
    // color_diff_sampling_format: custom true, index 0 (4:4:4).
    w.put_bool(true);
    w.put_uint(0);
    // scan_format: custom false (progressive default).
    w.put_bool(false);
    // frame_rate: custom false.
    w.put_bool(false);
    // pixel_aspect_ratio: custom false.
    w.put_bool(false);
    // clean_area: custom false.
    w.put_bool(false);
    // signal_range: custom true, preset index 1 (8-bit full range).
    w.put_bool(true);
    w.put_uint(1);
    // color_spec: custom false.
    w.put_bool(false);
    // picture_coding_mode = 0 (frames).
    w.put_uint(0);
    w.into_bytes()
}

/// Transform-parameter knobs for the HQ picture builders.
#[derive(Clone, Copy)]
struct HqParams {
    /// Sequence major version; version 3 emits extended transform params.
    major_version: u64,
    wavelet_index: u64,
    dwt_depth: u64,
    /// `Some((wavelet_index_ho, dwt_depth_ho))` writes the §12.4.4 extended
    /// parameters (requires `major_version >= 3`).
    ho: Option<(u64, u64)>,
    qindex: u64,
}

impl HqParams {
    /// The bootstrap-round defaults: depth-0 Haar-no-shift, qindex 0.
    fn depth0() -> Self {
        HqParams {
            major_version: 2,
            wavelet_index: 3,
            dwt_depth: 0,
            ho: None,
            qindex: 0,
        }
    }
}

/// Build a high-quality picture data-unit body with one slice covering the
/// whole picture, carrying the explicit per-component coefficients (in
/// subband stream order) with the Annex D default quantization matrix.
///
/// With the [`HqParams::depth0`] defaults the single LL subband *is* the
/// full picture, so the IDWT is the identity and `inverse_quant(coeff, 0)`
/// recovers `coeff` exactly. Returns the body bytes.
fn hq_picture_body(p: HqParams, picture_number: u32, y: &[i64], c1: &[i64], c2: &[i64]) -> Vec<u8> {
    let mut w = BitWriter::default();
    // picture_header: picture_number (4-byte literal).
    w.put_nbits(picture_number as u64, 32);
    w.byte_align();
    // transform_parameters:
    w.put_uint(p.wavelet_index);
    w.put_uint(p.dwt_depth);
    if p.major_version >= 3 {
        // extended_transform_parameters() (§12.4.4).
        match p.ho {
            Some((wavelet_index_ho, dwt_depth_ho)) => {
                w.put_bool(true);
                w.put_uint(wavelet_index_ho);
                w.put_bool(true);
                w.put_uint(dwt_depth_ho);
            }
            None => {
                w.put_bool(false);
                w.put_bool(false);
            }
        }
    } else {
        assert!(p.ho.is_none(), "asymmetric params need major_version >= 3");
    }
    // slice_parameters (HQ): slices_x=1, slices_y=1, prefix_bytes=0, scaler=1.
    w.put_uint(1);
    w.put_uint(1);
    w.put_uint(0);
    w.put_uint(1);
    // quant_matrix: custom false -> Annex D default lookup.
    w.put_bool(false);
    w.byte_align();

    // transform_data: one HQ slice.
    // hq_slice: prefix bytes (0), qindex (1-byte literal), then per
    // component: length code (1 byte) * scaler then the coefficients.
    w.put_nbits(p.qindex, 8);

    for comp in [y, c1, c2] {
        // Encode the component coefficients into a separate writer to measure
        // the byte length, then emit length + payload.
        let mut cw = BitWriter::default();
        for &v in comp {
            cw.put_sint(v);
        }
        let payload = cw.into_bytes();
        assert!(payload.len() <= 255, "test payload fits a 1-byte length");
        w.put_nbits(payload.len() as u64, 8);
        for b in payload {
            w.put_nbits(b as u64, 8);
        }
    }
    w.into_bytes()
}

/// Assemble a full sequence: PI(seq header) seq_header PI(HQ) picture PI(EOS).
fn build_stream_with(
    p: HqParams,
    width: u64,
    height: u64,
    y: &[i64],
    c1: &[i64],
    c2: &[i64],
) -> Vec<u8> {
    let seq_body = sequence_header_body(width, height, p.major_version);
    let pic_body = hq_picture_body(p, 7, y, c1, c2);

    let mut out = Vec::new();
    // PI #1 -> sequence header. next_offset = 13 + len(seq_body).
    let pi1_off = (13 + seq_body.len()) as u32;
    parse_info(&mut out, 0x00, pi1_off, 0);
    out.extend_from_slice(&seq_body);
    // PI #2 -> HQ picture. next_offset = 13 + len(pic_body).
    let pi2_off = (13 + pic_body.len()) as u32;
    parse_info(&mut out, 0xE8, pi2_off, pi1_off);
    out.extend_from_slice(&pic_body);
    // PI #3 -> end of sequence.
    parse_info(&mut out, 0x10, 0, pi2_off);
    out
}

/// [`build_stream_with`] using the bootstrap depth-0 defaults.
fn build_stream(width: u64, height: u64, y: &[i64], c1: &[i64], c2: &[i64]) -> Vec<u8> {
    build_stream_with(HqParams::depth0(), width, height, y, c1, c2)
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

    let p = HqParams {
        major_version: 3,
        wavelet_index: 3,
        dwt_depth: 0,
        ho: Some((1, 1)),
        qindex: 2,
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
    let p = HqParams {
        major_version: 3,
        wavelet_index: 1,
        dwt_depth: 4,
        ho: Some((1, 2)),
        qindex: 0,
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
