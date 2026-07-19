# Changelog

All notable changes to this crate are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **Container-tag declaration: FourCC `BBCD`.** `register(ctx)` now
  claims the one wire identifier the staged specification grounds — the
  parse-info prefix bytes `0x42 0x42 0x43 0x44`, the character string
  "BBCD" as expressed by ISO/IEC 646 (§10.5.1, NOTE 1), which every
  VC-2 data unit begins with — so a container's `CodecResolver` can
  route VC-2 essence tagged by its stream magic to the `"vc2"` decoder.
  The claim carries a confidence probe: a peeked first packet is
  decisive either way (the packet contract requires whole data units,
  each starting with the prefix), an out-of-band sequence header staged
  as the container's stream-format blob confirms, any other blob shape
  is non-disqualifying (the decoder tolerates unrecognized extradata),
  and a bare tag match resolves with weak confidence so a
  harder-evidenced claimant of the same tag would win. ST 2042-1:2022
  registers no container-scoped identifiers of its own (no AVI/MP4
  FourCC, Matroska CodecID, MP4 ObjectTypeIndication or MXF label;
  Annex C defers even level values to companion SMPTE documents), so no
  other tag is declared until staged references ground one.

- **Mixed and off-format ≤12-bit depths decode through the `Decoder`,
  represented via the per-plane significant-bits side-channel.** Custom
  §11.4.9 signal ranges whose derived component depths have no exact
  planar format — mixed pairs such as 12-bit luma with 10-bit chroma,
  and uniform odd depths like 9 or 11 — were previously reported
  unsupported by the wrapper. They now decode onto the natural common
  storage surface for the picture's deepest component (byte planes up
  to 8 bits, `P10Le` words up to 10, `P12Le` words up to 12) with every
  plane's §15.5 code values kept verbatim in the low bits of the
  storage word (LSB-anchored, the core partial-depth convention), and
  the emitted frame carries the `oxideav-core` 0.1.31 per-plane
  significant-bits side-channel (`VideoFrame::significant_bits` /
  `plane_significant_bits`) recording each plane's true depth —
  represented, not promoted or refused. Uniform 8/10/12-bit pictures
  attach no record and remain byte-identical, and the >12-bit promotion
  path (Table 10 presets 7/8 and deep custom ranges, mixed or not) also
  deliberately stays record-free and byte-identical: its `16 - depth`
  left shift already places each plane's full-scale at the top of the
  16-bit word, so an LSB-anchored depth record would misdescribe where
  the values sit. Covered end-to-end by tests for the 12/10 headline
  case, sub-byte mixes (8/6), uniform 9-bit, boundary depth 1 and the
  12/1 extreme spread, record presence switching across concatenated
  sequences, fragmented mixed-depth pictures, and no-record assertions
  on the exact-format and promoted paths. The conformance matrix gains
  an eighth pinned fixture for the headline shape — a custom range
  assembled from Table 10 rows (preset-4 luma pair, preset-3
  colour-difference pair) at 4:2:2 through a depth-1 LeGall transform —
  whose `Decoder` frame must byte-match the pinned standalone reference
  with the `[12, 10, 10]` record attached. Probe experiments confirm
  the black-box validator's envelope excludes **every** custom
  (index 0) §11.4.9 signal range — even one holding exactly the
  preset-3 values it accepts by index — so, like the 16-bit presets,
  the mixed fixture is a self-consistent reference riding the
  externally corroborated decode path. Hardening keeps pace: the
  every-truncation-point sweep and the every-bit-flip corruption sweep
  now also run over a mixed 12/10 custom-range stream (whose explicit
  §11.4.9 index-0 range fields add cut and flip points the preset
  headers don't have), and a wrapper-level flip sweep pushes every
  mutant through a fresh `Decoder`, asserting each emitted frame is
  well-formed — exactly three image planes and, when present, a
  three-entry significant-bits record with every value in 1..=16.

- **16-bit output through the registered `Decoder`.** Pictures whose
  video depth exceeds 12 bits — the Table 10 preset-7/8 (16-bit) signal
  ranges and custom §11.4.9 ranges with excursions above 4095 — now map
  onto the new `oxideav-core` 0.1.30 `Yuv420P16Le` / `Yuv422P16Le` /
  `Yuv444P16Le` formats instead of being reported unsupported. Depth-16
  code values pass through verbatim (their code space already is the
  16-bit surface); 13–15-bit components are promoted by a `16 - depth`
  left shift per plane, the same power-of-two scaling Table 10 uses
  between its own narrow-range presets, so signalled offset/excursion
  levels scale consistently onto the full-width words. Mixed-depth
  pictures where the deeper component needs more than 12 bits ride the
  same surface with per-plane shifts; mismatched pairs at or below 12
  bits remain unsupported rather than being silently promoted.
  Hardening keeps pace with the new paths: custom §11.4.9 signal
  *offsets* are now bounded to the representable 0..=65535 code space
  (documented implementation cap alongside the existing excursion
  bound), and the truncation sweep plus a new every-bit-flip corruption
  sweep also run over a 16-bit stream.

- **Fixture-pinned conformance matrix.** Seven hand-assembled
  single-picture HQ fixtures (staged with generation notes under
  `docs/video/vc2/fixtures/`) are committed as test vectors with their
  exact decoded planes pinned: 10/12-bit video-range streams across
  4:4:4 / 4:2:2 / 4:2:0 and Haar / LeGall / Deslauriers-Dubuc (9,7) at
  depths 1–2 — all five verified **bit-exact** against an independent
  black-box validator binary (opaque CLI only) — plus the two 16-bit
  presets (7/8), which every probed validator refuses at the sequence
  header (its envelope excludes signal-range indices 5..=8), pinned as
  self-consistent references on the externally corroborated code path.
  Tests also prove the committed fixture bytes remain reproducible
  from the spec-pseudocode harness, and that the 16-bit fixtures reach
  a `Decoder` frame word-for-word.

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
