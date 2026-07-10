//! VC-2 bit/byte input and the variable-length code readers.
//!
//! Implements the data-coding layer of SMPTE ST 2042-1:2022 Annex A:
//!
//! * A.2 — `read_byte`, `read_bit`, `byte_align`, `is_end_of_stream`.
//! * A.3 — `read_bool`, `read_nbits`, `read_uint_lit`.
//! * A.4 — bounded-block reads (`read_bitb` / `flush_inputb`) plus the
//!   unsigned/signed interleaved exp-Golomb codes (`read_uint`,
//!   `read_uintb`, `read_sint`, `read_sintb`).
//!
//! The reader maintains `state[current_byte]` and `state[next_bit]` as
//! described in A.2.1: bits are consumed MSB first, `next_bit` runs from
//! 7 down to 0, and a fresh byte is fetched when it underflows.
//!
//! Reads past the end of the input surface zero bits (A.2.2 leaves them
//! undefined) and latch the [`BitReader::overrun`] flag, so structured
//! parsers can turn a truncated stream into a deterministic
//! [`Error::UnexpectedEof`] instead of consuming fabricated data — and so
//! the variable-length decoders cannot spin forever on the endless zeros.

use crate::Error;

/// Bit reader over a VC-2 stream byte slice (Annex A.2).
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Index of the *next* byte to be fetched by `read_byte`.
    pos: usize,
    /// `state[current_byte]` — a copy of the byte being consumed.
    current_byte: u8,
    /// `state[next_bit]` — 7..=0, MSB first; -1 internally triggers refill.
    next_bit: i32,
    /// Remaining bit budget for bounded-block reads (`state[bits_left]`).
    bits_left: u64,
    /// Latched once any bit is consumed from beyond the end of `data`.
    overrun: bool,
}

impl<'a> BitReader<'a> {
    /// Create a reader positioned at the start of `data`.
    ///
    /// Per A.2.1, `state[current_byte]` is initialised with the first byte
    /// of the stream and `state[next_bit]` to 7.
    pub fn new(data: &'a [u8]) -> Self {
        let (current_byte, pos) = if data.is_empty() {
            // As if a synthetic past-the-end byte had been prefetched, so
            // the very first read latches `overrun`.
            (0, 1)
        } else {
            (data[0], 1)
        };
        BitReader {
            data,
            pos,
            current_byte,
            next_bit: 7,
            bits_left: 0,
            overrun: false,
        }
    }

    /// True once any bit has been consumed from beyond the end of the
    /// input. Such bits read as zero; callers of the structured parsers
    /// check this to reject truncated streams.
    #[inline]
    pub fn overrun(&self) -> bool {
        self.overrun
    }

    /// Byte offset of the byte currently being consumed, relative to the
    /// start of the stream slice. When byte-aligned this is the offset of the
    /// next byte to be read; mid-byte it is the offset of the partly-read
    /// byte. Used to honour parse-info next/previous offsets (§10.5.1).
    #[inline]
    pub fn byte_pos(&self) -> usize {
        // `current_byte` was fetched from `data[pos - 1]`; while any of its
        // bits remain (next_bit < 7) that byte is the logical position. Once
        // its last bit is consumed `read_byte` advances `pos`, so the freshly
        // fetched byte (`pos - 1`) is again the logical position.
        self.pos.saturating_sub(1)
    }

    /// `read_byte()` (A.2.2): advance to the next stream byte.
    #[inline]
    fn read_byte(&mut self) {
        self.next_bit = 7;
        if self.pos < self.data.len() {
            self.current_byte = self.data[self.pos];
            self.pos += 1;
        } else {
            // Past end of stream — A.2.2 leaves `current_byte` undefined; we
            // surface zero bits so callers can detect EOS deterministically.
            self.current_byte = 0;
            self.pos = self.data.len() + 1;
        }
    }

    /// `read_bit()` (A.2.3).
    #[inline]
    pub fn read_bit(&mut self) -> u32 {
        if self.pos > self.data.len() {
            // `current_byte` is the synthetic past-the-end byte.
            self.overrun = true;
        }
        let bit = ((self.current_byte >> self.next_bit) & 1) as u32;
        self.next_bit -= 1;
        if self.next_bit < 0 {
            self.next_bit = 7;
            self.read_byte();
        }
        bit
    }

    /// `byte_align()` (A.2.4): discard the rest of the current byte unless
    /// already byte-aligned.
    #[inline]
    pub fn byte_align(&mut self) {
        if self.next_bit != 7 {
            self.read_byte();
        }
    }

