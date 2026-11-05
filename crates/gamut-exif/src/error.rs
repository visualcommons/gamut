//! The crate's error type.

use gamut_ifd::FieldType;
use gamut_ifd::tags::TagCount;

/// An error from parsing, checking or serialising an EXIF blob.
///
/// Marked `#[non_exhaustive]`: new variants can be added post-1.0 without a breaking change. The
/// [`ExifError::Core`] variant forwards a failure from the underlying [`gamut_ifd`] TIFF/IFD
/// container. [`ExifError::WrongFieldType`] and [`ExifError::WrongCount`] come only from the
/// write-side conformance check ([`check_tag`](crate::check_tag) /
/// [`set_tag_checked`](crate::set_tag_checked)); the read path never returns them.
///
/// `From<ExifError> for gamut_core::Error` funnels every variant into the workspace error, so a
/// format crate can propagate an EXIF failure with `?`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExifError {
    /// The blob did not begin with the `Exif\0\0` marker, but a marker was required.
    #[error("missing \"Exif\\0\\0\" marker")]
    MissingMarker,
    /// The TIFF header's byte-order mark was neither `II` (little-endian) nor `MM` (big-endian).
    #[error("invalid TIFF byte-order mark")]
    BadByteOrder,
    /// The data ended before a structure the headers declared present.
    #[error("truncated EXIF data")]
    Truncated,
    /// A pointer or value offset addressed a location outside the EXIF data.
    #[error("offset {0} out of bounds")]
    BadOffset(u64),
    /// A named sub-IFD (`"Exif"`, `"GPS"`, or `"Interop"`) was malformed.
    #[error("malformed {0} sub-IFD")]
    InvalidIfd(&'static str),
    /// The embedded thumbnail (1st IFD) was malformed.
    #[error("invalid thumbnail: {0}")]
    BadThumbnail(&'static str),
    /// A text field held bytes that are not valid UTF-8.
    #[error("invalid UTF-8 text field")]
    Utf8,
    /// A value offered to [`set_tag_checked`](crate::set_tag_checked) has a field type CIPA
    /// DC-008 does not permit for the tag.
    ///
    /// Both sides are typed, so a caller can act on the refusal without parsing the message; the
    /// [`Display`](core::fmt::Display) rendering spells them the way the spec does
    /// (`"FNumber: CIPA DC-008 requires RATIONAL, not SHORT"`).
    #[error(
        "{tag}: CIPA DC-008 requires {}, not {}",
        spec_type_list(.expected),
        spec_type_name(*.actual)
    )]
    WrongFieldType {
        /// The tag's canonical name.
        tag: &'static str,
        /// The permitted type(s): exactly the tag's
        /// [`ExifTag::field_types`](crate::ExifTag::field_types), never empty here.
        expected: &'static [FieldType],
        /// The on-disk field-type code the offered value would have been written with
        /// ([`Value::type_code`](crate::Value::type_code)). A code rather than a [`FieldType`],
        /// because a [`Value::Unknown`](crate::Value::Unknown) carries a code no `FieldType`
        /// names.
        actual: u16,
    },
    /// A value offered to [`set_tag_checked`](crate::set_tag_checked) has a component count CIPA
    /// DC-008 does not allow for the tag.
    #[error("{tag}: CIPA DC-008 requires a count of {expected}, not {actual}")]
    WrongCount {
        /// The tag's canonical name.
        tag: &'static str,
        /// The permitted count(s), as the spec's `Count` column writes them.
        expected: TagCount,
        /// The component count of the offered value.
        actual: u64,
    },
    /// A failure from the underlying [`gamut_ifd`] TIFF/IFD container, or from the byte source.
    #[error(transparent)]
    Core(#[from] gamut_core::Error),
}

/// A [`Result`](core::result::Result) whose error is [`ExifError`].
pub type Result<T> = core::result::Result<T, ExifError>;

