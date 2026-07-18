# oxideav-vc2

[![CI](https://github.com/OxideAV/oxideav-vc2/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-vc2/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-vc2.svg)](https://crates.io/crates/oxideav-vc2) [![docs.rs](https://docs.rs/oxideav-vc2/badge.svg)](https://docs.rs/oxideav-vc2) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Pure-Rust decoder for **SMPTE ST 2042-1:2022 VC-2** (a.k.a. **Dirac Pro**),
an open, royalty-free, **intra-frame wavelet** video compression system
developed by the BBC and standardised by SMPTE. Every picture is
wavelet-transformed, the subbands are quantised and entropy-coded, and each
picture decodes independently — there is no temporal prediction.

This crate is a **clean-room** implementation, written solely against the
normative SMPTE PDF mirrored under `docs/video/vc2/`. Clause numbers in the
source track the 2022 edition.

## Status

The **complete intra-picture decode path** — including fragmented pictures
and the full Annex D default-matrix set — is implemented and validated
end-to-end on hand-assembled VC-2 streams.

| Area | Spec | State |
|------|------|-------|
| Data coding (bit/byte I/O, exp-Golomb, bounded blocks) | Annex A | ✅ full, table-checked against A.1 / A.2 |
| Stream structure + parse-info walk | §10 | ✅ Table 4 / Table 5 classification; aux/padding/reserved skipped via `next_parse_offset`; concatenated sequences (§10.3) |
| Sequence header + Annex B base-format defaults | §11 | ✅ decode-critical fields; preset signal ranges (Table 10) |
| `set_coding_parameters` (dimensions, video depth) | §11.6 | ✅ |
| Transform parameters + extended (asymmetric) params | §12.4 | ✅ filter / depth / slice params / quant matrix |
| Quantization (factor, offset, inverse-quant, DC prediction) | §13.3 / §13.4 | ✅ |
| Default quant matrices | Annex D | ✅ Tables D.1–D.8 complete: all asymmetric (`dwt_depth_ho > 0`) blocks + the mixed Haar/LeGall pair; out-of-table combos require the custom matrix per §12.4.5.3 |
| Slice unpacking — low delay | §13.5.3 | ✅ |
| Slice unpacking — high quality | §13.5.4 | ✅ |
| Picture fragments (setup/data, reassembly, constraints) | §14 | ✅ raster-order slice reassembly, deferred DC prediction, §14.1/§14.2 violations rejected; chunk-fed via `SequenceDecoder::push` |
| IDWT lifting filters (all 7 wavelets) | §15.4.4 / Tables 16–22 | ✅ reversibility-tested (LeGall) |
| Component IDWT + pad removal + clip + offset | §15 | ✅ |
| `oxideav-core` `Decoder` (registry + direct factory) | — | ✅ `register(ctx)` + `make_decoder`; 8/10/12/16-bit planar YUV output (Table 10 presets 7/8 and >12-bit custom ranges ride the full-width 16-bit formats); fragments may span packets |
| Hostile-input hardening | — | ✅ truncation → `UnexpectedEof` at every cut point (8- and 16-bit streams); saturating VLC/quant math; documented caps on depth / area / slice counts / signal offsets+excursions; bit-flip + garbage fuzz-lite in CI |
| Conformance fixtures | — | ✅ 7-case pinned matrix (`tests/data/`, staged under `docs/video/vc2/fixtures/`): 10/12-bit cases bit-exact vs an independent black-box validator across all samplings + 3 wavelets; 16-bit presets 7/8 pinned as self-consistent references (validator envelope excludes ranges 5..=8) |

### Not yet implemented

- No container tags are claimed yet (FourCC / Matroska mapping for VC-2 in
  containers is deliberately left to a coordinated fleet decision).
- Mixed luma/chroma bit depths at or below 12 bits (custom signal ranges
  only) have no exact planar format and are reported unsupported by the
  `Decoder` wrapper; the standalone API decodes them.

## Usage (standalone)

```rust,no_run
let stream: &[u8] = &[/* a VC-2 sequence */];
let pictures = oxideav_vc2::decode_sequence(stream).unwrap();
for pic in &pictures {
    println!("picture {} is {}x{}", pic.picture_number, pic.luma_width, pic.luma_height);
}
```

For chunked / packetized input, `SequenceDecoder::push` keeps the sequence
header and any partially assembled fragmented picture across calls.

## Usage (oxideav registry)

With the default `registry` feature the crate follows the workspace dual
API: `oxideav_vc2::register(&mut ctx)` installs a `"vc2"` decoder into the
codec registry, and `oxideav_vc2::make_decoder(&params)` is the direct
factory. Packets must carry whole VC-2 data units (the parse-info headers
are the codec's own framing); fragmented pictures may span packets.

The crate builds with `--no-default-features` for a dependency-free standalone
decoder; the default `registry` feature pulls in `oxideav-core` for fleet
registration.

## License

MIT © Karpelès Lab Inc. The VC-2 standard text is © SMPTE; this crate
implements the royalty-free codec without redistributing the standard.
