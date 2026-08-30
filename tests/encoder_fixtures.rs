//! Pinned encoder fixture matrix (r453).
//!
//! Every `tests/data/enc_*.drc` stream was produced by this crate's
//! encoder from the deterministic picture below; each `.ref.raw` is the
//! crate's own decode of those bytes (planes Y, C1, C2 concatenated,
//! row-major, one byte per sample at 8-bit depth, little-endian 16-bit
//! words otherwise). The tests here (1) regenerate each stream from its
//! configuration and require byte identity — the encoder cannot drift
//! silently — and (2) decode the pinned bytes and require the pinned
//! plane dump.
//!
//! ## Black-box validation record (r453)
//!
//! Validator: the `ffmpeg` CLI (Homebrew build, `Lavc62.28.100`), used
//! strictly as an opaque binary (`ffmpeg -i enc_<case>.drc -f rawvideo
//! out.raw; cmp out.raw enc_<case>.ref.raw`):
//!
//! * **Bit-exact** (validator output == pinned `.ref.raw`): all eleven
//!   non-v3 cases — lossless HQ at 4:4:4 / 4:2:2 / 4:2:0 across the
//!   LeGall, Haar (both), Deslauriers-Dubuc 9,7 + 13,7 and Fidelity
//!   wavelets at 8/10/12-bit, fixed-quant-index HQ, rate-controlled HQ,
//!   and both rate-controlled low-delay cases.
//! * `enc_hq_lossless_444_daub97_d1` (wavelet index 6): the validator's
//!   decode differs from ours on exactly the rightmost sample column
//!   (61 of 2304 samples; it emits bottom-of-range values there). Our
//!   synthesis follows ST 2042-1:2022 Table 22 and the §15.4.4.1 edge
//!   clamp (`pos = min(pos, len - 2)` for Type 3/4) verbatim and is
//!   integer-reversible; the divergent column is pinned from our decode,
//!   and the validator's full dump is staged alongside as
//!   `enc_hq_lossless_444_daub97_d1.validator.raw` for future triage.
//! * `enc_hq_asym_422_legall_ho1` and `enc_hq_frag_422_legall_d2`
//!   (major-version-3 features): outside the validator's envelope. It
//!   has no §12.4.4 extended-transform-parameters parse — probe
//!   experiment: an asymmetric-index-only stream is refused with a
//!   slice-count error, i.e. the extended-parameter bits are consumed
//!   as the fields that follow them — so the accepted asymmetric stream
//!   decodes to garbage and the fragmented stream is refused outright.
//!   Both are pinned as self-consistent references riding the code
//!   paths the eleven bit-exact cases validate (the same slice packer
//!   and lifting kernels; the only deltas are the §12.4.4 header bits
//!   and the §14 unit framing, both round-trip-tested).

use oxideav_vc2::params::ColorDiffFormat;
use oxideav_vc2::{encode_sequence, EncoderConfig, PictureInput, PictureKind, RateControl};

/// The deterministic test picture every fixture was encoded from
/// (gradient + LCG noise, seed 42) — byte-identical to the generator
/// that produced the pinned streams.
fn test_picture(
    w: usize,
    h: usize,
    f: ColorDiffFormat,
    depth: u32,
) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let (cw, ch) = match f {
        ColorDiffFormat::Yuv444 => (w, h),
        ColorDiffFormat::Yuv422 => (w / 2, h),
        ColorDiffFormat::Yuv420 => (w / 2, h / 2),
    };
    let mut s = 42u64;
    let mut plane = |pw: usize, ph: usize, phase: u64| -> Vec<u16> {
        let max = (1u64 << depth) - 1;
        (0..pw * ph)
            .map(|i| {
                let (x, y) = (i % pw, i / pw);
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let noise = (s >> 40) % (max / 8 + 1);
                let grad = ((x + y * 2 + phase as usize) as u64 * max) / (pw + ph * 2) as u64;
                ((grad + noise) % (max + 1)) as u16
            })
            .collect()
    };
    (plane(w, h, 0), plane(cw, ch, 3), plane(cw, ch, 7))
}

struct Case {
    name: &'static str,
    w: usize,
    h: usize,
    f: ColorDiffFormat,
    depth: u32,
    cfg: fn(EncoderConfig) -> EncoderConfig,
}

