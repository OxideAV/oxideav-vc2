//! Crate-local error type. Kept free of `oxideav-core` so the decoder
//! compiles and is testable with `--no-default-features` (the standalone
//! image-format pattern).

use core::fmt;

/// Errors surfaced while parsing or decoding a VC-2 stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The stream ended before a structure was fully read.
    UnexpectedEof,
    /// A parse-info prefix did not equal `0x42 0x42 0x43 0x44` ("BBCD").
    BadParseInfoPrefix([u8; 4]),
    /// A parse code outside the set defined in Table 4 was encountered.
    UnknownParseCode(u8),
    /// `wavelet_index` / `wavelet_index_ho` outside 0..=6 (§12.4.2 Table 15).
    UnsupportedWaveletIndex(u64),
    /// A default quantization matrix is required but undefined for this
    /// (filter, depth) combination, and no custom matrix was signalled
    /// (§12.4.5.3 / Annex D).
    MissingQuantMatrix,
    /// No sequence header was parsed before a picture data unit.
    MissingSequenceHeader,
    /// A structural value violated a normative constraint (e.g. zero slices).
    InvalidValue(&'static str),
    /// A feature defined in the spec is recognised but not yet implemented.
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnexpectedEof => write!(f, "unexpected end of VC-2 stream"),
            Error::BadParseInfoPrefix(p) => {
                write!(f, "bad parse info prefix {p:02x?} (expected BBCD)")
            }
            Error::UnknownParseCode(c) => write!(f, "unknown VC-2 parse code 0x{c:02x}"),
            Error::UnsupportedWaveletIndex(i) => {
                write!(f, "unsupported wavelet index {i} (valid 0..=6)")
            }
            Error::MissingQuantMatrix => {
                write!(
                    f,
                    "default quantization matrix undefined; custom matrix required"
                )
            }
            Error::MissingSequenceHeader => {
                write!(f, "picture data unit before any sequence header")
            }
            Error::InvalidValue(m) => write!(f, "invalid VC-2 value: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported VC-2 feature: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// Crate result alias.
pub type Result<T> = core::result::Result<T, Error>;
