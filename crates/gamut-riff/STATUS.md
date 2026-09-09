# gamut-riff — RIFF/WebP container implementation status

**v1 stabilization: GitHub issue #186.** `gamut-riff` factors the RIFF chunk container out as a
shared primitive under [`gamut-webp`](../gamut-webp), mirroring how
[`gamut-isobmff`](../gamut-isobmff) backs AVIF/HEIC. It models *structure only* — the 12-byte file
header, the chunk framing, and the `VP8X` feature header — never a coded bitstream, which is carried
opaquely. Runtime dependency: `gamut-core` only; `#![forbid(unsafe_code)]`.

**Keystone:** the **three readers at increasing strictness over one framing**. `RiffReader` iterates
chunks and refuses only what it cannot frame; `MetadataChunks::read` collects the metadata triple
and nothing else; `WebpLayout::parse` sorts every chunk into its role and enforces the ordering rule
of RFC 9649 §2.7. A caller picks the strictness the job needs, and all three agree byte for byte on
where each chunk begins and ends — the property `tests/fixtures.rs` pins against hand-written bytes
and `tests/oracle.rs` against libwebp's demuxer.

## Scope

The authority is **RFC 9649 §2** (*WebP Image Format*) and the Google *WebP Container* specification
in [`references/webp/`](../../references/webp). One carrier comes from outside RFC 9649: the `C2PA` chunk of C2PA 2.4 §A.3.7, whose manifest
store is carried as opaquely as `ICCP`/`EXIF`/`XMP ` are. The specification is vendored in
[`references/c2pa/`](../../references/c2pa); §A.3.7 fixes the chunk's identifier and its place as
the last sub-chunk of the form, and §18.5 fixes what a `c2pa.hash.data` exclusion covers. Nothing
here parses, signs or validates a store.

The canonical RIFF document is *cited* by RFC 9649
(as a Library of Congress FDD URL), not vendored, so the wider RIFF vocabulary it defines — `LIST`,
arbitrary form types, the AVI/WAVE chunks — is out of scope and unimplemented. What this crate calls
"RIFF" is precisely the subset WebP uses: a flat chunk list under a single `RIFF`/`WEBP` form.

**Asymmetric by design:** the writer *normalises* (canonical chunk order, feature flags derived from
the payloads actually present, every value validated up front), while the readers accept the range
the spec tells them to — a metadata or unknown chunk anywhere, a repeated chunk with the first
winning, a final chunk missing its pad byte, trailing data past the declared file size.

## Oracle

**libwebp's demuxer** (`WebPDemux`, via the `libwebp-sys2` dev-dependency, `demux` + `mux` + `static`
features) is the reference container parser, run differentially in both directions in
`tests/oracle.rs`: gamut-riff parses what libwebp writes, and libwebp parses what gamut-riff writes,
agreeing on the canvas and on each metadata payload byte for byte. Because this crate codes no
bitstream, each test borrows a real `VP8L` codestream from libwebp's own encoder and rewraps it.
Dev-only — the shipped library links no C.

## Public surface (frozen at v1)

| Item | Shape | Openness |
| ---- | ----- | -------- |
| `FourCc` | `pub [u8; 4]` newtype + 11 associated constants, `Display` escaping non-printables | public field by design — a FourCC *is* its four bytes |
| `Chunk<'a>` | `{ fourcc, payload: &'a [u8] }`, `Copy` | plain borrowed data; public fields |
| `RiffReader<'a>` | `new` + `Iterator<Item = Result<Chunk>>` + `trailing_bytes` | permissive; iteration ends after the first error |
| `RiffWriter` | `new`/`write_chunk`/`finish`, all fallible past the size fields | private buffer — internal representation stays free |
| `Vp8xHeader` | 5 feature flags + 1-based canvas, `to_payload`/`from_payload` | public fields; both directions validate the canvas |
| `MetadataChunks<'a>` | borrowed `icc`/`exif`/`xmp`/`c2pa` + `read`/`is_empty` | public fields; payloads never parsed or reserialized |
| `WebpLayout<'a>` | `parse` + the sorted roles, `#[non_exhaustive]` | strict reader; new roles can be added non-breakingly |
| `WebpChunkId` | fieldless variants + `Unknown(FourCc)`, `#[non_exhaustive]` | new chunk kinds can be recognised non-breakingly |
| `write_simple_lossless` / `write_simple_lossy` | one bitstream chunk, §2.5-§2.6 | free functions returning `Result<Vec<u8>>` |
| `write_extended` / `write_extended_with_metadata` / `write_extended_preserving` | the extended format at three levels of assistance | as above |
| `c2pa_span` | the `C2PA` chunk's whole byte span in a file, or `None` | free function returning `Result<Option<Range<usize>>>` |
| `VP8X_PAYLOAD_LEN`, `MAX_CANVAS_DIMENSION`, `C2PA_FOURCC` | documented spec constants | literals the surface's own docs name |

