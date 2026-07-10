//! `oxideav-core` Decoder wiring tests: the `register(ctx)` path, the
//! direct `make_decoder` factory (dual-API convention), packetized
//! fragment reassembly through the packet/frame contract, and the
//! pixel-format / plane-packing surface of the emitted frames.

#![cfg(feature = "registry")]

mod common;

use common::{
    build_units, fragment_data_body, fragment_setup_body, hq_slice_bytes, parse_info, picture_body,
    sequence_header_body, BitWriter, PicParams,
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

#[test]
fn ten_bit_stream_packs_little_endian_words() {
    // Same 2x2 picture but with signal-range preset 3 (10-bit video
    // levels): depth = intlog2(877) = 10, so planes are 16-bit LE words
    // and the offset is +512.
    let mut w = BitWriter::default();
    // parse_parameters: major=2, minor=0, profile=0, level=0.
    w.put_uint(2);
    w.put_uint(0);
    w.put_uint(0);
    w.put_uint(0);
    // base_video_format = 0 (custom), explicit 2x2 frame size, 4:4:4.
    w.put_uint(0);
    w.put_bool(true);
    w.put_uint(2);
    w.put_uint(2);
    w.put_bool(true);
    w.put_uint(0);
    // scan_format / frame_rate / pixel_aspect_ratio / clean_area: defaults.
    w.put_bool(false);
    w.put_bool(false);
    w.put_bool(false);
    w.put_bool(false);
    // signal_range: preset index 3 (10-bit video range).
    w.put_bool(true);
    w.put_uint(3);
    // color_spec: default; picture_coding_mode = 0.
    w.put_bool(false);
    w.put_uint(0);
    let seq = w.into_bytes();

    let p = PicParams::hq_depth0();
    let y = [100i64, -100, 0, 511];
    let c = [0i64; 4];
    let pic = picture_body(&p, 1, &[hq_slice_bytes(p.qindex, &y, &c, &c)]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);

    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    dec.send_packet(&packet(stream, 0)).unwrap();
    let Frame::Video(v) = dec.receive_frame().expect("frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(v.planes[0].stride, 4); // 2 samples/row * 2 bytes
    let words: Vec<u16> = v.planes[0]
        .data
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    let expect: Vec<u16> = y.iter().map(|&s| (s + 512) as u16).collect();
    assert_eq!(words, expect);
    // Chroma: 0 + 512 offset in 16-bit LE words.
    let cwords: Vec<u16> = v.planes[1]
        .data
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    assert!(cwords.iter().all(|&w| w == 512));
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
