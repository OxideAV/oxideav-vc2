//! Transform parameters (§12.4), subband structure (§13.2), slice
//! unpacking (§13.5) and the picture IDWT driver (§15) of
//! SMPTE ST 2042-1:2022.

use crate::bitio::BitReader;
use crate::params::SequenceHeader;
use crate::quant::{self, inverse_quant, MatrixLevel};
use crate::wavelet::{self, Plane, WaveletFilter};
use crate::{Error, Result};

/// Picture kind, derived from the parse code (§10.5.2 Table 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureKind {
    /// Low delay picture (0xC8) — fixed-size slices.
    LowDelay,
    /// High quality picture (0xE8) — per-component length codes.
    HighQuality,
}

impl PictureKind {
    /// `using_dc_prediction()` (Table 5): `(parse_code & 0x28) == 0x08`.
    /// LD uses DC prediction; HQ does not.
    pub fn uses_dc_prediction(self) -> bool {
        matches!(self, PictureKind::LowDelay)
    }
}

/// Per-subband orientation identity used to index the coefficient store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orient {
    /// Symmetric DC band (level 0, dwt_depth_ho == 0).
    LL,
    /// Asymmetric horizontal-only DC band (level 0, dwt_depth_ho > 0).
    L,
    /// Horizontal-only AC band.
    H,
    HL,
    LH,
    HH,
}

/// Wavelet transform parameters (§12.4).
#[derive(Debug, Clone)]
pub struct TransformParameters {
    pub wavelet_index: u64,
    pub dwt_depth: u64,
    pub wavelet_index_ho: u64,
    pub dwt_depth_ho: u64,
    pub slices_x: u64,
    pub slices_y: u64,
    // Low delay
    pub slice_bytes_numerator: u64,
    pub slice_bytes_denominator: u64,
    // High quality
    pub slice_prefix_bytes: u64,
    pub slice_size_scaler: u64,
    /// Per-level quantization matrix; index 0 is DC, 1.. are AC levels.
    pub quant_matrix: Vec<MatrixLevel>,
}

impl TransformParameters {
    /// Total number of transform levels (§13.2.1).
    pub fn total_levels(&self) -> u64 {
        self.dwt_depth_ho + self.dwt_depth
    }
}

/// Implementation cap on `dwt_depth_ho + dwt_depth`. The spec leaves the
/// depths unbounded (a custom quantization matrix covers any depth), but
/// each extra level doubles the pad granularity — beyond this even an 8K
/// frame is pure padding, so deeper values only ever appear in hostile
/// streams trying to force absurd shifts and allocations.
pub const MAX_TOTAL_TRANSFORM_DEPTH: u64 = 16;

/// Implementation cap on the padded luma area (samples). Comfortably twice
/// the largest Annex B format (8K at 7680x4320 = 33.2 M samples) while
/// keeping hostile dimension/depth pairs from provoking multi-gigabyte
/// coefficient allocations.
pub const MAX_PADDED_AREA: u64 = 1 << 26;

/// Implementation cap on `slices_x * slices_y` — real streams use
/// hundreds-to-thousands of slices; this bounds hostile per-picture work.
pub const MAX_SLICES: u64 = 1 << 22;

/// Implementation caps on the byte-count parameters of §12.4.5.2, sized so
/// downstream products (`slice_number * numerator`, `8 * scaler * length`)
/// stay far from overflow.
pub const MAX_SLICE_BYTES_NUMERATOR: u64 = 1 << 32;
/// See [`MAX_SLICE_BYTES_NUMERATOR`].
pub const MAX_SLICE_PREFIX_BYTES: u64 = 1 << 16;
/// See [`MAX_SLICE_BYTES_NUMERATOR`].
pub const MAX_SLICE_SIZE_SCALER: u64 = 1 << 16;

