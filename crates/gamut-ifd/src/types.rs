//! The field types an IFD entry value can take.

/// The type of a tag's value, stored as the 2-byte `Type` of an IFD entry (TIFF 6.0 §2).
///
/// The variants are named for the on-disk codes (`Byte` is `1`, `Short` is `3`, `Rational` is
/// `5`, …). The first twelve are the TIFF 6.0 field types; `Ifd` (code `13`) is the TIFF
/// Technical Note 1 sub-IFD offset type, and `Utf8` (code `129`) is the Exif 3.0 addition for
/// internationalised text. The BigTIFF 64-bit additions (`Long8`/`SLong8`/`Ifd8`, codes
/// `16`–`18`) appear only when the `bigtiff` feature is enabled, so the set stays additive and
/// a classic-only build treats those codes as unknown.
///
/// Non-exhaustive because the `bigtiff` feature adds variants: Cargo unifies features across a
/// build graph, so a dependent's exhaustive `match` would break the moment any other crate in the
/// same build enabled it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FieldType {
    /// `1` — an 8-bit unsigned integer.
    Byte,
    /// `2` — an 8-bit byte holding a 7-bit ASCII code; NUL-terminated.
    Ascii,
    /// `3` — a 16-bit unsigned integer.
    Short,
    /// `4` — a 32-bit unsigned integer.
    Long,
    /// `5` — two `Long`s: a fraction's numerator then denominator.
    Rational,
    /// `6` — an 8-bit signed (two's-complement) integer.
    SByte,
    /// `7` — an 8-bit byte whose meaning depends on the field.
    Undefined,
    /// `8` — a 16-bit signed (two's-complement) integer.
    SShort,
    /// `9` — a 32-bit signed (two's-complement) integer.
    SLong,
    /// `10` — two `SLong`s: a signed fraction's numerator then denominator.
    SRational,
    /// `11` — a 32-bit IEEE single-precision float.
    Float,
    /// `12` — a 64-bit IEEE double-precision float.
    Double,
    /// `13` — a 32-bit unsigned sub-IFD offset (TIFF Technical Note 1 / the Adobe PageMaker 6.0
    /// TIFF supplement). Shaped like `Long`, but marks the value as a child-directory pointer.
    Ifd,
    /// `129` — a UTF-8 string (Exif 3.0 / CIPA DC-008 §4.6.2); NUL-terminated. Like `Ascii`, but
    /// the bytes are explicitly UTF-8 rather than 7-bit ASCII, so non-ASCII text round-trips.
    Utf8,
    /// `16` — a 64-bit unsigned integer (BigTIFF; `references/tiff/bigtiff.html`).
    #[cfg(feature = "bigtiff")]
    Long8,
    /// `17` — a 64-bit signed (two's-complement) integer (BigTIFF).
    #[cfg(feature = "bigtiff")]
    SLong8,
    /// `18` — a 64-bit unsigned IFD offset (BigTIFF).
    #[cfg(feature = "bigtiff")]
    Ifd8,
}

impl FieldType {
    /// Returns the field type for an on-disk type code, or `None` for an unknown code.
    ///
    /// Without the `bigtiff` feature, the 64-bit BigTIFF codes (`16`–`18`) are unknown and return
    /// `None`, so a classic-only reader skips fields of those types rather than failing.
    #[must_use]
    pub fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            1 => FieldType::Byte,
            2 => FieldType::Ascii,
            3 => FieldType::Short,
            4 => FieldType::Long,
            5 => FieldType::Rational,
            6 => FieldType::SByte,
            7 => FieldType::Undefined,
            8 => FieldType::SShort,
            9 => FieldType::SLong,
            10 => FieldType::SRational,
            11 => FieldType::Float,
            12 => FieldType::Double,
            13 => FieldType::Ifd,
            129 => FieldType::Utf8,
            #[cfg(feature = "bigtiff")]
            16 => FieldType::Long8,
            #[cfg(feature = "bigtiff")]
            17 => FieldType::SLong8,
            #[cfg(feature = "bigtiff")]
            18 => FieldType::Ifd8,
            _ => return None,
        })
    }

    /// Returns the on-disk type code.
    #[must_use]
    pub fn code(self) -> u16 {
        match self {
            FieldType::Byte => 1,
            FieldType::Ascii => 2,
            FieldType::Short => 3,
            FieldType::Long => 4,
            FieldType::Rational => 5,
            FieldType::SByte => 6,
            FieldType::Undefined => 7,
            FieldType::SShort => 8,
            FieldType::SLong => 9,
            FieldType::SRational => 10,
            FieldType::Float => 11,
            FieldType::Double => 12,
            FieldType::Ifd => 13,
            FieldType::Utf8 => 129,
            #[cfg(feature = "bigtiff")]
            FieldType::Long8 => 16,
            #[cfg(feature = "bigtiff")]
            FieldType::SLong8 => 17,
            #[cfg(feature = "bigtiff")]
            FieldType::Ifd8 => 18,
        }
    }

    /// Returns the number of bytes in a single value of this type.
    #[must_use]
    pub fn size(self) -> usize {
        match self {
            FieldType::Byte
            | FieldType::Ascii
            | FieldType::SByte
            | FieldType::Undefined
            | FieldType::Utf8 => 1,
            FieldType::Short | FieldType::SShort => 2,
            FieldType::Long | FieldType::SLong | FieldType::Float | FieldType::Ifd => 4,
            FieldType::Rational | FieldType::SRational | FieldType::Double => 8,
            #[cfg(feature = "bigtiff")]
            FieldType::Long8 | FieldType::SLong8 | FieldType::Ifd8 => 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_type_codes_round_trip() {
        for code in 1..=13u16 {
            let ty = FieldType::from_code(code).expect("known type");
            assert_eq!(ty.code(), code);
        }
        assert_eq!(FieldType::from_code(0), None);
        assert_eq!(FieldType::from_code(14), None);
        assert_eq!(FieldType::Rational.size(), 8);
        assert_eq!(FieldType::Short.size(), 2);
        // TIFF TechNote 1's IFD type (13) is LONG-shaped: a 4-byte offset.
        assert_eq!(FieldType::Ifd.size(), 4);
        // Exif 3.0 UTF-8 (code 129): a one-byte-per-element string type, decoded even in a
        // classic-only build (it is not gated behind `bigtiff`).
        assert_eq!(FieldType::from_code(129), Some(FieldType::Utf8));
        assert_eq!(FieldType::Utf8.code(), 129);
        assert_eq!(FieldType::Utf8.size(), 1);
        assert_eq!(FieldType::from_code(128), None);
    }

    #[cfg(feature = "bigtiff")]
    #[test]
    fn bigtiff_field_type_codes_round_trip() {
        for code in 16..=18u16 {
            let ty = FieldType::from_code(code).expect("known BigTIFF type");
            assert_eq!(ty.code(), code);
        }
        assert_eq!(FieldType::from_code(19), None);
        // The BigTIFF 64-bit types are all 8 bytes wide.
        assert_eq!(FieldType::Long8.size(), 8);
        assert_eq!(FieldType::SLong8.size(), 8);
        assert_eq!(FieldType::Ifd8.size(), 8);
    }

    /// Without `bigtiff`, codes 16–18 are unknown so a classic-only reader skips those fields.
    #[cfg(not(feature = "bigtiff"))]
    #[test]
    fn bigtiff_codes_unknown_without_feature() {
        assert_eq!(FieldType::from_code(16), None);
        assert_eq!(FieldType::from_code(17), None);
        assert_eq!(FieldType::from_code(18), None);
    }
}
