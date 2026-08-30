//! `oxideav-core` integration for the encoder: a [`Encoder`] wrapping the
//! standalone [`SequenceEncoder`], the [`make_encoder`] factory the
//! registry installs, and the [`Vc2EncoderOptions`] schema.
//!
//! ## Frames in, data units out
//!
//! Frames arrive as planar YUV on the formats the decoder wrapper emits
//! (8-bit byte planes, or little-endian 16-bit words with the code values
//! LSB-anchored for the 10/12/16-bit formats). Each frame becomes one
//! packet holding whole VC-2 data units: the first packet of a sequence
//! opens with the sequence header, and [`Encoder::flush`] emits a final
//! packet carrying the end-of-sequence unit. With the `sequence-per-packet`
//! option every packet is instead a complete, self-contained sequence
//! (header … end-of-sequence) — the framing the staged Matroska registry
//! references describe for `V_DIRAC` tracks.
//!
//! ## Rate control resolution
//!
//! `qindex` (when 0 or above) selects a fixed quantisation index;
//! otherwise `picture-bytes` (when non-zero) targets a per-picture size;
//! otherwise a declared `bit_rate` with a known frame rate derives the
//! target; otherwise high-quality pictures code lossless (quantisation
//! index 0). Low-delay pictures have fixed-size slices and therefore
//! require one of the byte-target modes.

use oxideav_core::{
    parse_options, CodecParameters, Encoder, Frame, OptionField, OptionKind, OptionValue, Packet,
    PixelFormat,
};

use crate::encoder::{EncoderConfig, PictureInput, RateControl, SequenceEncoder};
use crate::params::ColorDiffFormat;
use crate::transform::PictureKind;

/// Typed options for the `"vc2"` encoder (see [`CodecOptionsStruct`]).
///
/// [`CodecOptionsStruct`]: oxideav_core::CodecOptionsStruct
#[derive(Debug, Clone)]
pub struct Vc2EncoderOptions {
    /// `"hq"` (high quality, profile 3) or `"ld"` (low delay, profile 0).
    pub profile: String,
    /// Wavelet index (Table 15, 0..=6). Default 1 (LeGall 5,3).
    pub wavelet: u32,
    /// 2-D transform depth. Default 2.
    pub depth: u32,
    /// Horizontal wavelet index, or -1 to match `wavelet` (§12.4.4.2).
    pub wavelet_ho: i32,
    /// Horizontal-only transform depth (§12.4.4.3). Default 0.
    pub depth_ho: u32,
    /// Slice columns; 0 sizes the grid at roughly 32 luma samples.
    pub slices_x: u32,
    /// Slice rows; 0 sizes the grid at roughly 32 luma samples.
    pub slices_y: u32,
    /// Fixed quantisation index, or -1 for automatic (rate / lossless).
    pub qindex: i32,
    /// Target coded bytes per picture; 0 leaves the rate to `bit_rate`
    /// or lossless.
    pub picture_bytes: u32,
    /// Slices per §14 data fragment; 0 codes plain picture data units.
    pub fragment_slices: u32,
    /// ST 2042-2 level to claim (checked when 1..=7). Default 0.
    pub level: u32,
    /// §13.5.4 `slice_prefix_bytes` (high quality), emitted as zeros.
    pub slice_prefix_bytes: u32,
    /// Emit each frame as a complete sequence (header … end-of-sequence)
    /// rather than one long sequence closed at flush.
    pub sequence_per_packet: bool,
}

impl Default for Vc2EncoderOptions {
    fn default() -> Self {
        Vc2EncoderOptions {
            profile: "hq".into(),
            wavelet: 1,
            depth: 2,
            wavelet_ho: -1,
            depth_ho: 0,
            slices_x: 0,
            slices_y: 0,
            qindex: -1,
            picture_bytes: 0,
            fragment_slices: 0,
            level: 0,
            slice_prefix_bytes: 0,
            sequence_per_packet: false,
        }
    }
}

