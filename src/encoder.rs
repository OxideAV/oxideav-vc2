//! VC-2 encoder (SMPTE ST 2042-1:2022) — the write-side counterpart of the
//! decoder modules, built from the same clauses run backwards.
//!
//! * **Forward DWT** — [`crate::transform::dwt`] / the analysis lifting of
//!   [`crate::wavelet`], the bit-exact inverse of §15.4 for all seven
//!   filters, including asymmetric (`dwt_depth_ho`) transforms; input
//!   pictures are edge-extended to the §13.2.3 padded dimensions.
//! * **Quantisation and slice packing** — §13.3 dead-zone forward
//!   quantisation ([`crate::quant::forward_quant`]), per-slice quantisation
//!   index election, write-side §13.4 DC prediction (low-delay pictures
//!   predict from the *reconstructed* DC band, so encoder and decoder
//!   agree bit-exactly), the §13.5.3 low-delay and §13.5.4 high-quality
//!   slice layouts with Annex A bounded blocks (trailing zero
//!   coefficients are omitted — the decoder's bounded reads return them),
//!   Annex D default matrices or a custom `quant_matrix` (§12.4.5.3).
//! * **Stream assembly** — §10.5 parse-info headers with exact
//!   next/previous offsets, the §11 sequence header expressed as Annex B
//!   base-format defaults plus only the §11.4 overrides that differ
//!   (preset indices where Tables 8–11 carry the value, explicit values
//!   otherwise), the §11.2.2 major-version rule, §12 picture headers and
//!   transform parameters (with the §12.4.4 extended parameters at major
//!   version 3), §14 fragmented pictures, end-of-sequence units and
//!   concatenated sequences.
//! * **Rate control** — [`RateControl`]: lossless (qindex 0), a fixed
//!   quantisation index, a picture byte target, or a bit rate derived from
//!   the frame rate. Low-delay pictures realise the target exactly through
//!   the §13.5.3.2 `slice_bytes` rational; high-quality pictures search
//!   the per-slice quantisation index against an even per-slice budget.
//!
//! Every emitted stream round-trips through this crate's decoder and, for
//! `RateControl::Lossless`, reproduces the input samples bit-exactly.

use crate::bitio::{sint_code_len, BitWriter};
use crate::conformance::{self, Violation};
use crate::params::{self, ColorDiffFormat, SequenceHeader, VideoParameters};
use crate::quant::{self, forward_quant, inverse_quant, MatrixLevel};
use crate::sequence::PARSE_INFO_PREFIX;
use crate::transform::{
    self, subband_layout, ComponentCoeffs, Orient, PictureKind, TransformParameters,
};
use crate::wavelet::Plane;
use crate::{Error, Result};

/// Size of a parse-info header (§10.5.1).
const PARSE_INFO_LEN: u32 = 13;

/// Largest low-delay quantisation index (7-bit field, §13.5.3.1).
const LD_MAX_QINDEX: u64 = 127;
/// Largest high-quality quantisation index (one byte, §13.5.4).
const HQ_MAX_QINDEX: u64 = 255;

/// How the encoder chooses quantisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Quantisation index 0 everywhere: the lifting transforms are
    /// integer-reversible and index 0 is the identity quantiser, so the
    /// decoder reproduces the input samples bit-exactly. High-quality
    /// pictures only (low-delay slices have a fixed size).
    Lossless,
    /// The same quantisation index for every slice (0..=127 for low
    /// delay, 0..=255 for high quality). High-quality pictures only.
    FixedQuantIndex(u64),
    /// Target size in bytes for each coded picture (slice data plus
    /// picture/transform headers and the parse-info header). Low-delay
    /// pictures hit it exactly; high-quality pictures elect per-slice
    /// indices against an even per-slice budget.
    PictureBytes(u64),
    /// Target bit rate; the per-picture byte target follows from the
    /// sequence's frame rate (halved per picture when coding fields).
    BitsPerSecond(u64),
}

/// Encoder configuration: the §11 video parameters plus the transform,
/// slice and rate choices.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Annex B base video format the sequence header is expressed
    /// against; every field of `video_parameters` that differs from the
    /// format's defaults is signalled as a §11.4 override.
    pub base_video_format: u64,
    /// The full §11.4 parameter map to code.
    pub video_parameters: VideoParameters,
    /// §11.5: 0 codes frames, 1 codes fields (each input picture is then
    /// one field, half the frame height).
    pub picture_coding_mode: u64,
    /// ST 2042-2 level claimed in the parse parameters (0 = none). For
    /// levels 1..=7 the configuration is checked against the level's
    /// constraints at construction.
    pub level: u64,
    /// Low-delay (profile 0) or high-quality (profile 3) pictures.
    pub kind: PictureKind,
    /// §12.4.2 vertical / symmetric wavelet index (Table 15, 0..=6).
    pub wavelet_index: u64,
    /// Number of 2-D transform levels.
    pub dwt_depth: u64,
    /// Horizontal wavelet index (§12.4.4.2); equal to `wavelet_index` for
    /// a symmetric transform.
    pub wavelet_index_ho: u64,
    /// Additional horizontal-only levels (§12.4.4.3), 0 for symmetric.
    pub dwt_depth_ho: u64,
    /// Slice grid (§12.4.5.2).
    pub slices_x: u64,
    /// See [`Self::slices_x`].
    pub slices_y: u64,
    /// Custom quantisation matrix (§12.4.5.3), indexed by level; `None`
    /// codes the Annex D default (an error when the annex defines none
    /// for the filter/depth combination).
    pub quant_matrix: Option<Vec<MatrixLevel>>,
    /// High quality: application bytes preceding every slice (§13.5.4),
    /// emitted as zeros.
    pub slice_prefix_bytes: u64,
    /// High quality: minimum `slice_size_scaler`; raised automatically
    /// when a component's coded length would not fit its one-byte code.
    pub slice_size_scaler: u64,
    /// Rate control mode.
    pub rate: RateControl,
    /// `Some(n)` codes every picture as §14 fragments: one setup fragment
    /// followed by data fragments carrying `n` slices each (the last may
    /// carry fewer). `None` codes plain picture data units.
    pub fragment_slices: Option<u16>,
}

