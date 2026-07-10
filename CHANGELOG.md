# Changelog

All notable changes to this crate are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **Hostile-stream hardening.** Truncated, malformed and adversarial
  inputs now produce prompt, deterministic errors instead of hangs,
  panics or unbounded allocations:
  - `BitReader` latches an `overrun` flag once any bit is consumed past
    the end of the input; the exp-Golomb readers bail out instead of
    spinning forever on the endless zero bits (previously an infinite
    loop in release builds), absurd in-band prefixes saturate instead of
    overflowing, and the structured parsers (parse-info, sequence
    header, fragment header, transform parameters, per-slice and
    end-of-picture) turn the flag into `UnexpectedEof`.
  - `quant_factor` computes in 128 bits and saturates (a 1-byte HQ
    qindex of 255 previously overflowed `i64` and panicked);
    `inverse_quant` saturates; the lifting, bit-shift and DC-prediction
    arithmetic wraps deterministically (the §15.5 clip bounds output).
  - Documented implementation caps rejected at parse time: total
    transform depth (16), padded picture area (2^26 samples), slice
    count (2^22), LD slice-bytes numerator and HQ prefix/scaler bytes;
    zero frame dimensions and signal excursions outside 1..=65535 (the
    16-bit output range) are rejected; a low-delay slice smaller than
    its own 7-bit header errors instead of underflowing.
  - `parse_sequence` / `decode_sequence` now require the terminating
    end-of-sequence data unit (§10.4.1), so a stream truncated at a
    data-unit boundary no longer decodes silently;
    `SequenceDecoder::mid_sequence()` exposes the state for chunked
    callers.
  - Tests: every-truncation-point sweep over a good stream, one test per
    cap, a max-qindex decode, an over-long HQ length code, and a
    deterministic garbage fuzz-lite loop.

- **`oxideav-core` `Decoder` wiring.** The `registry` feature's empty
  entry point is replaced by a real registration: `register(ctx)`
  installs a `"vc2"` video decoder (intra-only, lossy + lossless) into
  the codec registry, and `make_decoder(params)` is the direct factory
  (workspace dual-API convention). `Vc2Decoder` wraps the stateful
  `SequenceDecoder`, so packets carry whole VC-2 data units and
  fragmented pictures may span packets; frames are emitted as planar
  YUV 4:4:4/4:2:2/4:2:0 at 8 bit (byte planes) or 10/12 bit
  (little-endian 16-bit words), an extradata blob starting with a
  parse-info prefix primes the walker (out-of-band sequence header),
  `flush` surfaces truncated fragmented pictures as errors, and `reset`
  clears all carry-over state. `DecodedPicture` now carries
  `luma_depth` / `color_diff_depth`. 16-bit output presets are reported
  unsupported by the wrapper (no matching core pixel format yet);
  the standalone API still decodes them.

- **Picture fragments (§14).** Setup / data fragment headers
  (`fragment_header`, §14.2), slice reassembly in raster order and the
  deferred DC-prediction pass once all slices arrive (§14.4). The new
  stateful `SequenceDecoder` walker retains the sequence header and any
  partially assembled fragmented picture across `push` calls, so
  packetized input works; `decode_sequence` / `parse_sequence` reassemble
  fragments transparently and now also decode concatenated sequences (a
  VC-2 stream, §10.3), resetting per-sequence state at each
  end-of-sequence unit. The §14.1/§14.2 constraints are enforced as
  stream errors: no data fragment without a setup fragment, matching
  picture numbers and parse-code kind, no omitted / repeated /
  out-of-raster-order slices, no interleaved non-fragmented pictures or
  second setup fragments, and no end of sequence while a picture is
  incomplete. Fragment-vs-picture equality tests (HQ and LD with DC
  prediction), chunked-push reassembly, and one test per constraint
  violation.

- **Complete Annex D default quantization matrices.** The default-matrix
  lookup now covers every combination the annex defines: the asymmetric
  (`dwt_depth_ho > 0`) blocks of Tables D.1–D.7 for all seven symmetric
  filter pairs, and Table D.8 for the one mixed pair (Haar-no-shift 2-D
  with LeGall horizontal-only). New `quant::default_quant_matrix_full`
  entry point keyed on the full `(wavelet_index, wavelet_index_ho,
  dwt_depth, dwt_depth_ho)` parameter set; `MatrixLevel::H` variant for
  the horizontal-only band values (also used when parsing custom
  matrices). Combinations outside the tables — depths above 4, total
  depth above 5, other mixed pairs — normatively require a custom matrix
  (§12.4.5.3) and now fail with `MissingQuantMatrix` instead of every
  asymmetric stream doing so. End-to-end test decodes an asymmetric
  major-version-3 HQ stream through the extended transform parameters
  and the Table D.8 defaults.

- Initial clean-room bootstrap of the **SMPTE ST 2042-1:2022 VC-2** intra
  decoder, built solely against the normative PDF under `docs/video/vc2/`.
  The complete intra-picture decode path is implemented and validated
  end-to-end on a hand-assembled high-quality VC-2 stream:
  - **Data coding** (Annex A) — `BitReader` with `read_bit` / `read_byte` /
    `byte_align`, the fixed literals, the unsigned & signed interleaved
    exp-Golomb codes, and the bounded-block reads (`read_*b` /
    `flush_inputb`). Table A.1 / A.2 conformance tests.
  - **Stream structure** (§10) — parse-info ("BBCD") headers, the Table 4 /
    Table 5 parse-code classification, and a `parse_sequence` walk that
    decodes pictures and skips auxiliary / padding / reserved units via
    `next_parse_offset`.
  - **Sequence header** (§11) — parse parameters, the Annex B base-format
    decode-critical defaults (all 23 formats), source-parameter overrides,
    preset signal ranges (Table 10), and `set_coding_parameters` (§11.6,
    picture dimensions + video depth via `intlog2`).
  - **Transform parameters** (§12.4) — wavelet filter / depth, the §12.4.4
    extended asymmetric parameters, slice parameters (LD + HQ), and the
    §12.4.5.3 quantization-matrix read with the symmetric Annex D default
    matrices (Tables D.1–D.7, `dwt_depth_ho == 0` block) built in.
  - **Transform data** (§13) — subband dimensions (§13.2.3), inverse
    quantization (§13.3, with floor-division `mean`), DC-band prediction
    (§13.4), and slice unpacking for low-delay (§13.5.3) and high-quality
    (§13.5.4) pictures.
  - **Picture decode** (§15) — the integer lifting IDWT for all seven
    wavelet filters (§15.4.4 / Tables 16–22, reversibility-tested on
    LeGall), `vh_synthesis` / `h_synthesis`, pad removal (§15.4.5),
    clipping (§15.5) and unsigned offsetting.
- Public `decode_sequence` standalone entry point and a `registry` cargo
  feature (default on) exposing an `oxideav-core` registration shim.