/// `transform_parameters()` (§12.4.1).
pub fn transform_parameters(
    r: &mut BitReader,
    seq: &SequenceHeader,
    kind: PictureKind,
) -> Result<TransformParameters> {
    let wavelet_index = r.read_uint();
    let dwt_depth = r.read_uint();
    let mut wavelet_index_ho = wavelet_index;
    let mut dwt_depth_ho = 0;
    if seq.parse_parameters.major_version >= 3 {
        // extended_transform_parameters() (§12.4.4).
        if r.read_bool() {
            wavelet_index_ho = r.read_uint();
        }
        if r.read_bool() {
            dwt_depth_ho = r.read_uint();
        }
    }
    if wavelet_index > 6 {
        return Err(Error::UnsupportedWaveletIndex(wavelet_index));
    }
    if wavelet_index_ho > 6 {
        return Err(Error::UnsupportedWaveletIndex(wavelet_index_ho));
    }
    if dwt_depth.saturating_add(dwt_depth_ho) > MAX_TOTAL_TRANSFORM_DEPTH {
        return Err(Error::InvalidValue(
            "total transform depth exceeds the implementation cap",
        ));
    }
    // Padded-dimension guard: the coefficient store rounds each component
    // up to a multiple of 2^depth per axis (§13.2.3); bound the resulting
    // allocation before any plane is created. Chroma is never larger than
    // luma, so checking luma suffices.
    {
        let cp = &seq.coding_parameters;
        let scale_w = 1u64 << (dwt_depth_ho + dwt_depth);
        let scale_h = 1u64 << dwt_depth;
        let pw = cp
            .luma_width
            .div_ceil(scale_w)
            .checked_mul(scale_w)
            .ok_or(Error::InvalidValue("padded luma width overflows"))?;
        let ph = cp
            .luma_height
            .div_ceil(scale_h)
            .checked_mul(scale_h)
            .ok_or(Error::InvalidValue("padded luma height overflows"))?;
        match pw.checked_mul(ph) {
            Some(area) if area <= MAX_PADDED_AREA => {}
            _ => {
                return Err(Error::InvalidValue(
                    "padded picture area exceeds the implementation cap",
                ))
            }
        }
    }

    // slice_parameters() (§12.4.5.2).
    let slices_x = r.read_uint();
    let slices_y = r.read_uint();
    if slices_x == 0 || slices_y == 0 {
        return Err(Error::InvalidValue("slices_x / slices_y must be >= 1"));
    }
    match slices_x.checked_mul(slices_y) {
        Some(n) if n <= MAX_SLICES => {}
        _ => {
            return Err(Error::InvalidValue(
                "slice count exceeds the implementation cap",
            ))
        }
    }
    let mut slice_bytes_numerator = 0;
    let mut slice_bytes_denominator = 1;
    let mut slice_prefix_bytes = 0;
    let mut slice_size_scaler = 1;
    match kind {
        PictureKind::LowDelay => {
            slice_bytes_numerator = r.read_uint();
            slice_bytes_denominator = r.read_uint();
            if slice_bytes_denominator == 0 {
                return Err(Error::InvalidValue("slice_bytes_denominator must be >= 1"));
            }
            if slice_bytes_numerator > MAX_SLICE_BYTES_NUMERATOR {
                return Err(Error::InvalidValue(
                    "slice_bytes_numerator exceeds the implementation cap",
                ));
            }
        }
        PictureKind::HighQuality => {
            slice_prefix_bytes = r.read_uint();
            slice_size_scaler = r.read_uint();
            if slice_size_scaler == 0 {
                return Err(Error::InvalidValue("slice_size_scaler must be >= 1"));
            }
            if slice_prefix_bytes > MAX_SLICE_PREFIX_BYTES {
                return Err(Error::InvalidValue(
                    "slice_prefix_bytes exceeds the implementation cap",
                ));
            }
            if slice_size_scaler > MAX_SLICE_SIZE_SCALER {
                return Err(Error::InvalidValue(
                    "slice_size_scaler exceeds the implementation cap",
                ));
            }
        }
    }

    // quant_matrix() (§12.4.5.3).
    let quant_matrix =
        read_quant_matrix(r, wavelet_index, wavelet_index_ho, dwt_depth, dwt_depth_ho)?;

    if r.overrun() {
        return Err(Error::UnexpectedEof);
    }

    Ok(TransformParameters {
        wavelet_index,
        dwt_depth,
        wavelet_index_ho,
        dwt_depth_ho,
        slices_x,
        slices_y,
        slice_bytes_numerator,
        slice_bytes_denominator,
        slice_prefix_bytes,
        slice_size_scaler,
        quant_matrix,
    })
}

