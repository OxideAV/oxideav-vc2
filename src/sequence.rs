//! VC-2 sequence structure: parse-info headers (§10.5), data-unit dispatch
//! (§10.4.1), fragmented-picture reassembly (§14) and the stream walkers
//! that yield decoded pictures.
//!
//! [`SequenceDecoder`] is the stateful walker: it retains the current
//! sequence header and any partially assembled fragmented picture across
//! [`SequenceDecoder::push`] calls, so a stream can be fed in chunks (each
//! chunk holding whole data units). [`parse_sequence`] wraps it for the
//! one-shot whole-stream case.

use crate::bitio::BitReader;
use crate::params::{self, SequenceHeader};
use crate::picture::{self, DecodedPicture, ParsedPicture};
use crate::transform::{self, PictureKind, TransformData, TransformParameters};
use crate::{Error, Result};

/// The parse-info prefix "BBCD" (§10.5.1).
pub const PARSE_INFO_PREFIX: [u8; 4] = [0x42, 0x42, 0x43, 0x44];

/// A parsed parse-info header (§10.5.1).
#[derive(Debug, Clone, Copy)]
pub struct ParseInfo {
    pub parse_code: u8,
    pub next_parse_offset: u32,
    pub previous_parse_offset: u32,
}

/// Classify a parse code per Table 4 / Table 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataUnit {
    SequenceHeader,
    EndOfSequence,
    AuxiliaryData,
    Padding,
    Picture(PictureKind),
    /// A picture fragment (§14) — a setup fragment carrying transform
    /// parameters or a data fragment carrying consecutive slices.
    Fragment(PictureKind),
    /// A parse code not defined in this version — discard the data unit.
    Reserved,
}

/// `parse_info()` (§10.5.1): byte-align, verify the prefix, read parse code
/// and the next/previous offsets.
pub fn parse_info(r: &mut BitReader) -> Result<ParseInfo> {
    r.byte_align();
    let prefix = [
        r.read_uint_lit(1) as u8,
        r.read_uint_lit(1) as u8,
        r.read_uint_lit(1) as u8,
        r.read_uint_lit(1) as u8,
    ];
    if prefix != PARSE_INFO_PREFIX {
        return Err(Error::BadParseInfoPrefix(prefix));
    }
    let parse_code = r.read_uint_lit(1) as u8;
    let next_parse_offset = r.read_uint_lit(4) as u32;
    let previous_parse_offset = r.read_uint_lit(4) as u32;
    Ok(ParseInfo {
        parse_code,
        next_parse_offset,
        previous_parse_offset,
    })
}

/// Map a parse code to a [`DataUnit`] via the Table 5 predicate functions.
pub fn classify(parse_code: u8) -> DataUnit {
    // is_seq_header / is_end_of_sequence / is_auxiliary / is_padding.
    if parse_code == 0x00 {
        return DataUnit::SequenceHeader;
    }
    if parse_code == 0x10 {
        return DataUnit::EndOfSequence;
    }
    if (parse_code & 0xF8) == 0x20 {
        return DataUnit::AuxiliaryData;
    }
    if parse_code == 0x30 {
        return DataUnit::Padding;
    }
    // is_fragment: (parse_code & 0x0C) == 0x0C.
    let is_fragment = (parse_code & 0x0C) == 0x0C;
    // is_picture: (parse_code & 0x8C) == 0x88.
    let is_picture = (parse_code & 0x8C) == 0x88;
    // is_ld / is_hq.
    let is_ld = (parse_code & 0xF8) == 0xC8;
    let is_hq = (parse_code & 0xF8) == 0xE8;
    if is_fragment {
        if is_ld {
            return DataUnit::Fragment(PictureKind::LowDelay);
        }
        if is_hq {
            return DataUnit::Fragment(PictureKind::HighQuality);
        }
        return DataUnit::Reserved;
    }
    if is_picture {
        if is_ld {
            return DataUnit::Picture(PictureKind::LowDelay);
        }
        if is_hq {
            return DataUnit::Picture(PictureKind::HighQuality);
        }
    }
    DataUnit::Reserved
}

