//! VC-2 inverse discrete wavelet transform (SMPTE ST 2042-1:2022 §15.4).
//!
//! Implements the integer lifting synthesis filters (§15.4.4.1, the four
//! lift types and the per-filter lifting-stage parameter tables of
//! Tables 16–22), the one-dimensional synthesis (§15.4.4), horizontal
//! synthesis (§15.4.2), vertical-then-horizontal synthesis (§15.4.3) and
//! the overall component IDWT iteration (§15.4.1).

/// The four integer lifting operation types of §15.4.4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiftType {
    /// `A[2n] += (sum + rounding) >> S`
    Type1,
    /// `A[2n] -= (sum + rounding) >> S`
    Type2,
    /// `A[2n+1] += (sum + rounding) >> S`
    Type3,
    /// `A[2n+1] -= (sum + rounding) >> S`
    Type4,
}

/// One lifting stage: a filter type with its length `l`, offset `d`,
/// `taps`, and scale `s` (§15.4.4.1).
#[derive(Debug, Clone, Copy)]
pub struct LiftStage {
    pub kind: LiftType,
    pub s: u32,
    pub d: i32,
    pub taps: &'static [i64],
}

/// A complete wavelet filter: its ordered lifting stages and the trailing
/// `filter_bit_shift()` value (§15.4.4.3 Tables 16–22).
#[derive(Debug, Clone, Copy)]
pub struct WaveletFilter {
    pub stages: &'static [LiftStage],
    pub bit_shift: u32,
}

// Table 16 — Index 0: Deslauriers-Dubuc (9,7).
const FILTER_DD_9_7: WaveletFilter = WaveletFilter {
    stages: &[
        LiftStage {
            kind: LiftType::Type2,
            s: 2,
            d: 0,
            taps: &[1, 1],
        },
        LiftStage {
            kind: LiftType::Type3,
            s: 4,
            d: -1,
            taps: &[-1, 9, 9, -1],
        },
    ],
    bit_shift: 1,
};

// Table 17 — Index 1: LeGall (5,3).
const FILTER_LEGALL_5_3: WaveletFilter = WaveletFilter {
    stages: &[
        LiftStage {
            kind: LiftType::Type2,
            s: 2,
            d: 0,
            taps: &[1, 1],
        },
        LiftStage {
            kind: LiftType::Type3,
            s: 1,
            d: 0,
            taps: &[1, 1],
        },
    ],
    bit_shift: 1,
};

// Table 18 — Index 2: Deslauriers-Dubuc (13,7).
const FILTER_DD_13_7: WaveletFilter = WaveletFilter {
    stages: &[
        LiftStage {
            kind: LiftType::Type2,
            s: 5,
            d: -1,
            taps: &[-1, 9, 9, -1],
        },
        LiftStage {
            kind: LiftType::Type3,
            s: 4,
            d: -1,
            taps: &[-1, 9, 9, -1],
        },
    ],
    bit_shift: 1,
};

// Table 19 — Index 3: Haar with no shift.
const FILTER_HAAR_NO_SHIFT: WaveletFilter = WaveletFilter {
    stages: &[
        LiftStage {
            kind: LiftType::Type2,
            s: 1,
            d: 1,
            taps: &[1],
        },
        LiftStage {
            kind: LiftType::Type3,
            s: 0,
            d: 0,
            taps: &[1],
        },
    ],
    bit_shift: 0,
};

// Table 20 — Index 4: Haar with single shift.
const FILTER_HAAR_SINGLE_SHIFT: WaveletFilter = WaveletFilter {
    stages: &[
        LiftStage {
            kind: LiftType::Type2,
            s: 1,
            d: 1,
            taps: &[1],
        },
        LiftStage {
            kind: LiftType::Type3,
            s: 0,
            d: 0,
            taps: &[1],
        },
    ],
    bit_shift: 1,
};

// Table 21 — Index 5: Fidelity filter.
const FILTER_FIDELITY: WaveletFilter = WaveletFilter {
    stages: &[
        LiftStage {
            kind: LiftType::Type3,
            s: 8,
            d: -3,
            taps: &[-2, 10, -25, 81, 81, -25, 10, -2],
        },
        LiftStage {
            kind: LiftType::Type2,
            s: 8,
            d: -3,
            taps: &[-8, 21, -46, 161, 161, -46, 21, -8],
        },
    ],
    bit_shift: 0,
};

