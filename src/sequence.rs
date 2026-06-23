//! VC-2 sequence structure: parse-info headers (§10.5), data-unit dispatch
//! (§10.4.1) and the top-level `parse_sequence` driver that yields decoded
//! pictures.

use crate::bitio::BitReader;
use crate::params::{self, SequenceHeader};
use crate::picture::{self, DecodedPicture};
use crate::transform::PictureKind;
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
    /// A fragment (§14) — recognised but not yet assembled.
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

/// `parse_sequence()` (§10.4.1): walk parse-info headers and data units,
/// decoding every picture into the returned vector.
///
/// Auxiliary / padding data units and unknown parse codes are skipped using
/// the parse-info `next_parse_offset`. Fragmented pictures are recognised but
/// not yet assembled (returns [`Error::Unsupported`] if encountered).
pub fn parse_sequence(data: &[u8]) -> Result<Vec<DecodedPicture>> {
    let mut r = BitReader::new(data);
    let mut pictures = Vec::new();
    let mut seq_header: Option<SequenceHeader> = None;

    let mut pi = parse_info(&mut r)?;
    loop {
        // The offset of this parse-info header within the stream, so that we
        // can honour next_parse_offset for skip-only data units.
        let header_pos = r.byte_pos().saturating_sub(13);
        let unit = classify(pi.parse_code);
        match unit {
            DataUnit::EndOfSequence => break,
            DataUnit::SequenceHeader => {
                seq_header = Some(params::sequence_header(&mut r)?);
            }
            DataUnit::Picture(kind) => {
                let seq = seq_header.ok_or(Error::MissingSequenceHeader)?;
                let parsed = picture::picture_parse(&mut r, &seq, kind)?;
                pictures.push(picture::picture_decode(&parsed, &seq)?);
            }
            DataUnit::Fragment(_) => {
                return Err(Error::Unsupported(
                    "picture fragments (§14) not yet assembled",
                ));
            }
            DataUnit::AuxiliaryData | DataUnit::Padding | DataUnit::Reserved => {
                // Skip the data unit body using next_parse_offset (§10.4.4/5).
                skip_to_next(&mut r, &pi, header_pos)?;
            }
        }

        if r.is_end_of_stream() {
            break;
        }
        pi = parse_info(&mut r)?;
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
}
