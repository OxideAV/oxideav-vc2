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
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, CodecTag, Error, Frame, Packet, ProbeContext,
    RuntimeContext, TimeBase, VideoFrame,
};

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
fn parse_info_fourcc_tag_resolves_to_vc2() {
    // The one tag the staged spec grounds: the parse-info prefix bytes
    // "BBCD" (section 10.5.1, NOTE 1), claimed as a FourCC so a
    // container's CodecResolver can route VC-2 essence tagged by its
    // stream magic.
    let mut ctx = RuntimeContext::new();
    oxideav_vc2::register(&mut ctx);
    let tag = CodecTag::fourcc(b"BBCD");

    // Tag claim alone (no data hints) resolves.
    let id = ctx.codecs.resolve_tag(&ProbeContext::new(&tag));
    assert_eq!(id.expect("resolved").as_str(), "vc2");
    // Case-insensitive FourCC normalization applies to lookups too.
    let lower = CodecTag::fourcc(b"bbcd");
    assert!(ctx.codecs.resolve_tag(&ProbeContext::new(&lower)).is_some());

    // A peeked first packet holding real data units confirms the claim.
    let (stream, _) = simple_stream();
    let pctx = ProbeContext::new(&tag).packet(&stream);
    assert_eq!(
        ctx.codecs.resolve_tag(&pctx).expect("packet").as_str(),
        "vc2"
    );

    // A first packet that cannot be VC-2 data units vetoes it: packets
    // must carry whole data units, each starting with the prefix.
    let not_vc2 = [0u8; 16];
    let pctx = ProbeContext::new(&tag).packet(&not_vc2);
    assert!(ctx.codecs.resolve_tag(&pctx).is_none());

    // An out-of-band sequence header staged as the container's
    // stream-format blob also confirms (the extradata shape
    // Vc2Decoder::new accepts).
    let p = PicParams::hq_depth0();
    let seq = sequence_header_body(2, 2, p.major_version);
    let mut header = Vec::new();
    parse_info(&mut header, 0x00, (13 + seq.len()) as u32, 0);
    header.extend_from_slice(&seq);
    let pctx = ProbeContext::new(&tag).header(&header);
    assert_eq!(
        ctx.codecs.resolve_tag(&pctx).expect("header").as_str(),
        "vc2"
    );

    // Only the FourCC claim exists — no Matroska / WaveFormat / MP4-OTI
    // identifier is declared (none is derivable from staged material).
    let other = CodecTag::matroska("V_SYNTHETIC_TEST_ID");
    assert!(ctx.codecs.resolve_tag(&ProbeContext::new(&other)).is_none());
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
fn depth_derivation_boundary_15_vs_16() {
    // §11.6.3 depth = intlog2(excursion + 1): excursion 32767 is the
    // deepest 15-bit range (promotion shift 1), excursion 32768 already
    // derives depth 16 (verbatim words). Pins the intlog2 boundary on
    // the promotion path.
    let mk = |excursion: u64, offset: u64| SignalRange::Custom {
        luma_offset: offset,
        luma_excursion: excursion,
        color_diff_offset: offset,
        color_diff_excursion: excursion,
    };
    let y = [100i64, -100, 16383, -16384]; // 15-bit clip extremes last
    let c = [0i64; 4];
    let planes = decode_words(mk(32767, 16384), &y, &c, &c);
    assert_eq!(
        planes[0],
        y.map(|v| ((v + 16384) as u16) << 1).to_vec(),
        "depth 15 must promote by << 1"
    );
    assert_eq!(planes[0][2], 65534); // (2^15 - 1) << 1
    let planes = decode_words(mk(32768, 16384), &y, &c, &c);
    assert_eq!(
        planes[0],
        y.map(|v| (v + 32768) as u16).to_vec(),
        "depth 16 must pass words verbatim"
    );
}

/// Decode a single-picture 2x2 4:4:4 HQ depth-0 stream with the given
/// signal range and return the emitted `VideoFrame` whole (image planes
/// plus any attached side-channel).
fn decode_frame(range: SignalRange, y: &[i64; 4], c1: &[i64; 4], c2: &[i64; 4]) -> VideoFrame {
    let p = PicParams::hq_depth0();
    let seq = sequence_header_body_full(2, 2, p.major_version, 0, range);
    let pic = picture_body(&p, 1, &[hq_slice_bytes(p.qindex, y, c1, c2)]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);
    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    dec.send_packet(&packet(stream, 0)).unwrap();
    let Frame::Video(v) = dec.receive_frame().expect("frame") else {
        panic!("expected a video frame");
    };
    v
}

fn le_words(p: &oxideav_core::VideoPlane) -> Vec<u16> {
    p.data
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect()
}

#[test]
fn mixed_12bit_luma_10bit_chroma_carries_a_significant_bits_record() {
    // The headline mixed-depth case: custom §11.4.9 range with a 12-bit
    // luma excursion and a 10-bit chroma excursion. The picture rides
    // the deepest component's natural surface (P12-word planes),
    // verbatim LSB-anchored code values, and the frame carries the
    // per-plane significant-bits side-channel [12, 10, 10].
    let range = SignalRange::Custom {
        luma_offset: 256,
        luma_excursion: 4095,
        color_diff_offset: 512,
        color_diff_excursion: 1023,
    };
    let y = [100i64, -100, 2047, -2048]; // 12-bit clip extremes last
    let c1 = [7i64, -7, 511, -512]; // 10-bit clip extremes last
    let c2 = [1i64, -1, 0, 300];
    let v = decode_frame(range, &y, &c1, &c2);

    // Three image planes plus the trailing side-channel entry; the
    // typed accessors exclude it from the image-plane view.
    assert_eq!(v.planes.len(), 4);
    assert_eq!(v.image_planes().len(), 3);
    assert_eq!(v.significant_bits(), Some(&[12u8, 10, 10][..]));
    assert_eq!(v.plane_significant_bits(0), Some(12));
    assert_eq!(v.plane_significant_bits(1), Some(10));
    assert_eq!(v.plane_significant_bits(2), Some(10));

    // Verbatim §15.5 code values, no promotion shift on either plane.
    assert_eq!(
        le_words(&v.image_planes()[0]),
        y.map(|s| (s + 2048) as u16).to_vec()
    );
    assert_eq!(
        le_words(&v.image_planes()[1]),
        c1.map(|s| (s + 512) as u16).to_vec()
    );
    assert_eq!(
        le_words(&v.image_planes()[2]),
        c2.map(|s| (s + 512) as u16).to_vec()
    );
    // Each plane's full-scale is (1 << b) - 1 for its recorded depth.
    assert_eq!(le_words(&v.image_planes()[0])[2], 4095);
    assert_eq!(le_words(&v.image_planes()[1])[2], 1023);
}

#[test]
fn mixed_sub_byte_depths_ride_byte_planes_with_a_record() {
    // 8-bit luma with 6-bit chroma: the deepest component fits a byte,
    // so the surface is the plain 8-bit format — byte planes, record
    // [8, 6, 6], chroma full-scale 63.
    let range = SignalRange::Custom {
        luma_offset: 0,
        luma_excursion: 255,
        color_diff_offset: 32,
        color_diff_excursion: 63,
    };
    let y = [10i64, -10, 127, -128];
    let c = [5i64, -5, 31, -32]; // 6-bit clip extremes last
    let v = decode_frame(range, &y, &c, &c);
    assert_eq!(v.image_planes().len(), 3);
    assert_eq!(v.significant_bits(), Some(&[8u8, 6, 6][..]));
    // Byte-per-sample planes (stride == width), values LSB-anchored.
    assert_eq!(v.image_planes()[0].stride, 2);
    assert_eq!(
        v.image_planes()[0].data,
        y.map(|s| (s + 128) as u8).to_vec()
    );
    assert_eq!(v.image_planes()[1].data, c.map(|s| (s + 32) as u8).to_vec());
    assert_eq!(v.image_planes()[1].data[2], 63); // 6-bit full-scale
}

#[test]
fn uniform_9bit_stream_is_represented_on_p10_words() {
    // Equal 9-bit components (excursion 511): no exact format exists,
    // so the picture rides the P10-word surface with a [9, 9, 9]
    // record instead of being refused or promoted.
    let range = SignalRange::Custom {
        luma_offset: 256,
        luma_excursion: 511,
        color_diff_offset: 256,
        color_diff_excursion: 511,
    };
    let y = [100i64, -100, 255, -256]; // 9-bit clip extremes last
    let c = [0i64; 4];
    let v = decode_frame(range, &y, &c, &c);
    assert_eq!(v.significant_bits(), Some(&[9u8, 9, 9][..]));
    assert_eq!(
        le_words(&v.image_planes()[0]),
        y.map(|s| (s + 256) as u16).to_vec()
    );
    assert_eq!(le_words(&v.image_planes()[0])[2], 511); // 9-bit full-scale
    assert!(le_words(&v.image_planes()[1]).iter().all(|&w| w == 256));
}

#[test]
fn hostile_boundary_depths_still_decode_with_faithful_records() {
    // Depth 1 (excursion 1) on both components — the shallowest legal
    // signal range. Byte planes, record [1, 1, 1], code values 0/1.
    let range = SignalRange::Custom {
        luma_offset: 0,
        luma_excursion: 1,
        color_diff_offset: 1,
        color_diff_excursion: 1,
    };
    let y = [0i64, 1, -1, 2]; // clips to 0..=1 around offset 0
    let c = [0i64; 4];
    let v = decode_frame(range, &y, &c, &c);
    assert_eq!(v.significant_bits(), Some(&[1u8, 1, 1][..]));
    assert!(v.image_planes()[0].data.iter().all(|&b| b <= 1));

    // Maximal legal spread below the promotion cut: 12-bit luma with
    // 1-bit chroma shares the P12 surface under one record.
    let range = SignalRange::Custom {
        luma_offset: 2048,
        luma_excursion: 4095,
        color_diff_offset: 0,
        color_diff_excursion: 1,
    };
    let v = decode_frame(range, &y, &c, &c);
    assert_eq!(v.significant_bits(), Some(&[12u8, 1, 1][..]));
    assert!(le_words(&v.image_planes()[1]).iter().all(|&w| w <= 1));
}

#[test]
fn uniform_and_promoted_streams_attach_no_record() {
    // Uniform 10-bit (Table 10 preset 3): exact format, no side-channel
    // — the frame is byte-identical to previous releases.
    let y = [100i64, -100, 0, 511];
    let c = [0i64; 4];
    let v = decode_frame(SignalRange::Preset(3), &y, &c, &c);
    assert_eq!(v.planes.len(), 3);
    assert_eq!(v.significant_bits(), None);

    // Promoted >12-bit mix (16-bit luma / 12-bit chroma): the per-plane
    // promotion shift is the representation; no record is attached and
    // the plane bytes stay exactly as before (chroma << 4).
    let range = SignalRange::Custom {
        luma_offset: 0,
        luma_excursion: 65535,
        color_diff_offset: 2048,
        color_diff_excursion: 4095,
    };
    let yd = [-32768i64, 32767, 5, -5];
    let cd = [100i64, -100, 2047, -2048];
    let v = decode_frame(range, &yd, &cd, &cd);
    assert_eq!(v.planes.len(), 3);
    assert_eq!(v.significant_bits(), None);
    assert_eq!(
        le_words(&v.planes[0]),
        yd.map(|s| (s + 32768) as u16).to_vec()
    );
    assert_eq!(
        le_words(&v.planes[1]),
        cd.map(|s| ((s + 2048) as u16) << 4).to_vec()
    );
}

#[test]
fn record_presence_switches_across_concatenated_sequences() {
    // A mixed-depth sequence followed by a uniform 8-bit sequence from
    // the same decoder: the record must appear on the first frame and
    // be absent from the second (per-picture surface selection).
    let p = PicParams::hq_depth0();
    let mixed = SignalRange::Custom {
        luma_offset: 256,
        luma_excursion: 4095,
        color_diff_offset: 512,
        color_diff_excursion: 1023,
    };
    let y = [1i64, -1, 2, -2];
    let c = [0i64; 4];
    let mut stream = build_units(&[
        (
            0x00,
            sequence_header_body_full(2, 2, p.major_version, 0, mixed),
        ),
        (
            0xE8,
            picture_body(&p, 1, &[hq_slice_bytes(p.qindex, &y, &c, &c)]),
        ),
    ]);
    stream.extend_from_slice(&build_units(&[
        (0x00, sequence_header_body(2, 2, p.major_version)),
        (
            0xE8,
            picture_body(&p, 2, &[hq_slice_bytes(p.qindex, &y, &c, &c)]),
        ),
    ]));

    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    dec.send_packet(&packet(stream, 0)).unwrap();
    let Frame::Video(first) = dec.receive_frame().expect("first frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(first.significant_bits(), Some(&[12u8, 10, 10][..]));
    assert_eq!(first.image_planes().len(), 3);
    let Frame::Video(second) = dec.receive_frame().expect("second frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(second.significant_bits(), None);
    assert_eq!(second.planes.len(), 3);
    assert_eq!(second.planes[0].stride, 2); // back on byte planes
}

#[test]
fn fragmented_mixed_depth_picture_carries_the_record_once_complete() {
    // Fragment reassembly (§14) composes with the side-channel: the
    // record rides the single frame emitted when the last data fragment
    // lands, not any intermediate state.
    let p = PicParams {
        major_version: 3,
        slices_x: 2,
        slices_y: 2,
        ..PicParams::hq_depth0()
    };
    let c = [0i64; 4];
    let slices: Vec<Vec<u8>> = (0..4)
        .map(|i| hq_slice_bytes(p.qindex, &[i as i64; 4], &c, &c))
        .collect();
    let mixed = SignalRange::Custom {
        luma_offset: 256,
        luma_excursion: 4095,
        color_diff_offset: 512,
        color_diff_excursion: 1023,
    };
    let unit = |code: u8, body: Vec<u8>| -> Vec<u8> {
        let mut out = Vec::new();
        parse_info(&mut out, code, (13 + body.len()) as u32, 0);
        out.extend_from_slice(&body);
        out
    };
    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    let seq = sequence_header_body_full(4, 4, p.major_version, 0, mixed);
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
    assert_eq!(v.significant_bits(), Some(&[12u8, 10, 10][..]));
    assert_eq!(v.image_planes().len(), 3);
    assert_eq!(v.image_planes()[0].stride, 8); // 4 samples * 2 bytes
    assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
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
fn depth_switch_across_sequences_repacks_frames() {
    // A VC-2 stream may concatenate sequences (§10.3) with different
    // video parameters. The surface mapping is chosen per picture, so
    // an 8-bit sequence followed by a 16-bit one must yield a byte
    // frame then a word frame from the same decoder instance.
    let p = PicParams::hq_depth0();
    let y8 = [10i64, -20, 30, -40];
    let y16 = [-30000i64, 30000, 0, 1];
    let c = [0i64; 4];
    let mut stream = build_units(&[
        (0x00, sequence_header_body(2, 2, p.major_version)),
        (
            0xE8,
            picture_body(&p, 1, &[hq_slice_bytes(p.qindex, &y8, &c, &c)]),
        ),
    ]);
    stream.extend_from_slice(&build_units(&[
        (
            0x00,
            sequence_header_body_full(2, 2, p.major_version, 0, SignalRange::Preset(8)),
        ),
        (
            0xE8,
            picture_body(&p, 2, &[hq_slice_bytes(p.qindex, &y16, &c, &c)]),
        ),
    ]));

    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    dec.send_packet(&packet(stream, 0)).unwrap();

    let Frame::Video(first) = dec.receive_frame().expect("first frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(first.planes[0].stride, 2); // 8-bit: byte planes
    let expect8: Vec<u8> = y8.iter().map(|&v| (v + 128) as u8).collect();
    assert_eq!(first.planes[0].data, expect8);

    let Frame::Video(second) = dec.receive_frame().expect("second frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(second.planes[0].stride, 4); // 16-bit: LE word planes
    let words: Vec<u16> = second.planes[0]
        .data
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    assert_eq!(words, y16.map(|v| (v + 32768) as u16).to_vec());
}

#[test]
fn extradata_16bit_sequence_header_primes_the_decoder() {
    // A container may stage the 16-bit sequence header out of band; the
    // first packet then carries only the picture data unit and the
    // frame must come out as P16Le words.
    let p = PicParams::hq_depth0();
    let seq = sequence_header_body_full(2, 2, p.major_version, 0, SignalRange::Preset(7));
    let mut extradata = Vec::new();
    parse_info(&mut extradata, 0x00, (13 + seq.len()) as u32, 0);
    extradata.extend_from_slice(&seq);

    let mut params = vc2_params();
    params.extradata = extradata;
    let mut dec = oxideav_vc2::make_decoder(&params).expect("factory");

    let y = [4096i64, -4096, 0, 27392];
    let c = [0i64; 4];
    let pic = picture_body(&p, 1, &[hq_slice_bytes(p.qindex, &y, &c, &c)]);
    let mut pkt = Vec::new();
    parse_info(&mut pkt, 0xE8, (13 + pic.len()) as u32, 0);
    pkt.extend_from_slice(&pic);
    dec.send_packet(&packet(pkt, 0)).unwrap();
    let Frame::Video(v) = dec.receive_frame().expect("frame") else {
        panic!("expected a video frame");
    };
    let words: Vec<u16> = v.planes[0]
        .data
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    assert_eq!(words, y.map(|v| (v + 32768) as u16).to_vec());
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

#[test]
fn low_delay_mixed_depth_picture_carries_the_record() {
    // The low-delay slice path (§13.5.3) composes with the mixed-depth
    // representation: an LD picture under a 12/10 custom range reaches
    // the frame as verbatim P12-word planes matching the standalone
    // decode exactly, with the [12, 10, 10] record attached.
    use common::ld_slice_bytes;
    let p = PicParams {
        slices_x: 2,
        slices_y: 1,
        low_delay: true,
        slice_bytes_numerator: 16,
        slice_bytes_denominator: 1,
        ..PicParams::hq_depth0()
    };
    let range = SignalRange::Custom {
        luma_offset: 256,
        luma_excursion: 3504,
        color_diff_offset: 512,
        color_diff_excursion: 896,
    };
    let c = [5i64, -5, 7, -7];
    let left = [300i64, 150, -100, 20];
    let right = [1200i64, -1300, 3, -3];
    let slices = vec![
        ld_slice_bytes(p.qindex, p.ld_slice_bytes_len(0), &left, &c, &c),
        ld_slice_bytes(p.qindex, p.ld_slice_bytes_len(1), &right, &c, &c),
    ];
    let seq = sequence_header_body_full(4, 2, p.major_version, 0, range);
    let stream = build_units(&[(0x00, seq), (0xC8, picture_body(&p, 5, &slices))]);

    let pics = oxideav_vc2::decode_sequence(&stream).expect("standalone decode");
    assert_eq!(pics[0].luma_depth, 12);
    assert_eq!(pics[0].color_diff_depth, 10);

    let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
    dec.send_packet(&packet(stream, 0)).unwrap();
    let Frame::Video(v) = dec.receive_frame().expect("frame") else {
        panic!("expected a video frame");
    };
    assert_eq!(v.significant_bits(), Some(&[12u8, 10, 10][..]));
    assert_eq!(v.image_planes().len(), 3);
    for (plane, samples) in v
        .image_planes()
        .iter()
        .zip([&pics[0].y, &pics[0].c1, &pics[0].c2])
    {
        assert_eq!(le_words(plane), *samples);
    }
}

#[test]
fn corrupted_mixed_depth_streams_yield_well_formed_frames_or_errors() {
    // Flip every bit of a valid mixed 12/10 stream and push each mutant
    // through a fresh Decoder. Flips inside the §11.4.9 range fields
    // derive arbitrary depth pairs within the 1..=16 contract, so this
    // sweeps the whole surface-mapping table under hostile input: every
    // send/receive must return promptly, and any frame that does come
    // out must be well-formed — exactly three image planes, and any
    // attached significant-bits record covering them with per-plane
    // depths inside 1..=16.
    let p = PicParams::hq_depth0();
    let range = SignalRange::Custom {
        luma_offset: 256,
        luma_excursion: 3504,
        color_diff_offset: 512,
        color_diff_excursion: 896,
    };
    let seq = sequence_header_body_full(2, 2, p.major_version, 0, range);
    let y = [100i64, -100, 1200, -256];
    let c = [200i64, -200, 1, -1];
    let pic = picture_body(&p, 7, &[hq_slice_bytes(p.qindex, &y, &c, &c)]);
    let stream = build_units(&[(0x00, seq), (0xE8, pic)]);

    for byte in 0..stream.len() {
        let all = [0u8, 1, 2, 3, 4, 5, 6, 7];
        let one = [(byte % 8) as u8];
        let bits: &[u8] = if cfg!(miri) { &one } else { &all };
        for &bit in bits {
            let mut corrupt = stream.clone();
            corrupt[byte] ^= 1 << bit;
            let mut dec = oxideav_vc2::make_decoder(&vc2_params()).expect("factory");
            if dec.send_packet(&packet(corrupt, 0)).is_err() {
                continue;
            }
            while let Ok(frame) = dec.receive_frame() {
                let Frame::Video(v) = frame else {
                    panic!("byte {byte} bit {bit}: non-video frame");
                };
                assert_eq!(
                    v.image_planes().len(),
                    3,
                    "byte {byte} bit {bit}: wrong image-plane count"
                );
                if let Some(bits) = v.significant_bits() {
                    assert_eq!(bits.len(), 3, "byte {byte} bit {bit}: record length");
                    assert!(
                        bits.iter().all(|&b| (1..=16).contains(&b)),
                        "byte {byte} bit {bit}: out-of-contract record {bits:?}"
                    );
                }
            }
        }
    }
}
