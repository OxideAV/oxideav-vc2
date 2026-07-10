//! VC-2 inverse quantization (SMPTE ST 2042-1:2022 §13.3) and the default
//! quantization matrices of Annex D.
//!
//! [`quant_factor`] / [`quant_offset`] implement §13.3.2 exactly;
//! [`inverse_quant`] implements §13.3.1. The default matrices are the
//! complete Annex D set — Tables D.1–D.7 for the symmetric filter pairs
//! (every `dwt_depth_ho` block, not just the `dwt_depth_ho == 0` column)
//! plus Table D.8 for the one mixed pair the annex defines
//! (`wavelet_index == 3` Haar-no-shift with `wavelet_index_ho == 1`
//! LeGall). Combinations outside the tables (depths above 4, total depth
//! above 5, any other mixed pair) have no default and require a custom
//! matrix in the stream per §12.4.5.3. Custom matrices read from the
//! stream are fully supported by the parser and do not consult this table.

/// `quant_factor(index)` (§13.3.2).
#[inline]
pub fn quant_factor(index: u64) -> i64 {
    let base: i64 = 1i64 << (index / 4);
    match index % 4 {
        0 => 4 * base,
        1 => ((503829 * base) + 52958) / 105917,
        2 => ((665857 * base) + 58854) / 117708,
        3 => ((440253 * base) + 32722) / 65444,
        _ => unreachable!(),
    }
}

/// `quant_offset(index)` (§13.3.2).
#[inline]
pub fn quant_offset(index: u64) -> i64 {
    match index {
        0 => 1,
        1 => 2,
        _ => (quant_factor(index) + 1) / 2,
    }
}

/// `inverse_quant(quantized_coeff, quant_index)` (§13.3.1).
///
/// Dead-zone inverse quantization: scale the magnitude by the quant factor,
/// add the offset and a rounding `+2`, divide by 4, then re-apply the sign.
#[inline]
pub fn inverse_quant(quantized_coeff: i64, quant_index: u64) -> i64 {
    let mut magnitude = quantized_coeff.abs();
    if magnitude != 0 {
        magnitude *= quant_factor(quant_index);
        magnitude += quant_offset(quant_index);
        magnitude += 2;
        magnitude /= 4;
    }
    let sign = match quantized_coeff.cmp(&0) {
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
    };
    sign * magnitude
}

/// Orientation key within the default-matrix table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orient {
    LL,
    HL,
    LH,
    HH,
}

/// Public per-level default-matrix entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixLevel {
    /// Level-0 DC band quantizer matrix value — the LL band when
    /// `dwt_depth_ho == 0`, the L band otherwise.
    Ll(i64),
    /// Horizontal-only level (H band) quantizer matrix value, used for
    /// levels `1..=dwt_depth_ho` of an asymmetric transform.
    H(i64),
    /// 2-D AC level (HL, LH, HH) quantizer matrix values.
    Ac { hl: i64, lh: i64, hh: i64 },
}

/// Default symmetric quantization matrix for `wavelet_index` (Tables
/// D.1–D.7, the `dwt_depth_ho == 0` block) at a given `dwt_depth`.
///
/// Convenience wrapper over [`default_quant_matrix_full`] with
/// `wavelet_index_ho == wavelet_index` and `dwt_depth_ho == 0`.
pub fn default_quant_matrix(wavelet_index: u64, dwt_depth: u64) -> Option<Vec<MatrixLevel>> {
    default_quant_matrix_full(wavelet_index, wavelet_index, dwt_depth, 0)
}