/// A parsed fragment header (§14.2).
///
/// `fragment_slice_count == 0` marks a setup fragment (transform
/// parameters follow); a non-zero count marks a data fragment carrying
/// `fragment_slice_count` consecutive slices starting at the
/// `(fragment_x_offset, fragment_y_offset)` slice coordinate.
#[derive(Debug, Clone, Copy)]
pub struct FragmentHeader {
    pub picture_number: u32,
    /// Undefined for the purposes of the standard; does not contribute to
    /// decoding (§14.2).
    pub fragment_data_length: u16,
    pub fragment_slice_count: u16,
    /// Only present in the stream when `fragment_slice_count != 0`;
    /// zero otherwise.
    pub fragment_x_offset: u16,
    /// See [`Self::fragment_x_offset`].
    pub fragment_y_offset: u16,
}

/// `fragment_header()` (§14.2). Immediately follows a parse-info header
/// with a fragment parse code, so the reader is already byte-aligned.
pub fn fragment_header(r: &mut BitReader) -> Result<FragmentHeader> {
    let picture_number = r.read_uint_lit(4) as u32;
    let fragment_data_length = r.read_uint_lit(2) as u16;
    let fragment_slice_count = r.read_uint_lit(2) as u16;
    let (fragment_x_offset, fragment_y_offset) = if fragment_slice_count != 0 {
        (r.read_uint_lit(2) as u16, r.read_uint_lit(2) as u16)
    } else {
        (0, 0)
    };
    Ok(FragmentHeader {
        picture_number,
        fragment_data_length,
        fragment_slice_count,
        fragment_x_offset,
        fragment_y_offset,
    })
}

/// A fragmented picture being reassembled (§14.3 state).
struct FragmentState {
    picture_number: u32,
    kind: PictureKind,
    /// Sequence-header snapshot taken at the setup fragment; §10.4.3
    /// guarantees any repeated in-sequence header is byte-identical, so the
    /// snapshot cannot go stale while the picture is in flight.
    seq: SequenceHeader,
    tp: TransformParameters,
    td: TransformData,
    /// `state[fragment_slices_received]`.
    slices_received: u64,
}

/// Stateful VC-2 stream walker (§10.4.1 `parse_sequence`, including the
/// §14 fragment path).
///
/// Feed byte chunks containing whole data units — each starting with a
/// parse-info header — via [`Self::push`]; completed pictures come back in
/// stream order. The sequence header and any partially assembled fragmented
/// picture persist across calls, so a picture may be fragmented across
/// multiple pushes. An end-of-sequence data unit resets the per-sequence
/// state (§10.4.1 `reset_state`): a following sequence starts fresh, as in a
/// concatenated VC-2 stream (§10.3).
#[derive(Default)]
pub struct SequenceDecoder {
    seq_header: Option<SequenceHeader>,
    fragment: Option<FragmentState>,
}

impl SequenceDecoder {
    /// Fresh decoder with no sequence header and no picture in flight.
    pub fn new() -> Self {
        Self::default()
    }

    /// True while a fragmented picture is partially assembled — an error
    /// condition if the stream ends now (§14.1: "A sequence shall not end
    /// while a fragmented picture is incomplete").
    pub fn has_incomplete_picture(&self) -> bool {
        self.fragment.is_some()
    }

    /// Drop all per-sequence state (`reset_state`, §10.4.1).
    pub fn reset(&mut self) {
        self.seq_header = None;
        self.fragment = None;
    }

