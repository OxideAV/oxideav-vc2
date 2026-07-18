//! `oxideav-core` Decoder wiring tests: the `register(ctx)` path, the
//! direct `make_decoder` factory (dual-API convention), packetized
//! fragment reassembly through the packet/frame contract, and the
//! pixel-format / plane-packing surface of the emitted frames.

#![cfg(feature = "registry")]

mod common;

use common::{
    build_units, fragment_data_body, fragment_setup_body, hq_slice_bytes, parse_info, picture_body,
    sequence_header_body, sequence_header_body_full, PicParams, SignalRange,
};
use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, RuntimeContext, TimeBase};

fn vc2_params() -> CodecParameters {
    CodecParameters::video(CodecId::new("vc2"))
}

fn packet(data: Vec<u8>, pts: i64) -> Packet {
    Packet::new(0, TimeBase::MILLIS, data).with_pts(pts)
}

/// A single-picture 2x2 8-bit stream and its expected luma bytes.
fn simple_stream() -> (Vec<u8>, Vec<u8>) {
    let p = PicParams::hq_depth0();
    let y = [10i64, -20, 30, -40];
    let c = [0i64; 4];
    let seq = sequence_header_body(2, 2, p.major_version);
    let pic = picture_body(&p, 7, &[hq_slice_bytes(p.qindex, &y, &c, &c)]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);
    let expect: Vec<u8> = y.iter().map(|&v| (v + 128) as u8).collect();
    (stream, expect)
}

#[test]
fn registry_round_trip_via_register() {
    let mut ctx = RuntimeContext::new();
    oxideav_vc2::register(&mut ctx);
    assert!(ctx.codecs.has_decoder(&CodecId::new("vc2")));

    let mut dec = ctx.codecs.first_decoder(&vc2_params()).expect("factory");
    assert_eq!(dec.codec_id().as_str(), "vc2");

    let (stream, expect_y) = simple_stream();
    dec.send_packet(&packet(stream, 40)).expect("send");
    let frame = dec.receive_frame().expect("frame");
    let Frame::Video(v) = frame else {
        panic!("expected a video frame");
    };
    assert_eq!(v.pts, Some(40));
    assert_eq!(v.planes.len(), 3);
    assert_eq!(v.planes[0].stride, 2);
    assert_eq!(v.planes[0].data, expect_y);
    assert!(v.planes[1].data.iter().all(|&b| b == 128));

    // No more frames buffered; flush turns starvation into Eof.
    assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
    dec.flush().expect("flush");
    assert!(matches!(dec.receive_frame(), Err(Error::Eof)));
}

