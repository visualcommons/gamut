//! The tag table: the signatures and byte offsets that index a profile's tag element data.

use std::collections::HashSet;

use crate::bytes::ByteReader;
use crate::error::{IccError, Result};
use crate::primitives::Signature;

/// One row of the on-disk tag table (ICC.1:2022 §7.3): a tag signature plus the byte offset and
/// size of its element data within the profile.
///
/// This is an encoding detail — the parsed [`crate::IccProfile`] stores decoded tags, not offsets,
/// and the serializer recomputes the table — so it is internal to the crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TagEntry {
    pub(crate) signature: Signature,
    pub(crate) offset: u32,
    pub(crate) size: u32,
}

/// The byte offset just past the tag table — the header (128) + the count word (4) + `tag_count`
/// twelve-byte entries — and thus the lowest valid tag-data offset. Shared by the serializer's
/// layout and the strict parser's overlap check.
pub(crate) const fn tag_table_end(tag_count: usize) -> usize {
    128 + 4 + 12 * tag_count
}

/// Parses the tag table: a `u32` count at offset 128 followed by `count` twelve-byte rows
/// (ICC.1:2022 §7.3).
///
/// Validates that the table itself fits within `profile` and that no tag signature is duplicated.
/// Each tag's element-data bounds are checked by the caller as it is decoded.
pub(crate) fn parse_tag_table(profile: &[u8]) -> Result<Vec<TagEntry>> {
    // A parsed header guarantees the 128 bytes this skips.
    let mut r = ByteReader::at(profile, 128)?;
    let count = r.u32().map_err(|e| e.located(128, None))? as usize;
    // Reject a count that cannot fit before allocating; `12 * count + 132` must be within bounds.
    let table_end = count
        .checked_mul(12)
        .and_then(|n| n.checked_add(132))
        .ok_or(IccError::Malformed("icc: tag count overflow").located(128, None))?;
    if table_end > profile.len() {
        return Err(IccError::Malformed("icc: tag table exceeds profile").located(128, None));
    }
    let mut entries = Vec::with_capacity(count);
    let mut seen = HashSet::with_capacity(count);
    for _ in 0..count {
        let row = r.pos();
        let signature = r.signature()?;
        if !seen.insert(signature.0) {
            return Err(
                IccError::Malformed("icc: duplicate tag signature").located(row, Some(signature))
            );
        }
        let offset = r.u32()?;
        let size = r.u32()?;
        entries.push(TagEntry {
            signature,
            offset,
            size,
        });
    }
    Ok(entries)
}

/// Well-known tag signatures (ICC.1:2022 §9), as an ergonomic catalogue — a `KnownTag` converts
/// into its [`Signature`] (so `profile.get(KnownTag::MediaWhitePoint)` works directly), and
/// [`from_signature`](KnownTag::from_signature) looks a signature up in the catalogue.
///
/// The parser accepts *any* signature, so this is a convenience set (not exhaustive) and is not on
/// the parse path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KnownTag {
    /// `desc` — the profile description (`multiLocalizedUnicodeType` v4 / `textDescriptionType` v2).
    ProfileDescription,
    /// `cprt` — the copyright string.
    Copyright,
    /// `dmnd` — the device manufacturer description.
    DeviceManufacturerDesc,
    /// `dmdd` — the device model description.
    DeviceModelDesc,
    /// `wtpt` — the media white point (`XYZType`).
    MediaWhitePoint,
    /// `bkpt` — the media black point (`XYZType`).
    MediaBlackPoint,
    /// `rXYZ` — the red colorant column (`XYZType`).
    RedColorant,
    /// `gXYZ` — the green colorant column (`XYZType`).
    GreenColorant,
    /// `bXYZ` — the blue colorant column (`XYZType`).
    BlueColorant,
    /// `rTRC` — the red tone-response curve (`curveType` / `parametricCurveType`).
    RedTrc,
    /// `gTRC` — the green tone-response curve.
    GreenTrc,
    /// `bTRC` — the blue tone-response curve.
    BlueTrc,
    /// `kTRC` — the grey tone-response curve.
    GrayTrc,
    /// `A2B0` — the device-to-PCS lookup transform for the perceptual intent.
    AToB0,
    /// `A2B1` — the device-to-PCS lookup transform for the media-relative colorimetric intent.
    AToB1,
    /// `A2B2` — the device-to-PCS lookup transform for the saturation intent.
    AToB2,
    /// `B2A0` — the PCS-to-device lookup transform for the perceptual intent.
    BToA0,
    /// `B2A1` — the PCS-to-device lookup transform for the media-relative colorimetric intent.
    BToA1,
    /// `B2A2` — the PCS-to-device lookup transform for the saturation intent.
    BToA2,
    /// `gamt` — the out-of-gamut lookup transform.
    Gamut,
    /// `chad` — the chromatic-adaptation matrix (`s15Fixed16ArrayType`).
    ChromaticAdaptation,
    /// `lumi` — the luminance (`XYZType`).
    Luminance,
    /// `chrm` — the chromaticity (`chromaticityType`).
    Chromaticity,
    /// `view` — the viewing conditions (`viewingConditionsType`).
    ViewingConditions,
    /// `meas` — the measurement conditions (`measurementType`).
    Measurement,
    /// `tech` — the device technology (`signatureType`).
    Technology,
    /// `targ` — the characterization target (`textType`).
    CharTarget,
    /// `cicp` — coding-independent code points (`cicpType`).
    Cicp,
}

