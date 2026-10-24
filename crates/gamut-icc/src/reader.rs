//! The ICC profile reader.

use crate::error::Result;
use crate::profile::IccProfile;

/// Reader for an ICC profile, with parse options.
///
/// `IccReader::new().parse(bytes)` is equivalent to [`IccProfile::parse`]. Enable
/// [`strict`](IccReader::strict) to additionally reject non-conformant inputs that the lenient
/// default tolerates (nonzero reserved header bytes; tags whose data overlaps the header or tag
/// table).
#[derive(Debug, Clone, Default)]
pub struct IccReader {
    strict: bool,
}

impl IccReader {
    /// A reader with lenient parsing (the default).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets whether to reject non-conformant inputs in addition to malformed ones.
    #[must_use]
    pub fn strict(mut self, yes: bool) -> Self {
        self.strict = yes;
        self
    }

    /// Parses an ICC profile from its bytes.
    ///
    /// # Errors
    ///
    /// Returns [`IccError::MalformedAt`](crate::IccError::MalformedAt) if the profile is malformed,
    /// or — in strict mode — non-conformant, located as [`IccProfile::parse`] documents.
    pub fn parse(&self, bytes: &[u8]) -> Result<IccProfile> {
        IccProfile::parse_with(bytes, self.strict)
    }
}
