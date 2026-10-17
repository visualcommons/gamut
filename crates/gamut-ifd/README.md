# gamut-ifd

`gamut-ifd` is a pure-Rust implementation of the **TIFF Image File Directory (IFD) container core**:
the byte-order header, the field-type / value model, the IFD chain, and the offset-driven read/write
spine. It models *structure only* — no pixels, compression, or photometry.

## Goals

Part of the [gamut](../../README.md) workspace, this crate exists because the TIFF/IFD structure is
shared by two otherwise-separate efforts:

- **EXIF** ([`gamut-exif`](../gamut-exif), issue #34) — an EXIF blob is an `Exif\0\0` marker followed
  by a TIFF stream; its 0th/1st IFDs and Exif/GPS/Interop sub-IFDs are exactly IFD chains.
- **TIFF** ([`gamut-tiff`](../gamut-tiff), issue #107) — the TIFF image codec, whose container *is*
  its IFD structure.

Factoring the IFD core out keeps the two from duplicating the fiddly, security-sensitive offset
machinery. It is:

- **Memory-safe on hostile input.** `#![forbid(unsafe_code)]` — IFDs are offset-driven, a classic
  parser-exploit surface (offset loops, truncation, overlapping extents).
- **Endianness-honest.** TIFF carries its own byte order (II/MM); the [`ByteOrder`] is threaded
  through every access rather than fixed at compile time.
- **Dependency-light.** Builds only on [`gamut-core`](../gamut-core).

The public types deliberately mirror `gamut-tiff`'s structural types: the codec was migrated onto
this crate as a near-zero-diff refactor (issue #107), and now consumes it instead of an inlined copy.

## Usage

`read` / `read_header` parse a stream into a [`TiffFile`] (`ByteOrder` + `Variant` + a `Vec<Ifd>`);
`write` serialises one back, handling the two-pass offset layout (it is fallible: a stream that
does not fit classic TIFF's 2-byte entry counts or 4 GiB offsets is a typed error, never a silent
truncation). Each `Ifd` is a tag-sorted set of `Field`s, each holding a typed [`Value`];
`FieldType` carries the on-disk type codes.

An `Ifd` may also carry **sub-IFD trees** — child directories under a pointer tag (`SubIFDs`,
`ExifIFD`, …) attached with `set_sub_ifd`. `write` lays the whole tree out and synthesises the
pointer fields; `read_tree` is its inverse, following the pointer tags you name (with depth and
cycle guards) and rebuilding the same tree. `read_ifd_at` remains the per-pointer escape hatch for
lenient decoders.

```rust
use gamut_ifd::{ByteOrder, Ifd, TiffFile, Value, Variant, read_tree, write};

let mut exif = Ifd::new();
exif.set(33434, Value::Rational(vec![(1, 250)])); // ExposureTime
let mut ifd = Ifd::new();
ifd.set(256, Value::Short(vec![640])); // ImageWidth
ifd.set(257, Value::Short(vec![480])); // ImageLength
ifd.set_sub_ifd(34665, vec![exif]); // ExifIFD pointer
let file = TiffFile { order: ByteOrder::LittleEndian, variant: Variant::Classic, ifds: vec![ifd] };
let bytes = write(&file).unwrap();
assert_eq!(read_tree(&bytes, &[34665]).unwrap(), file); // write's inverse
```

Tag *numbers* are passed literally — tag *semantics* live in the consuming codec (e.g. `gamut-tiff`'s
`tags` module), not in this structural core. That is also why `read_tree` takes the pointer tags:
which `LONG` tags hold directory offsets is semantics the structure alone cannot know. The one
principled exception is the [`tags`] module: the handful of **structural pointer tags**
(`SubIFDs`, `ExifIFD`, `GPSInfo`, `InteroperabilityIFD`, plus the `MakerNote` blob carrier) name
the directory graph itself, so they live here (`tags::STANDARD_POINTER_TAGS` is the ready-made
`read_tree` set) instead of being re-declared by every consumer.

The same module holds the shared **tag registry** (issue #532): each tag a vendored specification
defines, as a `TagSpec` row of number, name, permitted field types and component count, grouped
into one `TagBlock` per specification — `tags::TIFF6` (TIFF 6.0, `references/tiff/tiff6.pdf`, 74
tags) and `tags::DNG` (DNG 1.7.1.0, `references/dng/DNG_Spec_1_7_1_0.pdf`, 118 tags). It records
identity and value shape, not meaning, and is read by `gamut-tiff` and `gamut-dng` (to recognise
standard tags) and `gamut-exif` (to name 0th-IFD tags CIPA DC-008 does not define). Tags from
specifications not vendored here — TIFF/EP, the Adobe technical notes, and the vendor lineages
exiv2 folds into its standard tables — are deliberately absent.

```rust
use gamut_ifd::tags::{DNG, TIFF6, TagCount};

let page = TIFF6.find(297).unwrap();
assert_eq!(page.name, "PageNumber");
assert_eq!(page.count, TagCount::Exact(2));
assert_eq!(DNG.find(50706).map(|s| s.name), Some("DNGVersion"));
```

### Streaming (RAW-grade) parsing

A multi-hundred-MB camera file should not need to be in memory to have its kilobytes of
directory structure read (issue #252). The `ReadAt` trait abstracts positioned reads —
implemented by `&[u8]`, by `StreamSource` (any `Read + Seek`, e.g. a `File`), and by `Rebased`
(an offset-shifted view: the primitive for maker-note mini-IFDs, whose internal offsets are
relative to the note start or the TIFF header, and for TIFF streams embedded inside another
container). `IfdReader` walks structure lazily on top: `read_ifd` fetches one directory body
and leaves each entry **raw** (tag, type code, count, and the value/offset word verbatim);
`value` fetches and decodes a single value on demand; `ifds()` iterates the top-level chain;
`read_file` / `read_tree` / the audited methods are the eager slice APIs' equivalents and
produce identical results — in fact the slice functions *are* thin wrappers over this engine
(one parser; the robustness corpus drives both entry points as a regression gate).

```rust
use gamut_ifd::{IfdReader, StreamSource, tags};

# fn demo() -> Result<(), Box<dyn std::error::Error>> {
let file = std::fs::File::open("shot.nef")?; // 300 MB of RAW; only KBs get read
let mut reader = IfdReader::open(StreamSource::new(file))?;
let raw = reader.read_ifd(reader.first_ifd_offset())?; // one directory body
if let Some(entry) = raw.entry(tags::SUB_IFDS) {
    let raw_ifd_offsets = reader.value(entry)?; // this value's bytes only
#   let _ = raw_ifd_offsets;
}
let whole_tree = reader.read_tree(tags::STANDARD_POINTER_TAGS)?; // == read_tree on the full bytes
# let _ = whole_tree;
# Ok(())
# }
```

Codecs that append image data after the stream (strips, tiles, an embedded JPEG) build on `write`'s
documented **layout contract**: every structure sits on an even word boundary (`align_word` is the
rule) and the layout is a pure function of structure sizes, so writing with correctly-sized
placeholders, measuring, patching (`Value::offset_array` builds the variant-width offset arrays),
and re-writing is byte-stable; `write_with` additionally returns a `SegmentMap` declaring every
emitted byte (padding included) and can **pin** a value at an exact absolute offset (the
maker-note preservation primitive).

### Byte-completeness auditing (issue #263)

For strict archival decoding, `audit` (and the composable `Auditor`) classifies **every byte**
of a stream into typed segments — header, directory bodies, values, declared data extents
(strips/tiles/free/embedded JPEG), and padding — under a **dual-ledger** cross-check: a
`Tracked` source records every byte the parse physically reads, and `SegmentMap::finish`
verifies reads ⊆ claims and claims ⊆ reads, so the report is a machine-checked proof of what
the parse touched, not a promise. `SegmentReport::is_fully_classified` is the zero-tolerance
verdict; unknown-type entries are preserved verbatim as `Value::Unknown` (their unsizable
out-of-line payloads are the one documented gap an audit can still surface).

Beyond the twelve TIFF 6.0 field types, `FieldType::Utf8` (`Value::Utf8`, on-disk code `129`) carries
the Exif 3.0 UTF-8 string type (CIPA DC-008 §4.6.2) — like `Ascii` but NUL-terminated UTF-8, so
internationalised EXIF text round-trips. It is always available (not gated behind `bigtiff`).

### BigTIFF

The `bigtiff` cargo feature adds BigTIFF (`references/tiff/bigtiff.html`): the `Variant::Big`
container with 64-bit offsets/counts and the `Long8` / `SLong8` / `Ifd8` field types. It is additive
and off by default — classic-only consumers (EXIF) stay lean; `gamut-tiff` enables it.

## Status

**v1 — surface frozen** (issue #181): the structural core (issue #107), sub-IFD tree read/write,
byte-range coverage accounting, and a malformed-input robustness corpus. The shared read/write
paths are differentially gated through the consuming codecs' oracles — `gamut-tiff` vs **libtiff**
(byte-level container round-trips) and `gamut-exif` vs **exiv2** (bare TIFF streams through the
same IFD machinery). See [STATUS.md](STATUS.md).

## License

Licensed under either of MIT or Apache-2.0 at your option.
