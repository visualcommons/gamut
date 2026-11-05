//! The crate's error type.

/// An error from parsing or serialising an EXIF blob.
///
/// Marked `#[non_exhaustive]`: new variants can be added post-1.0 without a breaking change. The
/// [`ExifError::Ifd`] variant forwards a failure from the underlying [`gamut_ifd`] TIFF/IFD
/// container.
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
    /// A failure from the underlying [`gamut_ifd`] TIFF/IFD container.
    #[error(transparent)]
    Ifd(#[from] gamut_core::Error),
}

/// A [`Result`](core::result::Result) whose error is [`ExifError`].
pub type Result<T> = core::result::Result<T, ExifError>;

#[cfg(test)]
mod tests {
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
        assert!(matches!(err, ExifError::Ifd(_)));
    }
}
