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

### Not yet implemented

- `oxideav-core` `Decoder` factory wiring (the `registry` feature registers
  an empty entry-point for now, mirroring the VP6 scaffold).

## Usage (standalone)

```rust,no_run
let stream: &[u8] = &[/* a VC-2 sequence */];
let pictures = oxideav_vc2::decode_sequence(stream).unwrap();
for pic in &pictures {
    println!("picture {} is {}x{}", pic.picture_number, pic.luma_width, pic.luma_height);
}
```

The crate builds with `--no-default-features` for a dependency-free standalone
decoder; the default `registry` feature pulls in `oxideav-core` for fleet
registration.

## License

MIT © Karpelès Lab Inc. The VC-2 standard text is © SMPTE; this crate
implements the royalty-free codec without redistributing the standard.