impl EncoderConfig {
    /// A custom-format (base format 0) configuration for a progressive
    /// `width × height` picture at the given chroma sampling and uniform
    /// bit depth (8, 10, 12 or 16 → Table 10 presets 1, 3, 4, 8; other
    /// depths use an explicit full-range custom signal range): high
    /// quality, LeGall (5,3) depth 2, a slice grid of roughly 32×32 luma
    /// samples, lossless.
    pub fn new(width: u64, height: u64, color_diff_format: ColorDiffFormat, depth: u32) -> Self {
        let mut vp = params::set_source_defaults(0).expect("format 0 is defined");
        vp.frame_width = width;
        vp.frame_height = height;
        vp.clean_width = width;
        vp.clean_height = height;
        vp.color_diff_format = color_diff_format;
        let (lo, le, co, ce) = match depth {
            8 => (0, 255, 128, 255),
            10 => (64, 876, 512, 896),
            12 => (256, 3504, 2048, 3584),
            16 => (0, 65535, 32768, 65535),
            d => {
                let max = (1u64 << d) - 1;
                (0, max, 1u64 << (d - 1), max)
            }
        };
        vp.luma_offset = lo;
        vp.luma_excursion = le;
        vp.color_diff_offset = co;
        vp.color_diff_excursion = ce;
        EncoderConfig {
            base_video_format: 0,
            video_parameters: vp,
            picture_coding_mode: 0,
            level: 0,
            kind: PictureKind::HighQuality,
            wavelet_index: 1,
            dwt_depth: 2,
            wavelet_index_ho: 1,
            dwt_depth_ho: 0,
            slices_x: width.div_ceil(32).max(1),
            slices_y: height.div_ceil(32).max(1),
            quant_matrix: None,
            slice_prefix_bytes: 0,
            slice_size_scaler: 1,
            rate: RateControl::Lossless,
            fragment_slices: None,
        }
    }

    /// Set a symmetric wavelet / depth.
    pub fn wavelet(mut self, wavelet_index: u64, dwt_depth: u64) -> Self {
        self.wavelet_index = wavelet_index;
        self.wavelet_index_ho = wavelet_index;
        self.dwt_depth = dwt_depth;
        self
    }

    /// Set an asymmetric transform: `wavelet_index_ho` horizontally with
    /// `dwt_depth_ho` extra horizontal-only levels (§12.4.4).
    pub fn asymmetric(mut self, wavelet_index_ho: u64, dwt_depth_ho: u64) -> Self {
        self.wavelet_index_ho = wavelet_index_ho;
        self.dwt_depth_ho = dwt_depth_ho;
        self
    }

    /// Set the slice grid.
    pub fn slices(mut self, slices_x: u64, slices_y: u64) -> Self {
        self.slices_x = slices_x;
        self.slices_y = slices_y;
        self
    }

    /// Set the picture kind (profile).
    pub fn picture_kind(mut self, kind: PictureKind) -> Self {
        self.kind = kind;
        self
    }

    /// Set the rate control mode.
    pub fn rate_control(mut self, rate: RateControl) -> Self {
        self.rate = rate;
        self
    }

    /// Code pictures as fragments of `slices_per_fragment` slices.
    pub fn fragments(mut self, slices_per_fragment: u16) -> Self {
        self.fragment_slices = Some(slices_per_fragment);
        self
    }

    /// The Annex C profile value for [`Self::kind`].
    pub fn profile(&self) -> u64 {
        match self.kind {
            PictureKind::LowDelay => 0,
            PictureKind::HighQuality => 3,
        }
    }

    /// True when the transform is asymmetric (§12.4.4 parameters needed).
    fn asymmetric_transform(&self) -> bool {
        self.dwt_depth_ho != 0 || self.wavelet_index_ho != self.wavelet_index
    }

    /// Parse code of the picture / fragment data units.
    fn parse_code(&self) -> u8 {
        match (self.kind, self.fragment_slices.is_some()) {
            (PictureKind::LowDelay, false) => 0xC8,
            (PictureKind::LowDelay, true) => 0xCC,
            (PictureKind::HighQuality, false) => 0xE8,
            (PictureKind::HighQuality, true) => 0xEC,
        }
    }
}

/// One input picture: three row-major planes of unsigned code values at
/// the sequence's coding dimensions (`luma_width × luma_height`, the
/// colour-difference planes at `color_diff_width × color_diff_height`;
/// fields when `picture_coding_mode == 1`).
#[derive(Debug, Clone, Copy)]
pub struct PictureInput<'a> {
    pub y: &'a [u16],
    pub c1: &'a [u16],
    pub c2: &'a [u16],
}

/// The §11.4 override plan for a sequence header: which custom flags are
/// set and which preset indices are signalled.
#[derive(Debug, Clone, Copy)]
struct HeaderPlan {
    frame_size: bool,
    chroma: bool,
    scan: bool,
    /// `Some(index)`: custom frame rate, Table 8 index or 0 (explicit).
    frame_rate: Option<u64>,
    pixel_aspect: Option<u64>,
    clean_area: bool,
    signal_range: Option<u64>,
    /// `Some((index, primaries, matrix, transfer))`: Table 11 index (0 =
    /// custom, with the per-part override flags).
    color_spec: Option<(u64, bool, bool, bool)>,
}

