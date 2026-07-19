//! `oxideav-core` integration: a [`Decoder`] wrapping the stateful
//! [`SequenceDecoder`], the [`make_decoder`] factory, and the
//! [`register`] entry point that installs the factory into a
//! [`RuntimeContext`] codec registry.
//!
//! Available behind the default-on `registry` cargo feature; the
//! standalone build path ([`crate::decode_sequence`] /
//! [`crate::SequenceDecoder`]) has no `oxideav-core` dependency.
//!
//! ## Packetization
//!
//! VC-2's own parse-info headers (§10.5) are the framing: each
//! [`Packet`] must carry whole data units (each beginning with a "BBCD"
//! parse-info header). Any split at data-unit boundaries works — one
//! packet per picture, per fragment, or a whole sequence per packet —
//! because the wrapped [`SequenceDecoder`] keeps the sequence header and
//! any partially assembled fragmented picture across packets.

use std::collections::VecDeque;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, Decoder, Frame, Packet, PixelFormat,
    RuntimeContext, VideoFrame, VideoPlane,
};

use crate::picture::DecodedPicture;
use crate::sequence::SequenceDecoder;
use crate::PARSE_INFO_PREFIX;

/// Registry identifier this crate claims.
pub const CODEC_ID: &str = "vc2";

/// VC-2 decoder speaking the `oxideav-core` [`Decoder`] packet/frame
/// contract. Construct via [`make_decoder`] (or through a registry
/// populated by [`register`]).
pub struct Vc2Decoder {
    codec_id: CodecId,
    walker: SequenceDecoder,
    pending: VecDeque<Frame>,
    flushed: bool,
}

impl Vc2Decoder {
    /// Build a decoder. If `params.extradata` starts with a parse-info
    /// prefix it is fed through the stream walker first (containers may
    /// stage the sequence header there); other extradata shapes are
    /// ignored and the sequence header is expected in-band instead.
    pub fn new(params: &CodecParameters) -> oxideav_core::Result<Self> {
        let mut walker = SequenceDecoder::new();
        if params.extradata.starts_with(&PARSE_INFO_PREFIX) {
            walker
                .push(&params.extradata)
                .map_err(map_err)
                .map_err(|e| oxideav_core::Error::invalid(format!("vc2: bad extradata: {e}")))?;
        }
        Ok(Vc2Decoder {
            codec_id: params.codec_id.clone(),
            walker,
            pending: VecDeque::new(),
            flushed: false,
        })
    }
}

/// Map a crate error onto the shared error type.
fn map_err(e: crate::Error) -> oxideav_core::Error {
    match e {
        crate::Error::Unsupported(_) => oxideav_core::Error::unsupported(e.to_string()),
        _ => oxideav_core::Error::invalid(e.to_string()),
    }
}

/// How a decoded picture maps onto a core [`PixelFormat`] surface.
///
/// For pictures whose two component depths are equal and one of 8/10/12,
/// the §15.5 code values are carried verbatim: the 8-bit formats hold
/// bytes and the `P10Le`/`P12Le` formats keep the significant bits in
/// the low end of each 16-bit word. No side-channel is attached — these
/// frames are byte-for-byte what they always were.
///
/// Every other depth pair at or below 12 bits — mixed pairs such as
/// 12-bit luma with 10-bit chroma, and the odd uniform depths (9, 11,
/// or anything derived from a custom §11.4.9 excursion) that no exact
/// format names — is **represented, not promoted**: the picture rides
/// the natural common storage surface for its *deepest* component (the
/// byte formats up to depth 8, `P10Le` up to 10, `P12Le` up to 12),
/// each plane keeps its code values verbatim in the low bits of the
/// storage word (LSB-anchored, the core partial-depth convention), and
/// the frame carries a per-plane significant-bits side-channel
/// (`VideoFrame::significant_bits`, one byte per plane) recording each
/// plane's true depth. Full-scale for a plane with `b` significant bits
/// is `(1 << b) - 1`; consumers that ignore the record simply see the
/// surface format's documented depth, which never under-states a plane.
///
/// Deeper pictures — any component depth in 13..=16, which includes the
/// Table 10 preset-7/8 (16-bit) signal ranges and custom §11.4.9 ranges
/// with excursions above 4095 — are carried on the full-width 16-bit
/// formats, whose words have all 16 bits significant. Each plane's code
/// values are promoted by a left shift of `16 - depth`. That power-of-two
/// scaling is the one Table 10 itself uses between its narrow-range
/// presets at different depths (preset 4 is preset 3 × 4; preset 7 is
/// preset 4 × 16), so the signalled signal-range parameters scale
/// consistently onto the surface: the zero level (`offset`) and nominal
/// range (`excursion`) land at `value << shift`. Depth-16 planes shift
/// by 0 — their code space already is the 16-bit surface.
///
/// The promoted >12-bit path attaches **no** significant-bits record,
/// even for mixed pairs (e.g. 16-bit luma with 12-bit chroma): after
/// the `16 - depth` promotion a plane's full-scale sits at the top of
/// the 16-bit word, so all 16 bits genuinely carry signal in the
/// LSB-anchored sense the side-channel is defined in — a record of the
/// pre-promotion depths would mislabel full-scale as `(1 << depth) - 1`
/// and misdescribe where the values sit. The promotion itself is the
/// precision-faithful representation there, and the path stays
/// byte-identical to previous releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SurfaceMapping {
    format: PixelFormat,
    /// Left shift promoting luma code values onto the output words
    /// (0 for every verbatim, LSB-anchored surface).
    luma_shift: u32,
    /// Left shift promoting colour-difference code values onto the
    /// output words.
    chroma_shift: u32,
    /// Per-plane significant-bits record (`[y, c1, c2]`) to attach to
    /// the emitted frame, present exactly when some plane's true depth
    /// differs from the surface format's documented depth.
    significant_bits: Option<[u8; 3]>,
}

