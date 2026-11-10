//! Which metadata carrier each gamut format crate can locate or write, as a queryable table.
//!
//! The metadata facade ([`gamut::metadata`](crate::metadata)) is container-agnostic and names no
//! format crate, so the list of format crates — and what each does with each carrier — lives
//! here, in the umbrella that already holds that list. It answers, statically and whichever format
//! features this build enabled, the question a caller asks before reaching for a format: "can
//! gamut read (or write) EXIF in a WebP?". The real matrix is not uniform — HEIC is decode-only,
//! JPEG and JPEG XL locate no C2PA store, IPTC-IIM rides only in the TIFF family — so the model is
//! per **(format × carrier × direction)** rather than a flat per-format flag, over the facade's own
//! [`Carrier`] vocabulary.
//!
//! Two questions, two functions:
//!
//! - [`supports`] — can the format crate **locate** ([`Direction::Read`]) or **write**
//!   ([`Direction::Write`]) the carrier as a raw payload? This is the crate's own surface
//!   (`metadata()` / `with_exif`-style setters).
//! - [`typed_wiring`] — does the format crate also expose the facade's typed models directly
//!   (a [`TypedMetadata`](crate::metadata::TypedMetadata) impl and a `with_metadata` encoder
//!   builder), behind that crate's optional `metadata` Cargo feature where it has one?
//!
//! Both answer for the surface a crate **defines**, not for what a particular build compiled: a
//! `const fn` sees neither the Cargo features another crate was built with nor the target. Each
//! function's own docs name the cases where that gap is real.
//!
//! The table is a transcription of each crate's `STATUS.md`; every arm below cites the row that
//! justifies it, and the cell changes in the pull request that changes the row. It is deliberately
//! a `const` table rather than a runtime registry: the set of formats is the workspace's, fixed at
//! build time, and a query must be answerable whichever format features are on.
//!
//! The audio/video half of the same question — which *media* containers carry which metadata — is
//! outside an image-first workspace and stays with the issue that asked for it.
//!
//! ```
//! use gamut::capability::{Direction, Format, supports, typed_wiring};
//! use gamut::metadata::Carrier;
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

