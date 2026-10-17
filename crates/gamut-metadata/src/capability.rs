//! Which metadata carrier each gamut format crate can locate or write, as a queryable table.
//!
//! The facade is container-agnostic, so it cannot *do* anything with a format; what it can do is
//! answer, statically and without pulling any format crate in, the question a caller asks before
//! reaching for one: "can gamut read (or write) EXIF in a WebP?". The real matrix is not uniform —
//! HEIC is decode-only, JPEG and JPEG XL locate no C2PA store, IPTC-IIM rides only in the TIFF
//! family — so the
//! model is per **(format × carrier × direction)** rather than a flat per-format flag.
//!
//! Two questions, two functions:
//!
//! - [`supports`] — can the format crate **locate** ([`Direction::Read`]) or **write**
//!   ([`Direction::Write`]) the carrier as a raw payload? This is the crate's own surface
//!   (`metadata()` / `with_exif`-style setters).
//! - [`typed_wiring`] — does the format crate also expose the facade's typed models directly
//!   (a [`TypedMetadata`](crate::TypedMetadata) impl and a `with_metadata` encoder builder),
//!   behind that crate's optional `metadata` Cargo feature where it has one?
//!
//! Both answer for the surface a crate **defines**, not for what a particular build compiled: a
//! `const fn` sees neither the Cargo features another crate was built with nor the target. Each
//! function's own docs name the cases where that gap is real.
//!
//! The table is a transcription of each crate's `STATUS.md` **as of this facade version**; every
//! arm below cites the row that justifies it, and the cell changes in the pull request that changes
//! the row. It is deliberately a `const` table rather than a runtime registry: the set of formats is
//! the workspace's, fixed at build time, and a query must be answerable from a crate that depends on
//! no format crate at all (the release topology forbids the reverse edge).
//!
//! The audio/video half of the same question — which *media* containers carry which metadata — is
//! outside an image-first workspace and stays with the issue that asked for it.
//!
//! ```
//! use gamut_metadata::capability::{Carrier, Direction, Format, supports, typed_wiring};
//!
//! // HEIC is decode-only: EXIF can be located but never written.
//! assert!(supports(Format::Heic, Carrier::Exif, Direction::Read));
//! assert!(!supports(Format::Heic, Carrier::Exif, Direction::Write));
//!
//! // Only the TIFF family carries the legacy IPTC-IIM block; elsewhere IPTC rides inside XMP.
//! assert!(supports(Format::Dng, Carrier::IptcIim, Direction::Write));
//! assert!(!supports(Format::Jpeg, Carrier::IptcIim, Direction::Write));
//!
//! // A typed `Metadata` accessor exists on every format crate (behind its `metadata` feature
//! // where it has one).
//! assert!(typed_wiring(Format::Png));
//! ```

/// A still-image container format with a gamut crate.
///
/// `#[non_exhaustive]` and `#[repr(u8)]` with **permanent, append-only** discriminants: a later
/// format is added at the end and never renumbers an existing one, so the value is stable across
/// the C ABI. Match with a wildcard arm; iterate with [`Format::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum Format {
    /// JPEG-1 (ISO/IEC 10918-1), `gamut-jpeg`.
    Jpeg = 0,
    /// PNG (W3C, 3rd edition), `gamut-png`.
    Png = 1,
    /// WebP (RIFF), `gamut-webp`.
    WebP = 2,
    /// AVIF (ISOBMFF/HEIF over AV1), `gamut-avif`.
    Avif = 3,
    /// HEIC/HEIF (ISOBMFF over HEVC), `gamut-heic` — decode-only.
    Heic = 4,
    /// JPEG XL (ISO/IEC 18181), `gamut-jxl`.
    Jxl = 5,
    /// TIFF 6.0, `gamut-tiff`.
    Tiff = 6,
    /// DNG 1.7.1 (a TIFF/EP profile), `gamut-dng`.
    Dng = 7,
}