fn plan_header(cfg: &EncoderConfig) -> Result<HeaderPlan> {
    let d = params::set_source_defaults(cfg.base_video_format)?;
    let v = &cfg.video_parameters;
    let frame_rate = (v.frame_rate_numer != d.frame_rate_numer
        || v.frame_rate_denom != d.frame_rate_denom)
        .then(|| {
            params::frame_rate_preset_index(v.frame_rate_numer, v.frame_rate_denom).unwrap_or(0)
        });
    let pixel_aspect = (v.pixel_aspect_ratio_numer != d.pixel_aspect_ratio_numer
        || v.pixel_aspect_ratio_denom != d.pixel_aspect_ratio_denom)
        .then(|| {
            params::pixel_aspect_ratio_preset_index(
                v.pixel_aspect_ratio_numer,
                v.pixel_aspect_ratio_denom,
            )
            .unwrap_or(0)
        });
    let signal_range = (v.luma_offset != d.luma_offset
        || v.luma_excursion != d.luma_excursion
        || v.color_diff_offset != d.color_diff_offset
        || v.color_diff_excursion != d.color_diff_excursion)
        .then(|| {
            params::signal_range_preset_index(
                v.luma_offset,
                v.luma_excursion,
                v.color_diff_offset,
                v.color_diff_excursion,
            )
            .unwrap_or(0)
        });
    let color_spec = (v.color_primaries_index != d.color_primaries_index
        || v.color_matrix_index != d.color_matrix_index
        || v.transfer_function_index != d.transfer_function_index)
        .then(|| {
            match params::color_spec_preset_index(
                v.color_primaries_index,
                v.color_matrix_index,
                v.transfer_function_index,
            ) {
                Some(i) => (i, false, false, false),
                // Index 0 starts from the (0, 0, 0) triple (Table 11 row 0)
                // and overrides the parts that differ.
                None => (
                    0,
                    v.color_primaries_index != 0,
                    v.color_matrix_index != 0,
                    v.transfer_function_index != 0,
                ),
            }
        });
    Ok(HeaderPlan {
        frame_size: v.frame_width != d.frame_width || v.frame_height != d.frame_height,
        chroma: v.color_diff_format != d.color_diff_format,
        scan: v.source_sampling != d.source_sampling,
        frame_rate,
        pixel_aspect,
        clean_area: v.clean_width != d.clean_width
            || v.clean_height != d.clean_height
            || v.left_offset != d.left_offset
            || v.top_offset != d.top_offset,
        signal_range,
        color_spec,
    })
}

/// The §11.2.2 major version the configuration requires.
fn major_version(cfg: &EncoderConfig, plan: &HeaderPlan) -> u64 {
    let v = &cfg.video_parameters;
    let extended_enums = plan.frame_rate.is_some_and(|i| i > 11)
        || plan.signal_range.is_some_and(|i| i > 4)
        || plan.color_spec.is_some_and(|(i, p, m, t)| {
            i > 4
                || (i == 0
                    && ((p && v.color_primaries_index > 3)
                        || (m && v.color_matrix_index > 3)
                        || (t && v.transfer_function_index > 3)))
        });
    if extended_enums || cfg.asymmetric_transform() || cfg.fragment_slices.is_some() {
        3
    } else if cfg.kind == PictureKind::HighQuality {
        2
    } else {
        1
    }
}

/// `sequence_header()` (§11.1) written from the plan.
fn write_sequence_header(cfg: &EncoderConfig, plan: &HeaderPlan, major: u64) -> Vec<u8> {
    let v = &cfg.video_parameters;
    let mut w = BitWriter::new();
    // parse_parameters (§11.2.1).
    w.put_uint(major);
    w.put_uint(0);
    w.put_uint(cfg.profile());
    w.put_uint(cfg.level);
    w.put_uint(cfg.base_video_format);
    // source_parameters (§11.4.1).
    w.put_bool(plan.frame_size);
    if plan.frame_size {
        w.put_uint(v.frame_width);
        w.put_uint(v.frame_height);
    }
    w.put_bool(plan.chroma);
    if plan.chroma {
        w.put_uint(v.color_diff_format.index());
    }
    w.put_bool(plan.scan);
    if plan.scan {
        w.put_uint(v.source_sampling);
    }
    w.put_bool(plan.frame_rate.is_some());
    if let Some(i) = plan.frame_rate {
        w.put_uint(i);
        if i == 0 {
            w.put_uint(v.frame_rate_numer);
            w.put_uint(v.frame_rate_denom);
        }
    }
    w.put_bool(plan.pixel_aspect.is_some());
    if let Some(i) = plan.pixel_aspect {
        w.put_uint(i);
        if i == 0 {
            w.put_uint(v.pixel_aspect_ratio_numer);
            w.put_uint(v.pixel_aspect_ratio_denom);
        }
    }
    w.put_bool(plan.clean_area);
    if plan.clean_area {
        w.put_uint(v.clean_width);
        w.put_uint(v.clean_height);
        w.put_uint(v.left_offset);
        w.put_uint(v.top_offset);
    }
    w.put_bool(plan.signal_range.is_some());
    if let Some(i) = plan.signal_range {
        w.put_uint(i);
        if i == 0 {
            w.put_uint(v.luma_offset);
            w.put_uint(v.luma_excursion);
            w.put_uint(v.color_diff_offset);
            w.put_uint(v.color_diff_excursion);
        }
    }
    w.put_bool(plan.color_spec.is_some());
    if let Some((i, p, m, t)) = plan.color_spec {
        w.put_uint(i);
        if i == 0 {
            w.put_bool(p);
            if p {
                w.put_uint(v.color_primaries_index);
            }
            w.put_bool(m);
            if m {
                w.put_uint(v.color_matrix_index);
            }
            w.put_bool(t);
            if t {
                w.put_uint(v.transfer_function_index);
            }
        }
    }
    // picture_coding_mode (§11.5).
    w.put_uint(cfg.picture_coding_mode);
    w.into_bytes()
}

