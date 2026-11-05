# gamut-heif

`gamut-heif` is the HEIF/MIAF still-image layer that `gamut-avif` and `gamut-heic` share. AVIF and
HEIC are one container — ISO/IEC 23008-12 HEIF, profiled by MIAF (ISO/IEC 23000-22) — around
different coded pictures, and this crate is everything outside the coded picture, written once:

- **the item model** — `HeifImage` / `HeifItem` read roles (primary item, alpha and depth
  auxiliaries, thumbnails, Exif/XMP items, derivation sources) and properties (`ispe`, `colr`,
  `pixi`, `clli`, `mdcv`, `clap`, and `irot`/`imir` composed into a `gamut_core::Orientation`) off a
  parsed `gamut_isobmff::IsoBmffImage`;
- **the derivation walk** — `iden` and `grid` resolved down to coded items under a cycle and depth
  guard and the `DecodeLimits` size and work caps;
- **presentation** — `iovl` compositing, YCbCr → RGB at 8 or 16 bits per sample with the chosen
  `ChromaUpsampling`, alpha, and the transformative properties, under `DecodeOptions`;
- **C2PA** — `c2pa_manifest_stores` and `C2paSummary::from_segments` locate and report manifest
  stores among a file's top-level boxes. A locator, never a validator.

It reads no coded picture. A codec crate implements the `Codestream` binding — its item types,
its configuration record, the colour its codestream signals, and how to call one of its backends
— and every decode surface is generic over it. The backends sit in a
`gamut_codec_abi::Registry` of the codec's own typed backend trait, so the registry's contract
(push order; `supports` the only fall-through; an accepting backend's error returned unchanged) is
the workspace's one contract.

Errors raised on a binding's behalf carry its `Codestream::ORIGIN`, so a caller sees the format it
decoded rather than this layer.

See [`STATUS.md`](STATUS.md) for what is implemented and what is not.