/// Default quantization matrix per Annex D for the full parameter set
/// signalled by §12.4 — including the asymmetric (`dwt_depth_ho > 0`)
/// blocks of Tables D.1–D.7 and the mixed Haar-no-shift / LeGall pair of
/// Table D.8.
///
/// Returns a vector indexed by level: index 0 is the DC entry (LL or L),
/// indices `1..=dwt_depth_ho` are the horizontal-only H entries, and the
/// remaining `dwt_depth` indices are the 2-D AC triples. Returns `None`
/// when Annex D defines no default for the combination — the stream must
/// then carry a custom matrix (§12.4.5.3 lists the cases where
/// `custom_quant_matrix` "shall be set to True").
pub fn default_quant_matrix_full(
    wavelet_index: u64,
    wavelet_index_ho: u64,
    dwt_depth: u64,
    dwt_depth_ho: u64,
) -> Option<Vec<MatrixLevel>> {
    let table = annex_d_table(wavelet_index, wavelet_index_ho)?;
    let (_, _, dc, h, ac) = *table
        .iter()
        .find(|&&(ho, d, ..)| ho as u64 == dwt_depth_ho && d as u64 == dwt_depth)?;
    debug_assert_eq!(h.len() as u64, dwt_depth_ho);
    debug_assert_eq!(ac.len() as u64, dwt_depth);
    let mut out = Vec::with_capacity(1 + h.len() + ac.len());
    out.push(MatrixLevel::Ll(dc));
    out.extend(h.iter().map(|&v| MatrixLevel::H(v)));
    out.extend(
        ac.iter()
            .map(|&(hl, lh, hh)| MatrixLevel::Ac { hl, lh, hh }),
    );
    Some(out)
}

/// One `(dwt_depth_ho, dwt_depth)` cell of an Annex D table: the level-0
/// DC value, the H values for levels `1..=dwt_depth_ho`, and the
/// (HL, LH, HH) triples for the 2-D levels. Each cell transcribes one
/// column of one `dwt_depth_ho` block, read top to bottom.
type Cell = (u8, u8, i64, &'static [i64], &'static [(i64, i64, i64)]);

/// Table selector: Annex D defines defaults only for the seven symmetric
/// pairs (D.1–D.7) and the mixed `wavelet_index == 3` (Haar no shift) /
/// `wavelet_index_ho == 1` (LeGall) pair (D.8).
fn annex_d_table(wavelet_index: u64, wavelet_index_ho: u64) -> Option<&'static [Cell]> {
    match (wavelet_index, wavelet_index_ho) {
        (0, 0) => Some(TABLE_D1),
        (1, 1) => Some(TABLE_D2),
        (2, 2) => Some(TABLE_D3),
        (3, 3) => Some(TABLE_D4),
        (4, 4) => Some(TABLE_D5),
        (5, 5) => Some(TABLE_D6),
        (6, 6) => Some(TABLE_D7),
        (3, 1) => Some(TABLE_D8),
        _ => None,
    }
}

// Table D.1 — Deslauriers-Dubuc (9,7), both dimensions.
#[rustfmt::skip]
const TABLE_D1: &[Cell] = &[
    (0, 0, 0, &[], &[]),
    (0, 1, 5, &[], &[(3, 3, 0)]),
    (0, 2, 5, &[], &[(3, 3, 0), (4, 4, 1)]),
    (0, 3, 5, &[], &[(3, 3, 0), (4, 4, 1), (5, 5, 2)]),
    (0, 4, 5, &[], &[(3, 3, 0), (4, 4, 1), (5, 5, 2), (6, 6, 3)]),
    (1, 0, 3, &[0], &[]),
    (1, 1, 3, &[0], &[(3, 3, 0)]),
    (1, 2, 3, &[0], &[(3, 3, 0), (4, 4, 1)]),
    (1, 3, 3, &[0], &[(3, 3, 0), (4, 4, 1), (5, 5, 2)]),
    (1, 4, 3, &[0], &[(3, 3, 0), (4, 4, 1), (5, 5, 2), (6, 6, 3)]),
    (2, 0, 3, &[0, 3], &[]),
    (2, 1, 3, &[0, 3], &[(5, 5, 3)]),
    (2, 2, 3, &[0, 3], &[(5, 5, 3), (6, 6, 4)]),
    (2, 3, 3, &[0, 3], &[(5, 5, 3), (6, 6, 4), (7, 7, 5)]),
    (3, 0, 3, &[0, 3, 5], &[]),
    (3, 1, 3, &[0, 3, 5], &[(8, 8, 5)]),
    (3, 2, 3, &[0, 3, 5], &[(8, 8, 5), (9, 9, 6)]),
    (4, 0, 3, &[0, 3, 5, 8], &[]),
    (4, 1, 3, &[0, 3, 5, 8], &[(10, 10, 8)]),
];