/// `transform_parameters()` (§12.4.1) written for a picture / setup
/// fragment, byte-aligned at the end (§12.3 `wavelet_transform`).
fn write_transform_parameters(
    w: &mut BitWriter,
    tp: &TransformParameters,
    kind: PictureKind,
    major: u64,
    custom_matrix: bool,
) {
    w.put_uint(tp.wavelet_index);
    w.put_uint(tp.dwt_depth);
    if major >= 3 {
        // extended_transform_parameters() (§12.4.4).
        w.put_bool(tp.asym_transform_index_flag);
        if tp.asym_transform_index_flag {
            w.put_uint(tp.wavelet_index_ho);
        }
        w.put_bool(tp.asym_transform_flag);
        if tp.asym_transform_flag {
            w.put_uint(tp.dwt_depth_ho);
        }
    }
    // slice_parameters() (§12.4.5.2).
    w.put_uint(tp.slices_x);
    w.put_uint(tp.slices_y);
    match kind {
        PictureKind::LowDelay => {
            w.put_uint(tp.slice_bytes_numerator);
            w.put_uint(tp.slice_bytes_denominator);
        }
        PictureKind::HighQuality => {
            w.put_uint(tp.slice_prefix_bytes);
            w.put_uint(tp.slice_size_scaler);
        }
    }
    // quant_matrix() (§12.4.5.3).
    w.put_bool(custom_matrix);
    if custom_matrix {
        for level in &tp.quant_matrix {
            match *level {
                MatrixLevel::Ll(v) | MatrixLevel::H(v) => w.put_uint(v.max(0) as u64),
                MatrixLevel::Ac { hl, lh, hh } => {
                    w.put_uint(hl.max(0) as u64);
                    w.put_uint(lh.max(0) as u64);
                    w.put_uint(hh.max(0) as u64);
                }
            }
        }
    }
    w.byte_align();
}

/// Bit cost of a coefficient run once trailing zeros are dropped: the
/// decoder's bounded reads (A.4.2) return 0 for every coefficient past
/// the block, so only the prefix up to the last non-zero value is coded.
/// Returns `(bits, coefficients_kept)`.
fn trimmed_cost(vals: &[i64]) -> (u64, usize) {
    let keep = vals.iter().rposition(|&v| v != 0).map_or(0, |p| p + 1);
    let bits = vals[..keep].iter().map(|&v| sint_code_len(v)).sum();
    (bits, keep)
}

/// Longest prefix of `vals` whose coded length fits in `budget` bits.
fn fit_prefix(vals: &[i64], budget: u64) -> usize {
    let mut used = 0;
    for (i, &v) in vals.iter().enumerate() {
        used += sint_code_len(v);
        if used > budget {
            return i;
        }
    }
    vals.len()
}

/// Quantised coefficients of one slice, in stream order, for one
/// candidate quantisation index.
struct SliceQ {
    /// Per component (Y, C1, C2), every layout band's slice region in
    /// raster order.
    comps: [Vec<i64>; 3],
    /// Reconstructed DC values for the slice region (low delay only:
    /// `(component, y, x, value)` triples committed when the slice is
    /// adopted).
    recon: Vec<(usize, usize, usize, i64)>,
}

/// Per-picture slice coder: the transformed components plus the running
/// reconstructed DC bands the low-delay prediction reads.
struct SliceCoder<'a> {
    tp: &'a TransformParameters,
    kind: PictureKind,
    layout: Vec<(u64, Orient)>,
    coeffs: [&'a ComponentCoeffs; 3],
    /// Reconstructed (dequantised, predicted) DC band per component —
    /// what the decoder's §13.4 pass will hold — for low-delay pictures.
    recon_dc: [Plane; 3],
}

