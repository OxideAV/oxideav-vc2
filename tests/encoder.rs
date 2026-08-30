//! Encoder round trips: every stream the encoder emits is decoded by this
//! crate's decoder and checked against the input (bit-exact for the
//! lossless modes, bounded error for the rate-controlled ones), walked by
//! the opt-in conformance checker, and inspected for the §10 / §11 / §12
//! header values the configuration implies.

use oxideav_vc2::conformance;
use oxideav_vc2::params::ColorDiffFormat;
use oxideav_vc2::{
    encode_sequence, DecodedPicture, EncoderConfig, PictureInput, PictureKind, RateControl,
    SequenceEncoder,
};

/// Deterministic test picture: a smooth gradient plus LCG noise, so the
/// subbands carry both large DC and non-trivial detail.
struct TestPicture {
    y: Vec<u16>,
    c1: Vec<u16>,
    c2: Vec<u16>,
}

fn test_picture(
    w: usize,
    h: usize,
    format: ColorDiffFormat,
    luma_depth: u32,
    chroma_depth: u32,
    seed: u64,
) -> TestPicture {
    let (cw, ch) = match format {
        ColorDiffFormat::Yuv444 => (w, h),
        ColorDiffFormat::Yuv422 => (w / 2, h),
        ColorDiffFormat::Yuv420 => (w / 2, h / 2),
    };
    let mut s = seed;
    let mut plane = |pw: usize, ph: usize, depth: u32, phase: u64| -> Vec<u16> {
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
    TestPicture {
        y: plane(w, h, luma_depth, 0),
        c1: plane(cw, ch, chroma_depth, 3),
        c2: plane(cw, ch, chroma_depth, 7),
    }
}

impl TestPicture {
    fn input(&self) -> PictureInput<'_> {
        PictureInput {
            y: &self.y,
            c1: &self.c1,
            c2: &self.c2,
        }
    }

    fn assert_exact(&self, pic: &DecodedPicture) {
        assert_eq!(pic.y, self.y, "luma");
        assert_eq!(pic.c1, self.c1, "c1");
        assert_eq!(pic.c2, self.c2, "c2");
    }

    /// Peak signal-to-noise ratio of the luma plane in dB.
    fn luma_psnr(&self, pic: &DecodedPicture, depth: u32) -> f64 {
        let max = ((1u64 << depth) - 1) as f64;
        let mse: f64 = self
            .y
            .iter()
            .zip(&pic.y)
            .map(|(&a, &b)| {
                let d = a as f64 - b as f64;
                d * d
            })
            .sum::<f64>()
            / self.y.len() as f64;
        if mse == 0.0 {
            f64::INFINITY
        } else {
            10.0 * (max * max / mse).log10()
        }
    }
}

fn decode_clean(stream: &[u8]) -> Vec<DecodedPicture> {
    let violations = conformance::check_stream(stream).expect("conformance walk");
    assert!(violations.is_empty(), "violations: {violations:?}");
    oxideav_vc2::decode_sequence(stream).expect("decode")
}

#[test]
fn lossless_hq_round_trips_every_wavelet_and_sampling() {
    for wavelet in 0..=6u64 {
        for (format, w, h) in [
            (ColorDiffFormat::Yuv444, 24, 20),
            (ColorDiffFormat::Yuv422, 30, 17),
            (ColorDiffFormat::Yuv420, 34, 22),
        ] {
            for depth in [1u64, 2, 3] {
                let pic = test_picture(w, h, format, 8, 8, wavelet * 100 + depth);
                let cfg = EncoderConfig::new(w as u64, h as u64, format, 8)
                    .wavelet(wavelet, depth)
                    .slices(3, 2);
                let stream = encode_sequence(cfg, &[pic.input()]).expect("encode");
                let pics = decode_clean(&stream);
                assert_eq!(pics.len(), 1, "wavelet {wavelet} depth {depth}");
                pic.assert_exact(&pics[0]);
            }
        }
    }
}

#[test]
fn lossless_round_trips_8_10_12_16_and_mixed_depths() {
    for (ld, cd) in [(8, 8), (10, 10), (12, 12), (16, 16), (12, 10), (10, 8)] {
        let pic = test_picture(16, 16, ColorDiffFormat::Yuv422, ld, cd, 5);
        let mut cfg = EncoderConfig::new(16, 16, ColorDiffFormat::Yuv422, ld);
        if ld != cd {
            // Mixed depths: explicit custom (index 0) signal range.
            let v = &mut cfg.video_parameters;
            v.color_diff_excursion = (1 << cd) - 1;
            v.color_diff_offset = 1 << (cd - 1);
        }
        let stream = encode_sequence(cfg, &[pic.input()]).expect("encode");
        let pics = decode_clean(&stream);
        assert_eq!(pics[0].luma_depth, ld);
        assert_eq!(pics[0].color_diff_depth, cd);
        pic.assert_exact(&pics[0]);
    }
}