// Table D.2 — LeGall (5,3), both dimensions.
#[rustfmt::skip]
const TABLE_D2: &[Cell] = &[
    (0, 0, 0, &[], &[]),
    (0, 1, 4, &[], &[(2, 2, 0)]),
    (0, 2, 4, &[], &[(2, 2, 0), (4, 4, 2)]),
    (0, 3, 4, &[], &[(2, 2, 0), (4, 4, 2), (5, 5, 3)]),
    (0, 4, 4, &[], &[(2, 2, 0), (4, 4, 2), (5, 5, 3), (7, 7, 5)]),
    (1, 0, 2, &[0], &[]),
    (1, 1, 2, &[0], &[(3, 3, 1)]),
    (1, 2, 2, &[0], &[(3, 3, 1), (4, 4, 2)]),
    (1, 3, 2, &[0], &[(3, 3, 1), (4, 4, 2), (6, 6, 4)]),
    (1, 4, 2, &[0], &[(3, 3, 1), (4, 4, 2), (6, 6, 4), (8, 8, 6)]),
    (2, 0, 2, &[0, 3], &[]),
    (2, 1, 2, &[0, 3], &[(6, 6, 4)]),
    (2, 2, 2, &[0, 3], &[(6, 6, 4), (7, 7, 5)]),
    (2, 3, 2, &[0, 3], &[(6, 6, 4), (7, 7, 5), (9, 9, 7)]),
    (3, 0, 2, &[0, 3, 6], &[]),
    (3, 1, 2, &[0, 3, 6], &[(8, 8, 6)]),
    (3, 2, 2, &[0, 3, 6], &[(8, 8, 6), (10, 10, 8)]),
    (4, 0, 2, &[0, 3, 6, 8], &[]),
    (4, 1, 2, &[0, 3, 6, 8], &[(11, 11, 9)]),
];

// Table D.3 — Deslauriers-Dubuc (13,7), both dimensions.
#[rustfmt::skip]
const TABLE_D3: &[Cell] = &[
    (0, 0, 0, &[], &[]),
    (0, 1, 5, &[], &[(3, 3, 0)]),
    (0, 2, 5, &[], &[(3, 3, 0), (4, 4, 1)]),
    (0, 3, 5, &[], &[(3, 3, 0), (4, 4, 1), (5, 5, 2)]),
    (0, 4, 5, &[], &[(3, 3, 0), (4, 4, 1), (5, 5, 2), (6, 6, 3)]),
    (1, 0, 3, &[0], &[]),
    (1, 1, 3, &[0], &[(3, 3, 0)]),
    (1, 2, 3, &[0], &[(3, 3, 0), (4, 4, 1)]),
    (1, 3, 3, &[0], &[(3, 3, 0), (4, 4, 1), (5, 5, 2)]),
    (1, 4, 3, &[0], &[(3, 3, 0), (4, 4, 1), (5, 5, 2), (6, 6, 3)]),
    (2, 0, 3, &[0, 3], &[]),
    (2, 1, 3, &[0, 3], &[(5, 5, 2)]),
    (2, 2, 3, &[0, 3], &[(5, 5, 2), (6, 6, 4)]),
    (2, 3, 3, &[0, 3], &[(5, 5, 2), (6, 6, 4), (7, 7, 5)]),
    (3, 0, 3, &[0, 3, 5], &[]),
    (3, 1, 3, &[0, 3, 5], &[(8, 8, 5)]),
    (3, 2, 3, &[0, 3, 5], &[(8, 8, 5), (9, 9, 6)]),
    (4, 0, 3, &[0, 3, 5, 8], &[]),
    (4, 1, 3, &[0, 3, 5, 8], &[(10, 10, 8)]),
];