    /// Walk every data unit in `data`, returning the pictures completed
    /// within it. `data` must start at a parse-info header and contain
    /// whole data units.
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<DecodedPicture>> {
        let mut r = BitReader::new(data);
        let mut pictures = Vec::new();

        loop {
            let pi = parse_info(&mut r)?;
            // The offset of this parse-info header within the chunk, so that
            // we can honour next_parse_offset for skip-only data units.
            let header_pos = r.byte_pos().saturating_sub(13);
            match classify(pi.parse_code) {
                DataUnit::EndOfSequence => {
                    // §14.1: a sequence shall not end mid-picture.
                    if self.fragment.is_some() {
                        return Err(Error::InvalidValue(
                            "end of sequence while a fragmented picture is incomplete",
                        ));
                    }
                    self.reset();
                }
                DataUnit::SequenceHeader => {
                    // §10.4.3: repeated in-sequence headers are byte-for-byte
                    // identical, so re-parsing mid-fragment is harmless.
                    self.seq_header = Some(params::sequence_header(&mut r)?);
                }
                DataUnit::Picture(kind) => {
                    // §14.1: no non-fragmented picture may interleave with an
                    // incomplete fragmented picture.
                    if self.fragment.is_some() {
                        return Err(Error::InvalidValue(
                            "picture data unit while a fragmented picture is incomplete",
                        ));
                    }
                    let seq = self.seq_header.ok_or(Error::MissingSequenceHeader)?;
                    let parsed = picture::picture_parse(&mut r, &seq, kind)?;
                    pictures.push(picture::picture_decode(&parsed, &seq)?);
                }
                DataUnit::Fragment(kind) => {
                    if let Some(pic) = self.fragment_parse(&mut r, kind)? {
                        pictures.push(pic);
                    }
                }
                DataUnit::AuxiliaryData | DataUnit::Padding | DataUnit::Reserved => {
                    // Skip the data unit body using next_parse_offset
                    // (§10.4.4 / §10.4.5).
                    skip_to_next(&mut r, &pi, header_pos)?;
                }
            }

            r.byte_align();
            if r.is_end_of_stream() {
                return Ok(pictures);
            }
        }
    }

    /// `fragment_parse()` (§14.1) plus the §10.4.1 hook: when the fragment
    /// completes its picture, decode and return it.
    fn fragment_parse(
        &mut self,
        r: &mut BitReader,
        kind: PictureKind,
    ) -> Result<Option<DecodedPicture>> {
        let fh = fragment_header(r)?;
        if fh.fragment_slice_count == 0 {
            // Setup fragment: transform parameters + initialize_fragment_state
            // (§14.3). §14.1 forbids a second setup fragment while a
            // fragmented picture is incomplete.
            if self.fragment.is_some() {
                return Err(Error::InvalidValue(
                    "setup fragment while a fragmented picture is incomplete",
                ));
            }
            let seq = self.seq_header.ok_or(Error::MissingSequenceHeader)?;
            let tp = transform::transform_parameters(r, &seq, kind)?;
            let td = transform::init_transform_data(&seq, &tp);
            self.fragment = Some(FragmentState {
                picture_number: fh.picture_number,
                kind,
                seq,
                tp,
                td,
                slices_received: 0,
            });
            return Ok(None);
        }

        // Data fragment (§14.4).
        let st = self.fragment.as_mut().ok_or(Error::InvalidValue(
            "data fragment without a preceding setup fragment",
        ))?;
        if st.kind != kind {
            return Err(Error::InvalidValue(
                "data fragment parse code disagrees with its setup fragment",
            ));
        }
        // §14.2: data fragments carry the same picture number as their
        // associated setup fragment.
        if st.picture_number != fh.picture_number {
            return Err(Error::InvalidValue(
                "data fragment picture number disagrees with its setup fragment",
            ));
        }
        let total_slices = st.tp.slices_x * st.tp.slices_y;
        let start = fh.fragment_y_offset as u64 * st.tp.slices_x + fh.fragment_x_offset as u64;
        // §14.2: slices are coded in raster order from (0, 0), none omitted
        // or repeated — so each data fragment must resume exactly where the
        // previous one stopped, and must not run past the picture.
        if start != st.slices_received {
            return Err(Error::InvalidValue(
                "data fragment slice offset out of raster order",
            ));
        }
        if start + fh.fragment_slice_count as u64 > total_slices {
            return Err(Error::InvalidValue(
                "data fragment carries more slices than the picture holds",
            ));
        }
        for s in 0..fh.fragment_slice_count as u64 {
            let idx = start + s;
            let sx = idx % st.tp.slices_x;
            let sy = idx / st.tp.slices_x;
            transform::unpack_slice(r, &st.tp, &mut st.td, sx, sy, kind)?;
            st.slices_received += 1;
        }
        if st.slices_received < total_slices {
            return Ok(None);
        }

        // state[fragmented_picture_done] (§14.4): finish DC prediction and
        // decode, then clear the in-flight state.
        let mut st = self.fragment.take().expect("fragment state present");
        if kind.uses_dc_prediction() {
            transform::apply_dc_prediction(&mut st.td);
        }
        let parsed = ParsedPicture {
            picture_number: st.picture_number,
            kind: st.kind,
            transform_parameters: st.tp,
            transform_data: st.td,
        };
        picture::picture_decode(&parsed, &st.seq).map(Some)
    }
}

