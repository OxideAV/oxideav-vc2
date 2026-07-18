//! Fixture-pinned conformance tests (round r417 matrix, staged with
//! generation notes under `docs/video/vc2/fixtures/`).
//!
//! Each case is a hand-assembled single-picture HQ stream: sequence
//! header (`base_video_format` 0, 8x8 luma, progressive), one
//! single-slice HQ picture with deterministic LCG coefficients in
//! subband stream order, EOS. The tests (a) regenerate the stream with
//! the shared harness and assert byte identity with the committed
//! fixture — the staged bytes stay reproducible from spec pseudocode —
//! and (b) decode the fixture and pin the exact output planes.
//!
//! The five 10/12-bit video-range cases were verified **bit-exact**
//! against an independent black-box validator binary (opaque CLI
//! invocation only) across all three chroma samplings and three
//! wavelet/depth combinations, so these pins guard externally
//! corroborated output. The validator's envelope excludes every
//! full-range and 16-bit signal-range preset (5..=8) — it refuses such
//! sequence headers outright — so the preset-7/8 pins are
//! self-consistent references riding the very code path the validated
//! cases exercise (the only depth-dependent deltas are the §11.6.3
//! depth derivation and the §15.5 clip/offset constants, unit-tested
//! against the spec formulas).

mod common;

use common::{
    build_units, hq_slice_bytes, picture_body, sequence_header_body_full, PicParams, SignalRange,
};

/// Deterministic LCG coefficients in `[-amp, amp]` (the generator the
/// staged fixtures were built with).
fn coeffs(n: usize, amp: i64, seed: u64) -> Vec<i64> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((s >> 33) as i64 % (2 * amp + 1)) - amp
        })
        .collect()
}

struct Case {
    /// Table 10 preset index.
    preset: u64,
    /// Coefficient amplitude used at generation time.
    amp: i64,
    /// Table 7 colour-difference sampling index.
    cds: u64,
    wavelet_index: u64,
    dwt_depth: u64,
    /// Committed fixture stream.
    stream: &'static [u8],
    /// Committed decode reference: planes Y, C1, C2 concatenated,
    /// row-major, LE 16-bit code values (no promotion applied).
    reference: &'static [u8],
}

const CASES: &[Case] = &[
    Case {
        preset: 3,
        amp: 300,
        cds: 0,
        wavelet_index: 4,
        dwt_depth: 1,
        stream: include_bytes!("data/vc2_preset3_444_haar_d1.drc"),
        reference: include_bytes!("data/vc2_preset3_444_haar_d1.ref_p16le.raw"),
    },
    Case {
        preset: 3,
        amp: 300,
        cds: 1,
        wavelet_index: 4,
        dwt_depth: 1,
        stream: include_bytes!("data/vc2_preset3_422_haar_d1.drc"),
        reference: include_bytes!("data/vc2_preset3_422_haar_d1.ref_p16le.raw"),
    },
    Case {
        preset: 3,
        amp: 300,
        cds: 2,
        wavelet_index: 1,
        dwt_depth: 2,
        stream: include_bytes!("data/vc2_preset3_420_legall_d2.drc"),
        reference: include_bytes!("data/vc2_preset3_420_legall_d2.ref_p16le.raw"),
    },
    Case {
        preset: 3,
        amp: 300,
        cds: 0,
        wavelet_index: 0,
        dwt_depth: 2,
        stream: include_bytes!("data/vc2_preset3_444_dd97_d2.drc"),
        reference: include_bytes!("data/vc2_preset3_444_dd97_d2.ref_p16le.raw"),
    },
    Case {
        preset: 4,
        amp: 1200,
        cds: 0,
        wavelet_index: 4,
        dwt_depth: 1,
        stream: include_bytes!("data/vc2_preset4_444_haar_d1.drc"),
        reference: include_bytes!("data/vc2_preset4_444_haar_d1.ref_p16le.raw"),
    },
    Case {
        preset: 7,
        amp: 20000,
        cds: 0,
        wavelet_index: 4,
        dwt_depth: 1,
        stream: include_bytes!("data/vc2_preset7_444_haar_d1.drc"),
        reference: include_bytes!("data/vc2_preset7_444_haar_d1.ref_p16le.raw"),
    },
    Case {
        preset: 8,
        amp: 20000,
        cds: 0,
        wavelet_index: 4,
        dwt_depth: 1,
        stream: include_bytes!("data/vc2_preset8_444_haar_d1.drc"),
        reference: include_bytes!("data/vc2_preset8_444_haar_d1.ref_p16le.raw"),
    },
];

