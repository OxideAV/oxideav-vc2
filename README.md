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
| Sequence header + Annex B base-format defaults | §11 | ✅ complete Annex B parameter map (Tables B.1–B.3); full §11.4 retention incl. frame rate / aspect / clean area / colour spec (Tables 8, 9, 11–14); preset signal ranges (Table 10, all rows 1..=8 tested against the staged verbatim transcription); `SourceOverrides` custom-flag record |
| `set_coding_parameters` (dimensions, video depth) | §11.6 | ✅ |
| Transform parameters + extended (asymmetric) params | §12.4 | ✅ filter / depth / slice params / quant matrix |
| Quantization (factor, offset, inverse-quant, DC prediction) | §13.3 / §13.4 | ✅ |
| Default quant matrices | Annex D | ✅ Tables D.1–D.8 complete: all asymmetric (`dwt_depth_ho > 0`) blocks + the mixed Haar/LeGall pair; out-of-table combos require the custom matrix per §12.4.5.3 |
| Slice unpacking — low delay | §13.5.3 | ✅ |
| Slice unpacking — high quality | §13.5.4 | ✅ |
| Picture fragments (setup/data, reassembly, constraints) | §14 | ✅ raster-order slice reassembly, deferred DC prediction, §14.1/§14.2 violations rejected; chunk-fed via `SequenceDecoder::push` |
| IDWT lifting filters (all 7 wavelets) | §15.4.4 / Tables 16–22 | ✅ reversibility-tested (LeGall) |
| Component IDWT + pad removal + clip + offset | §15 | ✅ |
| `oxideav-core` `Decoder` (registry + direct factory) | — | ✅ `register(ctx)` + `make_decoder`; `Vc2Decoder::sequence_header()` surfaces the live §11 parameter map; 8/10/12/16-bit planar YUV output (Table 10 presets 7/8 and >12-bit custom ranges ride the full-width 16-bit formats); mixed / off-format ≤12-bit custom ranges decode LSB-anchored on the deepest component's surface with a per-plane significant-bits side-channel; fragments may span packets |
| Hostile-input hardening | — | ✅ truncation → `UnexpectedEof` at every cut point (8-bit, 16-bit and mixed-depth streams); saturating VLC/quant math; documented caps on depth / area / slice counts / signal offsets+excursions; bit-flip + garbage fuzz-lite in CI, including a wrapper-level sweep asserting every emitted frame is well-formed (3 image planes, in-contract significant-bits record) |
| MXF mapping data | ST 2042-4 | ✅ `mxf` module: essence container / compression labels, picture-element key, sub-descriptor key + all 7 item ULs; Annex B CDCI descriptor mappings (frame layout, stored dims, ref levels, subsampling, colour label ULs); wrapped-stream sub-descriptor scanner (distinct wavelet filters, header identity, constant version/profile/level) |
| Profile / level conformance checks | Annex C + ST 2042-2 | ✅ opt-in `conformance` module: profile values + per-profile parse-code tables (C.1/C.2); generalized levels 0..=7 — base-format coverage, §5.3 custom-flag rules incl. the format-7 dimension / progressive-relabel / Level-4 48 fps carve-outs, §5.4 picture bounds (wavelet ≤ 4, depth ≤ 4, no asym flags at v3, equal DC per slice, quant values ≤ 127), §5.5 no mixed picture+fragment units; whole-stream walker |
| Conformance fixtures | — | ✅ 8-case pinned matrix (`tests/data/`, staged under `docs/video/vc2/fixtures/`): 10/12-bit cases bit-exact vs an independent black-box validator across all samplings + 3 wavelets; 16-bit presets 7/8 and the mixed 12/10 custom-range case pinned as self-consistent references (probe-verified: the validator's envelope excludes signal-range presets 5..=8 **and** every custom index-0 range) |

### Conformance posture (RP 2042-3)

The staged SMPTE RP 2042-3:2022 defines conformance *procedurally* — it
adds no bitstream provisions beyond ST 2042-1 (its §5.1 says so
explicitly): a bitstream is conformant when a validator finds no
errors, and a decoder is conformant for given bitstreams when its
decoded pictures match the reference decoding, repeated across picture
formats and coding options within the decoder's stated operational
parameters. The crate's pinned fixture matrix implements exactly that
decoder procedure, using an independent black-box validator binary as
the reference decoder wherever its envelope allows (10/12-bit
video-range presets across all samplings and three wavelets) and
spec-grounded self-consistent pins where it refuses input the standard
permits (presets 5..=8, custom index-0 ranges).

### Mixed and off-format bit depths

Custom §11.4.9 signal ranges can derive per-component depths that no
single planar pixel format names — mixed pairs (12-bit luma with 10-bit
chroma) or uniform odd depths (9, 11, …). At or below 12 bits these are
**represented, not promoted or refused**: the picture decodes onto the
natural storage surface for its deepest component (byte planes up to
8 bits, `P10Le` words up to 10, `P12Le` words up to 12), every plane
keeps its §15.5 code values verbatim in the low bits of the storage
word, and the emitted `VideoFrame` carries the `oxideav-core`
per-plane **significant-bits side-channel** (`significant_bits()` /
`plane_significant_bits(k)`) recording each plane's true depth —
full-scale for a plane with `b` significant bits is `(1 << b) - 1`.
Uniform 8/10/12-bit pictures attach no record and stay byte-identical,
as does the >12-bit promotion path (there the `16 - depth` left shift
already places full-scale at the top of the 16-bit word, so an
LSB-anchored depth record would misdescribe the values).

### Container tags

Per the workspace convention the codec crate declares the tags a
container's `CodecResolver` may route to it; `register(ctx)` claims the
four identifiers staged references ground:

- **FourCC `BBCD`** — the parse-info prefix bytes `0x42 0x42 0x43 0x44`,
  the character string "BBCD" as expressed by ISO/IEC 646 (§10.5.1,
  NOTE 1), which every VC-2 data unit begins with.
- **Matroska CodecID `V_DIRAC`** — the Matroska codec registry routes
  VC-2 under this ID (there is no `V_VC2`); each Matroska frame holds a
  whole VC-2 *sequence* and CodecPrivate is empty.
- **FourCC `drac`** and **MP4 ObjectTypeIndication `0xA4`** — the
  MP4 registration-authority codes for this stream family; the registry
  has no VC-2-specific sample entry, so VC-2-in-MP4 rides them.

All four share one confidence probe that walks a peeked packet's
parse-info headers: a packet whose every data unit carries an
ST 2042-1:2022 Table 4 parse code confirms (1.0), broken framing vetoes
(0.0), and intact `BBCD` framing around parse codes this version does
not define — the Dirac-era long-GOP core syntax that legitimately shares
these container tags — resolves weakly (0.25) so a claimant that decodes
those units can win the tag. An out-of-band sequence header staged as
the container's stream-format blob is graded the same way; a bare tag
match resolves with weak confidence.

ST 2042-1:2022 registers no container-scoped identifiers of its own
(Annex C defers even level values to companion SMPTE documents); the
Matroska and MP4 rows come from the container-registry references staged
in `docs/video/vc2/vc2-signal-range-presets-and-container-registry.md`.
The MXF-side identifiers of ST 2042-4 are 16-byte SMPTE ULs rather than
tags — they live in the `mxf` module together with the Annex B CDCI
descriptor mappings.

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
codec registry — along with the `BBCD` FourCC tag claim described above,
so `CodecResolver::resolve_tag` routes matching container streams here —
and `oxideav_vc2::make_decoder(&params)` is the direct factory. Packets
must carry whole VC-2 data units (the parse-info headers are the codec's
own framing); fragmented pictures may span packets.

The crate builds with `--no-default-features` for a dependency-free standalone
decoder; the default `registry` feature pulls in `oxideav-core` for fleet
registration.

## License

MIT © Karpelès Lab Inc. The VC-2 standard text is © SMPTE; this crate
implements the royalty-free codec without redistributing the standard.