// Table 22 — Index 6: Daubechies (9,7) integer approximation.
const FILTER_DAUB_9_7: WaveletFilter = WaveletFilter {
    stages: &[
        LiftStage {
            kind: LiftType::Type2,
            s: 12,
            d: 0,
            taps: &[1817, 1817],
        },
        LiftStage {
            kind: LiftType::Type4,
            s: 12,
            d: 0,
            taps: &[3616, 3616],
        },
        LiftStage {
            kind: LiftType::Type1,
            s: 12,
            d: 0,
            taps: &[217, 217],
        },
        LiftStage {
            kind: LiftType::Type3,
            s: 12,
            d: 0,
            taps: &[6497, 6497],
        },
    ],
    bit_shift: 1,
};

/// Return the [`WaveletFilter`] for a `wavelet_index` (§12.4.2 Table 15).
///
/// Returns `None` for indices outside 0..=6.
pub fn wavelet_filter(index: u64) -> Option<WaveletFilter> {
    Some(match index {
        0 => FILTER_DD_9_7,
        1 => FILTER_LEGALL_5_3,
        2 => FILTER_DD_13_7,
        3 => FILTER_HAAR_NO_SHIFT,
        4 => FILTER_HAAR_SINGLE_SHIFT,
        5 => FILTER_FIDELITY,
        6 => FILTER_DAUB_9_7,
        _ => return None,
    })
}

/// Apply one lifting stage to a 1-D signal `a` (§15.4.4.1).
///
/// The lifting arithmetic wraps on overflow: the spec's integers are
/// unbounded and every well-formed stream stays far inside `i64`, but a
/// hostile stream can drive dequantized coefficients near `i64::MAX` —
/// wrapping keeps the (garbage-in, garbage-out) result deterministic and
/// panic-free, and the §15.5 clip bounds the final output.
fn apply_lift(a: &mut [i64], stage: &LiftStage) {
    let l = stage.taps.len() as i32;
    let d = stage.d;
    let s = stage.s;
    let len = a.len() as i32;
    let rounding: i64 = if s > 0 { 1 << (s - 1) } else { 0 };
    let half = a.len() / 2;
    for n in 0..half as i32 {
        let mut sum: i64 = 0;
        for i in d..(l + d) {
            // Type 1/2 read odd positions `2*(n+i)-1`; Type 3/4 read even
            // positions `2*(n+i)`. Edge handling differs accordingly.
            let pos = match stage.kind {
                LiftType::Type1 | LiftType::Type2 => {
                    let mut p = 2 * (n + i) - 1;
                    p = p.min(len - 1);
                    p = p.max(1);
                    p
                }
                LiftType::Type3 | LiftType::Type4 => {
                    let mut p = 2 * (n + i);
                    p = p.min(len - 2);
                    p = p.max(0);
                    p
                }
            };
            sum = sum.wrapping_add(stage.taps[(i - d) as usize].wrapping_mul(a[pos as usize]));
        }
        let delta = sum.wrapping_add(rounding) >> s;
        match stage.kind {
            LiftType::Type1 => a[(2 * n) as usize] = a[(2 * n) as usize].wrapping_add(delta),
            LiftType::Type2 => a[(2 * n) as usize] = a[(2 * n) as usize].wrapping_sub(delta),
            LiftType::Type3 => {
                a[(2 * n + 1) as usize] = a[(2 * n + 1) as usize].wrapping_add(delta)
            }
            LiftType::Type4 => {
                a[(2 * n + 1) as usize] = a[(2 * n + 1) as usize].wrapping_sub(delta)
            }
        }
    }
}

/// `oned_synthesis(A, Index)` (§15.4.4): apply every lifting stage of the
/// selected filter, in order, to the in/out signal `a` (even length).
pub fn oned_synthesis(a: &mut [i64], filter: &WaveletFilter) {
    for stage in filter.stages {
        apply_lift(a, stage);
    }
}