const W: usize = 8;
const H: usize = 8;

fn rebuild(case: &Case) -> Vec<u8> {
    let p = PicParams {
        wavelet_index: case.wavelet_index,
        dwt_depth: case.dwt_depth,
        ..PicParams::hq_depth0()
    };
    let (cw, ch) = match case.cds {
        0 => (W, H),
        1 => (W / 2, H),
        _ => (W / 2, H / 2),
    };
    let y = coeffs(W * H, case.amp, 0x1234_5678 + case.preset);
    let c1 = coeffs(cw * ch, case.amp / 2, 0x9abc_def0 + case.preset);
    let c2 = coeffs(cw * ch, case.amp / 2, 0x0fed_cba9 + case.preset);
    let seq = sequence_header_body_full(
        W as u64,
        H as u64,
        p.major_version,
        case.cds,
        SignalRange::Preset(case.preset),
    );
    let pic = picture_body(&p, 0, &[hq_slice_bytes(p.qindex, &y, &c1, &c2)]);
    build_units(&[(0x00, seq), (0xE8, pic)])
}

#[test]
fn fixtures_are_reproducible_from_the_harness() {
    for case in CASES {
        assert_eq!(
            rebuild(case),
            case.stream,
            "preset {} cds {} wavelet {} depth {}: staged fixture bytes diverge from the generator",
            case.preset,
            case.cds,
            case.wavelet_index,
            case.dwt_depth
        );
    }
}

#[test]
fn fixture_decodes_match_pinned_references() {
    for case in CASES {
        let pics = oxideav_vc2::decode_sequence(case.stream).expect("fixture decode");
        assert_eq!(pics.len(), 1);
        let p = &pics[0];
        let mut raw = Vec::with_capacity(case.reference.len());
        for plane in [&p.y, &p.c1, &p.c2] {
            for &v in plane.iter() {
                raw.extend_from_slice(&v.to_le_bytes());
            }
        }
        assert_eq!(
            raw, case.reference,
            "preset {} cds {}: decoded planes diverge from the pinned reference",
            case.preset, case.cds
        );
    }
}

/// The 16-bit fixtures through the registered `Decoder`: depth-16 code
/// values must reach the frame verbatim (promotion shift 0), matching
/// the pinned standalone reference word for word.
#[test]
#[cfg(feature = "registry")]
fn sixteen_bit_fixture_frames_match_pinned_words() {
    use oxideav_core::{CodecId, CodecParameters, Frame, Packet, TimeBase};
    for case in CASES.iter().filter(|c| c.preset >= 7) {
        let params = CodecParameters::video(CodecId::new("vc2"));
        let mut dec = oxideav_vc2::make_decoder(&params).expect("factory");
        let pkt = Packet::new(0, TimeBase::MILLIS, case.stream.to_vec());
        dec.send_packet(&pkt).expect("send");
        let Frame::Video(v) = dec.receive_frame().expect("frame") else {
            panic!("expected a video frame");
        };
        let mut raw = Vec::with_capacity(case.reference.len());
        for plane in &v.planes {
            raw.extend_from_slice(&plane.data);
        }
        assert_eq!(
            raw, case.reference,
            "preset {}: Decoder frame diverges from the pinned reference",
            case.preset
        );
    }
}