#[test]
fn lossless_round_trips_asymmetric_transforms() {
    // Every filter pair with horizontal-only levels, including the mixed
    // Table D.8 pair (Haar-no-shift vertical / LeGall horizontal) and a
    // pair without an Annex D default (custom matrix required).
    for (wi, wi_ho, ho, depth) in [(1, 1, 1, 1), (3, 1, 2, 1), (1, 1, 3, 0), (0, 0, 1, 2)] {
        let pic = test_picture(40, 12, ColorDiffFormat::Yuv444, 10, 10, 77);
        let cfg = EncoderConfig::new(40, 12, ColorDiffFormat::Yuv444, 10)
            .wavelet(wi, depth)
            .asymmetric(wi_ho, ho)
            .slices(2, 2);
        let enc = SequenceEncoder::new(cfg.clone()).expect("config");
        assert_eq!(enc.major_version(), 3, "asymmetric transforms need v3");
        let stream = encode_sequence(cfg, &[pic.input()]).expect("encode");
        let pics = decode_clean(&stream);
        pic.assert_exact(&pics[0]);
    }
    // A pair outside Annex D fails without a custom matrix and succeeds
    // with one.
    let cfg = EncoderConfig::new(16, 8, ColorDiffFormat::Yuv444, 8)
        .wavelet(6, 1)
        .asymmetric(5, 1);
    assert!(matches!(
        SequenceEncoder::new(cfg.clone()),
        Err(oxideav_vc2::Error::MissingQuantMatrix)
    ));
    let pic = test_picture(16, 8, ColorDiffFormat::Yuv444, 8, 8, 3);
    let mut cfg = cfg;
    cfg.quant_matrix = Some(vec![
        oxideav_vc2::quant::MatrixLevel::Ll(2),
        oxideav_vc2::quant::MatrixLevel::H(1),
        oxideav_vc2::quant::MatrixLevel::Ac {
            hl: 3,
            lh: 3,
            hh: 0,
        },
    ]);
    let stream = encode_sequence(cfg, &[pic.input()]).expect("encode");
    pic.assert_exact(&decode_clean(&stream)[0]);
}

#[test]
fn fixed_quant_index_degrades_gracefully_and_shrinks() {
    let pic = test_picture(64, 48, ColorDiffFormat::Yuv420, 8, 8, 11);
    let mut last_len = usize::MAX;
    let mut last_psnr = f64::INFINITY;
    for q in [0u64, 4, 8, 12, 16, 20] {
        let cfg = EncoderConfig::new(64, 48, ColorDiffFormat::Yuv420, 8)
            .rate_control(RateControl::FixedQuantIndex(q));
        let stream = encode_sequence(cfg, &[pic.input()]).expect("encode");
        let pics = decode_clean(&stream);
        let psnr = pic.luma_psnr(&pics[0], 8);
        if q == 0 {
            pic.assert_exact(&pics[0]);
        } else {
            assert!(psnr > 15.0, "q {q} psnr {psnr}");
        }
        assert!(
            stream.len() <= last_len,
            "q {q}: {} > {last_len}",
            stream.len()
        );
        assert!(psnr <= last_psnr, "q {q}: psnr rose {psnr} > {last_psnr}");
        last_len = stream.len();
        last_psnr = psnr;
    }
}

#[test]
fn low_delay_hits_the_picture_byte_target_exactly() {
    let pic = test_picture(64, 64, ColorDiffFormat::Yuv422, 10, 10, 21);
    for target in [1500u64, 3000, 6000, 12000] {
        let cfg = EncoderConfig::new(64, 64, ColorDiffFormat::Yuv422, 10)
            .picture_kind(PictureKind::LowDelay)
            .slices(4, 4)
            .rate_control(RateControl::PictureBytes(target));
        let mut enc = SequenceEncoder::new(cfg).expect("config");
        assert_eq!(enc.major_version(), 1, "plain LD is a v1 stream");
        let header = enc.sequence_header_unit().len();
        let units = enc.encode_picture(&pic.input()).expect("encode");
        let picture_unit = units.len() - header;
        assert_eq!(picture_unit as u64, target, "target {target}");
        let mut stream = units;
        stream.extend_from_slice(&enc.end_sequence());
        let pics = decode_clean(&stream);
        let psnr = pic.luma_psnr(&pics[0], 10);
        assert!(psnr > 25.0, "target {target}: psnr {psnr}");
    }
}