/// Pick the output surface for a decoded picture, or explain why none of
/// the core formats fits.
fn surface_mapping(pic: &DecodedPicture) -> oxideav_core::Result<SurfaceMapping> {
    let full = pic.color_diff_width == pic.luma_width && pic.color_diff_height == pic.luma_height;
    let half_w =
        pic.color_diff_width == pic.luma_width / 2 && pic.color_diff_height == pic.luma_height;
    let half_both =
        pic.color_diff_width == pic.luma_width / 2 && pic.color_diff_height == pic.luma_height / 2;
    if !(full || half_w || half_both) {
        return Err(oxideav_core::Error::unsupported(format!(
            "vc2: no planar pixel format for {}x{} chroma on {}x{} luma",
            pic.color_diff_width, pic.color_diff_height, pic.luma_width, pic.luma_height
        )));
    }
    let equal = pic.luma_depth == pic.color_diff_depth;
    let deepest = pic.luma_depth.max(pic.color_diff_depth);
    if equal && matches!(pic.luma_depth, 8 | 10 | 12) {
        let format = match (pic.luma_depth, full, half_w) {
            (8, true, _) => PixelFormat::Yuv444P,
            (8, _, true) => PixelFormat::Yuv422P,
            (8, _, _) => PixelFormat::Yuv420P,
            (10, true, _) => PixelFormat::Yuv444P10Le,
            (10, _, true) => PixelFormat::Yuv422P10Le,
            (10, _, _) => PixelFormat::Yuv420P10Le,
            (12, true, _) => PixelFormat::Yuv444P12Le,
            (12, _, true) => PixelFormat::Yuv422P12Le,
            _ => PixelFormat::Yuv420P12Le,
        };
        return Ok(SurfaceMapping {
            format,
            luma_shift: 0,
            chroma_shift: 0,
            significant_bits: None,
        });
    }
    // Sequence-header validation bounds both excursions to 1..=65535, so
    // depths are always 1..=16 here (the defensive arm below is for
    // out-of-contract callers only). Everything the exact formats do not
    // name at or below 12 bits is represented, not promoted or refused:
    // verbatim LSB-anchored code values on the deepest component's
    // natural storage surface, plus a per-plane significant-bits record.
    if (1..=12).contains(&deepest) {
        let format = match (deepest, full, half_w) {
            (..=8, true, _) => PixelFormat::Yuv444P,
            (..=8, _, true) => PixelFormat::Yuv422P,
            (..=8, _, _) => PixelFormat::Yuv420P,
            (..=10, true, _) => PixelFormat::Yuv444P10Le,
            (..=10, _, true) => PixelFormat::Yuv422P10Le,
            (..=10, _, _) => PixelFormat::Yuv420P10Le,
            (_, true, _) => PixelFormat::Yuv444P12Le,
            (_, _, true) => PixelFormat::Yuv422P12Le,
            _ => PixelFormat::Yuv420P12Le,
        };
        let c = pic.color_diff_depth as u8;
        return Ok(SurfaceMapping {
            format,
            luma_shift: 0,
            chroma_shift: 0,
            significant_bits: Some([pic.luma_depth as u8, c, c]),
        });
    }
    if (13..=16).contains(&deepest) {
        let format = match (full, half_w) {
            (true, _) => PixelFormat::Yuv444P16Le,
            (_, true) => PixelFormat::Yuv422P16Le,
            _ => PixelFormat::Yuv420P16Le,
        };
        return Ok(SurfaceMapping {
            format,
            luma_shift: 16 - pic.luma_depth,
            chroma_shift: 16 - pic.color_diff_depth,
            // Promotion, not a record: see the SurfaceMapping docs.
            significant_bits: None,
        });
    }
    Err(oxideav_core::Error::unsupported(format!(
        "vc2: no planar pixel format for luma/chroma bit depths {}/{}",
        pic.luma_depth, pic.color_diff_depth
    )))
}