/// `parse_sequence()` (§10.4.1): walk parse-info headers and data units,
/// decoding every picture — fragmented or not — into the returned vector.
///
/// Auxiliary / padding data units and unknown parse codes are skipped using
/// the parse-info `next_parse_offset`. The input may hold several
/// concatenated sequences (a VC-2 stream, §10.3); an end-of-sequence data
/// unit resets the per-sequence state and parsing continues with any
/// remaining bytes. Errors if the input ends while a fragmented picture is
/// still incomplete.
pub fn parse_sequence(data: &[u8]) -> Result<Vec<DecodedPicture>> {
    let mut decoder = SequenceDecoder::new();
    let pictures = decoder.push(data)?;
    if decoder.has_incomplete_picture() {
        return Err(Error::InvalidValue(
            "stream ended while a fragmented picture is incomplete",
        ));
    }
    Ok(pictures)
}

/// Advance the reader to the next parse-info header using the current
/// header's `next_parse_offset` (bytes from this header to the next).
fn skip_to_next(r: &mut BitReader, pi: &ParseInfo, header_pos: usize) -> Result<()> {
    if pi.next_parse_offset == 0 {
        // Unknown length: nothing reliable to skip; align and let the loop's
        // EOS / prefix check handle termination.
        r.byte_align();
        return Ok(());
    }
    let target = header_pos + pi.next_parse_offset as usize;
    r.byte_align();
    let cur = r.byte_pos();
    if target > cur {
        r.skip_bytes((target - cur) as u64)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_table4_codes() {
        assert_eq!(classify(0x00), DataUnit::SequenceHeader);
        assert_eq!(classify(0x10), DataUnit::EndOfSequence);
        assert_eq!(classify(0x20), DataUnit::AuxiliaryData);
        assert_eq!(classify(0x30), DataUnit::Padding);
        assert_eq!(classify(0xC8), DataUnit::Picture(PictureKind::LowDelay));
        assert_eq!(classify(0xE8), DataUnit::Picture(PictureKind::HighQuality));
        assert_eq!(classify(0xCC), DataUnit::Fragment(PictureKind::LowDelay));
        assert_eq!(classify(0xEC), DataUnit::Fragment(PictureKind::HighQuality));
    }

    #[test]
    fn end_of_sequence_only_stream() {
        // BBCD + parse_code 0x10 + next_off=0 + prev_off=0 = 13-byte header,
        // a degenerate but valid sequence with no pictures.
        let mut buf = Vec::new();
        buf.extend_from_slice(&PARSE_INFO_PREFIX);
        buf.push(0x10);
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        let pics = parse_sequence(&buf).unwrap();
        assert!(pics.is_empty());
    }

    #[test]
    fn bad_prefix_rejected() {
        let buf = [0, 0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(matches!(
            parse_sequence(&buf),
            Err(Error::BadParseInfoPrefix(_))
        ));
    }

    #[test]
    fn fragment_header_setup_omits_offsets() {
        // picture_number 5, data_length 0x0102, slice_count 0 -> 8 bytes,
        // no x/y offsets in the stream.
        let bytes = [0, 0, 0, 5, 0x01, 0x02, 0, 0];
        let mut r = BitReader::new(&bytes);
        let fh = fragment_header(&mut r).unwrap();
        assert_eq!(fh.picture_number, 5);
        assert_eq!(fh.fragment_data_length, 0x0102);
        assert_eq!(fh.fragment_slice_count, 0);
        assert_eq!(fh.fragment_x_offset, 0);
        assert_eq!(fh.fragment_y_offset, 0);
        assert!(r.is_end_of_stream());
    }

    #[test]
    fn fragment_header_data_reads_offsets() {
        let bytes = [0, 0, 0, 7, 0, 0, 0, 3, 0, 2, 0, 1];
        let mut r = BitReader::new(&bytes);
        let fh = fragment_header(&mut r).unwrap();
        assert_eq!(fh.picture_number, 7);
        assert_eq!(fh.fragment_slice_count, 3);
        assert_eq!(fh.fragment_x_offset, 2);
        assert_eq!(fh.fragment_y_offset, 1);
        assert!(r.is_end_of_stream());
    }
}