impl From<ExifError> for gamut_core::Error {
    /// Funnels an [`ExifError`] into the workspace's unified [`gamut_core::Error`].
    ///
    /// [`ExifError::Core`] is handed back unchanged, so its kind, origin, offset and detail
    /// survive. Every other variant is [`ErrorKind::InvalidInput`](gamut_core::ErrorKind): the
    /// read-side ones describe malformed bytes, and the two write-side ones a value the caller
    /// offered. Each carries a static message naming its variant and the full EXIF diagnostic as
    /// [`detail`](gamut_core::Error::detail); [`ExifError::BadOffset`] also sets the
    /// [`byte_offset`](gamut_core::Error::byte_offset).
    fn from(err: ExifError) -> Self {
        let message = match err {
            ExifError::Core(core) => return core,
            ExifError::MissingMarker => "EXIF: missing Exif marker",
            ExifError::BadByteOrder => "EXIF: invalid TIFF byte-order mark",
            ExifError::Truncated => "EXIF: truncated data",
            ExifError::BadOffset(_) => "EXIF: offset out of bounds",
            ExifError::InvalidIfd(_) => "EXIF: malformed sub-IFD",
            ExifError::BadThumbnail(_) => "EXIF: invalid thumbnail",
            ExifError::Utf8 => "EXIF: invalid UTF-8 text field",
            ExifError::WrongFieldType { .. } => "EXIF: field type violates CIPA DC-008",
            ExifError::WrongCount { .. } => "EXIF: component count violates CIPA DC-008",
        };
        let core = gamut_core::Error::invalid_input(env!("CARGO_PKG_NAME"), message)
            .with_detail(err.to_string());
        match err {
            ExifError::BadOffset(at) => core.with_byte_offset(at),
            _ => core,
        }
    }
}

/// The spec's own spelling of an on-disk field-type code, so the error message can be checked
/// against CIPA DC-008 without a lookup table.
///
/// Takes the code rather than a [`FieldType`] deliberately: `FieldType` grows three variants when
/// another workspace crate turns on `gamut-ifd`'s `bigtiff` feature, which Cargo unifies into this
/// build, so an exhaustive match over the *variants* would compile in one configuration and not
/// the other. A code is also what a [`Value::Unknown`](crate::Value::Unknown) has to offer.
fn spec_type_name(code: u16) -> String {
    match code {
        1 => "BYTE".to_owned(),
        2 => "ASCII".to_owned(),
        3 => "SHORT".to_owned(),
        4 => "LONG".to_owned(),
        5 => "RATIONAL".to_owned(),
        6 => "SBYTE".to_owned(),
        7 => "UNDEFINED".to_owned(),
        8 => "SSHORT".to_owned(),
        9 => "SLONG".to_owned(),
        10 => "SRATIONAL".to_owned(),
        11 => "FLOAT".to_owned(),
        12 => "DOUBLE".to_owned(),
        13 => "IFD".to_owned(),
        129 => "UTF-8".to_owned(),
        other => format!("type code {other}"),
    }
}