/// Pack one component into a byte-per-sample [`VideoPlane`] (the 8-bit
/// formats).
fn pack_plane_bytes(samples: &[u16], width: usize) -> VideoPlane {
    VideoPlane {
        stride: width,
        data: samples.iter().map(|&v| v as u8).collect(),
    }
}

/// Pack one component into little-endian 16-bit words, promoting each
/// code value by `shift` (0 for the verbatim 10/12-bit formats).
///
/// §15.5 clips code values to `0..2^depth - 1` before offsetting, so a
/// `16 - depth` shift cannot exceed 16 bits (`(2^depth - 1) << shift ==
/// 65536 - 2^shift`); the saturation below is defensive only.
fn pack_plane_words(samples: &[u16], width: usize, shift: u32) -> VideoPlane {
    let mut data = Vec::with_capacity(samples.len() * 2);
    for &v in samples {
        let promoted = ((v as u32) << shift).min(u16::MAX as u32) as u16;
        data.extend_from_slice(&promoted.to_le_bytes());
    }
    VideoPlane {
        stride: width * 2,
        data,
    }
}

/// Convert a decoded picture into a core video frame.
fn to_frame(pic: &DecodedPicture, pts: Option<i64>) -> oxideav_core::Result<Frame> {
    // The mapping is validated (and its promotion shifts applied) even
    // though the format label itself is not carried on the frame —
    // stream-level properties live on the caller's CodecParameters.
    let m = surface_mapping(pic)?;
    let bytes8 = matches!(
        m.format,
        PixelFormat::Yuv444P | PixelFormat::Yuv422P | PixelFormat::Yuv420P
    );
    let pack = |samples: &[u16], width: usize, shift: u32| {
        if bytes8 {
            pack_plane_bytes(samples, width)
        } else {
            pack_plane_words(samples, width, shift)
        }
    };
    let frame = VideoFrame {
        pts,
        planes: vec![
            pack(&pic.y, pic.luma_width, m.luma_shift),
            pack(&pic.c1, pic.color_diff_width, m.chroma_shift),
            pack(&pic.c2, pic.color_diff_width, m.chroma_shift),
        ],
    };
    Ok(Frame::Video(match m.significant_bits {
        // Mixed / off-format depths: record each plane's true depth so
        // consumers know full-scale is (1 << b) - 1, not the surface
        // format's documented maximum. Uniform 8/10/12-bit pictures and
        // the promoted >12-bit path attach nothing and stay
        // byte-identical to previous releases.
        Some(bits) => frame.with_significant_bits(bits.to_vec()),
        None => frame,
    }))
}