fn cases() -> Vec<Case> {
    fn c(
        name: &'static str,
        w: usize,
        h: usize,
        f: ColorDiffFormat,
        depth: u32,
        cfg: fn(EncoderConfig) -> EncoderConfig,
    ) -> Case {
        Case {
            name,
            w,
            h,
            f,
            depth,
            cfg,
        }
    }
    use ColorDiffFormat::{Yuv420, Yuv422, Yuv444};
    vec![
        c("enc_hq_lossless_422_legall_d2", 32, 24, Yuv422, 10, |c| {
            c.slices(2, 2)
        }),
        c("enc_hq_lossless_420_haars_d1", 32, 24, Yuv420, 10, |c| {
            c.wavelet(4, 1).slices(2, 2)
        }),
        c("enc_hq_lossless_444_dd97_d2", 32, 24, Yuv444, 12, |c| {
            c.wavelet(0, 2).slices(2, 2)
        }),
        c("enc_hq_lossless_444_fidelity_d1", 32, 24, Yuv444, 10, |c| {
            c.wavelet(5, 1).slices(2, 2)
        }),
        c("enc_hq_lossless_444_daub97_d1", 32, 24, Yuv444, 10, |c| {
            c.wavelet(6, 1).slices(2, 2)
        }),
        c("enc_hq_lossless_444_haar0_d1", 32, 24, Yuv444, 10, |c| {
            c.wavelet(3, 1).slices(2, 2)
        }),
        c("enc_hq_lossless_444_dd137_d1", 32, 24, Yuv444, 10, |c| {
            c.wavelet(2, 1).slices(2, 2)
        }),
        c("enc_hq_q16_444_legall_d2", 32, 24, Yuv444, 10, |c| {
            c.slices(2, 2)
                .rate_control(RateControl::FixedQuantIndex(16))
        }),
        c("enc_hq_rate2000_422_legall_d2", 64, 48, Yuv422, 10, |c| {
            c.slices(4, 3).rate_control(RateControl::PictureBytes(2000))
        }),
        c("enc_ld_rate3000_422_legall_d2", 64, 48, Yuv422, 10, |c| {
            c.picture_kind(PictureKind::LowDelay)
                .slices(4, 3)
                .rate_control(RateControl::PictureBytes(3000))
        }),
        c("enc_ld_rate1200_420_legall_d1", 32, 24, Yuv420, 8, |c| {
            c.picture_kind(PictureKind::LowDelay)
                .wavelet(1, 1)
                .slices(2, 2)
                .rate_control(RateControl::PictureBytes(1200))
        }),
        c(
            "enc_hq_lossless_8bit_444_legall_d2",
            32,
            24,
            Yuv444,
            8,
            |c| c.slices(2, 2),
        ),
        c("enc_hq_frag_422_legall_d2", 32, 24, Yuv422, 10, |c| {
            c.slices(2, 2).fragments(3)
        }),
        c("enc_hq_asym_422_legall_ho1", 32, 24, Yuv422, 10, |c| {
            c.wavelet(1, 1).asymmetric(1, 1).slices(2, 2)
        }),
    ]
}

fn data(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).expect(name)
}

#[test]
fn pinned_streams_regenerate_byte_exactly() {
    for case in cases() {
        let (y, c1, c2) = test_picture(case.w, case.h, case.f, case.depth);
        let cfg = (case.cfg)(EncoderConfig::new(
            case.w as u64,
            case.h as u64,
            case.f,
            case.depth,
        ));
        let stream = encode_sequence(
            cfg,
            &[PictureInput {
                y: &y,
                c1: &c1,
                c2: &c2,
            }],
        )
        .expect(case.name);
        assert_eq!(
            stream,
            data(&format!("{}.drc", case.name)),
            "{} drifted from its pinned bytes",
            case.name
        );
    }
}

#[test]
fn pinned_streams_decode_to_pinned_references() {
    for case in cases() {
        let stream = data(&format!("{}.drc", case.name));
        let pics = oxideav_vc2::decode_sequence(&stream).expect(case.name);
        assert_eq!(pics.len(), 1, "{}", case.name);
        let p = &pics[0];
        let mut dump = Vec::new();
        for plane in [&p.y, &p.c1, &p.c2] {
            if case.depth <= 8 {
                dump.extend(plane.iter().map(|&v| v as u8));
            } else {
                for &v in plane.iter() {
                    dump.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
        assert_eq!(
            dump,
            data(&format!("{}.ref.raw", case.name)),
            "{} decode drifted from its pinned reference",
            case.name
        );
    }
}

#[test]
fn daub97_validator_dump_differs_only_in_the_last_column() {
    // The staged validator dump for the Daubechies (9,7) case diverges
    // from the Table 22 / §15.4.4.1 decode on the rightmost sample
    // column only; pin that shape so a change on either side is loud.
    let ours = data("enc_hq_lossless_444_daub97_d1.ref.raw");
    let theirs = data("enc_hq_lossless_444_daub97_d1.validator.raw");
    assert_eq!(ours.len(), theirs.len());
    let (w, h) = (32usize, 24usize);
    let word = |buf: &[u8], i: usize| u16::from_le_bytes([buf[2 * i], buf[2 * i + 1]]);
    let mut diffs = 0;
    for i in 0..ours.len() / 2 {
        if word(&ours, i) != word(&theirs, i) {
            let x = i % w; // every plane is 32 wide (4:4:4) and 24 tall
            assert_eq!(x, w - 1, "divergence off the last column at index {i}");
            diffs += 1;
        }
    }
    assert!(diffs > 0 && diffs <= 3 * h, "unexpected diff count {diffs}");
}