use gamut_metadata::Carrier;

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
///
/// A carrier the facade appends to its `#[non_exhaustive]` [`Carrier`] answers `false` until a
/// cell here names it.
#[must_use]
pub const fn supports(format: Format, carrier: Carrier, direction: Direction) -> bool {
    let read = matches!(direction, Direction::Read);
    match (format, carrier) {
        // gamut-jpeg STATUS.md P7: APP1 EXIF + XMP and multi-segment APP2 ICC, read (`metadata()`)
        // and write (`with_exif` / `with_xmp` / `with_icc_profile`).
        // gamut-png STATUS.md P8 (eXIf / iCCP / iTXt-XMP setters) and D5 (raw eXIf / iCCP / XMP
        // payloads on decode).
        // gamut-webp STATUS.md M4: `ICCP` / `EXIF` / `XMP ` chunks embedded on encode and preserved
        // on decode (`metadata` / `with_icc_profile` / `with_exif` / `with_xmp`).
        // gamut-avif STATUS.md M4: Exif / XMP items with a `cdsc` reference and `colr` `prof` ICC,
        // written by `AvifEncoder::with_exif` / `with_xmp` / `with_icc_profile` and read back.
        // gamut-jxl STATUS.md "Exif / XMP container boxes" (written by `with_exif` / `with_xmp`,
        // read back by `JxlDecoder::metadata`) and "Colour signalling" (`with_icc_profile` written,
        // `embedded_icc_profile` read).
        (
            Format::Jpeg | Format::Png | Format::WebP | Format::Avif | Format::Jxl,
            Carrier::Exif | Carrier::Xmp | Carrier::Icc,
        ) => true,

        // C2PA manifest stores located on read and written only as a signing workflow (see
        // `Carrier::C2pa`): gamut-png STATUS.md C1 (`caBX`, `PngMetadata::c2pa`); gamut-webp
        // STATUS.md M4 "C2PA manifest store" (the `C2PA` chunk, `gamut_webp::c2pa_span`); gamut-avif
        // STATUS.md "C2PA carriage" (#444, the top-level `uuid` box, `AvifContainer::c2pa_slot`).
        (Format::Png | Format::WebP | Format::Avif, Carrier::C2pa) => read,

        // gamut-heic STATUS.md B (Exif/XMP lens via `cdsc`, `colr` accessor) and S7 (C2PA manifest
        // store located in a top-level `uuid` box). The crate is decode-only: nothing is written.
        (Format::Heic, Carrier::Exif | Carrier::Xmp | Carrier::Icc | Carrier::C2pa) => read,

        // gamut-tiff STATUS.md "Additions since the freeze (#446)": `TiffDecoder::metadata` locates
        // the Exif (its 0th-IFD fields in IFD 0, `ExifIFD` and `GPSInfo`), XMP (700), IPTC-IIM
        // (33723), ICC (34675) and C2PA (52545) payloads, and `TiffEncoder::with_tiff_metadata`
        // writes the first four (the facade path refuses an Exif model holding a thumbnail, or a
        // 0th-IFD field outside `gamut_tiff::IFD0_EXIF_TAGS`, which a `TiffMetadata` has no
        // carrier for — #876).
        // gamut-dng STATUS.md P16: EXIF sub-IFD + XMP (700) / IPTC-IIM (33723) / ICC (34675),
        // embedded and decoded (`DngMetadata`, `DngEncoder::with_dng_metadata`).
        (
            Format::Tiff | Format::Dng,
            Carrier::Exif | Carrier::Xmp | Carrier::Icc | Carrier::IptcIim,
        ) => true,
        // The TIFF C2PA store (52545) is written only as a signing workflow (see
        // `Carrier::C2pa`); gamut-dng STATUS.md P26 (#442): the store is decoded as
        // `DecodedDng::c2pa`, likewise written only as a signing workflow.
        (Format::Tiff | Format::Dng, Carrier::C2pa) => read,

        // Every other cell is `false`, in one arm because they share the answer:
        // gamut-jpeg STATUS.md "Not implemented" (APP13 IPTC-IIM deferred; no APP11 C2PA
        // carriage); gamut-png (no IPTC-IIM chunk), gamut-webp (RIFF has no IPTC-IIM chunk),
        // gamut-avif and gamut-heic (HEIF has no IPTC-IIM item type) STATUS.md; gamut-jxl STATUS.md
        // (no IPTC-IIM box; the `jumb` C2PA box is neither located nor written). The arm also
        // answers for a carrier the facade appends, which `Carrier`'s `#[non_exhaustive]` makes
        // this match name with a wildcard.
        _ => false,
    }
}

/// Whether the crate for `format` exposes the facade's typed models directly — a
/// [`TypedMetadata`](crate::metadata::TypedMetadata) impl on its decoded metadata and a
/// `with_metadata` builder on its encoder.
///
/// `false` would mean the crate still hands its payloads over as raw bytes that a caller feeds to
/// [`Metadata::parse_blocks`](crate::metadata::Metadata::parse_blocks) by hand. Every format with a gamut
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
/// Every wired crate puts that surface behind an optional `metadata` Cargo feature of its own —
/// `gamut-jpeg`, `gamut-png`, `gamut-webp`, `gamut-avif`, `gamut-heic`, `gamut-jxl`, `gamut-tiff`
/// and `gamut-dng`, each off by default, and each forwarded weakly by this umbrella's `metadata`
/// feature. As with [`supports`], the answer describes what the crate defines, not what a given
/// build compiled.
#[must_use]
pub const fn typed_wiring(format: Format) -> bool {
    match format {
        // gamut-dng STATUS.md P16 (#353): the `metadata` feature (issue #1087), whose
        // `TypedMetadata` impl assigns the Exif sub-IFD onto the facade's `Exif` and hands over
        // the byte carriers. gamut-jpeg / gamut-jxl / gamut-heic: the `metadata` feature (issue
        // #420); gamut-png / gamut-webp / gamut-avif / gamut-tiff: the `metadata` feature (issue
        // #510). Every format is named, so one appended to `Format`
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
        (Format::Tiff, Carrier::Exif, Direction::Write),
        (Format::Tiff, Carrier::Xmp, Direction::Read),
        (Format::Tiff, Carrier::Xmp, Direction::Write),
        (Format::Tiff, Carrier::Icc, Direction::Read),
        (Format::Tiff, Carrier::Icc, Direction::Write),
        (Format::Tiff, Carrier::IptcIim, Direction::Read),
        (Format::Tiff, Carrier::IptcIim, Direction::Write),
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
        assert_eq!(Direction::ALL.map(|d| d as u8), [0, 1]);
    }
}