    /// `is_end_of_stream()` (A.2.5).
    #[inline]
    pub fn is_end_of_stream(&self) -> bool {
        // No more bytes available and all bits of current_byte consumed.
        self.pos >= self.data.len() && self.next_bit == 7
    }

    /// `read_bool()` (A.3.2).
    #[inline]
    pub fn read_bool(&mut self) -> bool {
        self.read_bit() == 1
    }

    /// `read_nbits(n)` (A.3.3): `n`-bit unsigned integer literal, MSB first.
    #[inline]
    pub fn read_nbits(&mut self, n: u32) -> u64 {
        let mut val: u64 = 0;
        for _ in 0..n {
            val <<= 1;
            val += self.read_bit() as u64;
        }
        val
    }

    /// `read_uint_lit(n)` (A.3.4): `n`-byte unsigned integer literal.
    #[inline]
    pub fn read_uint_lit(&mut self, n: u32) -> u64 {
        self.read_nbits(8 * n)
    }

    /// `read_uint()` (A.4.3): unsigned interleaved exp-Golomb.
    ///
    /// Robustness: past the end of the stream the reader surfaces zero
    /// bits forever, which look like an unterminated prefix — the loop
    /// bails out with `u64::MAX` once [`Self::overrun`] latches (callers
    /// reject the stream). Absurdly long in-band prefixes saturate at
    /// `u64::MAX - 1` instead of overflowing.
    pub fn read_uint(&mut self) -> u64 {
        let mut value: u64 = 1;
        while self.read_bit() == 0 {
            if self.overrun {
                return u64::MAX;
            }
            // Saturating shift-left by one (checked_shl only guards the
            // shift amount, not value overflow).
            value = if value >> 63 != 0 {
                u64::MAX
            } else {
                value << 1
            };
            if self.read_bit() == 1 {
                value = value.saturating_add(1);
            }
        }
        value - 1
    }

    /// `read_sint()` (A.4.4): signed interleaved exp-Golomb.
    pub fn read_sint(&mut self) -> i64 {
        let value = self.read_uint().min(i64::MAX as u64) as i64;
        if value != 0 && self.read_bit() == 1 {
            -value
        } else {
            value
        }
    }

    // --- Bounded-block reads (A.4.2) -------------------------------------

    /// Set `state[bits_left]` for the next bounded-block read sequence.
    #[inline]
    pub fn set_bits_left(&mut self, bits: u64) {
        self.bits_left = bits;
    }

    /// `read_bitb()` (A.4.2): returns 1 by default once the block is empty.
    #[inline]
    pub fn read_bitb(&mut self) -> u32 {
        if self.bits_left == 0 {
            1
        } else {
            self.bits_left -= 1;
            self.read_bit()
        }
    }

    /// `read_uintb()` (A.4.3, bounded variant). Same overrun / saturation
    /// robustness as [`Self::read_uint`] (a huge `bits_left` budget over a
    /// truncated stream must not spin).
    pub fn read_uintb(&mut self) -> u64 {
        let mut value: u64 = 1;
        while self.read_bitb() == 0 {
            if self.overrun {
                return u64::MAX;
            }
            value = if value >> 63 != 0 {
                u64::MAX
            } else {
                value << 1
            };
            if self.read_bitb() == 1 {
                value = value.saturating_add(1);
            }
        }
        value - 1
    }

    /// `read_sintb()` (A.4.4, bounded variant).
    pub fn read_sintb(&mut self) -> i64 {
        let value = self.read_uintb().min(i64::MAX as u64) as i64;
        if value != 0 && self.read_bitb() == 1 {
            -value
        } else {
            value
        }
    }

    /// `flush_inputb()` (A.4.2): discard the remainder of the block. A
    /// truncated stream stops the drain early (there is nothing left to
    /// discard past the end).
    pub fn flush_inputb(&mut self) {
        while self.bits_left > 0 {
            if self.overrun {
                self.bits_left = 0;
                return;
            }
            self.read_bit();
            self.bits_left -= 1;
        }
    }