/// `quant_matrix()` (§12.4.5.3) — read a custom matrix or apply the Annex D
/// default via `set_quant_matrix()`. Defaults exist for the seven symmetric
/// filter pairs and the mixed Haar-no-shift / LeGall pair (Tables D.1–D.8),
/// for `dwt_depth <= 4`, `dwt_depth_ho <= 4` and a total depth of at most 5;
/// every other combination normatively requires a custom matrix, so its
/// absence is a stream error.
fn read_quant_matrix(
    r: &mut BitReader,
    wavelet_index: u64,
    wavelet_index_ho: u64,
    dwt_depth: u64,
    dwt_depth_ho: u64,
) -> Result<Vec<MatrixLevel>> {
    let custom = r.read_bool();
    if custom {
        let total = dwt_depth_ho + dwt_depth;
        let mut matrix = Vec::with_capacity(total as usize + 1);
        // level 0: LL (symmetric) or L (asymmetric).
        matrix.push(MatrixLevel::Ll(r.read_uint() as i64));
        // levels 1..=dwt_depth_ho: the horizontal-only H bands.
        for _ in 1..=dwt_depth_ho {
            matrix.push(MatrixLevel::H(r.read_uint() as i64));
        }
        for _ in (dwt_depth_ho + 1)..=(dwt_depth_ho + dwt_depth) {
            let hl = r.read_uint() as i64;
            let lh = r.read_uint() as i64;
            let hh = r.read_uint() as i64;
            matrix.push(MatrixLevel::Ac { hl, lh, hh });
        }
        Ok(matrix)
    } else {
        // set_quant_matrix() — Annex D lookup.
        quant::default_quant_matrix_full(wavelet_index, wavelet_index_ho, dwt_depth, dwt_depth_ho)
            .ok_or(Error::MissingQuantMatrix)
    }
}

/// `subband_width(level, comp)` (§13.2.3). `w` is the component luma/chroma
/// width.
fn subband_width(w: u64, dwt_depth_ho: u64, dwt_depth: u64, level: u64) -> u64 {
    let scale_w = 1u64 << (dwt_depth_ho + dwt_depth);
    let pw = scale_w * w.div_ceil(scale_w);
    if level == 0 {
        pw >> (dwt_depth_ho + dwt_depth)
    } else {
        pw >> (dwt_depth_ho + dwt_depth - level + 1)
    }
}

/// `subband_height(level, comp)` (§13.2.3).
fn subband_height(h: u64, dwt_depth_ho: u64, dwt_depth: u64, level: u64) -> u64 {
    let scale_h = 1u64 << dwt_depth;
    let ph = scale_h * h.div_ceil(scale_h);
    if level <= dwt_depth_ho {
        ph >> dwt_depth
    } else {
        ph >> (dwt_depth_ho + dwt_depth - level + 1)
    }
}

/// `slice_left` / `slice_right` / `slice_top` / `slice_bottom` (§13.5.6.2).
fn slice_bounds(
    sub_w: u64,
    sub_h: u64,
    slices_x: u64,
    slices_y: u64,
    sx: u64,
    sy: u64,
) -> (u64, u64, u64, u64) {
    let left = (sub_w * sx) / slices_x;
    let right = (sub_w * (sx + 1)) / slices_x;
    let top = (sub_h * sy) / slices_y;
    let bottom = (sub_h * (sy + 1)) / slices_y;
    (left, right, top, bottom)
}

/// Coefficient store for one video component: a flat plane per (level,
/// orient). Indexed by the linear orientation list produced by
/// [`subband_layout`].
#[derive(Debug, Clone)]
pub struct ComponentCoeffs {
    pub width: u64,
    pub height: u64,
    /// `bands[i]` is the coefficient plane for layout entry `i`.
    pub bands: Vec<Plane>,
}

