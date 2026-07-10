//! Shared hand-assembly helpers for the integration tests: a minimal
//! MSB-first bit writer implementing the Annex A encodings (the inverse of
//! the reader the decoder uses), plus builders for sequence headers,
//! picture / fragment data-unit bodies and whole parse-info-framed streams.

#![allow(dead_code)] // each test binary uses a different subset

/// A minimal MSB-first bit writer with the VC-2 variable-length encoders.
#[derive(Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    nbits: u8,
}

impl BitWriter {
    pub fn put_bit(&mut self, bit: u32) {
        self.cur = (self.cur << 1) | (bit as u8 & 1);
        self.nbits += 1;
        if self.nbits == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    pub fn put_bool(&mut self, b: bool) {
        self.put_bit(b as u32);
    }

    pub fn put_nbits(&mut self, val: u64, n: u32) {
        for i in (0..n).rev() {
            self.put_bit(((val >> i) & 1) as u32);
        }
    }

    /// `byte_align`: pad the current byte with zero bits.
    pub fn byte_align(&mut self) {
        while self.nbits != 0 {
            self.put_bit(0);
        }
    }

    /// Number of bits written so far.
    pub fn bit_len(&self) -> u64 {
        self.bytes.len() as u64 * 8 + self.nbits as u64
    }

    /// Bit-exact append of another writer's contents (no padding).
    pub fn append(&mut self, other: &BitWriter) {
        for &b in &other.bytes {
            self.put_nbits(b as u64, 8);
        }
        for i in (0..other.nbits).rev() {
            self.put_bit(((other.cur >> i) & 1) as u32);
        }
    }

    /// Unsigned interleaved exp-Golomb (inverse of A.4.3 `read_uint`).
    pub fn put_uint(&mut self, value: u64) {
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
    pub fn put_sint(&mut self, value: i64) {
        self.put_uint(value.unsigned_abs());
        if value != 0 {
            self.put_bit((value < 0) as u32);
        }
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        self.byte_align();
        self.bytes
    }
}

/// Write a 13-byte parse-info header (§10.5.1).
pub fn parse_info(out: &mut Vec<u8>, parse_code: u8, next_offset: u32, prev_offset: u32) {
    out.extend_from_slice(&[0x42, 0x42, 0x43, 0x44]); // "BBCD"
    out.push(parse_code);
    out.extend_from_slice(&next_offset.to_be_bytes());
    out.extend_from_slice(&prev_offset.to_be_bytes());
}

/// Frame a list of `(parse_code, body)` data units into a stream with
/// chained next/previous parse offsets, appending the end-of-sequence
/// parse-info header.
pub fn build_units(units: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev_offset = 0u32;
    for (code, body) in units {
        let next_offset = (13 + body.len()) as u32;
        parse_info(&mut out, *code, next_offset, prev_offset);
        out.extend_from_slice(body);
        prev_offset = next_offset;
    }
    parse_info(&mut out, 0x10, 0, prev_offset);
    out
}

/// Build a sequence-header data-unit body (§11.1) for the custom base format
/// with explicit small frame size, 4:4:4, progressive, 8-bit full-range
/// signal. Major version 2 suffices for HQ pictures; version 3 unlocks the
/// §12.4.4 extended (asymmetric) transform parameters and fragments.
/// Returns the body bytes.
pub fn sequence_header_body(width: u64, height: u64, major_version: u64) -> Vec<u8> {
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

/// Transform / slice parameter knobs for the picture and fragment builders.
#[derive(Clone, Copy)]
pub struct PicParams {
    /// Sequence major version; version 3 emits extended transform params.
    pub major_version: u64,
    pub wavelet_index: u64,
    pub dwt_depth: u64,
    /// `Some((wavelet_index_ho, dwt_depth_ho))` writes the §12.4.4 extended
    /// parameters (requires `major_version >= 3`).
    pub ho: Option<(u64, u64)>,
    pub qindex: u64,
    pub slices_x: u64,
    pub slices_y: u64,
    /// Low-delay picture (fixed-size slices) instead of high-quality.
    pub low_delay: bool,
    /// LD only: slice bytes numerator / denominator (§12.4.5.2).
    pub slice_bytes_numerator: u64,
    pub slice_bytes_denominator: u64,
}

impl PicParams {
    /// The bootstrap-round defaults: single-slice HQ depth-0 Haar-no-shift,
    /// qindex 0 (identity quantizer, identity IDWT).
    pub fn hq_depth0() -> Self {
        PicParams {
            major_version: 2,
            wavelet_index: 3,
            dwt_depth: 0,
            ho: None,
            qindex: 0,
            slices_x: 1,
            slices_y: 1,
            low_delay: false,
            slice_bytes_numerator: 0,
            slice_bytes_denominator: 1,
        }
    }

    /// `slice_bytes()` (§13.5.3.2) for LD slice `slice_index`.
    pub fn ld_slice_bytes_len(&self, slice_index: u64) -> u64 {
        let a = ((slice_index + 1) * self.slice_bytes_numerator) / self.slice_bytes_denominator;
        let b = (slice_index * self.slice_bytes_numerator) / self.slice_bytes_denominator;
        a - b
    }
}

/// Write `transform_parameters()` (§12.4) with the Annex D default
/// quantization matrix (custom flag false).
fn write_transform_parameters(w: &mut BitWriter, p: &PicParams) {
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
    // slice_parameters (§12.4.5.2).
    w.put_uint(p.slices_x);
    w.put_uint(p.slices_y);
    if p.low_delay {
        w.put_uint(p.slice_bytes_numerator);
        w.put_uint(p.slice_bytes_denominator);
    } else {
        // prefix_bytes = 0, size_scaler = 1.
        w.put_uint(0);
        w.put_uint(1);
    }
    // quant_matrix: custom false -> Annex D default lookup.
    w.put_bool(false);
}

/// One high-quality slice (§13.5.4), byte-aligned by construction:
/// qindex byte, then per component a 1-byte length code and the
/// coefficients (in subband stream order for the slice's area).
pub fn hq_slice_bytes(qindex: u64, y: &[i64], c1: &[i64], c2: &[i64]) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put_nbits(qindex, 8);
    for comp in [y, c1, c2] {
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

/// One low-delay slice (§13.5.3), exactly `slice_len` bytes: 7-bit qindex,
/// the luma-length field, the luma coefficients, the interleaved C1/C2
/// coefficients, zero padding up to the fixed slice size.
pub fn ld_slice_bytes(qindex: u64, slice_len: u64, y: &[i64], c1: &[i64], c2: &[i64]) -> Vec<u8> {
    assert_eq!(c1.len(), c2.len());
    let total_bits = 8 * slice_len;
    let length_bits = oxideav_vc2::params::intlog2(total_bits - 7);

    let mut yw = BitWriter::default();
    for &v in y {
        yw.put_sint(v);
    }
    let mut cw = BitWriter::default();
    for (&a, &b) in c1.iter().zip(c2) {
        cw.put_sint(a);
        cw.put_sint(b);
    }

    let mut w = BitWriter::default();
    w.put_nbits(qindex, 7);
    w.put_nbits(yw.bit_len(), length_bits);
    w.append(&yw);
    w.append(&cw);
    assert!(
        w.bit_len() <= total_bits,
        "coefficients overflow the fixed LD slice size"
    );
    while w.bit_len() < total_bits {
        w.put_bit(0);
    }
    w.into_bytes()
}

/// Picture data-unit body (§12.1): picture header, transform parameters,
/// then the pre-encoded slices concatenated in raster order.
pub fn picture_body(p: &PicParams, picture_number: u32, slices: &[Vec<u8>]) -> Vec<u8> {
    let mut w = BitWriter::default();
    // picture_header: picture_number (4-byte literal).
    w.put_nbits(picture_number as u64, 32);
    w.byte_align();
    write_transform_parameters(&mut w, p);
    w.byte_align();
    let mut out = w.into_bytes();
    for s in slices {
        out.extend_from_slice(s);
    }
    out
}

/// Setup-fragment data-unit body (§14.2 header with a slice count of zero,
/// then transform parameters).
pub fn fragment_setup_body(p: &PicParams, picture_number: u32) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put_nbits(picture_number as u64, 32);
    w.put_nbits(0, 16); // fragment_data_length (unused by decoding)
    w.put_nbits(0, 16); // fragment_slice_count = 0 -> setup
    write_transform_parameters(&mut w, p);
    w.into_bytes()
}

/// Data-fragment data-unit body (§14.2 header with the slice coordinate of
/// the first carried slice, then the pre-encoded slices).
pub fn fragment_data_body(
    picture_number: u32,
    x_offset: u16,
    y_offset: u16,
    slices: &[Vec<u8>],
) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put_nbits(picture_number as u64, 32);
    w.put_nbits(0, 16); // fragment_data_length (unused by decoding)
    w.put_nbits(slices.len() as u64, 16);
    w.put_nbits(x_offset as u64, 16);
    w.put_nbits(y_offset as u64, 16);
    let mut out = w.into_bytes();
    for s in slices {
        out.extend_from_slice(s);
    }
    out
}