impl oxideav_core::CodecOptionsStruct for Vc2EncoderOptions {
    const SCHEMA: &'static [OptionField] = &[
        OptionField {
            name: "profile",
            kind: OptionKind::Enum(&["hq", "ld"]),
            default: OptionValue::String(String::new()),
            help: "picture profile: hq (high quality) or ld (low delay)",
        },
        OptionField {
            name: "wavelet",
            kind: OptionKind::U32,
            default: OptionValue::U32(1),
            help: "wavelet index 0..=6 (Table 15); 1 = LeGall (5,3)",
        },
        OptionField {
            name: "depth",
            kind: OptionKind::U32,
            default: OptionValue::U32(2),
            help: "2-D wavelet transform depth",
        },
        OptionField {
            name: "wavelet-ho",
            kind: OptionKind::I32,
            default: OptionValue::I32(-1),
            help: "horizontal wavelet index, -1 = same as wavelet",
        },
        OptionField {
            name: "depth-ho",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "extra horizontal-only transform depth",
        },
        OptionField {
            name: "slices-x",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "slice columns; 0 = about one slice per 32 luma samples",
        },
        OptionField {
            name: "slices-y",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "slice rows; 0 = about one slice per 32 luma samples",
        },
        OptionField {
            name: "qindex",
            kind: OptionKind::I32,
            default: OptionValue::I32(-1),
            help: "fixed quantisation index; -1 = rate-controlled / lossless",
        },
        OptionField {
            name: "picture-bytes",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "target coded bytes per picture; 0 = use bit_rate or lossless",
        },
        OptionField {
            name: "fragment-slices",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "slices per fragment data unit; 0 = whole-picture units",
        },
        OptionField {
            name: "level",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "ST 2042-2 level to claim; validated when 1..=7",
        },
        OptionField {
            name: "slice-prefix-bytes",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "high-quality slice prefix bytes (zeros)",
        },
        OptionField {
            name: "sequence-per-packet",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "close a whole sequence in every packet (V_DIRAC framing)",
        },
    ];

    fn apply(&mut self, key: &str, v: &OptionValue) -> oxideav_core::Result<()> {
        match key {
            "profile" => self.profile = v.as_str()?.to_owned(),
            "wavelet" => self.wavelet = v.as_u32()?,
            "depth" => self.depth = v.as_u32()?,
            "wavelet-ho" => self.wavelet_ho = v.as_i32()?,
            "depth-ho" => self.depth_ho = v.as_u32()?,
            "slices-x" => self.slices_x = v.as_u32()?,
            "slices-y" => self.slices_y = v.as_u32()?,
            "qindex" => self.qindex = v.as_i32()?,
            "picture-bytes" => self.picture_bytes = v.as_u32()?,
            "fragment-slices" => self.fragment_slices = v.as_u32()?,
            "level" => self.level = v.as_u32()?,
            "slice-prefix-bytes" => self.slice_prefix_bytes = v.as_u32()?,
            "sequence-per-packet" => self.sequence_per_packet = v.as_bool()?,
            _ => unreachable!("guarded by SCHEMA"),
        }
        Ok(())
    }
}

/// Chroma sampling, bit depth and word width for a supported pixel
/// format.
fn format_layout(pf: PixelFormat) -> Option<(ColorDiffFormat, u32, bool)> {
    Some(match pf {
        PixelFormat::Yuv444P => (ColorDiffFormat::Yuv444, 8, false),
        PixelFormat::Yuv422P => (ColorDiffFormat::Yuv422, 8, false),
        PixelFormat::Yuv420P => (ColorDiffFormat::Yuv420, 8, false),
        PixelFormat::Yuv444P10Le => (ColorDiffFormat::Yuv444, 10, true),
        PixelFormat::Yuv422P10Le => (ColorDiffFormat::Yuv422, 10, true),
        PixelFormat::Yuv420P10Le => (ColorDiffFormat::Yuv420, 10, true),
        PixelFormat::Yuv444P12Le => (ColorDiffFormat::Yuv444, 12, true),
        PixelFormat::Yuv422P12Le => (ColorDiffFormat::Yuv422, 12, true),
        PixelFormat::Yuv420P12Le => (ColorDiffFormat::Yuv420, 12, true),
        PixelFormat::Yuv444P16Le => (ColorDiffFormat::Yuv444, 16, true),
        PixelFormat::Yuv422P16Le => (ColorDiffFormat::Yuv422, 16, true),
        PixelFormat::Yuv420P16Le => (ColorDiffFormat::Yuv420, 16, true),
        _ => return None,
    })
}