impl Decoder for Vc2Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> oxideav_core::Result<()> {
        if self.flushed {
            return Err(oxideav_core::Error::invalid(
                "vc2: send_packet after flush; reset first",
            ));
        }
        if packet.data.is_empty() {
            return Ok(());
        }
        let pictures = self.walker.push(&packet.data).map_err(map_err)?;
        for pic in &pictures {
            self.pending.push_back(to_frame(pic, packet.pts)?);
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> oxideav_core::Result<Frame> {
        match self.pending.pop_front() {
            Some(f) => Ok(f),
            None if self.flushed => Err(oxideav_core::Error::Eof),
            None => Err(oxideav_core::Error::NeedMore),
        }
    }

    fn flush(&mut self) -> oxideav_core::Result<()> {
        self.flushed = true;
        if self.walker.has_incomplete_picture() {
            // §14.1: a sequence shall not end while a fragmented picture is
            // incomplete. Drop the partial picture so a later reset starts
            // clean, but surface the truncation.
            self.walker.reset();
            return Err(oxideav_core::Error::invalid(
                "vc2: stream ended while a fragmented picture is incomplete",
            ));
        }
        Ok(())
    }

    fn reset(&mut self) -> oxideav_core::Result<()> {
        self.walker.reset();
        self.pending.clear();
        self.flushed = false;
        Ok(())
    }
}

/// Direct decoder factory (the workspace dual-API convention: usable
/// standalone and as the registry's [`oxideav_core::DecoderFactory`]).
pub fn make_decoder(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    Ok(Box::new(Vc2Decoder::new(params)?))
}

/// Install the VC-2 decoder into the runtime context's codec registry
/// under the `"vc2"` codec id.
pub fn register(ctx: &mut RuntimeContext) {
    let mut caps = CodecCapabilities::video("vc2_sw");
    caps.intra_only = true; // every VC-2 picture decodes independently
    caps.lossy = true;
    caps.lossless = true; // reversible LeGall / Haar paths
    ctx.codecs.register(
        CodecInfo::new(CodecId::new(CODEC_ID))
            .capabilities(caps)
            .decoder(make_decoder),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pic(luma: (usize, usize, u32), chroma: (usize, usize, u32)) -> DecodedPicture {
        DecodedPicture {
            picture_number: 0,
            luma_width: luma.0,
            luma_height: luma.1,
            color_diff_width: chroma.0,
            color_diff_height: chroma.1,
            luma_depth: luma.2,
            color_diff_depth: chroma.2,
            y: vec![0; luma.0 * luma.1],
            c1: vec![0; chroma.0 * chroma.1],
            c2: vec![0; chroma.0 * chroma.1],
        }
    }

    #[test]
    fn pixel_format_mapping() {
        let f = |l, c| surface_mapping(&pic(l, c)).map(|m| m.format);
        assert!(matches!(f((4, 2, 8), (4, 2, 8)), Ok(PixelFormat::Yuv444P)));
        assert!(matches!(f((4, 2, 8), (2, 2, 8)), Ok(PixelFormat::Yuv422P)));
        assert!(matches!(f((4, 2, 8), (2, 1, 8)), Ok(PixelFormat::Yuv420P)));
        assert!(matches!(
            f((4, 2, 10), (2, 2, 10)),
            Ok(PixelFormat::Yuv422P10Le)
        ));
        assert!(matches!(
            f((4, 2, 12), (4, 2, 12)),
            Ok(PixelFormat::Yuv444P12Le)
        ));
        // Depth 16 (Table 10 presets 7/8) rides the full-width formats.
        assert!(matches!(
            f((4, 2, 16), (4, 2, 16)),
            Ok(PixelFormat::Yuv444P16Le)
        ));
        assert!(matches!(
            f((4, 2, 16), (2, 2, 16)),
            Ok(PixelFormat::Yuv422P16Le)
        ));
        assert!(matches!(
            f((4, 2, 16), (2, 1, 16)),
            Ok(PixelFormat::Yuv420P16Le)
        ));
        // Unrepresentable chroma geometry is rejected at any depth.
        assert!(f((4, 2, 16), (3, 2, 16)).is_err());
        assert!(f((4, 2, 10), (3, 2, 8)).is_err());
        // Out-of-contract depths (the sequence header bounds real ones
        // to 1..=16) hit the defensive rejection arm.
        assert!(f((4, 2, 0), (4, 2, 0)).is_err());
        assert!(f((4, 2, 17), (4, 2, 17)).is_err());
    }

    #[test]
    fn mixed_shallow_depths_ride_the_deepest_surface_with_a_record() {
        let f = |l, c| surface_mapping(&pic(l, c)).unwrap();
        // The user-ruled headline case: 12-bit luma + 10-bit chroma on
        // the P12 surface, verbatim words, record [12, 10, 10].
        let m = f((4, 2, 12), (2, 2, 10));
        assert_eq!(m.format, PixelFormat::Yuv422P12Le);
        assert_eq!((m.luma_shift, m.chroma_shift), (0, 0));
        assert_eq!(m.significant_bits, Some([12, 10, 10]));
        // 10/8 (formerly refused): P10 surface, record [10, 8, 8].
        let m = f((4, 2, 10), (4, 2, 8));
        assert_eq!(m.format, PixelFormat::Yuv444P10Le);
        assert_eq!(m.significant_bits, Some([10, 8, 8]));
        // Deepest component at or below 8 bits: byte planes carry it.
        let m = f((4, 2, 8), (2, 1, 6));
        assert_eq!(m.format, PixelFormat::Yuv420P);
        assert_eq!(m.significant_bits, Some([8, 6, 6]));
        // Chroma may be the deeper component.
        let m = f((4, 2, 10), (4, 2, 12));
        assert_eq!(m.format, PixelFormat::Yuv444P12Le);
        assert_eq!(m.significant_bits, Some([10, 12, 12]));
        // Extreme spread still fits one surface.
        let m = f((4, 2, 12), (2, 2, 1));
        assert_eq!(m.format, PixelFormat::Yuv422P12Le);
        assert_eq!(m.significant_bits, Some([12, 1, 1]));
    }

    #[test]
    fn uniform_off_format_depths_are_represented_not_refused() {
        // Equal depths with no exact format (9, 11, 7, ...) use the same
        // representation: nearest not-shallower surface plus a record.
        let f = |l, c| surface_mapping(&pic(l, c)).unwrap();
        let m = f((4, 2, 9), (4, 2, 9));
        assert_eq!(m.format, PixelFormat::Yuv444P10Le);
        assert_eq!((m.luma_shift, m.chroma_shift), (0, 0));
        assert_eq!(m.significant_bits, Some([9, 9, 9]));
        let m = f((4, 2, 11), (2, 1, 11));
        assert_eq!(m.format, PixelFormat::Yuv420P12Le);
        assert_eq!(m.significant_bits, Some([11, 11, 11]));
        let m = f((4, 2, 1), (4, 2, 1));
        assert_eq!(m.format, PixelFormat::Yuv444P);
        assert_eq!(m.significant_bits, Some([1, 1, 1]));
    }

    #[test]
    fn exact_and_promoted_surfaces_attach_no_record() {
        let f = |l, c| surface_mapping(&pic(l, c)).unwrap();
        // Uniform 8/10/12: exact formats, no side-channel.
        assert_eq!(f((4, 2, 8), (2, 1, 8)).significant_bits, None);
        assert_eq!(f((4, 2, 10), (2, 2, 10)).significant_bits, None);
        assert_eq!(f((4, 2, 12), (4, 2, 12)).significant_bits, None);
        // The promoted >12-bit path — uniform or mixed — attaches no
        // record either: promotion places full-scale at the top of the
        // word, so an LSB-anchored depth record would misdescribe it.
        assert_eq!(f((4, 2, 16), (2, 2, 16)).significant_bits, None);
        assert_eq!(f((4, 2, 13), (4, 2, 13)).significant_bits, None);
        assert_eq!(f((4, 2, 16), (2, 2, 12)).significant_bits, None);
        assert_eq!(f((4, 2, 14), (2, 1, 10)).significant_bits, None);
    }

    #[test]
    fn deep_custom_ranges_promote_onto_the_16bit_surface() {
        // Equal 13-bit components: exact format absent, promoted by << 3.
        let m = surface_mapping(&pic((4, 2, 13), (4, 2, 13))).unwrap();
        assert_eq!(m.format, PixelFormat::Yuv444P16Le);
        assert_eq!(m.luma_shift, 3);
        assert_eq!(m.chroma_shift, 3);
        // Mixed 16/12: the deep component forces the 16-bit surface and
        // each plane promotes by its own 16 - depth.
        let m = surface_mapping(&pic((4, 2, 16), (2, 2, 12))).unwrap();
        assert_eq!(m.format, PixelFormat::Yuv422P16Le);
        assert_eq!(m.luma_shift, 0);
        assert_eq!(m.chroma_shift, 4);
        // Depth 16 needs no promotion at all.
        let m = surface_mapping(&pic((4, 2, 16), (2, 1, 16))).unwrap();
        assert_eq!(m.luma_shift, 0);
        assert_eq!(m.chroma_shift, 0);
    }

    #[test]
    fn plane_packing_widths() {
        let p8 = pack_plane_bytes(&[1, 2, 3, 4], 2);
        assert_eq!(p8.stride, 2);
        assert_eq!(p8.data, vec![1, 2, 3, 4]);
        let p10 = pack_plane_words(&[0x0102, 0x0304], 2, 0);
        assert_eq!(p10.stride, 4);
        assert_eq!(p10.data, vec![0x02, 0x01, 0x04, 0x03]); // little-endian
    }

    #[test]
    fn word_packing_applies_promotion_shift() {
        // 13-bit code values on the 16-bit surface: v << 3, little-endian.
        // Max code 8191 lands at 65528 (top of the promoted lattice).
        let p = pack_plane_words(&[0, 1, 4096, 8191], 4, 3);
        assert_eq!(p.stride, 8);
        let words: Vec<u16> = p
            .data
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(words, vec![0, 8, 32768, 65528]);
        // The defensive saturation never wraps even on out-of-contract
        // input words.
        let p = pack_plane_words(&[u16::MAX], 1, 3);
        assert_eq!(p.data, u16::MAX.to_le_bytes().to_vec());
    }
}