impl Format {
    /// Every format, in discriminant order — the way to enumerate a `#[non_exhaustive]` enum.
    ///
    /// A slice, not a fixed-length array, precisely because the enum is `#[non_exhaustive]`:
    /// appending a format would change an array constant's *type*, breaking every caller who
    /// named it, which is the opposite of what `#[non_exhaustive]` promises.
    pub const ALL: &'static [Self] = &[
        Self::Jpeg,
        Self::Png,
        Self::WebP,
        Self::Avif,
        Self::Heic,
        Self::Jxl,
        Self::Tiff,
        Self::Dng,
    ];

    /// The Cargo package that implements the format.
    #[must_use]
    pub const fn crate_name(self) -> &'static str {
        match self {
            Self::Jpeg => "gamut-jpeg",
            Self::Png => "gamut-png",
            Self::WebP => "gamut-webp",
            Self::Avif => "gamut-avif",
            Self::Heic => "gamut-heic",
            Self::Jxl => "gamut-jxl",
            Self::Tiff => "gamut-tiff",
            Self::Dng => "gamut-dng",
        }
    }
}

/// A metadata carrier: one genuinely distinct serialization a container holds, matching the
/// variants of [`MetadataBlock`](crate::MetadataBlock).
///
/// `#[non_exhaustive]` and `#[repr(u8)]` with permanent, append-only discriminants, like
/// [`Format`]. Iterate with [`Carrier::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum Carrier {
    /// An EXIF blob (a TIFF stream, with or without the `Exif\0\0` marker).
    Exif = 0,
    /// An XMP packet — which is also where IPTC Core/Extension lives.
    Xmp = 1,
    /// An ICC profile.
    Icc = 2,
    /// The legacy binary IPTC-IIM dataset stream.
    IptcIim = 3,
    /// A C2PA manifest store (JUMBF superbox). Read-only by nature everywhere: the facade never
    /// copies a store forward (see [`C2paPolicy`](crate::C2paPolicy)), so no format **writes** one
    /// as a carrier. The format crates that can place a store do so only as a signing workflow —
    /// a `with_c2pa_reserved` slot, or a `with_c2pa` store computed for that exact output — which
    /// is not carrying metadata from one file to another, and [`supports`] does not count it.
    C2pa = 4,
}

impl Carrier {
    /// Every carrier, in discriminant order. A slice for the same reason as [`Format::ALL`].
    pub const ALL: &'static [Self] = &[Self::Exif, Self::Xmp, Self::Icc, Self::IptcIim, Self::C2pa];
}

/// Which way the metadata moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Direction {
    /// Locating the carrier's payload in an existing file (the crate's `metadata()` / lens).
    Read = 0,
    /// Writing the carrier into a file the crate encodes (the crate's `with_*` builders).
    Write = 1,
}

impl Direction {
    /// Both directions. An array, since this enum is exhaustive and cannot gain a variant.
    pub const ALL: [Self; 2] = [Self::Read, Self::Write];
}