#[test]
fn direct_make_decoder_factory() {
    // The dual-API convention: the factory is callable without a registry.
    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    let (stream, expect_y) = simple_stream();
    dec.send_packet(&packet(stream, 0)).expect("send");
    let Frame::Video(v) = dec.receive_frame().expect("frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(v.planes[0].data, expect_y);
}

#[test]
fn fragmented_picture_spans_packets() {
    // One data unit per packet: seq header, setup fragment, two data
    // fragments. The frame appears only once the last fragment arrives.
    let p = PicParams {
        major_version: 3,
        slices_x: 2,
        slices_y: 2,
        ..PicParams::hq_depth0()
    };
    let c = [0i64; 4];
    let slices: Vec<Vec<u8>> = (0..4)
        .map(|i| hq_slice_bytes(p.qindex, &[i as i64 + 1; 4], &c, &c))
        .collect();

    let unit = |code: u8, body: Vec<u8>| -> Vec<u8> {
        let mut out = Vec::new();
        parse_info(&mut out, code, (13 + body.len()) as u32, 0);
        out.extend_from_slice(&body);
        out
    };

    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    dec.send_packet(&packet(unit(0x00, sequence_header_body(4, 4, 3)), 1))
        .unwrap();
    dec.send_packet(&packet(unit(0xEC, fragment_setup_body(&p, 9)), 2))
        .unwrap();
    assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
    dec.send_packet(&packet(
        unit(0xEC, fragment_data_body(9, 0, 0, &slices[0..2])),
        3,
    ))
    .unwrap();
    assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
    dec.send_packet(&packet(
        unit(0xEC, fragment_data_body(9, 0, 1, &slices[2..4])),
        4,
    ))
    .unwrap();
    let Frame::Video(v) = dec.receive_frame().expect("frame") else {
        panic!("expected a video frame");
    };
    // The frame carries the pts of the packet that completed it.
    assert_eq!(v.pts, Some(4));
    // Quadrant layout: slice k filled a 2x2 block with value k+1 (+128).
    assert_eq!(
        v.planes[0].data,
        vec![
            129, 129, 130, 130, //
            129, 129, 130, 130, //
            131, 131, 132, 132, //
            131, 131, 132, 132,
        ]
    );

    // reset() drops carry-over state so the decoder can be reused.
    dec.reset().expect("reset");
    let (stream, expect_y) = simple_stream();
    dec.send_packet(&packet(stream, 5)).unwrap();
    let Frame::Video(v) = dec.receive_frame().expect("frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(v.planes[0].data, expect_y);
}

#[test]
fn fragmented_16bit_picture_spans_packets() {
    // One data unit per packet at video depth 16 (Table 10 preset 8):
    // the P16Le frame appears only once the last fragment arrives, with
    // verbatim word packing across the reassembled slices.
    let p = PicParams {
        major_version: 3,
        slices_x: 2,
        slices_y: 2,
        ..PicParams::hq_depth0()
    };
    let c = [0i64; 4];
    let values: [i64; 4] = [-30000, 30000, -1, 1];
    let slices: Vec<Vec<u8>> = values
        .iter()
        .map(|&v| hq_slice_bytes(p.qindex, &[v; 4], &c, &c))
        .collect();

    let unit = |code: u8, body: Vec<u8>| -> Vec<u8> {
        let mut out = Vec::new();
        parse_info(&mut out, code, (13 + body.len()) as u32, 0);
        out.extend_from_slice(&body);
        out
    };

    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    let seq = sequence_header_body_full(4, 4, p.major_version, 0, SignalRange::Preset(8));
    dec.send_packet(&packet(unit(0x00, seq), 1)).unwrap();
    dec.send_packet(&packet(unit(0xEC, fragment_setup_body(&p, 9)), 2))
        .unwrap();
    dec.send_packet(&packet(
        unit(0xEC, fragment_data_body(9, 0, 0, &slices[0..2])),
        3,
    ))
    .unwrap();
    assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
    dec.send_packet(&packet(
        unit(0xEC, fragment_data_body(9, 0, 1, &slices[2..4])),
        4,
    ))
    .unwrap();
    let Frame::Video(v) = dec.receive_frame().expect("frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(v.planes[0].stride, 8); // 4 samples/row * 2 bytes
    let words: Vec<u16> = v.planes[0]
        .data
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    let q = values.map(|v| (v + 32768) as u16);
    #[rustfmt::skip]
    let expect = vec![
        q[0], q[0], q[1], q[1],
        q[0], q[0], q[1], q[1],
        q[2], q[2], q[3], q[3],
        q[2], q[2], q[3], q[3],
    ];
    assert_eq!(words, expect);
    assert!(v.planes[1]
        .data
        .chunks_exact(2)
        .all(|b| u16::from_le_bytes([b[0], b[1]]) == 32768));
}

#[test]
fn flush_mid_fragmented_picture_errors() {
    let p = PicParams {
        major_version: 3,
        slices_x: 2,
        slices_y: 2,
        ..PicParams::hq_depth0()
    };
    let mut unit = Vec::new();
    let seq = sequence_header_body(4, 4, 3);
    parse_info(&mut unit, 0x00, (13 + seq.len()) as u32, 0);
    unit.extend_from_slice(&seq);
    let setup = fragment_setup_body(&p, 9);
    parse_info(&mut unit, 0xEC, (13 + setup.len()) as u32, 0);
    unit.extend_from_slice(&setup);

    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    dec.send_packet(&packet(unit, 0)).unwrap();
    assert!(matches!(dec.flush(), Err(Error::InvalidData(_))));
    // The failed flush dropped the partial picture; reset re-arms input.
    dec.reset().expect("reset");
    let (stream, _) = simple_stream();
    dec.send_packet(&packet(stream, 0)).unwrap();
    assert!(dec.receive_frame().is_ok());
}

/// Decode a single-picture 2x2 4:4:4 HQ depth-0 stream with the given
/// signal range and return the three planes as LE 16-bit words.
fn decode_words(range: SignalRange, y: &[i64; 4], c1: &[i64; 4], c2: &[i64; 4]) -> Vec<Vec<u16>> {
    let p = PicParams::hq_depth0();
    let seq = sequence_header_body_full(2, 2, p.major_version, 0, range);
    let pic = picture_body(&p, 1, &[hq_slice_bytes(p.qindex, y, c1, c2)]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);
    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    dec.send_packet(&packet(stream, 0)).unwrap();
    let Frame::Video(v) = dec.receive_frame().expect("frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(v.planes.len(), 3);
    v.planes
        .iter()
        .map(|p| {
            assert_eq!(p.stride, 4); // 2 samples/row * 2 bytes
            p.data
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect()
        })
        .collect()
}

#[test]
fn ten_bit_stream_packs_little_endian_words() {
    // Same 2x2 picture but with signal-range preset 3 (10-bit video
    // levels): depth = intlog2(877) = 10, so planes are 16-bit LE words
    // and the offset is +512.
    let y = [100i64, -100, 0, 511];
    let c = [0i64; 4];
    let planes = decode_words(SignalRange::Preset(3), &y, &c, &c);
    let expect: Vec<u16> = y.iter().map(|&s| (s + 512) as u16).collect();
    assert_eq!(planes[0], expect);
    // Chroma: 0 + 512 offset in 16-bit LE words.
    assert!(planes[1].iter().all(|&w| w == 512));
    assert!(planes[2].iter().all(|&w| w == 512));
}

#[test]
fn preset7_16bit_video_stream_decodes_verbatim_words() {
    // Table 10 preset 7 (16-bit video levels): luma excursion 56064 and
    // chroma excursion 57344 both derive video depth intlog2(·+1) = 16,
    // so the picture rides Yuv444P16Le with no promotion shift and the
    // §15.5 offset is +32768 on every component.
    let y = [1000i64, -1000, 0, 27392]; // 27392 + 32768 = 4096 + 56064
    let c1 = [-32768i64, 32767, 0, 1]; // clip-range extremes survive
    let c2 = [24576i64, -24576, 0, -1];
    let planes = decode_words(SignalRange::Preset(7), &y, &c1, &c2);
    let off = |v: i64| (v + 32768) as u16;
    assert_eq!(planes[0], y.map(off).to_vec());
    assert_eq!(planes[1], c1.map(off).to_vec());
    assert_eq!(planes[2], c2.map(off).to_vec());
    // The signalled nominal white (offset 4096 + excursion 56064) is a
    // representable output word untouched by any scaling.
    assert_eq!(planes[0][3], 60160);
}

#[test]
fn preset8_16bit_full_range_reaches_both_extremes() {
    // Table 10 preset 8 (16-bit full range): depth 16, offset +32768.
    // The clip range is [-32768, 32767], so the decoded words span the
    // whole 16-bit lattice — 0 and 65535 are both reachable, which is
    // exactly the "all 16 bits significant" contract of the P16Le
    // formats.
    let y = [-32768i64, 32767, -40000, 40000]; // last two clip at §15.5
    let c = [0i64; 4];
    let planes = decode_words(SignalRange::Preset(8), &y, &c, &c);
    assert_eq!(planes[0], vec![0, 65535, 0, 65535]);
    assert!(planes[1].iter().all(|&w| w == 32768));
}

#[test]
fn custom_13bit_range_promotes_by_shift_3() {
    // Custom §11.4.9 range with excursion 8191: depth = intlog2(8192) =
    // 13 on both components, so there is no exact planar format and the
    // picture is promoted onto Yuv444P16Le. Every output word is the
    // §15.5 code value (v + 4096) shifted left by 16 - 13 = 3 — the same
    // ×2^k scaling Table 10 applies between its own presets.
    let range = SignalRange::Custom {
        luma_offset: 512,
        luma_excursion: 8191,
        color_diff_offset: 4096,
        color_diff_excursion: 8191,
    };
    let y = [100i64, -100, 0, 4095]; // 4095 is the positive clip bound
    let c = [7i64, -7, 4095, -4096];
    let planes = decode_words(range, &y, &c, &c);
    let promote = |v: i64| ((v + 4096) as u16) << 3;
    assert_eq!(planes[0], y.map(promote).to_vec());
    assert_eq!(planes[1], c.map(promote).to_vec());
    // Extremes of the promoted lattice: 0 and (2^13 - 1) << 3 = 65528.
    assert_eq!(planes[1][2], 65528);
    assert_eq!(planes[1][3], 0);
}

#[test]
fn mixed_deep_depths_promote_per_plane() {
    // Luma excursion 65535 (depth 16) with chroma excursion 4095 (depth
    // 12): the deeper component forces the 16-bit surface; luma words
    // pass verbatim while chroma promotes by 16 - 12 = 4.
    let range = SignalRange::Custom {
        luma_offset: 0,
        luma_excursion: 65535,
        color_diff_offset: 2048,
        color_diff_excursion: 4095,
    };
    let y = [-32768i64, 32767, 5, -5];
    let c = [100i64, -100, 2047, -2048];
    let planes = decode_words(range, &y, &c, &c);
    assert_eq!(planes[0], y.map(|v| (v + 32768) as u16).to_vec());
    assert_eq!(planes[1], c.map(|v| ((v + 2048) as u16) << 4).to_vec());
    assert_eq!(planes[1][2], 65520); // (2^12 - 1) << 4
}

#[test]
fn shallow_mixed_depths_stay_unsupported() {
    // Luma 10-bit with chroma 8-bit: no exact format, and no component
    // needs the 16-bit surface — the wrapper must refuse rather than
    // silently promote into a deeper format's significant bits.
    let range = SignalRange::Custom {
        luma_offset: 0,
        luma_excursion: 1023,
        color_diff_offset: 128,
        color_diff_excursion: 255,
    };
    let p = PicParams::hq_depth0();
    let seq = sequence_header_body_full(2, 2, p.major_version, 0, range);
    let c = [0i64; 4];
    let pic = picture_body(&p, 1, &[hq_slice_bytes(p.qindex, &c, &c, &c)]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);
    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    assert!(matches!(
        dec.send_packet(&packet(stream, 0)),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn preset7_422_subsampling_maps_to_yuv422p16() {
    // 2x2 luma with 4:2:2 sampling: 1x2 chroma planes on the 16-bit
    // surface (Yuv422P16Le), stride 2 bytes per chroma row.
    let p = PicParams::hq_depth0();
    let seq = sequence_header_body_full(2, 2, p.major_version, 1, SignalRange::Preset(7));
    let y = [10i64, -10, 20, -20];
    let c1 = [300i64, -300];
    let c2 = [1i64, -1];
    let pic = picture_body(&p, 1, &[hq_slice_bytes(p.qindex, &y, &c1, &c2)]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);
    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    dec.send_packet(&packet(stream, 0)).unwrap();
    let Frame::Video(v) = dec.receive_frame().expect("frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(v.planes[0].stride, 4);
    assert_eq!(v.planes[1].stride, 2);
    assert_eq!(v.planes[1].data.len(), 4); // 1x2 samples, 2 bytes each
    let words = |p: &oxideav_core::VideoPlane| -> Vec<u16> {
        p.data
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect()
    };
    assert_eq!(words(&v.planes[1]), vec![300 + 32768, 32768 - 300]);
    assert_eq!(words(&v.planes[2]), vec![32769, 32767]);
}

#[test]
fn extradata_sequence_header_primes_the_decoder() {
    // A container may stage the sequence header in extradata; the first
    // packet can then hold just the picture data unit.
    let p = PicParams::hq_depth0();
    let y = [1i64, 2, 3, 4];
    let c = [0i64; 4];
    let seq = sequence_header_body(2, 2, p.major_version);
    let mut extradata = Vec::new();
    parse_info(&mut extradata, 0x00, (13 + seq.len()) as u32, 0);
    extradata.extend_from_slice(&seq);

    let mut params = vc2_params();
    params.extradata = extradata;
    let mut dec = oxideav_vc2::make_decoder(&params).expect("factory");

    let pic = picture_body(&p, 1, &[hq_slice_bytes(p.qindex, &y, &c, &c)]);
    let mut pkt = Vec::new();
    parse_info(&mut pkt, 0xE8, (13 + pic.len()) as u32, 0);
    pkt.extend_from_slice(&pic);
    dec.send_packet(&packet(pkt, 0)).unwrap();
    let Frame::Video(v) = dec.receive_frame().expect("frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(v.planes[0].data, vec![129, 130, 131, 132]);
}