/// Linear list of (level, orient) entries in the order they appear in the
/// VC-2 stream within a slice (§13.2.1 / §13.5.3).
pub fn subband_layout(dwt_depth_ho: u64, dwt_depth: u64) -> Vec<(u64, Orient)> {
    let mut out = Vec::new();
    if dwt_depth_ho == 0 {
        out.push((0, Orient::LL));
        for level in 1..=dwt_depth {
            out.push((level, Orient::HL));
            out.push((level, Orient::LH));
            out.push((level, Orient::HH));
        }
    } else {
        out.push((0, Orient::L));
        for level in 1..=dwt_depth_ho {
            out.push((level, Orient::H));
        }
        for level in (dwt_depth_ho + 1)..=(dwt_depth_ho + dwt_depth) {
            out.push((level, Orient::HL));
            out.push((level, Orient::LH));
            out.push((level, Orient::HH));
        }
    }
    out
}

/// `initialize_wavelet_data()` (§13.2.2) for one component.
fn init_component(w: u64, h: u64, tp: &TransformParameters) -> ComponentCoeffs {
    let layout = subband_layout(tp.dwt_depth_ho, tp.dwt_depth);
    let mut bands = Vec::with_capacity(layout.len());
    for &(level, _orient) in &layout {
        let sw = subband_width(w, tp.dwt_depth_ho, tp.dwt_depth, level);
        let sh = subband_height(h, tp.dwt_depth_ho, tp.dwt_depth, level);
        bands.push(Plane::new(sw as usize, sh as usize));
    }
    ComponentCoeffs {
        width: w,
        height: h,
        bands,
    }
}

/// Quantizer values per layout entry (§13.5.5 `slice_quantizers`).
fn slice_quantizers(qindex: u64, tp: &TransformParameters) -> Vec<u64> {
    let layout = subband_layout(tp.dwt_depth_ho, tp.dwt_depth);
    let mut out = Vec::with_capacity(layout.len());
    for &(level, orient) in &layout {
        // The matrix value for this (level, orient).
        let matrix_val = match tp.quant_matrix.get(level as usize) {
            Some(MatrixLevel::Ll(v)) | Some(MatrixLevel::H(v)) => *v,
            Some(MatrixLevel::Ac { hl, lh, hh }) => match orient {
                Orient::HL => *hl,
                Orient::LH => *lh,
                Orient::HH => *hh,
                _ => *hl,
            },
            None => 0,
        };
        let qval = (qindex as i64 - matrix_val).max(0) as u64;
        out.push(qval);
    }
    out
}

/// Unpacked transform data for the three components.
#[derive(Debug, Clone)]
pub struct TransformData {
    pub y: ComponentCoeffs,
    pub c1: ComponentCoeffs,
    pub c2: ComponentCoeffs,
}

/// `initialize_wavelet_data()` (§13.2.2) for all three components — the
/// zeroed coefficient store slices are unpacked into. Also the state
/// initialised by a fragmented picture's setup fragment (§14.3).
pub fn init_transform_data(seq: &SequenceHeader, tp: &TransformParameters) -> TransformData {
    let cp = &seq.coding_parameters;
    TransformData {
        y: init_component(cp.luma_width, cp.luma_height, tp),
        c1: init_component(cp.color_diff_width, cp.color_diff_height, tp),
        c2: init_component(cp.color_diff_width, cp.color_diff_height, tp),
    }
}

/// `slice(sx, sy)` (§13.5.2): unpack one slice into the coefficient store.
/// Called in raster order by [`transform_data`] for picture data units and
/// per carried slice by fragment data units (§14.4).
pub fn unpack_slice(
    r: &mut BitReader,
    tp: &TransformParameters,
    td: &mut TransformData,
    sx: u64,
    sy: u64,
    kind: PictureKind,
) -> Result<()> {
    // A slice starting past the end of the input means the picture was
    // truncated — fail rather than unpack fabricated zero bits.
    if r.overrun() {
        return Err(Error::UnexpectedEof);
    }
    match kind {
        PictureKind::LowDelay => ld_slice(r, tp, td, sx, sy),
        PictureKind::HighQuality => hq_slice(r, tp, td, sx, sy),
    }
}