/// Whether the crate for `format` can locate (`Read`) or write (`Write`) `carrier` as a raw payload.
///
/// This is the **raw** surface — the crate's own byte-level `metadata()` / `with_*` API — as
/// against the facade's typed models, which are [`typed_wiring`]. Each arm cites the `STATUS.md`
/// row of the crate it describes.
///
/// # What a `const` table cannot see
///
/// It answers for the surface the crate **defines**, not for the surface a given build compiled:
/// a `const fn` observes neither another crate's Cargo features nor the target. Most format crates
/// ship their raw surface unconditionally, but not all — `gamut-jxl` gates its reader
/// (`JxlDecoder::metadata`, `JxlDecoder::embedded_icc_profile`) on its `decode` feature and its
/// encoder on `encode`, so under `default-features = false, features = ["encode"]` no reader
/// exists while this table still answers `true` for [`Direction::Read`]. A caller who switches a
/// format crate's own default features off owns that intersection.
#[must_use]
pub const fn supports(format: Format, carrier: Carrier, direction: Direction) -> bool {
    let read = matches!(direction, Direction::Read);
    match (format, carrier) {
        // gamut-jpeg STATUS.md P7: APP1 EXIF + XMP and multi-segment APP2 ICC, read (`metadata()`)
        // and write (`with_exif` / `with_xmp` / `with_icc_profile`).
        (Format::Jpeg, Carrier::Exif | Carrier::Xmp | Carrier::Icc) => true,
        // gamut-jpeg STATUS.md "Not implemented": APP13 IPTC-IIM deferred; no APP11 (C2PA) carriage.
        (Format::Jpeg, Carrier::IptcIim | Carrier::C2pa) => false,

        // gamut-png STATUS.md P8 (eXIf / iCCP / iTXt-XMP setters) and D5 (raw eXIf / iCCP / XMP
        // payloads on decode).
        (Format::Png, Carrier::Exif | Carrier::Xmp | Carrier::Icc) => true,
        // gamut-png STATUS.md C1: the `caBX` manifest store is located on decode
        // (`PngMetadata::c2pa`); written only as a signing workflow (see `Carrier::C2pa`).
        (Format::Png, Carrier::C2pa) => read,
        // gamut-png STATUS.md: no IPTC-IIM chunk exists in PNG.
        (Format::Png, Carrier::IptcIim) => false,

        // gamut-webp STATUS.md M4: `ICCP` / `EXIF` / `XMP ` chunks embedded on encode and preserved
        // on decode (`metadata` / `with_icc_profile` / `with_exif` / `with_xmp`).
        (Format::WebP, Carrier::Exif | Carrier::Xmp | Carrier::Icc) => true,
        // gamut-webp STATUS.md M4 "C2PA manifest store": the `C2PA` chunk is preserved on decode
        // (`WebpMetadata::c2pa`); written only as a signing workflow (see `Carrier::C2pa`).
        (Format::WebP, Carrier::C2pa) => read,
        // gamut-webp STATUS.md: RIFF has no IPTC-IIM chunk.
        (Format::WebP, Carrier::IptcIim) => false,

        // gamut-avif STATUS.md M4: Exif / XMP items with a `cdsc` reference and `colr` `prof` ICC,
        // written by `AvifEncoder::with_exif` / `with_xmp` / `with_icc_profile` and read back.
        (Format::Avif, Carrier::Exif | Carrier::Xmp | Carrier::Icc) => true,
        // gamut-avif STATUS.md "C2PA carriage" (#444): the top-level `uuid` box is located on read
        // (`AvifContainer::c2pa_slot`); written only as a signing workflow (see `Carrier::C2pa`).
        (Format::Avif, Carrier::C2pa) => read,
        // gamut-avif STATUS.md: no IPTC-IIM item type.
        (Format::Avif, Carrier::IptcIim) => false,

        // gamut-heic STATUS.md B (Exif/XMP lens via `cdsc`, `colr` accessor) and S7 (C2PA manifest
        // store located in a top-level `uuid` box). The crate is decode-only: nothing is written.
        (Format::Heic, Carrier::Exif | Carrier::Xmp | Carrier::Icc | Carrier::C2pa) => read,
        // gamut-heic STATUS.md: HEIF has no IPTC-IIM item type.
        (Format::Heic, Carrier::IptcIim) => false,

        // gamut-jxl STATUS.md "Exif / XMP container boxes" (written by `with_exif` / `with_xmp`,
        // read back by `JxlDecoder::metadata`) and "Colour signalling" (`ColorSpec::Icc` written,
        // `embedded_icc_profile` read).
        (Format::Jxl, Carrier::Exif | Carrier::Xmp | Carrier::Icc) => true,
        // gamut-jxl STATUS.md: no IPTC-IIM box; the `jumb` (C2PA) box is neither located nor
        // written, and a Brotli-compressed `brob` metadata box is a typed `Unsupported`.
        (Format::Jxl, Carrier::IptcIim | Carrier::C2pa) => false,

        // gamut-tiff STATUS.md "Additions since the freeze (#446)": `TiffDecoder::metadata` locates
        // the `ExifIFD`, XMP (700), IPTC-IIM (33723), ICC (34675) and C2PA (52545) payloads, and
        // `TiffEncoder::with_tiff_metadata` writes the first four. The write cells stay `false` until
        // `gamut-cli`'s TIFF output writes what this table grants it: that command reads these
        // cells to decide what to carry, and flipping them first would make it drop the carried
        // blocks without the warning it gives today (#870). The C2PA store is written
        // only as a signing workflow (see `Carrier::C2pa`), so every carrier reads alike.
        (Format::Tiff, _) => read,

        // gamut-dng STATUS.md P16: EXIF sub-IFD + XMP (700) / IPTC-IIM (33723) / ICC (34675),
        // embedded and decoded (`DngMetadata`, `DngMetadata::blocks`).
        (Format::Dng, Carrier::Exif | Carrier::Xmp | Carrier::Icc | Carrier::IptcIim) => true,
        // gamut-dng STATUS.md P26 (#442): the store is decoded as `DngMetadata::c2pa`; written only
        // as a signing workflow (see `Carrier::C2pa`).
        (Format::Dng, Carrier::C2pa) => read,
    }
}