impl<'a> SliceCoder<'a> {
    fn new(
        tp: &'a TransformParameters,
        kind: PictureKind,
        coeffs: [&'a ComponentCoeffs; 3],
    ) -> Self {
        let recon_dc = [
            Plane::new(coeffs[0].bands[0].width, coeffs[0].bands[0].height),
            Plane::new(coeffs[1].bands[0].width, coeffs[1].bands[0].height),
            Plane::new(coeffs[2].bands[0].width, coeffs[2].bands[0].height),
        ];
        SliceCoder {
            tp,
            kind,
            layout: subband_layout(tp.dwt_depth_ho, tp.dwt_depth),
            coeffs,
            recon_dc,
        }
    }

    /// Quantise slice `(sx, sy)` at `qindex`.
    fn quantise(&self, sx: u64, sy: u64, qindex: u64) -> SliceQ {
        let quant = transform::slice_quantizers(qindex, self.tp);
        let mut comps: [Vec<i64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let mut recon = Vec::new();
        for (c, out) in comps.iter_mut().enumerate() {
            let store = self.coeffs[c];
            // Scratch copy of the DC reconstruction so in-slice neighbours
            // see the values this candidate produces.
            let mut scratch = if self.kind.uses_dc_prediction() {
                Some(self.recon_dc[c].clone())
            } else {
                None
            };
            for (i, _) in self.layout.iter().enumerate() {
                let qi = quant[i];
                let band = &store.bands[i];
                let (left, right, top, bottom) = transform::slice_bounds(
                    band.width as u64,
                    band.height as u64,
                    self.tp.slices_x,
                    self.tp.slices_y,
                    sx,
                    sy,
                );
                for y in top as usize..bottom as usize {
                    for x in left as usize..right as usize {
                        let coeff = band.get(y, x);
                        let q = match (i, scratch.as_mut()) {
                            (0, Some(rec)) => {
                                // Write-side §13.4: predict from reconstructed
                                // neighbours, code the residual, track the
                                // reconstruction the decoder will form.
                                let pred = dc_predict(rec, y, x);
                                let q = forward_quant(coeff - pred, qi);
                                let value = pred + inverse_quant(q, qi);
                                rec.set(y, x, value);
                                recon.push((c, y, x, value));
                                q
                            }
                            _ => forward_quant(coeff, qi),
                        };
                        out.push(q);
                    }
                }
            }
        }
        SliceQ { comps, recon }
    }

    /// Adopt a candidate: commit its DC reconstruction.
    fn commit(&mut self, q: &SliceQ) {
        for &(c, y, x, v) in &q.recon {
            self.recon_dc[c].set(y, x, v);
        }
    }
}

/// The §13.4 `dc_prediction` predictor for `(y, x)` over an already
/// reconstructed band (the decoder's in-place pass reads exactly these
/// neighbours, all earlier in raster order).
fn dc_predict(band: &Plane, y: usize, x: usize) -> i64 {
    if x > 0 && y > 0 {
        let a = band.get(y, x - 1);
        let b = band.get(y - 1, x - 1);
        let c = band.get(y - 1, x);
        floor_div(a + b + c + 1, 3)
    } else if x > 0 {
        band.get(0, x - 1)
    } else if y > 0 {
        band.get(y - 1, 0)
    } else {
        0
    }
}

/// Floor division for `b > 0` (§5.6.4).
fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    if a % b != 0 && (a < 0) != (b < 0) {
        q - 1
    } else {
        q
    }
}

/// The coded form of one slice.
struct CodedSlice {
    bytes: Vec<u8>,
    /// High quality: the largest component byte length before the
    /// scaler's rounding, to size `slice_size_scaler`.
    max_component_bytes: u64,
}

/// Search the smallest quantisation index in `lo..=hi` accepted by
/// `fits` (assumed monotone: once a slice fits it keeps fitting at
/// coarser indices). Falls back to a linear walk if the assumption is
/// violated locally, and returns `hi` when nothing fits.
fn elect_qindex(lo: u64, hi: u64, fits: &mut dyn FnMut(u64) -> bool) -> u64 {
    let (mut a, mut b) = (lo, hi);
    while a < b {
        let mid = (a + b) / 2;
        if fits(mid) {
            b = mid;
        } else {
            a = mid + 1;
        }
    }
    let mut q = a;
    while q < hi && !fits(q) {
        q += 1;
    }
    q
}

/// Pack a low-delay slice (§13.5.3) into its fixed `slice_bytes`.
fn code_ld_slice(
    coder: &mut SliceCoder,
    sx: u64,
    sy: u64,
    fixed_q: Option<u64>,
) -> Result<CodedSlice> {
    let tp = coder.tp;
    let total_bits = 8 * transform::slice_bytes(tp, sx, sy);
    if total_bits < 8 {
        return Err(Error::InvalidValue(
            "low-delay slice budget smaller than its own header",
        ));
    }
    let length_bits = params::intlog2(total_bits - 7) as u64;
    let avail = total_bits - 7 - length_bits;
    // Interleaved colour-difference run (§13.5.6.4).
    let interleave = |q: &SliceQ| -> Vec<i64> {
        q.comps[1]
            .iter()
            .zip(&q.comps[2])
            .flat_map(|(&a, &b)| [a, b])
            .collect()
    };
    let cost = |q: &SliceQ| -> (u64, u64) {
        let (yb, _) = trimmed_cost(&q.comps[0]);
        let (cb, _) = trimmed_cost(&interleave(q));
        (yb, cb)
    };
    let qindex = match fixed_q {
        Some(q) => q.min(LD_MAX_QINDEX),
        None => elect_qindex(0, LD_MAX_QINDEX, &mut |q| {
            let (yb, cb) = cost(&coder.quantise(sx, sy, q));
            yb + cb <= avail
        }),
    };
    let sq = coder.quantise(sx, sy, qindex);
    let cd = interleave(&sq);
    let (yb, ykeep) = trimmed_cost(&sq.comps[0]);
    let (cb, ckeep) = trimmed_cost(&cd);
    // Truncate (coefficients past the cut decode as zero) when even the
    // coarsest index overflows the fixed budget.
    let (ykeep, ckeep) = if yb + cb <= avail {
        (ykeep, ckeep)
    } else {
        let yk = fit_prefix(&sq.comps[0][..ykeep], avail);
        let ybits: u64 = sq.comps[0][..yk].iter().map(|&v| sint_code_len(v)).sum();
        let ck = fit_prefix(&cd[..ckeep], avail - ybits);
        (yk, ck)
    };
    let y_len: u64 = sq.comps[0][..ykeep].iter().map(|&v| sint_code_len(v)).sum();
    let mut w = BitWriter::new();
    w.put_nbits(qindex, 7);
    w.put_nbits(y_len, length_bits as u32);
    for &v in &sq.comps[0][..ykeep] {
        w.put_sint(v);
    }
    for &v in &cd[..ckeep] {
        w.put_sint(v);
    }
    // The colour-difference bounded block runs to the end of the slice
    // (§13.5.3), so the fill must be one-bits: inside a bounded block a
    // solitary 1 is the code for 0, whereas a 0 bit would open a bogus
    // non-zero code. The decoder reads any trimmed coefficients from
    // this fill as zeros and flushes the rest.
    while w.bit_len() < total_bits {
        w.put_bit(1);
    }
    debug_assert_eq!(w.bit_len(), total_bits);
    coder.commit(&sq);
    Ok(CodedSlice {
        bytes: w.into_bytes(),
        max_component_bytes: 0,
    })
}

/// Pack a high-quality slice (§13.5.4). `budget` bounds the whole slice
/// (prefix, qindex, length codes and component data) in bytes when rate
/// controlled; the per-component byte length is rounded up to the
/// scaler. A component whose rounded length exceeds `255 * scaler` is
/// still coded (with a wrapped code) and reported through
/// `max_component_bytes` so the caller can raise the scaler and retry.
fn code_hq_slice(
    coder: &mut SliceCoder,
    sx: u64,
    sy: u64,
    fixed_q: Option<u64>,
    budget: Option<u64>,
) -> CodedSlice {
    let tp = coder.tp;
    let scaler = tp.slice_size_scaler;
    let overhead = tp.slice_prefix_bytes + 1 + 3;
    let comp_bytes = |q: &SliceQ| -> [u64; 3] {
        let mut out = [0u64; 3];
        for (c, vals) in q.comps.iter().enumerate() {
            let (bits, _) = trimmed_cost(vals);
            out[c] = bits.div_ceil(8);
        }
        out
    };
    let rounded = |b: u64| b.div_ceil(scaler) * scaler;
    let size =
        |q: &SliceQ| -> u64 { overhead + comp_bytes(q).iter().map(|&b| rounded(b)).sum::<u64>() };
    let qindex = match (fixed_q, budget) {
        (Some(q), _) => q.min(HQ_MAX_QINDEX),
        (None, Some(budget)) => elect_qindex(0, HQ_MAX_QINDEX, &mut |q| {
            size(&coder.quantise(sx, sy, q)) <= budget
        }),
        (None, None) => 0,
    };
    let sq = coder.quantise(sx, sy, qindex);
    let bytes = comp_bytes(&sq);
    let mut w = BitWriter::new();
    for _ in 0..tp.slice_prefix_bytes {
        w.put_nbits(0, 8);
    }
    w.put_nbits(qindex, 8);
    for (c, vals) in sq.comps.iter().enumerate() {
        let len = rounded(bytes[c]);
        w.put_nbits((len / scaler) & 0xFF, 8);
        let (_, keep) = trimmed_cost(vals);
        let start = w.bit_len();
        for &v in &vals[..keep] {
            w.put_sint(v);
        }
        // One-bit fill: the whole `8 * len` block is bounded-read
        // territory, and a 1 bit is the exp-Golomb code for 0 — so the
        // trimmed trailing zero coefficients decode as the zeros they
        // are (a 0 fill bit would instead open a bogus non-zero code).
        while w.bit_len() < start + 8 * len {
            w.put_bit(1);
        }
    }
    coder.commit(&sq);
    CodedSlice {
        bytes: w.into_bytes(),
        max_component_bytes: bytes.into_iter().max().unwrap_or(0),
    }
}

/// Stateful VC-2 stream writer: emits a sequence header on the first
/// picture of each sequence, then picture (or fragment) data units, and
/// an end-of-sequence unit on [`Self::end_sequence`]. Concatenating the
/// returned byte runs yields a VC-2 stream (§10.3) the decoder walks in
/// one pass.
pub struct SequenceEncoder {
    cfg: EncoderConfig,
    seq: SequenceHeader,
    seq_header_body: Vec<u8>,
    major: u64,
    base_tp: TransformParameters,
    custom_matrix: bool,
    picture_number: u32,
    /// Size of the previous data unit (header included) — the next
    /// unit's `previous_parse_offset`; 0 at a sequence start.
    prev_unit_len: u32,
    in_sequence: bool,
    /// Byte target per coded picture for the rate-controlled modes.
    target_picture_bytes: Option<u64>,
}

impl SequenceEncoder {
    /// Validate a configuration, build its sequence header and check the
    /// claimed level's constraints (levels 1..=7).
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        if cfg.wavelet_index > 6 {
            return Err(Error::UnsupportedWaveletIndex(cfg.wavelet_index));
        }
        if cfg.wavelet_index_ho > 6 {
            return Err(Error::UnsupportedWaveletIndex(cfg.wavelet_index_ho));
        }
        if cfg.dwt_depth + cfg.dwt_depth_ho > transform::MAX_TOTAL_TRANSFORM_DEPTH {
            return Err(Error::InvalidValue(
                "total transform depth exceeds the implementation cap",
            ));
        }
        if cfg.slices_x == 0 || cfg.slices_y == 0 {
            return Err(Error::InvalidValue("slices_x / slices_y must be >= 1"));
        }
        if cfg.slice_size_scaler == 0 {
            return Err(Error::InvalidValue("slice_size_scaler must be >= 1"));
        }
        if cfg.fragment_slices == Some(0) {
            return Err(Error::InvalidValue(
                "a data fragment must carry at least one slice",
            ));
        }
        match (cfg.kind, cfg.rate) {
            (PictureKind::LowDelay, RateControl::Lossless | RateControl::FixedQuantIndex(_)) => {
                return Err(Error::Unsupported(
                    "low-delay pictures have fixed-size slices: use PictureBytes or BitsPerSecond",
                ))
            }
            (PictureKind::HighQuality, RateControl::FixedQuantIndex(q)) if q > HQ_MAX_QINDEX => {
                return Err(Error::InvalidValue("quantisation index above 255"))
            }
            _ => {}
        }
        let plan = plan_header(&cfg)?;
        let major = major_version(&cfg, &plan);
        let seq_header_body = write_sequence_header(&cfg, &plan, major);
        // Parse the header back: the decoder's §11 validation and derived
        // coding parameters are the single source of truth.
        let mut r = crate::bitio::BitReader::new(&seq_header_body);
        let seq = params::sequence_header(&mut r)?;
        let quant_matrix = match &cfg.quant_matrix {
            Some(m) => {
                if m.len() as u64 != cfg.dwt_depth_ho + cfg.dwt_depth + 1 {
                    return Err(Error::InvalidValue(
                        "custom quantisation matrix must hold one entry per level",
                    ));
                }
                m.clone()
            }
            None => quant::default_quant_matrix_full(
                cfg.wavelet_index,
                cfg.wavelet_index_ho,
                cfg.dwt_depth,
                cfg.dwt_depth_ho,
            )
            .ok_or(Error::MissingQuantMatrix)?,
        };
        let base_tp = TransformParameters {
            wavelet_index: cfg.wavelet_index,
            dwt_depth: cfg.dwt_depth,
            wavelet_index_ho: cfg.wavelet_index_ho,
            dwt_depth_ho: cfg.dwt_depth_ho,
            asym_transform_index_flag: cfg.wavelet_index_ho != cfg.wavelet_index,
            asym_transform_flag: cfg.dwt_depth_ho != 0,
            slices_x: cfg.slices_x,
            slices_y: cfg.slices_y,
            slice_bytes_numerator: 1,
            slice_bytes_denominator: 1,
            slice_prefix_bytes: cfg.slice_prefix_bytes,
            slice_size_scaler: cfg.slice_size_scaler,
            quant_matrix,
        };
        let cp = &seq.coding_parameters;
        let (pw, ph) = transform::padded_dims(cp.luma_width, cp.luma_height, &base_tp);
        if (pw as u64) * (ph as u64) > transform::MAX_PADDED_AREA {
            return Err(Error::InvalidValue(
                "padded picture area exceeds the implementation cap",
            ));
        }
        let target_picture_bytes = match cfg.rate {
            RateControl::PictureBytes(n) => Some(n),
            RateControl::BitsPerSecond(bps) => {
                let v = &cfg.video_parameters;
                let pictures_per_frame = if cfg.picture_coding_mode == 1 { 2 } else { 1 };
                // bytes/picture = bps * denom / (8 * numer * pictures_per_frame)
                let bytes = (bps as u128 * v.frame_rate_denom as u128)
                    / (8 * v.frame_rate_numer as u128 * pictures_per_frame);
                Some(bytes.min(u64::MAX as u128) as u64)
            }
            _ => None,
        };
        let enc = SequenceEncoder {
            custom_matrix: cfg.quant_matrix.is_some(),
            cfg,
            seq,
            seq_header_body,
            major,
            base_tp,
            picture_number: 0,
            prev_unit_len: 0,
            in_sequence: false,
            target_picture_bytes,
        };
        let violations = enc.level_violations();
        if !violations.is_empty() {
            return Err(Error::InvalidValue(
                "configuration violates the claimed ST 2042-2 level (see level_violations)",
            ));
        }
        Ok(enc)
    }