/// The `dc_prediction` pass over the level-0 band of every component
/// (§13.4), run once all slices are present. Level 0 is the LL band for a
/// symmetric transform and the L band otherwise (§14.4 spells out both; the
/// band occupies index 0 of the layout in either case).
pub fn apply_dc_prediction(td: &mut TransformData) {
    dc_prediction(&mut td.y.bands[0]);
    dc_prediction(&mut td.c1.bands[0]);
    dc_prediction(&mut td.c2.bands[0]);
}

/// `transform_data()` (§13.5.2): unpack every slice into the coefficient
/// store and apply DC prediction for LD pictures.
pub fn transform_data(
    r: &mut BitReader,
    seq: &SequenceHeader,
    tp: &TransformParameters,
    kind: PictureKind,
) -> Result<TransformData> {
    let mut td = init_transform_data(seq, tp);

    for sy in 0..tp.slices_y {
        for sx in 0..tp.slices_x {
            unpack_slice(r, tp, &mut td, sx, sy, kind)?;
        }
    }
    // Catch truncation inside the final slice (unpack_slice only checks at
    // slice start). A complete picture ends at or before the input's end.
    if r.overrun() {
        return Err(Error::UnexpectedEof);
    }

    if kind.uses_dc_prediction() {
        apply_dc_prediction(&mut td);
    }

    Ok(td)
}

/// `slice_bytes()` (§13.5.3.2).
fn slice_bytes(tp: &TransformParameters, sx: u64, sy: u64) -> u64 {
    let slice_number = sy * tp.slices_x + sx;
    let a = ((slice_number + 1) * tp.slice_bytes_numerator) / tp.slice_bytes_denominator;
    let b = (slice_number * tp.slice_bytes_numerator) / tp.slice_bytes_denominator;
    a - b
}

/// `ld_slice()` (§13.5.3).
fn ld_slice(
    r: &mut BitReader,
    tp: &TransformParameters,
    td: &mut TransformData,
    sx: u64,
    sy: u64,
) -> Result<()> {
    let slice_total_bits = 8 * slice_bytes(tp, sx, sy);
    // Each LD slice must at least hold its own 7-bit qindex plus a
    // non-empty luma-length field (§13.5.3.1); a smaller fixed size can
    // only come from degenerate slice_bytes fractions.
    if slice_total_bits < 8 {
        return Err(Error::InvalidValue(
            "low-delay slice smaller than its own header",
        ));
    }
    let qindex = r.read_nbits(7);
    let quant = slice_quantizers(qindex, tp);

    let length_bits = crate::params::intlog2(slice_total_bits - 7);
    let slice_y_length = r.read_nbits(length_bits);

    // Luma.
    r.set_bits_left(slice_y_length);
    unpack_component_bands(r, &mut td.y, tp, &quant, sx, sy);
    r.flush_inputb();

    // Colour difference — interleaved C1/C2.
    let used = 7 + length_bits as u64 + slice_y_length;
    let cdiff_bits = slice_total_bits.saturating_sub(used);
    r.set_bits_left(cdiff_bits);
    unpack_color_diff_bands(r, td, tp, &quant, sx, sy);
    r.flush_inputb();
    Ok(())
}

/// `hq_slice()` (§13.5.4).
fn hq_slice(
    r: &mut BitReader,
    tp: &TransformParameters,
    td: &mut TransformData,
    sx: u64,
    sy: u64,
) -> Result<()> {
    r.skip_bytes(tp.slice_prefix_bytes)?;
    let qindex = r.read_uint_lit(1);
    let quant = slice_quantizers(qindex, tp);

    // The three components in turn, each with its own length code.
    for comp in 0..3 {
        let length = tp.slice_size_scaler * r.read_uint_lit(1);
        r.set_bits_left(8 * length);
        let store = match comp {
            0 => &mut td.y,
            1 => &mut td.c1,
            _ => &mut td.c2,
        };
        unpack_component_bands(r, store, tp, &quant, sx, sy);
        r.flush_inputb();
    }
    Ok(())
}

