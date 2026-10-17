//! TIFF tag numbers — the 2-byte `Tag` field of an IFD entry.
//!
//! Values are from the TIFF 6.0 specification, §8 (Baseline Field Reference Guide) and the
//! Part 2 extension sections. Only the tags the encoder/decoder act on are named here; unknown
//! tags are still parsed structurally by the reader. The full TIFF 6.0 set — every tag's name,
//! field types and count — is the shared [`gamut_ifd::tags::TIFF6`] registry, which
//! [`is_known_tag`] reads.

/// `NewSubfileType` (254) — a bit field describing the kind of data in this subfile.
pub const NEW_SUBFILE_TYPE: u16 = 254;
/// `ImageWidth` (256) — the number of columns, i.e. pixels per row.
pub const IMAGE_WIDTH: u16 = 256;
/// `PageNumber` (297) — the page index and total page count of a multi-page document.
pub const PAGE_NUMBER: u16 = 297;
/// `ImageLength` (257) — the number of rows (scanlines).
pub const IMAGE_LENGTH: u16 = 257;
/// `BitsPerSample` (258) — bits per component, one value per sample.
pub const BITS_PER_SAMPLE: u16 = 258;
/// `Compression` (259) — the compression scheme applied to the image data.
pub const COMPRESSION: u16 = 259;
/// `FillOrder` (266) — the logical bit order within a byte (1 = MSB-first, the default).
pub const FILL_ORDER: u16 = 266;
/// `PhotometricInterpretation` (262) — the colour space of the image data.
pub const PHOTOMETRIC_INTERPRETATION: u16 = 262;
/// `StripOffsets` (273) — the byte offset of each strip.
pub const STRIP_OFFSETS: u16 = 273;
/// `SamplesPerPixel` (277) — the number of components per pixel.
pub const SAMPLES_PER_PIXEL: u16 = 277;
/// `RowsPerStrip` (278) — the number of rows in each strip.
pub const ROWS_PER_STRIP: u16 = 278;
/// `StripByteCounts` (279) — the number of (compressed) bytes in each strip.
pub const STRIP_BYTE_COUNTS: u16 = 279;
/// `XResolution` (282) — pixels per resolution unit in the horizontal direction.
pub const X_RESOLUTION: u16 = 282;
/// `YResolution` (283) — pixels per resolution unit in the vertical direction.
pub const Y_RESOLUTION: u16 = 283;
/// `PlanarConfiguration` (284) — chunky (1) or planar (2) component storage.
pub const PLANAR_CONFIGURATION: u16 = 284;
/// `ResolutionUnit` (296) — the unit for `XResolution`/`YResolution`.
pub const RESOLUTION_UNIT: u16 = 296;
/// `Predictor` (317) — the prediction scheme applied before compression.
pub const PREDICTOR: u16 = 317;
/// `ExtraSamples` (338) — the meaning of each extra component (e.g. alpha) beyond the photometric.
pub const EXTRA_SAMPLES: u16 = 338;
/// `SampleFormat` (339) — how each sample's bits are interpreted (unsigned/signed integer, float).
pub const SAMPLE_FORMAT: u16 = 339;
/// `ColorMap` (320) — the palette for palette-colour images.
pub const COLOR_MAP: u16 = 320;
/// `TileWidth` (322) — the width of each tile in pixels (a multiple of 16).
pub const TILE_WIDTH: u16 = 322;
/// `TileLength` (323) — the height of each tile in pixels (a multiple of 16).
pub const TILE_LENGTH: u16 = 323;
/// `TileOffsets` (324) — the byte offset of each tile.
pub const TILE_OFFSETS: u16 = 324;
/// `TileByteCounts` (325) — the number of (compressed) bytes in each tile.
pub const TILE_BYTE_COUNTS: u16 = 325;
/// `SubIFDs` (330) — offsets of child image IFDs (reduced-resolution / page / mask subfiles).
pub const SUB_IFDS: u16 = gamut_ifd::tags::SUB_IFDS;
/// `XMP` (700) — an XMP metadata packet, stored as a byte array.
pub const XMP: u16 = 700;
/// `Copyright` (33432) — the copyright notice.
pub const COPYRIGHT: u16 = 33432;
/// `ImageDescription` (270) — a title or description of the image.
pub const IMAGE_DESCRIPTION: u16 = 270;
/// `Make` (271) — the manufacturer of the input equipment.
pub const MAKE: u16 = 271;
/// `Model` (272) — the model name of the input equipment.
pub const MODEL: u16 = 272;
/// `Orientation` (274) — the orientation of the stored image relative to its display (1–8).
pub const ORIENTATION: u16 = 274;
/// `Software` (305) — the software that created the image.
pub const SOFTWARE: u16 = 305;
/// `DateTime` (306) — the date and time the image was created or last changed.
pub const DATE_TIME: u16 = 306;
/// `Artist` (315) — the person who created the image.
pub const ARTIST: u16 = 315;
/// `WhitePoint` (318) — the chromaticity of the image's white point.
pub const WHITE_POINT: u16 = 318;
/// `PrimaryChromaticities` (319) — the chromaticities of the image's primaries.
pub const PRIMARY_CHROMATICITIES: u16 = 319;
/// `IPTC` (33723) — an IPTC/NAA metadata block.
pub const IPTC_NAA: u16 = 33723;
/// `ExifIFD` (34665) — the offset of the Exif private sub-IFD.
pub const EXIF_IFD: u16 = gamut_ifd::tags::EXIF_IFD;
/// `ICCProfile` (34675) — an embedded ICC colour profile.
pub const ICC_PROFILE: u16 = 34675;
/// `GPSInfo` (34853) — the offset of the GPS private sub-IFD.
pub const GPS_INFO: u16 = gamut_ifd::tags::GPS_INFO;
/// `InteroperabilityIFD` (40965) — the offset of the Exif Interoperability sub-IFD.
pub const INTEROPERABILITY_IFD: u16 = gamut_ifd::tags::INTEROPERABILITY_IFD;
/// `C2PA` (52545, `0xCD41`) — the C2PA manifest store, type `UNDEFINED` (C2PA 2.4 §A.3.6).
///
/// A private tag in TIFF 6.0 §7 terms, but one this crate now reads and writes, so
/// [`is_known_tag`] recognises it. Its placement and exclusion rules are [`gamut_ifd::c2pa`]'s;
/// the payload is carried verbatim ([`TiffMetadata::c2pa`](crate::TiffMetadata::c2pa)).
pub const C2PA_MANIFEST_STORE: u16 = gamut_ifd::c2pa::C2PA_MANIFEST_STORE;