/// A 2-D integer subband / picture plane, row-major.
#[derive(Debug, Clone, Default)]
pub struct Plane {
    pub width: usize,
    pub height: usize,
    pub data: Vec<i64>,
}

impl Plane {
    pub fn new(width: usize, height: usize) -> Self {
        Plane {
            width,
            height,
            data: vec![0; width * height],
        }
    }

    #[inline]
    pub fn get(&self, y: usize, x: usize) -> i64 {
        self.data[y * self.width + x]
    }

    #[inline]
    pub fn set(&mut self, y: usize, x: usize, v: i64) {
        self.data[y * self.width + x] = v;
    }
}

/// `h_synthesis(L_data, H_data)` (§15.4.2): horizontal-only synthesis,
/// returning a plane of twice the width and same height.
pub fn h_synthesis(l_data: &Plane, h_data: &Plane, filter_ho: &WaveletFilter) -> Plane {
    let width = l_data.width * 2;
    let height = l_data.height;
    let mut synth = Plane::new(width, height);
    // Step 2 — interleave L (even) and H (odd) columns.
    for y in 0..height {
        for x in 0..l_data.width {
            synth.set(y, 2 * x, l_data.get(y, x));
            synth.set(y, 2 * x + 1, h_data.get(y, x));
        }
    }
    // Step 3 — horizontal 1-D synthesis on each row.
    let mut row = vec![0i64; width];
    for y in 0..height {
        for (x, slot) in row.iter_mut().enumerate() {
            *slot = synth.get(y, x);
        }
        oned_synthesis(&mut row, filter_ho);
        for (x, &v) in row.iter().enumerate() {
            synth.set(y, x, v);
        }
    }
    // Step 4 — remove accuracy bits.
    apply_bit_shift(&mut synth, filter_ho.bit_shift);
    synth
}

/// `vh_synthesis(LL, HL, LH, HH)` (§15.4.3): vertical then horizontal
/// synthesis, returning a plane of twice the dimensions.
pub fn vh_synthesis(
    ll: &Plane,
    hl: &Plane,
    lh: &Plane,
    hh: &Plane,
    filter_v: &WaveletFilter,
    filter_ho: &WaveletFilter,
) -> Plane {
    let width = ll.width * 2;
    let height = ll.height * 2;
    let mut synth = Plane::new(width, height);
    // Step 2 — quincunx interleave of the four subbands.
    for y in 0..ll.height {
        for x in 0..ll.width {
            synth.set(2 * y, 2 * x, ll.get(y, x));
            synth.set(2 * y, 2 * x + 1, hl.get(y, x));
            synth.set(2 * y + 1, 2 * x, lh.get(y, x));
            synth.set(2 * y + 1, 2 * x + 1, hh.get(y, x));
        }
    }
    // Step 3a — vertical 1-D synthesis on each column.
    let mut col = vec![0i64; height];
    for x in 0..width {
        for (y, slot) in col.iter_mut().enumerate() {
            *slot = synth.get(y, x);
        }
        oned_synthesis(&mut col, filter_v);
        for (y, &v) in col.iter().enumerate() {
            synth.set(y, x, v);
        }
    }
    // Step 3b — horizontal 1-D synthesis on each row.
    let mut row = vec![0i64; width];
    for y in 0..height {
        for (x, slot) in row.iter_mut().enumerate() {
            *slot = synth.get(y, x);
        }
        oned_synthesis(&mut row, filter_ho);
        for (x, &v) in row.iter().enumerate() {
            synth.set(y, x, v);
        }
    }
    // Step 4 — remove accuracy bits (filter_bit_shift of the HO filter).
    apply_bit_shift(&mut synth, filter_ho.bit_shift);
    synth
}