/// `slice_band()` over every layout band of one component (§13.5.6.3).
fn unpack_component_bands(
    r: &mut BitReader,
    store: &mut ComponentCoeffs,
    tp: &TransformParameters,
    quant: &[u64],
    sx: u64,
    sy: u64,
) {
    let layout = subband_layout(tp.dwt_depth_ho, tp.dwt_depth);
    for (i, &(level, _orient)) in layout.iter().enumerate() {
        let qi = quant[i];
        let band = &mut store.bands[i];
        let (sub_w, sub_h) = (band.width as u64, band.height as u64);
        let (left, right, top, bottom) =
            slice_bounds(sub_w, sub_h, tp.slices_x, tp.slices_y, sx, sy);
        let _ = level;
        for y in top..bottom {
            for x in left..right {
                let val = r.read_sintb();
                band.set(y as usize, x as usize, inverse_quant(val, qi));
            }
        }
    }
}

/// `color_diff_slice_band()` (§13.5.6.4): C1/C2 interleaved coefficient by
/// coefficient over every layout band.
fn unpack_color_diff_bands(
    r: &mut BitReader,
    td: &mut TransformData,
    tp: &TransformParameters,
    quant: &[u64],
    sx: u64,
    sy: u64,
) {
    let layout = subband_layout(tp.dwt_depth_ho, tp.dwt_depth);
    for (i, &(_level, _orient)) in layout.iter().enumerate() {
        let qi = quant[i];
        let (sub_w, sub_h) = {
            let b = &td.c1.bands[i];
            (b.width as u64, b.height as u64)
        };
        let (left, right, top, bottom) =
            slice_bounds(sub_w, sub_h, tp.slices_x, tp.slices_y, sx, sy);
        for y in top..bottom {
            for x in left..right {
                let v1 = r.read_sintb();
                td.c1.bands[i].set(y as usize, x as usize, inverse_quant(v1, qi));
                let v2 = r.read_sintb();
                td.c2.bands[i].set(y as usize, x as usize, inverse_quant(v2, qi));
            }
        }
    }
}

/// `dc_prediction()` (§13.4): in-place spatial prediction of the DC band.
///
/// Wrapping addition: the spec's integers are unbounded, and while every
/// well-formed stream stays far inside `i64`, hostile coefficient runs can
/// accumulate arbitrarily — wrap deterministically (the §15.5 clip bounds
/// the final output) instead of aborting.
fn dc_prediction(band: &mut Plane) {
    for y in 0..band.height {
        for x in 0..band.width {
            let prediction = if x > 0 && y > 0 {
                let a = band.get(y, x - 1);
                let b = band.get(y - 1, x - 1);
                let c = band.get(y - 1, x);
                mean3(a, b, c)
            } else if x > 0 {
                band.get(0, x - 1)
            } else if y > 0 {
                band.get(y - 1, 0)
            } else {
                0
            };
            let cur = band.get(y, x);
            band.set(y, x, cur.wrapping_add(prediction));
        }
    }
}

/// `mean(s0, s1, s2)` (§5.6.4): integer unbiased mean of three values.
///
/// Uses the spec's floor division (`//`, rounding toward −infinity per
/// §5.6.4 NOTE 1), which differs from Rust's truncating `/` for negative
/// numerators — `mean(-1,-2,-3)` must be −2, not −1. The sum wraps for the
/// same robustness reason as [`dc_prediction`].
#[inline]
fn mean3(a: i64, b: i64, c: i64) -> i64 {
    floor_div(a.wrapping_add(b).wrapping_add(c).wrapping_add(1), 3)
}

/// Floor division `a // b` for `b > 0` (§5.6.4): rounds toward −infinity.
#[inline]
fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    let r = a % b;
    if r != 0 && (r < 0) != (b < 0) {
        q - 1
    } else {
        q
    }
}