/// Whether `tag` is one this crate recognises — every tag TIFF 6.0 defines, read from the shared
/// [`gamut_ifd::tags::TIFF6`] registry, plus the pointer and metadata tags this crate reads and
/// writes and the few later TIFF extension tags listed below — as opposed to a private or unknown
/// tag a strict deconstruct should flag.
///
/// Recognition is *structural*: a recognised tag is a standard one, not necessarily one the
/// encoder/decoder act on (e.g. `Orientation`, `Software`, `DateTime`). Tags inside an Exif/GPS
/// sub-IFD belong to a different namespace and are not judged here. A number TIFF 6.0 leaves
/// unassigned is not recognised, even inside one of its runs: 516, between `JPEGRestartInterval`
/// and `JPEGLosslessPredictors`, is flagged.
#[must_use]
pub fn is_known_tag(tag: u16) -> bool {
    gamut_ifd::tags::TIFF6.find(tag).is_some()
        || matches!(
            tag,
            // Pointer and metadata tags from specifications the registry does not carry, each
            // read or written by this crate.
            SUB_IFDS
                | XMP
                | IPTC_NAA
                | EXIF_IFD
                | ICC_PROFILE
                | GPS_INFO
                | INTEROPERABILITY_IFD
                | C2PA_MANIFEST_STORE
                // Later TIFF extension tags a valid file may carry but the codec does not act on.
                // Their specifications (TIFF-F, the Adobe technical notes) are not vendored, so
                // they stay out of the registry and are recognised here by number only.
                | 326 // BadFaxLines
                | 327 // CleanFaxData
                | 328 // ConsecutiveBadFaxLines
                | 343 // ClipPath
                | 344 // XClipPathUnits
                | 345 // YClipPathUnits
                | 346 // Indexed
                | 347 // JPEGTables
                | 351 // OPIProxy
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_tags_are_known() {
        // Every named structural/codec/pointer tag must be recognised — a guard against adding a
        // constant but forgetting to extend `is_known_tag`.
        for tag in [
            NEW_SUBFILE_TYPE,
            IMAGE_WIDTH,
            PAGE_NUMBER,
            IMAGE_LENGTH,
            BITS_PER_SAMPLE,
            COMPRESSION,
            FILL_ORDER,
            PHOTOMETRIC_INTERPRETATION,
            STRIP_OFFSETS,
            SAMPLES_PER_PIXEL,
            ROWS_PER_STRIP,
            STRIP_BYTE_COUNTS,
            X_RESOLUTION,
            Y_RESOLUTION,
            PLANAR_CONFIGURATION,
            RESOLUTION_UNIT,
            PREDICTOR,
            EXTRA_SAMPLES,
            SAMPLE_FORMAT,
            COLOR_MAP,
            TILE_WIDTH,
            TILE_LENGTH,
            TILE_OFFSETS,
            TILE_BYTE_COUNTS,
            SUB_IFDS,
            XMP,
            COPYRIGHT,
            IMAGE_DESCRIPTION,
            MAKE,
            MODEL,
            ORIENTATION,
            SOFTWARE,
            DATE_TIME,
            ARTIST,
            WHITE_POINT,
            PRIMARY_CHROMATICITIES,
            IPTC_NAA,
            EXIF_IFD,
            ICC_PROFILE,
            GPS_INFO,
            INTEROPERABILITY_IFD,
            C2PA_MANIFEST_STORE,
        ] {
            assert!(is_known_tag(tag), "tag {tag} should be known");
        }
    }

    #[test]
    fn every_tiff6_registry_tag_is_known() {
        for spec in gamut_ifd::tags::TIFF6.specs() {
            assert!(is_known_tag(spec.id), "{} should be known", spec.name);
        }
    }

    #[test]
    fn each_later_extension_tag_is_known() {
        // One assertion per alternative: the gate deletes whole match arms, never one
        // alternative of an or-pattern, so only these can tell a dropped number.
        for tag in [326, 327, 328, 343, 344, 345, 346, 347, 351] {
            assert!(is_known_tag(tag), "tag {tag} should be known");
        }
    }

    #[test]
    fn private_tags_are_unknown() {
        // A private/maker tag and an arbitrary gap value are flagged.
        assert!(!is_known_tag(0x9999));
        assert!(!is_known_tag(50341)); // PrintImageMatching (proprietary)
        assert!(!is_known_tag(700 + 1));
    }

    #[test]
    fn an_unassigned_number_inside_a_tiff6_run_is_unknown() {
        // 516 lies inside the 512..=521 JPEG run but TIFF 6.0 Section 22 assigns it nothing.
        assert!(!is_known_tag(516));
    }

    #[test]
    fn a_dng_tag_is_not_a_tiff_tag() {
        // The registry's DNG block answers for gamut-dng, not for a plain TIFF.
        assert!(!is_known_tag(50706)); // DNGVersion
    }
}
