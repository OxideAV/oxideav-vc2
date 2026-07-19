//! # oxideav-vc2
//!
//! Pure-Rust decoder for **SMPTE ST 2042-1:2022 VC-2** ("Dirac Pro") — an
//! open, royalty-free, intra-frame wavelet video compression system. Each
//! picture is wavelet-transformed (LeGall 5/3, Deslauriers-Dubuc, Haar,
//! Fidelity, Daubechies), the subbands are quantised and entropy-coded, and
//! every picture decodes independently (no temporal prediction).
//!
//! This is a clean-room implementation built solely against the normative
//! SMPTE PDF mirrored under `docs/video/vc2/`. Clause numbers in the source
//! track that 2022 edition.
//!
//! ## What is implemented
//!
//! * **Data coding** (Annex A) — bit/byte input, byte alignment, the fixed
//!   literals (`read_bool` / `read_nbits` / `read_uint_lit`), the unsigned
//!   and signed interleaved exp-Golomb codes, and the bounded-block reads
//!   used inside slices. See [`bitio`].
//! * **Stream structure** (§10) — parse-info headers ("BBCD"), the Table 4 /
//!   Table 5 parse-code classification, and the `parse_sequence` walk over
//!   data units, skipping auxiliary / padding / unknown units. Concatenated
//!   sequences (a VC-2 stream, §10.3) decode in one pass. See [`sequence`].
//! * **Picture fragments** (§14) — setup / data fragment parsing, slice
//!   reassembly in raster order with the §14.1/§14.2 stream constraints
//!   enforced, and deferred DC prediction once the picture completes. The
//!   stateful [`SequenceDecoder`] keeps a partially assembled picture
//!   across [`SequenceDecoder::push`] calls for packetized input.
//! * **Sequence header** (§11) — parse parameters, the Annex B base-format
//!   defaults (decode-critical fields), source-parameter overrides, preset
//!   signal ranges (Table 10) and `set_coding_parameters` (§11.6). See
//!   [`params`].
//! * **Transform parameters** (§12.4) — wavelet filter / depth, the §12.4.4
//!   extended (asymmetric) parameters, slice parameters and the §12.4.5.3
//!   quantization-matrix read, with the symmetric Annex D defaults built in.
//!   See [`transform`].
//! * **Transform data** (§13) — subband dimensions (§13.2.3), inverse
//!   quantization (§13.3), DC-band prediction (§13.4), and slice unpacking
//!   for both low-delay (§13.5.3) and high-quality (§13.5.4) pictures. See
//!   [`transform`].
//! * **Picture decode** (§15) — the integer lifting IDWT (§15.4), pad
//!   removal (§15.4.5), clipping (§15.5) and unsigned offsetting. See
//!   [`wavelet`] and [`picture`].
//!
//! ## `oxideav-core` integration
//!
//! With the default-on `registry` feature the crate exposes the workspace
//! dual API: the [`register`] entry point installs a `"vc2"` decoder into a
//! [`oxideav_core::RuntimeContext`] codec registry, and [`make_decoder`] is
//! the direct factory. Packets carry whole VC-2 data units (the parse-info
//! framing is the codec's own); fragmented pictures may span packets.
//! `register` also claims the container tag the staged specification
//! grounds — the FourCC `BBCD`, VC-2's own §10.5.1 parse-info prefix —
//! with a confidence probe, so container `CodecResolver` lookups route
//! matching streams here. Output rides exact planar formats where one
//! exists; mixed or off-format ≤12-bit custom signal ranges decode
//! LSB-anchored on the deepest component's surface with the core
//! per-plane significant-bits side-channel attached, and >12-bit ranges
//! promote onto the full-width 16-bit formats. See [`decoder`].
//!
//! ## Quick start (standalone)
//!
//! ```no_run
//! let stream: &[u8] = &[/* a VC-2 sequence */];
//! let pictures = oxideav_vc2::decode_sequence(stream).unwrap();
//! for pic in &pictures {
//!     println!("picture {} is {}x{}", pic.picture_number, pic.luma_width, pic.luma_height);
//! }
//! ```

pub mod bitio;
pub mod error;
pub mod params;
pub mod picture;
pub mod quant;
pub mod sequence;
pub mod transform;
pub mod wavelet;

pub use error::{Error, Result};
pub use picture::DecodedPicture;
pub use sequence::{
    classify, DataUnit, FragmentHeader, ParseInfo, SequenceDecoder, PARSE_INFO_PREFIX,
};
pub use transform::PictureKind;

/// Decode a complete VC-2 stream into its constituent pictures.
///
/// The input is a raw VC-2 byte stream that begins with a parse-info header
/// ("BBCD") and holds one or more complete sequences, each ending with an
/// end-of-sequence data unit (§10.3 / §10.4.1). Fragmented pictures (§14)
/// are reassembled transparently. Each returned [`DecodedPicture`] carries
/// clipped, unsigned component planes. For chunked / packetized input use
/// [`SequenceDecoder`] directly.
pub fn decode_sequence(data: &[u8]) -> Result<Vec<DecodedPicture>> {
    sequence::parse_sequence(data)
}

#[cfg(feature = "registry")]
pub mod decoder;

#[cfg(feature = "registry")]
pub use decoder::{make_decoder, register, Vc2Decoder, CODEC_ID};

#[cfg(feature = "registry")]
oxideav_core::register!("vc2", decoder::register);