/// `idwt()` (§15.4.1) for one component coefficient store.
pub fn idwt(coeffs: &ComponentCoeffs, tp: &TransformParameters) -> Result<Plane> {
    let filter_v: WaveletFilter = wavelet::wavelet_filter(tp.wavelet_index)
        .ok_or(Error::UnsupportedWaveletIndex(tp.wavelet_index))?;
    let filter_ho: WaveletFilter = wavelet::wavelet_filter(tp.wavelet_index_ho)
        .ok_or(Error::UnsupportedWaveletIndex(tp.wavelet_index_ho))?;

    let layout = subband_layout(tp.dwt_depth_ho, tp.dwt_depth);
    // Index helper: find the band index for (level, orient).
    let find = |level: u64, orient: Orient| -> usize {
        layout
            .iter()
            .position(|&(l, o)| l == level && o == orient)
            .expect("layout contains the requested band")
    };

    // Start from the DC band (level 0).
    let mut dc = coeffs.bands[0].clone();

    // Horizontal-only stages (§15.4.1).
    for n in 1..=tp.dwt_depth_ho {
        let h = &coeffs.bands[find(n, Orient::H)];
        dc = wavelet::h_synthesis(&dc, h, &filter_ho);
    }
    // 2-D stages.
    for n in (tp.dwt_depth_ho + 1)..=(tp.dwt_depth_ho + tp.dwt_depth) {
        let hl = &coeffs.bands[find(n, Orient::HL)];
        let lh = &coeffs.bands[find(n, Orient::LH)];
        let hh = &coeffs.bands[find(n, Orient::HH)];
        dc = wavelet::vh_synthesis(&dc, hl, lh, hh, &filter_v, &filter_ho);
    }
    Ok(dc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subband_dims_symmetric_depth2() {
        // 8x8 luma, dwt_depth 2, ho 0.
        // level 0 -> 8 >> 2 = 2; level 1 -> 8 >> (2-1+1)=8>>2=2; level 2 -> 8>>(2-2+1)=8>>1=4.
        assert_eq!(subband_width(8, 0, 2, 0), 2);
        assert_eq!(subband_width(8, 0, 2, 1), 2);
        assert_eq!(subband_width(8, 0, 2, 2), 4);
        assert_eq!(subband_height(8, 0, 2, 0), 2);
        assert_eq!(subband_height(8, 0, 2, 2), 4);
    }

    #[test]
    fn layout_symmetric_depth2() {
        let l = subband_layout(0, 2);
        assert_eq!(l.len(), 1 + 2 * 3);
        assert_eq!(l[0], (0, Orient::LL));
        assert_eq!(l[1], (1, Orient::HL));
        assert_eq!(l[6], (2, Orient::HH));
    }

    #[test]
    fn mean3_matches_spec() {
        // mean(1,2,3) = (1+2+3+1)//3 = 7//3 = 2.
        assert_eq!(mean3(1, 2, 3), 2);
        // mean(-1,-2,-3) = (-6 + 1)//3 = -5//3 = -2 (floor).
        assert_eq!(mean3(-1, -2, -3), -2);
    }

    #[test]
    fn slice_bytes_distributes_remainder() {
        let tp = test_tp(0, 1, 5, 2); // 5/2 bytes per slice across 4 slices
                                      // slice 0: (1*5)//2 - 0 = 2; slice 1: (2*5)//2 - (1*5)//2 = 5-2=3; etc.
        assert_eq!(slice_bytes(&tp, 0, 0), 2);
        assert_eq!(slice_bytes(&tp, 1, 0), 3);
    }

    fn test_tp(dwt_depth_ho: u64, dwt_depth: u64, num: u64, den: u64) -> TransformParameters {
        TransformParameters {
            wavelet_index: 1,
            dwt_depth,
            wavelet_index_ho: 1,
            dwt_depth_ho,
            slices_x: 2,
            slices_y: 2,
            slice_bytes_numerator: num,
            slice_bytes_denominator: den,
            slice_prefix_bytes: 0,
            slice_size_scaler: 1,
            quant_matrix: quant::default_quant_matrix(1, dwt_depth).unwrap(),
        }
    }
}