/// Whether the crate for `format` exposes the facade's typed models directly — a
/// [`TypedMetadata`](crate::TypedMetadata) impl on its decoded metadata and a `with_metadata`
/// builder on its encoder.
///
/// `false` would mean the crate still hands its payloads over as raw bytes that a caller feeds to
/// [`Metadata::from_blocks`](crate::Metadata::from_blocks) by hand. Every format with a gamut
/// crate is wired today, so the answer is `true` throughout; a format appended to the
/// `#[non_exhaustive]` [`Format`] starts `false` until its crate is wired.
///
/// Every encoding crate spells the builder `with_metadata`. In `gamut-png` and `gamut-tiff` the
/// builder carrying the crate's own whole metadata type is named apart from it
/// (`with_png_metadata`, `with_tiff_metadata`). `gamut-heic` is decode-only and has no encoder
/// builder at all.
///
/// # How a `true` cell is switched on
///
/// Seven of the eight wired crates put that surface behind an optional `metadata` Cargo feature of
/// their own — `gamut-jpeg`, `gamut-png`, `gamut-webp`, `gamut-avif`, `gamut-heic`, `gamut-jxl`
/// and `gamut-tiff`, each off by default, and each forwarded weakly by the `gamut` umbrella's
/// `metadata` feature. `gamut-dng` has **no such feature**: it depends on this crate
/// unconditionally, because its `DngMetadata` holds the facade's `Exif` by value, so
/// `gamut-dng/metadata` is not a feature that can be asked for. As with [`supports`], the answer
/// describes what the crate defines, not what a given build compiled.
#[must_use]
pub const fn typed_wiring(format: Format) -> bool {
    match format {
        // gamut-dng STATUS.md P16 (#353): `DngMetadata::exif` is the facade's `Exif`, its
        // `TypedMetadata` impl hands over the byte carriers. gamut-jpeg / gamut-jxl / gamut-heic:
        // the `metadata` feature (issue #420); gamut-png / gamut-webp / gamut-avif / gamut-tiff:
        // the `metadata` feature (issue #510). Every format is named, so one appended to `Format`
        // does not compile until its cell is decided here.
        Format::Dng
        | Format::Jpeg
        | Format::Png
        | Format::WebP
        | Format::Avif
        | Format::Heic
        | Format::Jxl
        | Format::Tiff => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The complete set of `true` cells, transcribed from the per-crate `STATUS.md` rows the table
    /// cites. Every other (format, carrier, direction) is `false`.
    const SUPPORTED: &[(Format, Carrier, Direction)] = &[
        (Format::Jpeg, Carrier::Exif, Direction::Read),
        (Format::Jpeg, Carrier::Exif, Direction::Write),
        (Format::Jpeg, Carrier::Xmp, Direction::Read),
        (Format::Jpeg, Carrier::Xmp, Direction::Write),
        (Format::Jpeg, Carrier::Icc, Direction::Read),
        (Format::Jpeg, Carrier::Icc, Direction::Write),
        (Format::Png, Carrier::Exif, Direction::Read),
        (Format::Png, Carrier::Exif, Direction::Write),
        (Format::Png, Carrier::Xmp, Direction::Read),
        (Format::Png, Carrier::Xmp, Direction::Write),
        (Format::Png, Carrier::Icc, Direction::Read),
        (Format::Png, Carrier::Icc, Direction::Write),
        (Format::Png, Carrier::C2pa, Direction::Read),
        (Format::WebP, Carrier::Exif, Direction::Read),
        (Format::WebP, Carrier::Exif, Direction::Write),
        (Format::WebP, Carrier::Xmp, Direction::Read),
        (Format::WebP, Carrier::Xmp, Direction::Write),
        (Format::WebP, Carrier::Icc, Direction::Read),
        (Format::WebP, Carrier::Icc, Direction::Write),
        (Format::WebP, Carrier::C2pa, Direction::Read),
        (Format::Avif, Carrier::Exif, Direction::Read),
        (Format::Avif, Carrier::Exif, Direction::Write),
        (Format::Avif, Carrier::Xmp, Direction::Read),
        (Format::Avif, Carrier::Xmp, Direction::Write),
        (Format::Avif, Carrier::Icc, Direction::Read),
        (Format::Avif, Carrier::Icc, Direction::Write),
        (Format::Avif, Carrier::C2pa, Direction::Read),
        (Format::Heic, Carrier::Exif, Direction::Read),
        (Format::Heic, Carrier::Xmp, Direction::Read),
        (Format::Heic, Carrier::Icc, Direction::Read),
        (Format::Heic, Carrier::C2pa, Direction::Read),
        (Format::Jxl, Carrier::Exif, Direction::Read),
        (Format::Jxl, Carrier::Exif, Direction::Write),
        (Format::Jxl, Carrier::Xmp, Direction::Read),
        (Format::Jxl, Carrier::Xmp, Direction::Write),
        (Format::Jxl, Carrier::Icc, Direction::Read),
        (Format::Jxl, Carrier::Icc, Direction::Write),
        (Format::Tiff, Carrier::Exif, Direction::Read),
        (Format::Tiff, Carrier::Xmp, Direction::Read),
        (Format::Tiff, Carrier::Icc, Direction::Read),
        (Format::Tiff, Carrier::IptcIim, Direction::Read),
        (Format::Tiff, Carrier::C2pa, Direction::Read),
        (Format::Dng, Carrier::Exif, Direction::Read),
        (Format::Dng, Carrier::Exif, Direction::Write),
        (Format::Dng, Carrier::Xmp, Direction::Read),
        (Format::Dng, Carrier::Xmp, Direction::Write),
        (Format::Dng, Carrier::Icc, Direction::Read),
        (Format::Dng, Carrier::Icc, Direction::Write),
        (Format::Dng, Carrier::IptcIim, Direction::Read),
        (Format::Dng, Carrier::IptcIim, Direction::Write),
        (Format::Dng, Carrier::C2pa, Direction::Read),
    ];

    #[test]
    fn supports_equals_the_documented_matrix_in_every_cell() {
        // Walks the full product so a flipped arm anywhere in `supports` is a named cell here.
        for &format in Format::ALL {
            for &carrier in Carrier::ALL {
                for direction in Direction::ALL {
                    let expected = SUPPORTED.contains(&(format, carrier, direction));
                    assert_eq!(
                        supports(format, carrier, direction),
                        expected,
                        "{format:?} / {carrier:?} / {direction:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn typed_wiring_names_every_format() {
        let wired: Vec<Format> = Format::ALL
            .iter()
            .copied()
            .filter(|&f| typed_wiring(f))
            .collect();
        assert_eq!(wired, Format::ALL);
    }

    #[test]
    fn crate_name_follows_the_workspace_naming() {
        for &format in Format::ALL {
            let name = format.crate_name();
            assert!(name.starts_with("gamut-"), "{format:?}: {name}");
            assert_eq!(
                name.trim_start_matches("gamut-"),
                format!("{format:?}").to_ascii_lowercase(),
                "{format:?}: {name}"
            );
        }
    }

    #[test]
    fn discriminants_are_the_documented_append_only_values() {
        // The `repr(u8)` values are a public contract (C ABI); pin them so a reorder is a failure.
        // Collected rather than `map`ped over an array: `ALL` is a slice, so its length is part of
        // what these compare, and an appended variant without its discriminant fails here.
        assert_eq!(
            Format::ALL.iter().map(|&f| f as u8).collect::<Vec<_>>(),
            (0..8).collect::<Vec<u8>>()
        );
        assert_eq!(
            Carrier::ALL.iter().map(|&c| c as u8).collect::<Vec<_>>(),
            (0..5).collect::<Vec<u8>>()
        );
        assert_eq!(Direction::ALL.map(|d| d as u8), [0, 1]);
    }
}