Adding chunk kinds, layout roles, writer helpers, or trait impls stays backward-compatible;
removing or reshaping any of the above would not. `MetadataChunks` is the one exhaustive struct
here, deliberately so — a new carrier should make every writer of a struct literal look at it — and
adding `c2pa` for the manifest store is therefore a **breaking** change, the crate's first since v1.
The migration is one line: `c2pa: None`, or `..Default::default()`.

## Container coverage

The per-requirement conformance ledger is [`gamut-webp/STATUS.md`](../gamut-webp/STATUS.md) §A,
whose declared owner is this crate. Every row there is ✅ or ⊘ as of v1.

| Component | Spec | Status |
| --- | --- | --- |
| Chunk framing: FourCC + `uint32` LE size + payload + pad-to-even | §2.3 | ✅ |
| Pad byte MUST be zero (rejected when present and non-zero) | §2.3 | ✅ |
| File header: `RIFF` magic, size field, `WEBP` form | §2.4 | ✅ |
| File-size ceiling `2^32 - 10` on write; overrun rejected on read | §2.4 | ✅ |
| Trailing data past *File Size* ignored, but surfaced to the caller | §2.4 | ✅ |
| Simple formats: `VP8 ` / `VP8L` | §2.5, §2.6 | ✅ |
| `VP8X` feature flags + 1-based 24-bit canvas | §2.7 | ✅ |
| Canvas bounds: `1..=2^24` per dimension, product ≤ `2^32 - 1` | §2.7 | ✅ |
| Reconstruction-chunk ordering enforced on read | §2.7 | ✅ |
| `ALPH` carried opaquely (its payload is gamut-webp's) | §2.7.1.2 | ✅ |
| `ICCP` colour profile, verbatim | §2.7.1.4 | ✅ |
| `EXIF` / `XMP ` metadata, verbatim, first of each kind wins | §2.7.1.5 | ✅ |
| Unknown chunks: ignored on read, order preserved, re-emittable | §2.7.1.6 | ✅ |
| `C2PA` manifest store, verbatim, written as the last sub-chunk of the form | C2PA §A.3.7 | ✅ |
| `c2pa.hash.data` exclusion span: whole chunk, pad byte excluded | C2PA §18.5 | ✅ |
| `ANIM` / `ANMF` animation | §2.7.1.1 | ⊘ out of scope |

## Settled design decisions (intentional, not gaps)

- **Animation is out of scope, not unimplemented.** Multi-frame sequences fall outside the
  workspace's image-first charter (decision 2026-06-09, `gamut-webp/STATUS.md`). The FourCCs stay
  *recognised* — `WebpChunkId::Anim`/`Anmf` — so `WebpLayout::parse` reports an animated file as
  `Unsupported` instead of mis-parsing it as a still image or calling it corrupt.
- **The form type is `WEBP`, not a parameter.** `RiffReader::new` and `RiffWriter::new` hard-code it.
  The crate is named for the container family but scoped to WebP's use of it; a generic form
  parameter would be additive if another RIFF format ever landed in the workspace.
- **Metadata crosses the boundary verbatim.** `ICCP`/`EXIF`/`XMP ` payloads are borrowed, never
  parsed or reserialized, so they survive a read/write cycle byte for byte — the property the typed
  metadata crates (`gamut-exif`, `gamut-icc`, `gamut-xmp`) need in order to borrow rather than copy.
- **Repeated chunks: first wins, everywhere.** §2.7.1.4-§2.7.1.5 let readers "ignore all except the
  first one"; `WebpLayout` applies the same rule to `ALPH` and the bitstream so one policy covers the
  whole container.
- **`ICCP` is ordered, `EXIF`/`XMP ` are not.** §2.7 lists `ICCP` among the chunks that MUST appear
  in order and §2.7.1.4 adds "MUST appear before the image data", while the same paragraph exempts
  metadata and unknown chunks. The asymmetry is the spec's, not an oversight.
- **The `C2PA` chunk is placed by the writer, not policed by the reader.** §A.3.7 says the chunk
  "shall appear as the last sub-chunk of the first RIFF header chunk", so `write_extended_preserving`
  emits it after `EXIF`, `XMP ` and even the preserved unknown chunks. The readers accept it
  anywhere, because §2.7 does not list `C2PA` among the chunks whose order a reader may fail a file
  over, and a store found in a file gamut did not write is still a store. It is *recognised*
  (`WebpChunkId::C2pa`) rather than left unknown, so a read/modify/write cycle re-emits it once, in
  its mandated place, instead of twice.
- **A `C2PA` chunk never rides along among the unknown chunks.** `write_extended_preserving` writes
  at most one, always in the store's place at the end. With `MetadataChunks::c2pa` set, a copy in
  `unknown` is **dropped**: two would be resolved by "first of each kind wins" in favour of the
  passed-through copy, and `c2pa_span` would then report a range over bytes the caller never
  configured — exactly the range a signer excludes from its hash. With no store configured the copy
  is **kept** and written in the store's place, because in the file it came from it *is* the store;
  dropping it would lose a foreign manifest store with no signal. `write_extended` is the unfiltered
  escape hatch for a caller assembling a file by hand.
- **The layers are deliberately asymmetric.** This function is total — it always produces a
  conformant file — while `gamut-webp`'s `WebpEncoder::with_unknown_chunks` *rejects* the same input
  with a typed error. The high-level builder can name the offending call, which is the better
  diagnostic; the low-level writer has no caller to blame and stays usable for a re-wrap that must
  not fail.
- **No `VP8X` feature flag advertises a store.** RFC 9649 §2.5's flag byte defines no C2PA bit and
  the reserved bits "MUST be 0", so presence is decided by the chunk alone — the same rule the crate
  already applies to `ICCP`/`EXIF`/`XMP `, where flags are advisory.
- **The reported span is the whole chunk, minus the pad byte.** `c2pa_span` covers the identifier
  and the size field as well as the payload, because an update manifest may resize the store and
  that changes the size field's value (§18.5). The RIFF pad byte after an odd-length store stays
  outside it: §2.3 makes the byte framing the container adds, which is why `Chunk::payload` excludes
  it too. (§18.7.3.5's *general box hash* draws the boundary the other way, "to the padding byte, if
  any, inclusive" — that is `c2pa.hash.boxes`, a different assertion this crate does not serve.)
- **A non-zero pad byte fails its chunk.** The byte "MUST be 0 to conform with RIFF" (§2.3) and is
  attacker-controlled otherwise; a chunk whose framing is already known bad is never handed out. A
  pad byte *absent* from a final chunk still parses — there is then nothing to check.
- **The crate stays consumer-only.** Unlike the `gamut-isobmff` v1 precedent it gets no `gamut`
  umbrella feature and no CLI subcommand: it is reached through `gamut-webp`, which is its only
  dependent, and a demonstration surface for a container with no codec of its own would duplicate
  `gamut webp`. Revisit if a second dependent appears.

## Deferred (all additive — none blocks v1)

| Item | Notes | Status |
|------|-------|--------|
| Generic RIFF form types | `RiffReader::with_form(data, FourCc)` and a `RiffWriter::with_form`, for a non-WebP RIFF format. No consumer today; purely additive. | ☐ |
| `LIST` / nested chunks | Not used by WebP's flat layout. The `ANMF` payload is the only nesting the spec defines, and animation is out of scope. | ☐ |
| A cheap `is_webp` sniff | `gamut-cli` hand-rolls the 12-byte signature check (`crates/gamut-cli/src/input.rs`) because no helper is exported. Additive whenever a second caller wants it. | ☐ |
| Fuzz harness | Issue #264 tracks fuzz coverage for the workspace's parser entry points; `RiffReader` belongs on that list. `tests/robustness.rs` covers exhaustive truncation and bit-flips deterministically in the meantime. | ☐ |
| Property-based round-trip tests | Issue #240 tracks adopting proptest workspace-wide; the writer→reader round-trip and the pad-byte invariant are natural candidates. | ☐ |

## Out of scope

`gamut-riff` is deliberately the WebP container and nothing more. The following are **not** provided
here and are not planned:

- **Any bitstream coding.** `VP8 `, `VP8L`, and the `ALPH` payload are opaque byte slices; they
  belong to [`gamut-webp`](../gamut-webp).
- **Metadata semantics.** The container assigns `ICCP`/`EXIF`/`XMP ` payloads no meaning; parsing
  them is `gamut-icc`, `gamut-exif`, and `gamut-xmp`'s job.
- **Animation assembly.** Frame disposal, blending, and canvas composition (§2.7.2) need a
  multi-frame API that the single-image `gamut_core` traits do not have.
- **The non-WebP RIFF universe.** AVI, WAVE, `LIST`, and `INFO` have no in-repo specification and no
  consumer.