/// Step 4 of §15.4.2 / §15.4.3: `synth[y][x] = (synth + (1<<(shift-1))) >> shift`.
fn apply_bit_shift(synth: &mut Plane, shift: u32) {
    if shift > 0 {
        let add = 1i64 << (shift - 1);
        for v in synth.data.iter_mut() {
            *v = v.wrapping_add(add) >> shift;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_filter_indices_present() {
        for idx in 0..=6 {
            assert!(wavelet_filter(idx).is_some(), "index {idx}");
        }
        assert!(wavelet_filter(7).is_none());
    }

    #[test]
    fn legall_stage_count() {
        let f = wavelet_filter(1).unwrap();
        assert_eq!(f.stages.len(), 2);
        assert_eq!(f.bit_shift, 1);
    }

    #[test]
    fn daubechies_has_four_stages() {
        let f = wavelet_filter(6).unwrap();
        assert_eq!(f.stages.len(), 4);
    }

    #[test]
    fn haar_no_shift_dc_only_roundtrips_constant() {
        // A constant DC subband (L) with zero H detail through Haar-no-shift
        // h_synthesis should produce a constant plane (the Haar synthesis of
        // a DC-only signal). Validates interleave + lifting wiring.
        let l = Plane {
            width: 1,
            height: 1,
            data: vec![10],
        };
        let h = Plane {
            width: 1,
            height: 1,
            data: vec![0],
        };
        let f = wavelet_filter(3).unwrap();
        let out = h_synthesis(&l, &h, &f);
        assert_eq!(out.width, 2);
        assert_eq!(out.height, 1);
        // Stage 1 (Type2): A[0] -= (A[1]+1)>>1 = (0+1)>>1 = 0  -> A[0]=10
        // Stage 2 (Type3): A[1] += A[0] -> A[1] = 0 + 10 = 10
        assert_eq!(out.data, vec![10, 10]);
    }

    #[test]
    fn legall_vh_synthesis_dc_only_is_constant() {
        // A constant LL band with zero HL/LH/HH detail must synthesize to a
        // constant 2x2 block (a flat picture has no high-frequency content),
        // after the Step-4 bit shift. Exercises vertical+horizontal lifting
        // wiring and the accuracy-bit removal of the LeGall (5,3) filter.
        let ll = Plane {
            width: 1,
            height: 1,
            data: vec![20],
        };
        let zero = Plane {
            width: 1,
            height: 1,
            data: vec![0],
        };
        let f = wavelet_filter(1).unwrap(); // LeGall (5,3), bit_shift 1
        let out = vh_synthesis(&ll, &zero, &zero, &zero, &f, &f);
        assert_eq!((out.width, out.height), (2, 2));
        // All four output samples are equal (flat region) after the shift.
        let first = out.data[0];
        assert!(out.data.iter().all(|&v| v == first), "got {:?}", out.data);
    }

    #[test]
    fn legall_oned_inverts_forward_on_known_signal() {
        // Forward LeGall (5,3) analysis of a ramp, then synthesis, recovers
        // the ramp exactly (the lifting filters are reversible integer
        // transforms). We do the forward step here by inverting the synthesis
        // lifting stages, then confirm `oned_synthesis` reproduces the input.
        let original = [3i64, 9, 1, 7, 5, 2, 8, 4];
        let mut a = original;
        forward_legall(&mut a);
        oned_synthesis(&mut a, &wavelet_filter(1).unwrap());
        assert_eq!(a, original);
    }

    /// Inverse of LeGall (5,3) `oned_synthesis`: undo the two lifting stages
    /// in reverse order so that synthesis recovers the original. Used purely
    /// to drive the reversibility test above.
    fn forward_legall(a: &mut [i64]) {
        let len = a.len() as i32;
        let half = a.len() / 2;
        // Reverse of stage 2 (Type3, S=1, taps[1,1], even positions):
        //   A[2n+1] -= (A[2n] + A[2n+2] + 1) >> 1
        for n in 0..half as i32 {
            let p0 = (2 * n).clamp(0, len - 2) as usize;
            let p2 = (2 * (n + 1)).clamp(0, len - 2) as usize;
            let sum = a[p0] + a[p2] + 1;
            a[(2 * n + 1) as usize] -= sum >> 1;
        }
        // Reverse of stage 1 (Type2, S=2, taps[1,1], odd positions):
        //   A[2n] += (A[2n-1] + A[2n+1] + 2) >> 2
        for n in 0..half as i32 {
            let p_lo = (2 * n - 1).clamp(1, len - 1) as usize;
            let p_hi = (2 * n + 1).clamp(1, len - 1) as usize;
            let sum = a[p_lo] + a[p_hi] + 2;
            a[(2 * n) as usize] += sum >> 2;
        }
    }
}