impl KnownTag {
    /// Every catalogued tag, for enumeration. A slice, so the catalogue can grow (the enum is
    /// `#[non_exhaustive]`) without the constant's type changing.
    pub const ALL: &'static [KnownTag] = &[
        KnownTag::ProfileDescription,
        KnownTag::Copyright,
        KnownTag::DeviceManufacturerDesc,
        KnownTag::DeviceModelDesc,
        KnownTag::MediaWhitePoint,
        KnownTag::MediaBlackPoint,
        KnownTag::RedColorant,
        KnownTag::GreenColorant,
        KnownTag::BlueColorant,
        KnownTag::RedTrc,
        KnownTag::GreenTrc,
        KnownTag::BlueTrc,
        KnownTag::GrayTrc,
        KnownTag::AToB0,
        KnownTag::AToB1,
        KnownTag::AToB2,
        KnownTag::BToA0,
        KnownTag::BToA1,
        KnownTag::BToA2,
        KnownTag::Gamut,
        KnownTag::ChromaticAdaptation,
        KnownTag::Luminance,
        KnownTag::Chromaticity,
        KnownTag::ViewingConditions,
        KnownTag::Measurement,
        KnownTag::Technology,
        KnownTag::CharTarget,
        KnownTag::Cicp,
    ];

    /// The catalogued tag for `signature`, if recognized.
    #[must_use]
    pub fn from_signature(signature: Signature) -> Option<KnownTag> {
        KnownTag::ALL
            .iter()
            .copied()
            .find(|&tag| Signature::from(tag) == signature)
    }
}

impl From<KnownTag> for Signature {
    /// The four-byte signature the tag is stored under.
    fn from(tag: KnownTag) -> Signature {
        let code: &[u8; 4] = match tag {
            KnownTag::ProfileDescription => b"desc",
            KnownTag::Copyright => b"cprt",
            KnownTag::DeviceManufacturerDesc => b"dmnd",
            KnownTag::DeviceModelDesc => b"dmdd",
            KnownTag::MediaWhitePoint => b"wtpt",
            KnownTag::MediaBlackPoint => b"bkpt",
            KnownTag::RedColorant => b"rXYZ",
            KnownTag::GreenColorant => b"gXYZ",
            KnownTag::BlueColorant => b"bXYZ",
            KnownTag::RedTrc => b"rTRC",
            KnownTag::GreenTrc => b"gTRC",
            KnownTag::BlueTrc => b"bTRC",
            KnownTag::GrayTrc => b"kTRC",
            KnownTag::AToB0 => b"A2B0",
            KnownTag::AToB1 => b"A2B1",
            KnownTag::AToB2 => b"A2B2",
            KnownTag::BToA0 => b"B2A0",
            KnownTag::BToA1 => b"B2A1",
            KnownTag::BToA2 => b"B2A2",
            KnownTag::Gamut => b"gamt",
            KnownTag::ChromaticAdaptation => b"chad",
            KnownTag::Luminance => b"lumi",
            KnownTag::Chromaticity => b"chrm",
            KnownTag::ViewingConditions => b"view",
            KnownTag::Measurement => b"meas",
            KnownTag::Technology => b"tech",
            KnownTag::CharTarget => b"targ",
            KnownTag::Cicp => b"cicp",
        };
        Signature(*code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 128-byte zero header is the minimum any profile bytes start with; tag-table tests append
    /// the count and rows after it.
    fn header_padding() -> Vec<u8> {
        vec![0u8; 128]
    }

    #[test]
    fn parses_rows_after_the_count() {
        let mut b = header_padding();
        b.extend_from_slice(&2u32.to_be_bytes()); // count
        for (sig, off, size) in [(b"wtpt", 200u32, 20u32), (b"rXYZ", 220, 20)] {
            b.extend_from_slice(sig);
            b.extend_from_slice(&off.to_be_bytes());
            b.extend_from_slice(&size.to_be_bytes());
        }
        b.resize(240, 0); // room for the referenced data
        let entries = parse_tag_table(&b).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].signature, Signature(*b"wtpt"));
        assert_eq!(entries[1].offset, 220);
    }

    #[test]
    fn rejects_duplicate_signature() {
        let mut b = header_padding();
        b.extend_from_slice(&2u32.to_be_bytes());
        for _ in 0..2 {
            b.extend_from_slice(b"wtpt");
            b.extend_from_slice(&156u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
        }
        let err = parse_tag_table(&b).unwrap_err();
        assert_eq!(err.message(), "icc: duplicate tag signature");
        // The second row (132 + 12) holds the repeat.
        assert_eq!(err.offset(), Some(144));
        assert_eq!(err.tag(), Some(Signature(*b"wtpt")));
    }

    #[test]
    fn rejects_count_that_overflows_the_buffer() {
        let mut b = header_padding();
        b.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes()); // absurd count
        let err = parse_tag_table(&b).unwrap_err();
        assert_eq!(err.message(), "icc: tag table exceeds profile");
        assert_eq!((err.offset(), err.tag()), (Some(128), None));
    }

    #[test]
    fn rejects_truncated_table() {
        let mut b = header_padding();
        b.extend_from_slice(&1u32.to_be_bytes()); // count 1, but no row follows
        let err = parse_tag_table(&b).unwrap_err();
        assert_eq!(err.message(), "icc: tag table exceeds profile");
        assert_eq!((err.offset(), err.tag()), (Some(128), None));
    }

    #[test]
    fn missing_tag_count_is_located_at_the_count() {
        let mut b = header_padding();
        b.extend_from_slice(&[0, 0]); // half a count word
        let err = parse_tag_table(&b).unwrap_err();
        assert_eq!(err.message(), "icc: unexpected end of data");
        assert_eq!((err.offset(), err.tag()), (Some(128), None));
    }

    #[test]
    fn known_tag_signature_round_trip() {
        for &tag in KnownTag::ALL {
            assert_eq!(KnownTag::from_signature(Signature::from(tag)), Some(tag));
        }
        assert_eq!(KnownTag::from_signature(Signature(*b"zzzz")), None);
    }
}
