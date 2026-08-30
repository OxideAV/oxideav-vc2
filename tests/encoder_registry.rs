//! `oxideav-core` encoder wrapper tests: registry wiring, the options
//! schema, frame/packet round trips through the crate's own decoder, and
//! the sequence framing modes.
#![cfg(feature = "registry")]

use oxideav_core::{
    CodecId, CodecOptions, CodecParameters, Error, Frame, PixelFormat, Rational, RuntimeContext,
    VideoFrame, VideoPlane,
};

fn enc_params(w: u32, h: u32, pf: PixelFormat) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new("vc2"));
    p.width = Some(w);
    p.height = Some(h);
    p.pixel_format = Some(pf);
    p
}

/// Deterministic 3-plane frame in the wrapper's plane layout.
fn test_frame(w: usize, h: usize, pf: PixelFormat, pts: i64) -> (Frame, [Vec<u16>; 3]) {
    let (cw, ch, depth, words) = match pf {
        PixelFormat::Yuv444P => (w, h, 8, false),
        PixelFormat::Yuv422P10Le => (w / 2, h, 10, true),
        PixelFormat::Yuv420P => (w / 2, h / 2, 8, false),
        PixelFormat::Yuv444P16Le => (w, h, 16, true),
        _ => unimplemented!("test format"),
    };
    let mut s = 7u64;
    let mut samples = |pw: usize, ph: usize| -> Vec<u16> {
        let max = (1u64 << depth) - 1;
        (0..pw * ph)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((s >> 40) % (max + 1)) as u16
            })
            .collect()
    };
    let planes: [Vec<u16>; 3] = [samples(w, h), samples(cw, ch), samples(cw, ch)];
    let pack = |vals: &[u16], pw: usize| -> VideoPlane {
        if words {
            let mut data = Vec::with_capacity(vals.len() * 2);
            for &v in vals {
                data.extend_from_slice(&v.to_le_bytes());
            }
            VideoPlane {
                stride: pw * 2,
                data,
            }
        } else {
            VideoPlane {
                stride: pw,
                data: vals.iter().map(|&v| v as u8).collect(),
            }
        }
    };
    let frame = Frame::Video(VideoFrame {
        pts: Some(pts),
        planes: vec![
            pack(&planes[0], w),
            pack(&planes[1], cw),
            pack(&planes[2], cw),
        ],
    });
    (frame, planes)
}

#[test]
fn registry_advertises_and_builds_the_encoder() {
    let mut ctx = RuntimeContext::new();
    oxideav_vc2::register(&mut ctx);
    assert!(ctx.codecs.has_encoder(&CodecId::new("vc2")));
    assert!(ctx
        .codecs
        .encoder_options_schema(&CodecId::new("vc2"))
        .is_some_and(|s| s.iter().any(|f| f.name == "qindex")));
    let mut enc = ctx
        .codecs
        .first_encoder(&enc_params(16, 8, PixelFormat::Yuv444P))
        .expect("factory");
    assert_eq!(enc.codec_id().as_str(), "vc2");
    assert_eq!(enc.output_params().width, Some(16));
    assert!(matches!(enc.receive_packet(), Err(Error::NeedMore)));
}

#[test]
fn lossless_frames_round_trip_through_the_registry_decoder() {
    for pf in [
        PixelFormat::Yuv444P,
        PixelFormat::Yuv422P10Le,
        PixelFormat::Yuv420P,
        PixelFormat::Yuv444P16Le,
    ] {
        let (w, h) = (24usize, 16usize);
        let mut enc = oxideav_vc2::make_encoder(&enc_params(w as u32, h as u32, pf))
            .expect("encoder factory");
        let (frame, planes) = test_frame(w, h, pf, 90);
        enc.send_frame(&frame).expect("send");
        let packet = enc.receive_packet().expect("packet");
        assert!(packet.flags.keyframe);
        assert_eq!(packet.pts, Some(90));
        enc.flush().expect("flush");
        let eos = enc.receive_packet().expect("eos packet");
        assert!(matches!(enc.receive_packet(), Err(Error::Eof)));

        let mut stream = packet.data.clone();
        stream.extend_from_slice(&eos.data);
        let pics = oxideav_vc2::decode_sequence(&stream).expect("decode");
        assert_eq!(pics.len(), 1, "{pf:?}");
        // The wrapper masks input to the coding depth and codes losslessly.
        assert_eq!(pics[0].y, planes[0], "{pf:?} luma");
        assert_eq!(pics[0].c1, planes[1], "{pf:?} c1");
        assert_eq!(pics[0].c2, planes[2], "{pf:?} c2");
    }
}

