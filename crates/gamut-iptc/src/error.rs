//! The crate's error type.

/// An error from parsing or serializing IPTC photo metadata.
///
/// The two failure modes mirror the crate's error contract: [`Malformed`](IptcError::Malformed)
/// for a value that cannot be represented on the wire (strict write) or an IIM/IRB structure that
/// is corrupt (honest read), and [`Unsupported`](IptcError::Unsupported) for a `1:90`
/// coded-character-set designation gamut declines to guess at. Exposing the crate's own type —
/// rather than the shared [`gamut_core::Error`] — keeps the failing carrier identifiable when
/// embedded in a wider metadata pipeline.
///
/// Marked `#[non_exhaustive]` so a future failure category can be added without a breaking change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IptcError {
    /// A value could not be encoded on the wire, or an IIM/IRB structure was malformed. The string
    /// names the specific structure or constraint at fault.
    #[error("{0}")]
    Malformed(&'static str),
    /// A feature the crate declines to handle — chiefly a `1:90` coded-character-set designation
    /// beyond Latin-1 and UTF-8. The string names the unsupported feature.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

/// A [`Result`](core::result::Result) whose error is [`IptcError`].
pub type Result<T> = core::result::Result<T, IptcError>;

impl From<IptcError> for gamut_core::Error {
    /// Funnels an [`IptcError`] into the workspace's unified [`gamut_core::Error`], so a caller that
    /// speaks the core error can `?` an IPTC result. [`IptcError::Malformed`] classifies as
    /// [`gamut_core::ErrorKind::InvalidInput`] and [`IptcError::Unsupported`] as
    /// [`gamut_core::ErrorKind::Unsupported`]; the static message is kept unchanged and the origin
    /// names this crate.
    fn from(err: IptcError) -> Self {
        match err {
            IptcError::Malformed(message) => {
                gamut_core::Error::invalid_input(env!("CARGO_PKG_NAME"), message)
            }
            IptcError::Unsupported(message) => {
                gamut_core::Error::unsupported(env!("CARGO_PKG_NAME"), message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    #[test]
    fn malformed_converts_to_core_invalid_input_with_its_message() {
        let core = gamut_core::Error::from(IptcError::Malformed("IIM: truncated dataset"));
        assert_eq!(core.kind(), ErrorKind::InvalidInput);
        assert_eq!(core.static_message(), Some("IIM: truncated dataset"));
        assert_eq!(core.origin(), Some("gamut-iptc"));
    }

    #[test]
    fn unsupported_converts_to_core_unsupported_with_its_message() {
        let core = gamut_core::Error::from(IptcError::Unsupported("1:90 charset"));
        assert_eq!(core.kind(), ErrorKind::Unsupported);
        assert_eq!(core.static_message(), Some("1:90 charset"));
        assert_eq!(core.origin(), Some("gamut-iptc"));
    }
}