/// VC-2 encoder speaking the `oxideav-core` [`Encoder`] frame/packet
/// contract. Construct via [`make_encoder`] (or through a registry
/// populated by [`crate::register`]).
pub struct Vc2Encoder {
    inner: SequenceEncoder,
    output_params: CodecParameters,
    words: bool,
    depth: u32,
    sequence_per_packet: bool,
    pending: std::collections::VecDeque<Packet>,
    time_base: oxideav_core::TimeBase,
    flushed: bool,
}

/// Map a crate error onto the shared error type.
fn map_err(e: crate::Error) -> oxideav_core::Error {
    match e {
        crate::Error::Unsupported(_) => oxideav_core::Error::unsupported(e.to_string()),
        _ => oxideav_core::Error::invalid(e.to_string()),
    }
}

impl Vc2Encoder {
    /// Build an encoder from stream parameters: `width`, `height` and a
    /// supported planar `pixel_format` are required; `frame_rate`,
    /// `bit_rate` and the [`Vc2EncoderOptions`] keys in
    /// `params.options` refine the configuration.
    pub fn new(params: &CodecParameters) -> oxideav_core::Result<Self> {
        let opts: Vc2EncoderOptions = parse_options(&params.options)?;
        let (width, height) = match (params.width, params.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => (w as u64, h as u64),
            _ => {
                return Err(oxideav_core::Error::invalid(
                    "vc2: encoder requires width and height",
                ))
            }
        };
        let pf = params
            .pixel_format
            .ok_or_else(|| oxideav_core::Error::invalid("vc2: encoder requires a pixel format"))?;
        let (chroma, depth, words) = format_layout(pf).ok_or_else(|| {
            oxideav_core::Error::unsupported(format!("vc2: no coding layout for {pf:?}"))
        })?;
        let mut cfg = EncoderConfig::new(width, height, chroma, depth);
        cfg.kind = match opts.profile.as_str() {
            "ld" => PictureKind::LowDelay,
            _ => PictureKind::HighQuality,
        };
        cfg.wavelet_index = opts.wavelet as u64;
        cfg.wavelet_index_ho = if opts.wavelet_ho < 0 {
            opts.wavelet as u64
        } else {
            opts.wavelet_ho as u64
        };
        cfg.dwt_depth = opts.depth as u64;
        cfg.dwt_depth_ho = opts.depth_ho as u64;
        if opts.slices_x > 0 {
            cfg.slices_x = opts.slices_x as u64;
        }
        if opts.slices_y > 0 {
            cfg.slices_y = opts.slices_y as u64;
        }
        cfg.level = opts.level as u64;
        cfg.slice_prefix_bytes = opts.slice_prefix_bytes as u64;
        if opts.fragment_slices > 0 {
            cfg.fragment_slices = Some(opts.fragment_slices.min(u16::MAX as u32) as u16);
        }
        if let Some(fr) = params.frame_rate {
            if fr.num > 0 && fr.den > 0 {
                cfg.video_parameters.frame_rate_numer = fr.num as u64;
                cfg.video_parameters.frame_rate_denom = fr.den as u64;
            }
        }
        cfg.rate = if opts.qindex >= 0 {
            RateControl::FixedQuantIndex(opts.qindex as u64)
        } else if opts.picture_bytes > 0 {
            RateControl::PictureBytes(opts.picture_bytes as u64)
        } else if let Some(bps) = params.bit_rate.filter(|&b| b > 0) {
            RateControl::BitsPerSecond(bps)
        } else {
            RateControl::Lossless
        };
        let inner = SequenceEncoder::new(cfg).map_err(map_err)?;
        let mut output_params = CodecParameters::video(params.codec_id.clone());
        output_params.width = params.width;
        output_params.height = params.height;
        output_params.pixel_format = params.pixel_format;
        output_params.frame_rate = params.frame_rate;
        output_params.bit_rate = params.bit_rate;
        Ok(Vc2Encoder {
            inner,
            output_params,
            words,
            depth,
            sequence_per_packet: opts.sequence_per_packet,
            pending: std::collections::VecDeque::new(),
            time_base: oxideav_core::TimeBase::new(1, 1_000_000),
            flushed: false,
        })
    }

