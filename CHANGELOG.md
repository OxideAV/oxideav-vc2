# Changelog

All notable changes to this crate are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

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