// Table D.4 — Haar with no shift, both dimensions. Values here depend on
// the dwt_depth column as well as the level, so every cell is spelled out.
#[rustfmt::skip]
const TABLE_D4: &[Cell] = &[
    (0, 0, 0, &[], &[]),
    (0, 1, 8, &[], &[(4, 4, 0)]),
    (0, 2, 12, &[], &[(8, 8, 4), (4, 4, 0)]),
    (0, 3, 16, &[], &[(12, 12, 8), (8, 8, 4), (4, 4, 0)]),
    (0, 4, 20, &[], &[(16, 16, 12), (12, 12, 8), (8, 8, 4), (4, 4, 0)]),
    (1, 0, 4, &[0], &[]),
    (1, 1, 10, &[6], &[(4, 4, 0)]),
    (1, 2, 14, &[10], &[(8, 8, 4), (4, 4, 0)]),
    (1, 3, 18, &[14], &[(12, 12, 8), (8, 8, 4), (4, 4, 0)]),
    (1, 4, 22, &[18], &[(16, 16, 12), (12, 12, 8), (8, 8, 4), (4, 4, 0)]),
    (2, 0, 6, &[2, 0], &[]),
    (2, 1, 12, &[8, 6], &[(4, 4, 0)]),
    (2, 2, 16, &[12, 10], &[(8, 8, 4), (4, 4, 0)]),
    (2, 3, 20, &[16, 14], &[(12, 12, 8), (8, 8, 4), (4, 4, 0)]),
    (3, 0, 8, &[4, 2, 0], &[]),
    (3, 1, 14, &[10, 8, 6], &[(4, 4, 0)]),
    (3, 2, 18, &[14, 12, 10], &[(8, 8, 4), (4, 4, 0)]),
    (4, 0, 10, &[6, 4, 2, 0], &[]),
    (4, 1, 16, &[12, 10, 8, 6], &[(4, 4, 0)]),
];

// Table D.5 — Haar with single shift per level, both dimensions.
#[rustfmt::skip]
const TABLE_D5: &[Cell] = &[
    (0, 0, 0, &[], &[]),
    (0, 1, 8, &[], &[(4, 4, 0)]),
    (0, 2, 8, &[], &[(4, 4, 0), (4, 4, 0)]),
    (0, 3, 8, &[], &[(4, 4, 0), (4, 4, 0), (4, 4, 0)]),
    (0, 4, 8, &[], &[(4, 4, 0), (4, 4, 0), (4, 4, 0), (4, 4, 0)]),
    (1, 0, 4, &[0], &[]),
    (1, 1, 6, &[2], &[(4, 4, 0)]),
    (1, 2, 6, &[2], &[(4, 4, 0), (4, 4, 0)]),
    (1, 3, 6, &[2], &[(4, 4, 0), (4, 4, 0), (4, 4, 0)]),
    (1, 4, 6, &[2], &[(4, 4, 0), (4, 4, 0), (4, 4, 0), (4, 4, 0)]),
    (2, 0, 4, &[0, 2], &[]),
    (2, 1, 4, &[0, 2], &[(4, 4, 0)]),
    (2, 2, 4, &[0, 2], &[(4, 4, 0), (4, 4, 0)]),
    (2, 3, 4, &[0, 2], &[(4, 4, 0), (4, 4, 0), (4, 4, 0)]),
    (3, 0, 4, &[0, 2, 4], &[]),
    (3, 1, 4, &[0, 2, 4], &[(6, 6, 2)]),
    (3, 2, 4, &[0, 2, 4], &[(6, 6, 2), (6, 6, 2)]),
    (4, 0, 4, &[0, 2, 4, 6], &[]),
    (4, 1, 4, &[0, 2, 4, 6], &[(8, 8, 4)]),
];