    /// The standalone encoder's parsed sequence header — the §11
    /// parameter map the emitted stream carries.
    pub fn sequence_header(&self) -> &crate::params::SequenceHeader {
        self.inner.sequence_header()
    }

    /// Unpack one plane into unsigned code values, validating its size.
    fn plane_samples(
        &self,
        plane: &oxideav_core::VideoPlane,
        w: usize,
        h: usize,
    ) -> oxideav_core::Result<Vec<u16>> {
        let bytes_per = if self.words { 2 } else { 1 };
        let row = w * bytes_per;
        if plane.stride < row || plane.data.len() < plane.stride * (h - 1) + row {
            return Err(oxideav_core::Error::invalid(
                "vc2: input plane smaller than the declared dimensions",
            ));
        }
        let mask = if self.depth >= 16 {
            u16::MAX
        } else {
            ((1u32 << self.depth) - 1) as u16
        };
        let mut out = Vec::with_capacity(w * h);
        for y in 0..h {
            let src = &plane.data[y * plane.stride..y * plane.stride + row];
            if self.words {
                for pair in src.chunks_exact(2) {
                    out.push(u16::from_le_bytes([pair[0], pair[1]]) & mask);
                }
            } else {
                out.extend(src.iter().map(|&b| b as u16));
            }
        }
        Ok(out)
    }
}

impl Encoder for Vc2Encoder {
    fn codec_id(&self) -> &oxideav_core::CodecId {
        &self.output_params.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.output_params
    }

    fn send_frame(&mut self, frame: &Frame) -> oxideav_core::Result<()> {
        if self.flushed {
            return Err(oxideav_core::Error::invalid(
                "vc2: send_frame after flush; construct a new encoder",
            ));
        }
        let v = match frame {
            Frame::Video(v) => v,
            _ => return Err(oxideav_core::Error::invalid("vc2: expected a video frame")),
        };
        let planes = v.image_planes();
        if planes.len() != 3 {
            return Err(oxideav_core::Error::invalid(
                "vc2: expected 3 planar components",
            ));
        }
        let cp = self.inner.sequence_header().coding_parameters;
        let y = self.plane_samples(&planes[0], cp.luma_width as usize, cp.luma_height as usize)?;
        let c1 = self.plane_samples(
            &planes[1],
            cp.color_diff_width as usize,
            cp.color_diff_height as usize,
        )?;
        let c2 = self.plane_samples(
            &planes[2],
            cp.color_diff_width as usize,
            cp.color_diff_height as usize,
        )?;
        let mut data = self
            .inner
            .encode_picture(&PictureInput {
                y: &y,
                c1: &c1,
                c2: &c2,
            })
            .map_err(map_err)?;
        if self.sequence_per_packet {
            data.extend_from_slice(&self.inner.end_sequence());
        }
        let mut packet = Packet::new(0, self.time_base, data);
        packet.pts = v.pts;
        packet.dts = v.pts;
        packet.flags.keyframe = true; // every VC-2 picture is intra
        self.pending.push_back(packet);
        Ok(())
    }

    fn receive_packet(&mut self) -> oxideav_core::Result<Packet> {
        match self.pending.pop_front() {
            Some(p) => Ok(p),
            None if self.flushed => Err(oxideav_core::Error::Eof),
            None => Err(oxideav_core::Error::NeedMore),
        }
    }

    fn flush(&mut self) -> oxideav_core::Result<()> {
        if !self.flushed {
            self.flushed = true;
            let tail = self.inner.end_sequence();
            if !tail.is_empty() {
                let mut packet = Packet::new(0, self.time_base, tail);
                packet.flags.keyframe = false;
                self.pending.push_back(packet);
            }
        }
        Ok(())
    }
}

/// Direct encoder factory (the workspace dual-API convention: usable
/// standalone and as the registry's [`oxideav_core::EncoderFactory`]).
pub fn make_encoder(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Encoder>> {
    Ok(Box::new(Vc2Encoder::new(params)?))
}
