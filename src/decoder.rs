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

/// Pick the output [`PixelFormat`] for a decoded picture, or explain why
/// none of the core formats fits.
fn pixel_format(pic: &DecodedPicture) -> oxideav_core::Result<PixelFormat> {
    if pic.luma_depth != pic.color_diff_depth {
        return Err(oxideav_core::Error::unsupported(format!(
            "vc2: mixed luma/chroma bit depths ({}/{}) have no planar pixel format",
            pic.luma_depth, pic.color_diff_depth
        )));
    }
    let full = pic.color_diff_width == pic.luma_width && pic.color_diff_height == pic.luma_height;
    let half_w =
        pic.color_diff_width == pic.luma_width / 2 && pic.color_diff_height == pic.luma_height;
    let half_both =
        pic.color_diff_width == pic.luma_width / 2 && pic.color_diff_height == pic.luma_height / 2;
    let fmt = match (pic.luma_depth, full, half_w, half_both) {
        (8, true, ..) => PixelFormat::Yuv444P,
        (8, _, true, _) => PixelFormat::Yuv422P,
        (8, _, _, true) => PixelFormat::Yuv420P,
        (10, true, ..) => PixelFormat::Yuv444P10Le,
        (10, _, true, _) => PixelFormat::Yuv422P10Le,
        (10, _, _, true) => PixelFormat::Yuv420P10Le,
        (12, true, ..) => PixelFormat::Yuv444P12Le,
        (12, _, true, _) => PixelFormat::Yuv422P12Le,
        (12, _, _, true) => PixelFormat::Yuv420P12Le,
        _ => {
            return Err(oxideav_core::Error::unsupported(format!(
                "vc2: no pixel format for depth {} with {}x{} chroma on {}x{} luma",
                pic.luma_depth,
                pic.color_diff_width,
                pic.color_diff_height,
                pic.luma_width,
                pic.luma_height
            )))
        }
    };
    Ok(fmt)
}

/// Pack one component into a [`VideoPlane`]: bytes for 8-bit output,
/// little-endian 16-bit words otherwise.
fn pack_plane(samples: &[u16], width: usize, depth: u32) -> VideoPlane {
    if depth <= 8 {
        VideoPlane {
            stride: width,
            data: samples.iter().map(|&v| v as u8).collect(),
        }
    } else {
        let mut data = Vec::with_capacity(samples.len() * 2);
        for &v in samples {
            data.extend_from_slice(&v.to_le_bytes());
        }
        VideoPlane {
            stride: width * 2,
            data,
        }
    }
}

/// Convert a decoded picture into a core video frame.
fn to_frame(pic: &DecodedPicture, pts: Option<i64>) -> oxideav_core::Result<Frame> {
    // Validated for a supported format even though the label itself is not
    // carried on the frame — stream-level properties live on the caller's
    // CodecParameters.
    let _ = pixel_format(pic)?;
    Ok(Frame::Video(VideoFrame {
        pts,
        planes: vec![
            pack_plane(&pic.y, pic.luma_width, pic.luma_depth),
            pack_plane(&pic.c1, pic.color_diff_width, pic.color_diff_depth),
            pack_plane(&pic.c2, pic.color_diff_width, pic.color_diff_depth),
        ],
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
        let f = |l, c| pixel_format(&pic(l, c));
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
        // 16-bit output and mixed depths have no core pixel format yet.
        assert!(f((4, 2, 16), (4, 2, 16)).is_err());
        assert!(f((4, 2, 10), (4, 2, 8)).is_err());
    }

    #[test]
    fn plane_packing_widths() {
        let p8 = pack_plane(&[1, 2, 3, 4], 2, 8);
        assert_eq!(p8.stride, 2);
        assert_eq!(p8.data, vec![1, 2, 3, 4]);
        let p10 = pack_plane(&[0x0102, 0x0304], 2, 10);
        assert_eq!(p10.stride, 4);
        assert_eq!(p10.data, vec![0x02, 0x01, 0x04, 0x03]); // little-endian
    }
}