#[test]
fn low_delay_dc_prediction_matches_decoder_at_coarse_quant() {
    // Coarse quantisation makes the write-side DC prediction matter: the
    // encoder predicts from *reconstructed* neighbours. Verify by
    // recoding the decoded picture at the same setting — an encoder
    // whose prediction disagreed with the decoder would drift.
    let pic = test_picture(48, 32, ColorDiffFormat::Yuv444, 8, 8, 4);
    let cfg = EncoderConfig::new(48, 32, ColorDiffFormat::Yuv444, 8)
        .picture_kind(PictureKind::LowDelay)
        .slices(3, 2)
        .rate_control(RateControl::PictureBytes(900));
    let stream = encode_sequence(cfg.clone(), &[pic.input()]).expect("encode");
    let first = decode_clean(&stream);
    let psnr = pic.luma_psnr(&first[0], 8);
    assert!(psnr > 18.0, "psnr {psnr}");
    let again = encode_sequence(
        cfg,
        &[PictureInput {
            y: &first[0].y,
            c1: &first[0].c1,
            c2: &first[0].c2,
        }],
    )
    .expect("re-encode");
    let second = decode_clean(&again);
    let psnr2 = pic.luma_psnr(&second[0], 8);
    assert!(psnr2 >= psnr - 3.0, "re-encode drifted: {psnr} -> {psnr2}");
}

#[test]
fn high_quality_rate_control_respects_the_target() {
    let pic = test_picture(64, 64, ColorDiffFormat::Yuv444, 8, 8, 9);
    let mut prev_psnr = 0.0;
    for target in [2000u64, 4000, 8000] {
        let cfg = EncoderConfig::new(64, 64, ColorDiffFormat::Yuv444, 8)
            .slices(4, 4)
            .rate_control(RateControl::PictureBytes(target));
        let mut enc = SequenceEncoder::new(cfg).expect("config");
        let header = enc.sequence_header_unit().len();
        let units = enc.encode_picture(&pic.input()).expect("encode");
        let picture_unit = (units.len() - header) as u64;
        assert!(picture_unit <= target, "target {target}: {picture_unit}");
        assert!(
            picture_unit > target * 8 / 10,
            "target {target}: {picture_unit} far below"
        );
        let mut stream = units;
        stream.extend_from_slice(&enc.end_sequence());
        let pics = decode_clean(&stream);
        let psnr = pic.luma_psnr(&pics[0], 8);
        assert!(
            psnr > prev_psnr,
            "target {target}: psnr {psnr} <= {prev_psnr}"
        );
        prev_psnr = psnr;
    }
}

#[test]
fn bit_rate_target_uses_the_frame_rate() {
    // 25 fps, 800 kbit/s -> 4000 bytes per frame; fields halve it.
    let pic = test_picture(64, 32, ColorDiffFormat::Yuv422, 8, 8, 13);
    let mut cfg = EncoderConfig::new(64, 32, ColorDiffFormat::Yuv422, 8)
        .picture_kind(PictureKind::LowDelay)
        .slices(2, 2)
        .rate_control(RateControl::BitsPerSecond(800_000));
    cfg.video_parameters.frame_rate_numer = 25;
    cfg.video_parameters.frame_rate_denom = 1;
    let mut enc = SequenceEncoder::new(cfg.clone()).expect("config");
    let header = enc.sequence_header_unit().len();
    let units = enc.encode_picture(&pic.input()).expect("encode");
    assert_eq!(units.len() - header, 4000);
    let seq = enc.sequence_header();
    assert_eq!(seq.video_parameters.frame_rate_numer, 25);
    assert_eq!(
        seq.source_overrides.frame_rate_index,
        Some(3),
        "Table 8 preset"
    );

    // Field coding: each picture is half a frame, 2000 bytes.
    cfg.picture_coding_mode = 1;
    cfg.video_parameters.source_sampling = 1;
    let field = test_picture(64, 16, ColorDiffFormat::Yuv422, 8, 8, 14);
    let mut enc = SequenceEncoder::new(cfg).expect("config");
    let header = enc.sequence_header_unit().len();
    let units = enc.encode_picture(&field.input()).expect("encode");
    assert_eq!(units.len() - header, 2000);
}

