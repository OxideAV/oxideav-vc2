//! VC-2 inverse quantization (SMPTE ST 2042-1:2022 §13.3) and the default
//! quantization matrices of Annex D.
//!
//! [`quant_factor`] / [`quant_offset`] implement §13.3.2 exactly;
//! [`inverse_quant`] implements §13.3.1. The default matrices are the
//! symmetric (`dwt_depth_ho == 0`) column of Tables D.1–D.7 — the
//! perceptually-unweighted matrices used when `custom_quant_matrix` is
//! false. Custom matrices read from the stream (§12.4.5.3) are fully
//! supported by the parser and do not consult this table.

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

/// Default symmetric quantization matrix for `wavelet_index` (Tables
/// D.1–D.7, the `dwt_depth_ho == 0` block) at a given `dwt_depth`.
///
/// Returns a vector indexed by level: index 0 is the DC (LL) entry, and
/// indices 1..=dwt_depth are the AC triples. Returns `None` when no default
/// matrix is defined (a custom matrix is then mandatory per §12.4.5.3).
pub fn default_quant_matrix(wavelet_index: u64, dwt_depth: u64) -> Option<Vec<MatrixLevel>> {
    if dwt_depth > 4 || wavelet_index > 6 {
        return None;
    }
    let mut out = Vec::with_capacity(dwt_depth as usize + 1);
    // Level 0 (LL): the per-depth DC value.
    out.push(MatrixLevel::Ll(ll_value(wavelet_index, dwt_depth)));
    for level in 1..=dwt_depth {
        let (hl, lh, hh) = ac_value(wavelet_index, dwt_depth, level)?;
        out.push(MatrixLevel::Ac { hl, lh, hh });
    }
    Some(out)
}

/// Public per-level default-matrix entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixLevel {
    /// Level-0 DC band quantizer matrix value.
    Ll(i64),
    /// AC level (HL, LH, HH) quantizer matrix values.
    Ac { hl: i64, lh: i64, hh: i64 },
}

/// Level-0 LL default value as a function of (filter, depth) — Tables
/// D.1–D.7, top row of the `dwt_depth_ho == 0` block.
fn ll_value(wavelet_index: u64, dwt_depth: u64) -> i64 {
    // Columns are dwt_depth = 0,1,2,3,4.
    let row: [i64; 5] = match wavelet_index {
        0 => [0, 5, 5, 5, 5],    // D.1 DD(9,7)
        1 => [0, 4, 4, 4, 4],    // D.2 LeGall(5,3)
        2 => [0, 5, 5, 5, 5],    // D.3 DD(13,7)
        3 => [0, 8, 12, 16, 20], // D.4 Haar no shift
        4 => [0, 8, 8, 8, 8],    // D.5 Haar single shift
        5 => [0, 0, 0, 0, 0],    // D.6 Fidelity
        6 => [0, 3, 3, 3, 3],    // D.7 Daubechies(9,7)
        _ => [0; 5],
    };
    row[dwt_depth as usize]
}

/// AC default (HL, LH, HH) triple for (filter, depth, level) — Tables
/// D.1–D.7, the `dwt_depth_ho == 0` block. `level` runs 1..=dwt_depth.
fn ac_value(wavelet_index: u64, dwt_depth: u64, level: u64) -> Option<(i64, i64, i64)> {
    // Per-filter (depth-column, level-row) AC triples for the symmetric
    // block. Each table maps level -> triple, and a triple is constant
    // across depth columns once the level exists (the same diagonal repeats
    // down the depth columns in Annex D), so the triple depends only on
    // (filter, level) within the symmetric block — except Haar-no-shift
    // (index 3) whose triples grow with depth.
    let d = dwt_depth as usize;
    let l = level as usize;
    let triple = match wavelet_index {
        0 => DD_9_7_AC.get(l),              // D.1
        1 => LEGALL_AC.get(l),              // D.2
        2 => DD_13_7_AC.get(l),             // D.3
        4 => HAAR_SS_AC.get(l),             // D.5
        5 => FIDELITY_AC.get(l),            // D.6
        6 => DAUB_AC.get(l),                // D.7
        3 => return haar_no_shift_ac(d, l), // D.4 grows with depth
        _ => None,
    };
    triple.copied()
}

// Symmetric-block AC triples, indexed by level (1..=4). Index 0 is a
// placeholder for the LL row.
const DD_9_7_AC: AcTable = AcTable(&[(0, 0, 0), (3, 3, 0), (4, 4, 1), (5, 5, 2), (6, 6, 3)]);
const LEGALL_AC: AcTable = AcTable(&[(0, 0, 0), (2, 2, 0), (4, 4, 2), (5, 5, 3), (7, 7, 5)]);
const DD_13_7_AC: AcTable = AcTable(&[(0, 0, 0), (3, 3, 0), (4, 4, 1), (5, 5, 2), (6, 6, 3)]);
const HAAR_SS_AC: AcTable = AcTable(&[(0, 0, 0), (4, 4, 0), (4, 4, 0), (4, 4, 0), (4, 4, 0)]);
const FIDELITY_AC: AcTable =
    AcTable(&[(0, 0, 0), (4, 4, 8), (8, 8, 12), (13, 13, 17), (17, 17, 21)]);
const DAUB_AC: AcTable = AcTable(&[(0, 0, 0), (1, 1, 0), (4, 4, 2), (6, 6, 5), (9, 9, 7)]);

struct AcTable(&'static [(i64, i64, i64)]);
impl AcTable {
    fn get(&self, level: usize) -> Option<&'static (i64, i64, i64)> {
        self.0.get(level)
    }
}

/// Haar-with-no-shift (Table D.4) AC triples for the symmetric block. Unlike
/// the other filters the triples here depend on both depth column and level:
/// the value family is `4*k, 4*k, max(4*(k-1),0)` reading down the diagonal.
fn haar_no_shift_ac(dwt_depth: usize, level: usize) -> Option<(i64, i64, i64)> {
    // Table D.4 dwt_depth_ho=0 block, columns dwt_depth 1..4, rows level 1..4.
    //  depth=1: L1=(4,4,0)
    //  depth=2: L1=(8,8,4)  L2=(4,4,0)
    //  depth=3: L1=(12,12,8) L2=(8,8,4) L3=(4,4,0)
    //  depth=4: L1=(16,16,12) L2=(12,12,8) L3=(8,8,4) L4=(4,4,0)
    // i.e. value scales with (dwt_depth - level): k = dwt_depth - level + 1.
    if level == 0 || level > dwt_depth {
        return None;
    }
    let k = (dwt_depth - level) as i64; // 0 at the finest existing level
    let main = 4 * (k + 1);
    let hh = 4 * k;
    Some((main, main, hh))
}

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
}