#[test]
fn options_steer_profile_rate_and_fragments() {
    let mut params = enc_params(64, 48, PixelFormat::Yuv422P10Le);
    params.options = CodecOptions::new()
        .set("profile", "ld")
        .set("picture-bytes", "2500")
        .set("slices-x", "4")
        .set("slices-y", "3")
        .set("fragment-slices", "5");
    let mut enc = oxideav_vc2::make_encoder(&params).expect("factory");
    let (frame, _) = test_frame(64, 48, PixelFormat::Yuv422P10Le, 0);
    enc.send_frame(&frame).expect("send");
    let packet = enc.receive_packet().expect("packet");
    // Setup + 3 data fragments (12 slices / 5 -> 5,5,2), LD parse code.
    let frags = packet
        .data
        .windows(5)
        .filter(|w| &w[..4] == b"BBCD" && w[4] == 0xCC)
        .count();
    assert_eq!(frags, 4);
    enc.flush().expect("flush");
    let mut stream = packet.data;
    stream.extend_from_slice(&enc.receive_packet().expect("eos").data);
    let pics = oxideav_vc2::decode_sequence(&stream).expect("decode");
    assert_eq!(pics.len(), 1);
}

#[test]
fn bit_rate_and_frame_rate_derive_the_picture_target() {
    let mut params = enc_params(64, 48, PixelFormat::Yuv420P);
    params.options = CodecOptions::new()
        .set("profile", "ld")
        .set("slices-x", "4");
    params.bit_rate = Some(600_000);
    params.frame_rate = Some(Rational::new(25, 1));
    let mut enc = oxideav_vc2::make_encoder(&params).expect("factory");
    let (frame, _) = test_frame(64, 48, PixelFormat::Yuv420P, 0);
    enc.send_frame(&frame).expect("send");
    let first = enc.receive_packet().expect("packet");
    // 600 kbit/s at 25 fps = 3000 bytes/picture; the first packet also
    // carries the sequence header.
    let header_len = {
        let next = u32::from_be_bytes(first.data[5..9].try_into().unwrap());
        next as usize
    };
    assert_eq!(first.data.len() - header_len, 3000);
    // A later picture ships without the header at exactly the target.
    enc.send_frame(&frame).expect("send 2");
    let second = enc.receive_packet().expect("packet 2");
    assert_eq!(second.data.len(), 3000);
}

#[test]
fn sequence_per_packet_emits_self_contained_sequences() {
    let mut params = enc_params(16, 8, PixelFormat::Yuv444P);
    params.options = CodecOptions::new().set("sequence-per-packet", "true");
    let mut enc = oxideav_vc2::make_encoder(&params).expect("factory");
    let (frame, _) = test_frame(16, 8, PixelFormat::Yuv444P, 1);
    enc.send_frame(&frame).expect("send");
    enc.send_frame(&frame).expect("send 2");
    for _ in 0..2 {
        let packet = enc.receive_packet().expect("packet");
        // Each packet decodes on its own: header ... EOS.
        let pics = oxideav_vc2::decode_sequence(&packet.data).expect("self-contained");
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].picture_number, 0, "sequences restart numbering");
    }
    // Nothing left for flush to close.
    enc.flush().expect("flush");
    assert!(matches!(enc.receive_packet(), Err(Error::Eof)));
}

#[test]
fn bad_configurations_error_at_construction_or_send() {
    // Unknown option key.
    let mut params = enc_params(16, 8, PixelFormat::Yuv444P);
    params.options = CodecOptions::new().set("bogus", "1");
    assert!(oxideav_vc2::make_encoder(&params).is_err());
    // LD without any byte target.
    let mut params = enc_params(16, 8, PixelFormat::Yuv444P);
    params.options = CodecOptions::new().set("profile", "ld");
    assert!(oxideav_vc2::make_encoder(&params).is_err());
    // Missing dimensions / format.
    assert!(oxideav_vc2::make_encoder(&CodecParameters::video(CodecId::new("vc2"))).is_err());
    // Non-video frame and short planes.
    let mut enc = oxideav_vc2::make_encoder(&enc_params(16, 8, PixelFormat::Yuv444P)).unwrap();
    let bad = Frame::Video(VideoFrame {
        pts: None,
        planes: vec![
            VideoPlane {
                stride: 16,
                data: vec![0; 16],
            };
            3
        ],
    });
    assert!(enc.send_frame(&bad).is_err());
    // After flush the encoder refuses more input.
    let mut enc = oxideav_vc2::make_encoder(&enc_params(16, 8, PixelFormat::Yuv444P)).unwrap();
    enc.flush().expect("flush");
    let (frame, _) = test_frame(16, 8, PixelFormat::Yuv444P, 0);
    assert!(enc.send_frame(&frame).is_err());
}