    /// Skip `n` whole bytes (used for slice prefix bytes / data units).
    /// Errors once the skip runs past the end of the stream.
    pub fn skip_bytes(&mut self, n: u64) -> Result<(), Error> {
        for _ in 0..n {
            self.read_uint_lit(1);
            if self.overrun {
                return Err(Error::UnexpectedEof);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nbits_msb_first() {
        // 0b1011_0010 = 0xB2
        let mut r = BitReader::new(&[0xB2]);
        assert_eq!(r.read_nbits(4), 0b1011);
        assert_eq!(r.read_nbits(4), 0b0010);
    }

    #[test]
    fn uint_table_a1() {
        // Table A.1: bit sequences → decoded values.
        // Pack the listed prefixes back-to-back into bytes and read them.
        // 1 -> 0; 001 -> 1; 011 -> 2; 00001 -> 3 ...
        let cases: &[(&[u32], u64)] = &[
            (&[1], 0),
            (&[0, 0, 1], 1),
            (&[0, 1, 1], 2),
            (&[0, 0, 0, 0, 1], 3),
            (&[0, 0, 0, 1, 1], 4),
            (&[0, 1, 0, 0, 1], 5),
            (&[0, 1, 0, 1, 1], 6),
            (&[0, 0, 0, 0, 0, 0, 1], 7),
            (&[0, 0, 0, 0, 0, 1, 1], 8),
            (&[0, 0, 0, 1, 0, 0, 1], 9),
        ];
        for (bits, expected) in cases {
            let bytes = pack_bits(bits);
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.read_uint(), *expected, "bits {bits:?}");
        }
    }

    #[test]
    fn sint_table_a2() {
        // Table A.2: bit sequences → signed values.
        let cases: &[(&[u32], i64)] = &[
            (&[0, 0, 0, 1, 1, 1], -4),
            (&[0, 0, 0, 0, 1, 1], -3),
            (&[0, 1, 1, 1], -2),
            (&[0, 0, 1, 1], -1),
            (&[1], 0),
            (&[0, 0, 1, 0], 1),
            (&[0, 1, 1, 0], 2),
            (&[0, 0, 0, 0, 1, 0], 3),
            (&[0, 0, 0, 1, 1, 0], 4),
        ];
        for (bits, expected) in cases {
            let bytes = pack_bits(bits);
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.read_sint(), *expected, "bits {bits:?}");
        }
    }

    #[test]
    fn read_uint_terminates_at_eof() {
        // Past the end the reader surfaces zero bits forever, which look
        // like an unterminated exp-Golomb prefix; the loop must bail out
        // (returning u64::MAX) with the overrun flag latched, not spin.
        let mut r = BitReader::new(&[0x00, 0x00]);
        assert_eq!(r.read_uint(), u64::MAX);
        assert!(r.overrun());
        // Same for the bounded variant with a huge budget.
        let mut r = BitReader::new(&[0x00]);
        r.set_bits_left(u64::MAX);
        assert_eq!(r.read_uintb(), u64::MAX);
        assert!(r.overrun());
    }

    #[test]
    fn read_uint_saturates_absurd_in_band_prefix() {
        // 40 zero bytes = 160 (follow=0, data=0) pairs, then a terminating
        // 1: encodes a value beyond 64 bits. Must saturate, not overflow.
        let mut data = vec![0u8; 40];
        data.push(0x80);
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_uint(), u64::MAX - 1);
        assert!(!r.overrun());
    }

    #[test]
    fn exact_final_byte_is_not_overrun() {
        let mut r = BitReader::new(&[0xAB]);
        assert_eq!(r.read_uint_lit(1), 0xAB);
        assert!(!r.overrun());
        assert!(r.is_end_of_stream());
        r.read_bit();
        assert!(r.overrun());
    }

    #[test]
    fn empty_input_overruns_immediately() {
        let mut r = BitReader::new(&[]);
        assert!(r.is_end_of_stream());
        r.read_bit();
        assert!(r.overrun());
    }

    #[test]
    fn skip_bytes_errors_past_end() {
        let mut r = BitReader::new(&[1, 2, 3]);
        assert!(r.skip_bytes(3).is_ok());
        assert!(!r.overrun());
        assert!(r.skip_bytes(1).is_err());
    }

    #[test]
    fn flush_inputb_stops_at_eof() {
        let mut r = BitReader::new(&[0xFF]);
        r.set_bits_left(u64::MAX);
        r.flush_inputb(); // must terminate promptly
        assert!(r.overrun());
    }

    #[test]
    fn bounded_block_returns_default_one() {
        // An exhausted block reads as a solitary 1 -> read_uintb returns 0.
        let mut r = BitReader::new(&[0x00]);
        r.set_bits_left(0);
        assert_eq!(r.read_uintb(), 0);
        assert_eq!(r.read_sintb(), 0);
    }

    /// Pack a list of bit values (MSB first) into bytes, zero-padding the
    /// final byte.
    fn pack_bits(bits: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cur = 0u8;
        let mut n = 0u32;
        for &b in bits {
            cur = (cur << 1) | (b as u8 & 1);
            n += 1;
            if n == 8 {
                out.push(cur);
                cur = 0;
                n = 0;
            }
        }
        if n > 0 {
            cur <<= 8 - n;
            out.push(cur);
        }
        out
    }
}