// Table D.6 — Fidelity, both dimensions. (The annex notes these values do
// not correctly compensate the filter gain but are kept for compatibility;
// Table D.9's corrected values are informative custom-matrix suggestions
// and are not defaults.)
#[rustfmt::skip]
const TABLE_D6: &[Cell] = &[
    (0, 0, 0, &[], &[]),
    (0, 1, 0, &[], &[(4, 4, 8)]),
    (0, 2, 0, &[], &[(4, 4, 8), (8, 8, 12)]),
    (0, 3, 0, &[], &[(4, 4, 8), (8, 8, 12), (13, 13, 17)]),
    (0, 4, 0, &[], &[(4, 4, 8), (8, 8, 12), (13, 13, 17), (17, 17, 21)]),
    (1, 0, 0, &[4], &[]),
    (1, 1, 0, &[4], &[(6, 6, 10)]),
    (1, 2, 0, &[4], &[(6, 6, 10), (11, 11, 15)]),
    (1, 3, 0, &[4], &[(6, 6, 10), (11, 11, 15), (15, 15, 19)]),
    (1, 4, 0, &[4], &[(6, 6, 10), (11, 11, 15), (15, 15, 19), (19, 19, 23)]),
    (2, 0, 0, &[4, 6], &[]),
    (2, 1, 0, &[4, 6], &[(8, 8, 12)]),
    (2, 2, 0, &[4, 6], &[(8, 8, 12), (13, 13, 17)]),
    (2, 3, 0, &[4, 6], &[(8, 8, 12), (13, 13, 17), (17, 17, 21)]),
    (3, 0, 0, &[4, 6, 8], &[]),
    (3, 1, 0, &[4, 6, 8], &[(11, 11, 15)]),
    (3, 2, 0, &[4, 6, 8], &[(11, 11, 15), (15, 15, 19)]),
    (4, 0, 0, &[4, 6, 8, 11], &[]),
    (4, 1, 0, &[4, 6, 8, 11], &[(13, 13, 17)]),
];

// Table D.7 — Daubechies (9,7), both dimensions.
#[rustfmt::skip]
const TABLE_D7: &[Cell] = &[
    (0, 0, 0, &[], &[]),
    (0, 1, 3, &[], &[(1, 1, 0)]),
    (0, 2, 3, &[], &[(1, 1, 0), (4, 4, 2)]),
    (0, 3, 3, &[], &[(1, 1, 0), (4, 4, 2), (6, 6, 5)]),
    (0, 4, 3, &[], &[(1, 1, 0), (4, 4, 2), (6, 6, 5), (9, 9, 7)]),
    (1, 0, 1, &[0], &[]),
    (1, 1, 1, &[0], &[(3, 3, 2)]),
    (1, 2, 1, &[0], &[(3, 3, 2), (6, 6, 4)]),
    (1, 3, 1, &[0], &[(3, 3, 2), (6, 6, 4), (8, 8, 7)]),
    (1, 4, 1, &[0], &[(3, 3, 2), (6, 6, 4), (8, 8, 7), (11, 11, 9)]),
    (2, 0, 1, &[0, 3], &[]),
    (2, 1, 1, &[0, 3], &[(6, 6, 5)]),
    (2, 2, 1, &[0, 3], &[(6, 6, 5), (9, 9, 8)]),
    (2, 3, 1, &[0, 3], &[(6, 6, 5), (9, 9, 8), (11, 11, 10)]),
    (3, 0, 1, &[0, 3, 6], &[]),
    (3, 1, 1, &[0, 3, 6], &[(10, 10, 8)]),
    (3, 2, 1, &[0, 3, 6], &[(10, 10, 8), (12, 12, 11)]),
    (4, 0, 1, &[0, 3, 6, 10], &[]),
    (4, 1, 1, &[0, 3, 6, 10], &[(13, 13, 12)]),
];