    /// The ST 2042-2 constraints the configuration breaks for its claimed
    /// level (empty for level 0 and for conforming configurations) —
    /// the `conformance` module's sequence-header and picture checks.
    pub fn level_violations(&self) -> Vec<Violation> {
        let mut v = conformance::check_sequence_header(&self.seq);
        v.extend(conformance::check_transform_parameters(
            &self.seq,
            &self.base_tp,
        ));
        v
    }

    /// The parsed form of the sequence header this encoder emits.
    pub fn sequence_header(&self) -> &SequenceHeader {
        &self.seq
    }

    /// The §11.2.2 major version signalled.
    pub fn major_version(&self) -> u64 {
        self.major
    }

    /// The complete sequence-header data unit (parse-info header + body)
    /// as it opens each sequence — e.g. for out-of-band staging.
    pub fn sequence_header_unit(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_parse_info(&mut out, 0x00, self.seq_header_body.len() as u32, 0);
        out.extend_from_slice(&self.seq_header_body);
        out
    }

    /// Picture number the next picture will carry (§12.2).
    pub fn next_picture_number(&self) -> u32 {
        self.picture_number
    }

    fn push_unit(&mut self, out: &mut Vec<u8>, parse_code: u8, body: &[u8]) {
        let len = PARSE_INFO_LEN + body.len() as u32;
        write_parse_info(out, parse_code, len, self.prev_unit_len);
        out.extend_from_slice(body);
        self.prev_unit_len = len;
        self.in_sequence = true;
    }