#[test]
fn fragmented_pictures_round_trip_and_signal_v3() {
    let pic = test_picture(40, 24, ColorDiffFormat::Yuv420, 10, 10, 31);
    for (kind, rate) in [
        (PictureKind::HighQuality, RateControl::Lossless),
        (PictureKind::LowDelay, RateControl::PictureBytes(2500)),
    ] {
        let cfg = EncoderConfig::new(40, 24, ColorDiffFormat::Yuv420, 10)
            .picture_kind(kind)
            .slices(5, 3)
            .rate_control(rate)
            .fragments(4); // 15 slices -> 4 data fragments (4,4,4,3)
        let enc = SequenceEncoder::new(cfg.clone()).expect("config");
        assert_eq!(enc.major_version(), 3);
        let stream = encode_sequence(cfg, &[pic.input(), pic.input()]).expect("encode");
        // Count fragment data units: setup + 4 data per picture.
        let code = if kind == PictureKind::LowDelay {
            0xCC
        } else {
            0xEC
        };
        let fragments = stream
            .windows(5)
            .filter(|w| &w[..4] == b"BBCD" && w[4] == code)
            .count();
        assert_eq!(fragments, 2 * 5);
        let pics = decode_clean(&stream);
        assert_eq!(pics.len(), 2);
        assert_eq!(pics[1].picture_number, 1);
        if kind == PictureKind::HighQuality {
            pic.assert_exact(&pics[0]);
            pic.assert_exact(&pics[1]);
        } else {
            assert!(pic.luma_psnr(&pics[0], 10) > 20.0);
        }
    }
}

#[test]
fn sequences_concatenate_and_picture_numbers_restart() {
    let a = test_picture(16, 16, ColorDiffFormat::Yuv444, 8, 8, 1);
    let b = test_picture(16, 16, ColorDiffFormat::Yuv444, 8, 8, 2);
    let cfg = EncoderConfig::new(16, 16, ColorDiffFormat::Yuv444, 8);
    let mut enc = SequenceEncoder::new(cfg).expect("config");
    let mut stream = Vec::new();
    stream.extend_from_slice(&enc.encode_picture(&a.input()).unwrap());
    stream.extend_from_slice(&enc.encode_picture(&b.input()).unwrap());
    stream.extend_from_slice(&enc.end_sequence());
    assert!(!enc.in_sequence());
    assert!(enc.end_sequence().is_empty(), "no double EOS");
    stream.extend_from_slice(&enc.encode_picture(&b.input()).unwrap());
    stream.extend_from_slice(&enc.end_sequence());
    let pics = decode_clean(&stream);
    assert_eq!(pics.len(), 3);
    assert_eq!(
        pics.iter().map(|p| p.picture_number).collect::<Vec<_>>(),
        [0, 1, 0]
    );
    a.assert_exact(&pics[0]);
    b.assert_exact(&pics[1]);
    b.assert_exact(&pics[2]);
    // Two sequence headers, two end-of-sequence units.
    let count = |code: u8| {
        stream
            .windows(5)
            .filter(|w| &w[..4] == b"BBCD" && w[4] == code)
            .count()
    };
    assert_eq!(count(0x00), 2);
    assert_eq!(count(0x10), 2);
}

#[test]
fn parse_info_offsets_chain_exactly() {
    let pic = test_picture(16, 16, ColorDiffFormat::Yuv444, 8, 8, 8);
    let cfg = EncoderConfig::new(16, 16, ColorDiffFormat::Yuv444, 8)
        .slices(2, 2)
        .fragments(2);
    let stream = encode_sequence(cfg, &[pic.input()]).expect("encode");
    // Walk every header by next_parse_offset and check the back links.
    let mut pos = 0usize;
    let mut prev_len = 0u32;
    let mut headers = 0;
    loop {
        assert_eq!(&stream[pos..pos + 4], b"BBCD");
        let next = u32::from_be_bytes(stream[pos + 5..pos + 9].try_into().unwrap());
        let prev = u32::from_be_bytes(stream[pos + 9..pos + 13].try_into().unwrap());
        assert_eq!(prev, prev_len, "previous offset at {pos}");
        headers += 1;
        if stream[pos + 4] == 0x10 {
            assert_eq!(next, 0, "end of sequence has no successor");
            assert_eq!(pos + 13, stream.len());
            break;
        }
        assert!(next >= 13);
        prev_len = next;
        pos += next as usize;
    }
    // header + setup + 2 data fragments (4 slices / 2) + EOS.
    assert_eq!(headers, 5);
}