/// Joins the permitted types the way CIPA DC-008 writes them: `"SHORT or LONG"`.
fn spec_type_list(types: &[FieldType]) -> String {
    types
        .iter()
        .map(|t| spec_type_name(t.code()))
        .collect::<Vec<_>>()
        .join(" or ")
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    #[test]
    fn messages_are_stable() {
        assert_eq!(
            ExifError::MissingMarker.to_string(),
            "missing \"Exif\\0\\0\" marker"
        );
        assert_eq!(
            ExifError::BadByteOrder.to_string(),
            "invalid TIFF byte-order mark"
        );
        assert_eq!(ExifError::Truncated.to_string(), "truncated EXIF data");
        assert_eq!(
            ExifError::BadOffset(42).to_string(),
            "offset 42 out of bounds"
        );
        assert_eq!(
            ExifError::InvalidIfd("GPS").to_string(),
            "malformed GPS sub-IFD"
        );
        assert_eq!(
            ExifError::BadThumbnail("length overflow").to_string(),
            "invalid thumbnail: length overflow"
        );
        assert_eq!(ExifError::Utf8.to_string(), "invalid UTF-8 text field");
    }

    #[test]
    fn wraps_ifd_errors_transparently() {
        let inner = gamut_core::Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: bad");
        let err: ExifError = inner.into();
        // `#[error(transparent)]` forwards the inner Display verbatim.
        assert_eq!(
            err.to_string(),
            "invalid input: TIFF: bad [origin: gamut-exif]"
        );
        assert!(matches!(err, ExifError::Core(_)));
    }

    #[test]
    fn core_errors_convert_back_unchanged() {
        // A core error that went through `ExifError::Core` comes back out with its kind, origin,
        // message and offset intact: the conversion does not reclassify it as invalid input.
        let inner = gamut_core::Error::unsupported("gamut-ifd", "BigTIFF").with_byte_offset(8);
        let core = gamut_core::Error::from(ExifError::Core(inner));
        assert_eq!(core.kind(), ErrorKind::Unsupported);
        assert_eq!(core.origin(), Some("gamut-ifd"));
        assert_eq!(core.static_message(), Some("BigTIFF"));
        assert_eq!(core.byte_offset(), Some(8));
        assert_eq!(core.detail(), None);
    }

    #[test]
    fn every_exif_variant_converts_to_invalid_input_with_its_diagnostic() {
        let cases = [
            (ExifError::MissingMarker, "EXIF: missing Exif marker"),
            (
                ExifError::BadByteOrder,
                "EXIF: invalid TIFF byte-order mark",
            ),
            (ExifError::Truncated, "EXIF: truncated data"),
            (ExifError::BadOffset(42), "EXIF: offset out of bounds"),
            (ExifError::InvalidIfd("GPS"), "EXIF: malformed sub-IFD"),
            (ExifError::BadThumbnail("x"), "EXIF: invalid thumbnail"),
            (ExifError::Utf8, "EXIF: invalid UTF-8 text field"),
            (
                ExifError::WrongFieldType {
                    tag: "FNumber",
                    expected: &[FieldType::Rational],
                    actual: 3,
                },
                "EXIF: field type violates CIPA DC-008",
            ),
            (
                ExifError::WrongCount {
                    tag: "GPSLatitude",
                    expected: TagCount::Exact(3),
                    actual: 2,
                },
                "EXIF: component count violates CIPA DC-008",
            ),
        ];
        for (err, message) in cases {
            let detail = err.to_string();
            let core = gamut_core::Error::from(err);
            assert_eq!(core.kind(), ErrorKind::InvalidInput, "{detail}");
            assert_eq!(core.origin(), Some("gamut-exif"), "{detail}");
            assert_eq!(core.static_message(), Some(message), "{detail}");
            assert_eq!(core.detail(), Some(detail.as_str()));
        }
    }

    #[test]
    fn only_a_bad_offset_carries_a_byte_offset() {
        let core = gamut_core::Error::from(ExifError::BadOffset(42));
        assert_eq!(core.byte_offset(), Some(42));
        let core = gamut_core::Error::from(ExifError::Truncated);
        assert_eq!(core.byte_offset(), None);
    }

    #[test]
    fn spec_type_name_spells_every_exif_field_type() {
        // The names quoted in the write-side error must be CIPA DC-008's own, so a reader can
        // look the constraint up. Every type an EXIF value can have, plus the fallback.
        for (code, name) in [
            (1, "BYTE"),
            (2, "ASCII"),
            (3, "SHORT"),
            (4, "LONG"),
            (5, "RATIONAL"),
            (6, "SBYTE"),
            (7, "UNDEFINED"),
            (8, "SSHORT"),
            (9, "SLONG"),
            (10, "SRATIONAL"),
            (11, "FLOAT"),
            (12, "DOUBLE"),
            (13, "IFD"),
            (129, "UTF-8"),
        ] {
            assert_eq!(spec_type_name(code), name);
        }
        // A code no TIFF revision this crate speaks defines still names itself.
        assert_eq!(spec_type_name(200), "type code 200");
        assert_eq!(spec_type_name(16), "type code 16");
    }

    #[test]
    fn spec_type_list_joins_alternatives_the_way_the_spec_does() {
        assert_eq!(spec_type_list(&[FieldType::Rational]), "RATIONAL");
        assert_eq!(
            spec_type_list(&[FieldType::Short, FieldType::Long]),
            "SHORT or LONG"
        );
        assert_eq!(
            spec_type_list(&[FieldType::Ascii, FieldType::Utf8]),
            "ASCII or UTF-8"
        );
        assert_eq!(spec_type_list(&[]), "");
    }
}