    /// Encode one picture, returning the data units emitted for it (the
    /// sequence header first when a sequence starts).
    pub fn encode_picture(&mut self, pic: &PictureInput) -> Result<Vec<u8>> {
        let cp = self.seq.coding_parameters;
        let need_y = (cp.luma_width * cp.luma_height) as usize;
        let need_c = (cp.color_diff_width * cp.color_diff_height) as usize;
        if pic.y.len() != need_y || pic.c1.len() != need_c || pic.c2.len() != need_c {
            return Err(Error::InvalidValue(
                "input planes do not match the coding dimensions",
            ));
        }
        let mut out = Vec::new();
        if !self.in_sequence {
            let body = self.seq_header_body.clone();
            self.push_unit(&mut out, 0x00, &body);
        }
        let (tp, slices) = self.code_picture(pic)?;
        let picture_number = self.picture_number;
        self.picture_number = self.picture_number.wrapping_add(1);
        let code = self.cfg.parse_code();
        match self.cfg.fragment_slices {
            None => {
                // picture_parse() (§12.1): header, transform parameters,
                // slices.
                let mut w = BitWriter::new();
                w.put_uint_lit(picture_number as u64, 4);
                write_transform_parameters(
                    &mut w,
                    &tp,
                    self.cfg.kind,
                    self.major,
                    self.custom_matrix,
                );
                let mut body = w.into_bytes();
                for s in &slices {
                    body.extend_from_slice(s);
                }
                self.push_unit(&mut out, code, &body);
            }
            Some(per_fragment) => {
                // Setup fragment (§14.2 / §14.3).
                let mut w = BitWriter::new();
                write_transform_parameters(
                    &mut w,
                    &tp,
                    self.cfg.kind,
                    self.major,
                    self.custom_matrix,
                );
                let tp_bytes = w.into_bytes();
                let mut body = Vec::new();
                write_fragment_header(&mut body, picture_number, tp_bytes.len(), 0, 0, 0);
                body.extend_from_slice(&tp_bytes);
                self.push_unit(&mut out, code, &body);
                // Data fragments (§14.4), raster order, `per_fragment`
                // slices each.
                for (chunk_idx, chunk) in slices.chunks(per_fragment as usize).enumerate() {
                    let first = chunk_idx as u64 * per_fragment as u64;
                    let data_len: usize = chunk.iter().map(|s| s.len()).sum();
                    let mut body = Vec::new();
                    write_fragment_header(
                        &mut body,
                        picture_number,
                        data_len,
                        chunk.len() as u16,
                        (first % tp.slices_x) as u16,
                        (first / tp.slices_x) as u16,
                    );
                    for s in chunk {
                        body.extend_from_slice(s);
                    }
                    self.push_unit(&mut out, code, &body);
                }
            }
        }
        Ok(out)
    }

    /// Emit the end-of-sequence data unit (§10.4.1) and reset the
    /// per-sequence state; the next picture opens a new sequence with a
    /// fresh sequence header. Returns nothing when no sequence is open.
    pub fn end_sequence(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.in_sequence {
            write_parse_info(&mut out, 0x10, 0, self.prev_unit_len);
            self.prev_unit_len = 0;
            self.in_sequence = false;
            self.picture_number = 0;
        }
        out
    }

    /// True between the first picture of a sequence and its
    /// [`Self::end_sequence`].
    pub fn in_sequence(&self) -> bool {
        self.in_sequence
    }