#[test]
fn sequence_header_uses_presets_and_only_needed_overrides() {
    // Base format 13 (HD 1080p-60) at its own defaults: no override flag
    // at all, level 3 accepted.
    let mut cfg = EncoderConfig::new(1920, 1080, ColorDiffFormat::Yuv422, 10);
    cfg.base_video_format = 13;
    cfg.video_parameters = oxideav_vc2::params::set_source_defaults(13).unwrap();
    cfg.level = 3;
    // 1920x1080 at depth 2: the DC band is 480x270, so a 30x27 grid
    // splits it evenly (the §5.4 equal-DC-per-slice level constraint).
    cfg.slices_x = 30;
    cfg.slices_y = 27;
    let enc = SequenceEncoder::new(cfg.clone()).expect("level-3 config");
    let ov = enc.sequence_header().source_overrides;
    assert_eq!(ov, Default::default());
    assert_eq!(enc.major_version(), 2);
    assert_eq!(enc.sequence_header().parse_parameters.profile, 3);
    assert_eq!(enc.sequence_header().parse_parameters.level, 3);

    // Level 3 forbids custom dimensions: a 1000x1000 override is refused
    // with the violation reported.
    let mut bad = cfg.clone();
    bad.video_parameters.frame_width = 1000;
    bad.video_parameters.frame_height = 1000;
    assert!(matches!(
        SequenceEncoder::new(bad),
        Err(oxideav_vc2::Error::InvalidValue(_))
    ));

    // A wavelet index above 4 breaks the §5.4 picture constraint.
    let mut bad = cfg;
    bad.wavelet_index = 5;
    bad.wavelet_index_ho = 5;
    assert!(SequenceEncoder::new(bad).is_err());

    // Custom colour spec / frame rate presets land as indices; extended
    // enumerations bump the major version to 3.
    let mut cfg = EncoderConfig::new(16, 16, ColorDiffFormat::Yuv444, 8);
    cfg.video_parameters.frame_rate_numer = 120;
    cfg.video_parameters.frame_rate_denom = 1;
    cfg.video_parameters.color_primaries_index = 4;
    cfg.video_parameters.color_matrix_index = 4;
    cfg.video_parameters.transfer_function_index = 5;
    let enc = SequenceEncoder::new(cfg).expect("config");
    let ov = enc.sequence_header().source_overrides;
    assert_eq!(ov.frame_rate_index, Some(16));
    assert_eq!(ov.color_spec_index, Some(7));
    // Base format 0's default signal range already carries the 8-bit
    // full-range values, so no override is signalled for it.
    assert_eq!(ov.signal_range_index, None);
    assert!(ov.custom_dimensions_flag && ov.custom_chroma_format_flag);
    assert_eq!(enc.major_version(), 3);
}

#[test]
fn low_delay_rejects_lossless_and_bad_planes_are_refused() {
    let cfg =
        EncoderConfig::new(16, 16, ColorDiffFormat::Yuv444, 8).picture_kind(PictureKind::LowDelay);
    assert!(matches!(
        SequenceEncoder::new(cfg),
        Err(oxideav_vc2::Error::Unsupported(_))
    ));
    let cfg = EncoderConfig::new(16, 16, ColorDiffFormat::Yuv444, 8);
    let mut enc = SequenceEncoder::new(cfg).unwrap();
    let short = [0u16; 10];
    assert!(enc
        .encode_picture(&PictureInput {
            y: &short,
            c1: &short,
            c2: &short
        })
        .is_err());
}

#[test]
fn large_lossless_slices_raise_the_size_scaler() {
    // One slice over a 96x96 16-bit picture needs far more than 255
    // bytes per component: the encoder must raise slice_size_scaler and
    // still round-trip bit-exactly.
    let pic = test_picture(96, 96, ColorDiffFormat::Yuv444, 16, 16, 42);
    let cfg = EncoderConfig::new(96, 96, ColorDiffFormat::Yuv444, 16).slices(1, 1);
    let stream = encode_sequence(cfg, &[pic.input()]).expect("encode");
    let pics = decode_clean(&stream);
    pic.assert_exact(&pics[0]);
}

#[test]
fn encoder_output_survives_bit_corruption() {
    // Hostile-transport sweep: every byte of an encoder-emitted stream,
    // flipped at its top and bottom bit, must run the decoder and the
    // conformance walker to a clean Ok/Err — no panic, hang or
    // unbounded allocation (the decoder's existing hardening applies to
    // the encoder's own output shape too).
    let pic = test_picture(24, 16, ColorDiffFormat::Yuv422, 10, 10, 55);
    let cfg = EncoderConfig::new(24, 16, ColorDiffFormat::Yuv422, 10).slices(2, 2);
    let stream = encode_sequence(cfg, &[pic.input()]).expect("encode");
    for i in 0..stream.len() {
        for bit in [0x01u8, 0x80] {
            let mut mutated = stream.clone();
            mutated[i] ^= bit;
            let _ = oxideav_vc2::decode_sequence(&mutated);
            let _ = conformance::check_stream(&mutated);
        }
    }
}