// Table D.8 — the one mixed pair with defaults: wavelet_index == 3
// (Haar with no shift) vertically with wavelet_index_ho == 1 (LeGall)
// horizontally. Note the HL != LH asymmetry in the AC triples.
#[rustfmt::skip]
const TABLE_D8: &[Cell] = &[
    (0, 0, 0, &[], &[]),
    (0, 1, 6, &[], &[(4, 2, 0)]),
    (0, 2, 6, &[], &[(4, 2, 0), (5, 3, 1)]),
    (0, 3, 6, &[], &[(4, 2, 0), (5, 3, 1), (6, 4, 2)]),
    (0, 4, 6, &[], &[(4, 2, 0), (5, 3, 1), (6, 4, 2), (6, 5, 2)]),
    (1, 0, 2, &[0], &[]),
    (1, 1, 3, &[1], &[(4, 2, 0)]),
    (1, 2, 3, &[1], &[(4, 2, 0), (5, 3, 1)]),
    (1, 3, 3, &[1], &[(4, 2, 0), (5, 3, 1), (6, 4, 2)]),
    (1, 4, 3, &[1], &[(4, 2, 0), (5, 3, 1), (6, 4, 2), (6, 5, 2)]),
    (2, 0, 2, &[0, 3], &[]),
    (2, 1, 2, &[0, 3], &[(6, 4, 2)]),
    (2, 2, 2, &[0, 3], &[(6, 4, 2), (6, 5, 2)]),
    (2, 3, 2, &[0, 3], &[(6, 4, 2), (6, 5, 2), (7, 5, 3)]),
    (3, 0, 2, &[0, 3, 6], &[]),
    (3, 1, 2, &[0, 3, 6], &[(8, 7, 4)]),
    (3, 2, 2, &[0, 3, 6], &[(8, 7, 4), (9, 7, 5)]),
    (4, 0, 2, &[0, 3, 6, 8], &[]),
    (4, 1, 2, &[0, 3, 6, 8], &[(11, 9, 7)]),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quant_factor_index_zero() {
        // index 0: base = 1, 4*base = 4.
        assert_eq!(quant_factor(0), 4);
    }

    #[test]
    fn quant_offset_specials() {
        assert_eq!(quant_offset(0), 1);
        assert_eq!(quant_offset(1), 2);
        // index 4: factor = 4*2 = 8 -> offset = (8+1)/2 = 4.
        assert_eq!(quant_offset(4), 4);
    }

    #[test]
    fn inverse_quant_zero_passthrough() {
        assert_eq!(inverse_quant(0, 5), 0);
    }

    #[test]
    fn inverse_quant_index_zero_is_near_identity() {
        // index 0: factor 4, offset 1. magnitude = (|c|*4 + 1 + 2)/4.
        // for c = 7 -> (28 + 3)/4 = 7.
        assert_eq!(inverse_quant(7, 0), 7);
        assert_eq!(inverse_quant(-7, 0), -7);
    }

    #[test]
    fn legall_default_depth2() {
        // Table D.2 LeGall dwt_depth_ho=0, dwt_depth=2:
        // L0 LL = 4; L1 = (2,2,0); L2 = (4,4,2).
        let m = default_quant_matrix(1, 2).unwrap();
        assert_eq!(m[0], MatrixLevel::Ll(4));
        assert_eq!(
            m[1],
            MatrixLevel::Ac {
                hl: 2,
                lh: 2,
                hh: 0
            }
        );
        assert_eq!(
            m[2],
            MatrixLevel::Ac {
                hl: 4,
                lh: 4,
                hh: 2
            }
        );
    }

    #[test]
    fn haar_no_shift_default_depth3() {
        // Table D.4 dwt_depth_ho=0, dwt_depth=3:
        // L0 LL = 16; L1 = (12,12,8); L2 = (8,8,4); L3 = (4,4,0).
        let m = default_quant_matrix(3, 3).unwrap();
        assert_eq!(m[0], MatrixLevel::Ll(16));
        assert_eq!(
            m[1],
            MatrixLevel::Ac {
                hl: 12,
                lh: 12,
                hh: 8
            }
        );
        assert_eq!(
            m[2],
            MatrixLevel::Ac {
                hl: 8,
                lh: 8,
                hh: 4
            }
        );
        assert_eq!(
            m[3],
            MatrixLevel::Ac {
                hl: 4,
                lh: 4,
                hh: 0
            }
        );
    }

    #[test]
    fn depth_beyond_four_has_no_default() {
        assert!(default_quant_matrix(1, 5).is_none());
    }

    #[test]
    fn every_table_has_all_nineteen_cells() {
        // Each Annex D table covers exactly the (ho, depth) pairs with
        // ho <= 4, depth <= 4 and ho + depth <= 5: 5+5+4+3+2 = 19 cells,
        // and each cell's slice lengths match its declared depths.
        for (wi, wi_ho) in [
            (0, 0),
            (1, 1),
            (2, 2),
            (3, 3),
            (4, 4),
            (5, 5),
            (6, 6),
            (3, 1),
        ] {
            for ho in 0..=5u64 {
                for depth in 0..=5u64 {
                    let m = default_quant_matrix_full(wi, wi_ho, depth, ho);
                    let in_annex = ho <= 4 && depth <= 4 && ho + depth <= 5;
                    assert_eq!(
                        m.is_some(),
                        in_annex,
                        "pair ({wi},{wi_ho}) ho {ho} depth {depth}"
                    );
                    if let Some(m) = m {
                        assert_eq!(m.len() as u64, 1 + ho + depth);
                        assert!(matches!(m[0], MatrixLevel::Ll(_)));
                        for l in 1..=ho {
                            assert!(matches!(m[l as usize], MatrixLevel::H(_)));
                        }
                        for l in (ho + 1)..=(ho + depth) {
                            assert!(matches!(m[l as usize], MatrixLevel::Ac { .. }));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn legall_asymmetric_ho2_depth1() {
        // Table D.2, dwt_depth_ho=2 block, dwt_depth=1 column:
        // L = 2; H1 = 0; H2 = 3; level 3 = (6, 6, 4).
        let m = default_quant_matrix_full(1, 1, 1, 2).unwrap();
        assert_eq!(m[0], MatrixLevel::Ll(2));
        assert_eq!(m[1], MatrixLevel::H(0));
        assert_eq!(m[2], MatrixLevel::H(3));
        assert_eq!(
            m[3],
            MatrixLevel::Ac {
                hl: 6,
                lh: 6,
                hh: 4
            }
        );
    }

    #[test]
    fn haar_no_shift_asymmetric_grows_with_depth() {
        // Table D.4, dwt_depth_ho=1 block: the L / H values grow with the
        // dwt_depth column (unlike every other filter's blocks).
        let d0 = default_quant_matrix_full(3, 3, 0, 1).unwrap();
        assert_eq!(d0[0], MatrixLevel::Ll(4));
        assert_eq!(d0[1], MatrixLevel::H(0));
        let d2 = default_quant_matrix_full(3, 3, 2, 1).unwrap();
        assert_eq!(d2[0], MatrixLevel::Ll(14));
        assert_eq!(d2[1], MatrixLevel::H(10));
        assert_eq!(
            d2[2],
            MatrixLevel::Ac {
                hl: 8,
                lh: 8,
                hh: 4
            }
        );
        assert_eq!(
            d2[3],
            MatrixLevel::Ac {
                hl: 4,
                lh: 4,
                hh: 0
            }
        );
    }

    #[test]
    fn mixed_haar_legall_pair_table_d8() {
        // Table D.8 (wavelet_index 3, wavelet_index_ho 1) — the only mixed
        // pair with defaults. Its AC triples are HL/LH-asymmetric.
        let m = default_quant_matrix_full(3, 1, 2, 1).unwrap();
        assert_eq!(m[0], MatrixLevel::Ll(3));
        assert_eq!(m[1], MatrixLevel::H(1));
        assert_eq!(
            m[2],
            MatrixLevel::Ac {
                hl: 4,
                lh: 2,
                hh: 0
            }
        );
        assert_eq!(
            m[3],
            MatrixLevel::Ac {
                hl: 5,
                lh: 3,
                hh: 1
            }
        );
        // The reverse pairing (LeGall 2-D with Haar horizontal) has no
        // default, nor does any other mixed combination.
        assert!(default_quant_matrix_full(1, 3, 2, 1).is_none());
        assert!(default_quant_matrix_full(3, 4, 1, 1).is_none());
    }

    #[test]
    fn total_depth_over_five_has_no_default() {
        // ho=2, depth=4: both within 0..=4 but the sum exceeds 5.
        assert!(default_quant_matrix_full(1, 1, 4, 2).is_none());
        // ho=1, depth=4 sums to exactly 5 and is defined.
        assert!(default_quant_matrix_full(1, 1, 4, 1).is_some());
    }
}