    /// Transform, quantise and pack one picture into its slices.
    fn code_picture(&self, pic: &PictureInput) -> Result<(TransformParameters, Vec<Vec<u8>>)> {
        let cp = self.seq.coding_parameters;
        let mut tp = self.base_tp.clone();
        let y_plane =
            transform::pad_component(pic.y, cp.luma_width, cp.luma_height, cp.luma_depth, &tp);
        let c1_plane = transform::pad_component(
            pic.c1,
            cp.color_diff_width,
            cp.color_diff_height,
            cp.color_diff_depth,
            &tp,
        );
        let c2_plane = transform::pad_component(
            pic.c2,
            cp.color_diff_width,
            cp.color_diff_height,
            cp.color_diff_depth,
            &tp,
        );
        let y = transform::dwt(&y_plane, cp.luma_width, cp.luma_height, &tp)?;
        let c1 = transform::dwt(&c1_plane, cp.color_diff_width, cp.color_diff_height, &tp)?;
        let c2 = transform::dwt(&c2_plane, cp.color_diff_width, cp.color_diff_height, &tp)?;
        let n_slices = tp.slices_x * tp.slices_y;
        let fixed_q = match self.cfg.rate {
            RateControl::Lossless => Some(0),
            RateControl::FixedQuantIndex(q) => Some(q),
            _ => None,
        };
        match self.cfg.kind {
            PictureKind::LowDelay => {
                let target = self
                    .target_picture_bytes
                    .expect("low-delay rate control always carries a byte target");
                // Slice budget = target minus the fixed per-picture
                // overhead (parse-info + picture header + transform
                // parameters); the transform parameters carry the
                // numerator itself, so size them with the final value.
                let overhead = |num: u64| -> u64 {
                    let mut probe = tp.clone();
                    probe.slice_bytes_numerator = num;
                    probe.slice_bytes_denominator = n_slices;
                    let mut w = BitWriter::new();
                    write_transform_parameters(
                        &mut w,
                        &probe,
                        self.cfg.kind,
                        self.major,
                        self.custom_matrix,
                    );
                    PARSE_INFO_LEN as u64 + 4 + w.into_bytes().len() as u64
                };
                let mut num = target.saturating_sub(overhead(target)).max(n_slices);
                num = target.saturating_sub(overhead(num)).max(n_slices);
                tp.slice_bytes_numerator = num;
                tp.slice_bytes_denominator = n_slices;
                let mut coder = SliceCoder::new(&tp, self.cfg.kind, [&y, &c1, &c2]);
                let mut slices = Vec::with_capacity(n_slices as usize);
                for sy in 0..tp.slices_y {
                    for sx in 0..tp.slices_x {
                        slices.push(code_ld_slice(&mut coder, sx, sy, None)?.bytes);
                    }
                }
                Ok((tp, slices))
            }
            PictureKind::HighQuality => {
                loop {
                    let budget = self.target_picture_bytes.map(|target| {
                        let mut w = BitWriter::new();
                        write_transform_parameters(
                            &mut w,
                            &tp,
                            self.cfg.kind,
                            self.major,
                            self.custom_matrix,
                        );
                        let overhead = PARSE_INFO_LEN as u64 + 4 + w.into_bytes().len() as u64;
                        target.saturating_sub(overhead)
                    });
                    let mut coder = SliceCoder::new(&tp, self.cfg.kind, [&y, &c1, &c2]);
                    let mut slices = Vec::with_capacity(n_slices as usize);
                    let mut max_component = 0u64;
                    for sy in 0..tp.slices_y {
                        for sx in 0..tp.slices_x {
                            // Even split with the remainder spread over the
                            // first slices, so the picture total is exact.
                            let idx = sy * tp.slices_x + sx;
                            let slice_budget =
                                budget.map(|b| b / n_slices + u64::from(idx < b % n_slices));
                            let coded = code_hq_slice(&mut coder, sx, sy, fixed_q, slice_budget);
                            max_component = max_component.max(coded.max_component_bytes);
                            slices.push(coded.bytes);
                        }
                    }
                    if max_component <= 255 * tp.slice_size_scaler {
                        return Ok((tp, slices));
                    }
                    // A component outgrew its one-byte length code: raise
                    // the scaler to the smallest value that fits and redo
                    // the picture with it.
                    tp.slice_size_scaler = max_component.div_ceil(255);
                }
            }
        }
    }
}

/// `parse_info()` (§10.5.1) written.
fn write_parse_info(out: &mut Vec<u8>, parse_code: u8, next: u32, prev: u32) {
    out.extend_from_slice(&PARSE_INFO_PREFIX);
    out.push(parse_code);
    out.extend_from_slice(&next.to_be_bytes());
    out.extend_from_slice(&prev.to_be_bytes());
}

/// `fragment_header()` (§14.2) written. `fragment_data_length` carries
/// the payload byte count when it fits the field (the standard leaves
/// the field's content undefined and decoders ignore it).
fn write_fragment_header(
    out: &mut Vec<u8>,
    picture_number: u32,
    data_len: usize,
    slice_count: u16,
    x_offset: u16,
    y_offset: u16,
) {
    out.extend_from_slice(&picture_number.to_be_bytes());
    let data_len = u16::try_from(data_len).unwrap_or(0);
    out.extend_from_slice(&data_len.to_be_bytes());
    out.extend_from_slice(&slice_count.to_be_bytes());
    if slice_count != 0 {
        out.extend_from_slice(&x_offset.to_be_bytes());
        out.extend_from_slice(&y_offset.to_be_bytes());
    }
}

/// Encode a whole sequence in one call: sequence header, every picture,
/// end-of-sequence.
pub fn encode_sequence(cfg: EncoderConfig, pictures: &[PictureInput]) -> Result<Vec<u8>> {
    let mut enc = SequenceEncoder::new(cfg)?;
    let mut out = Vec::new();
    for p in pictures {
        out.extend_from_slice(&enc.encode_picture(p)?);
    }
    if !enc.in_sequence() {
        // A picture-less sequence is still a valid sequence (§10.3): open
        // it with the header so the end-of-sequence unit has a partner.
        out.extend_from_slice(&enc.sequence_header_unit());
        enc.in_sequence = true;
        enc.prev_unit_len = PARSE_INFO_LEN + enc.seq_header_body.len() as u32;
    }
    out.extend_from_slice(&enc.end_sequence());
    Ok(out)
}
