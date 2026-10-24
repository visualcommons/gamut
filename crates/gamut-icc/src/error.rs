//! The crate's error type.

use crate::primitives::Signature;

/// An error from parsing or serializing an ICC profile.
///
/// Every failure the ICC reader and writer raise is a malformed input or a violated spec
/// invariant; the message names the specific structure at fault (e.g. `"icc: duplicate tag
/// signature"`). Exposing the crate's own type — rather than the shared [`gamut_core::Error`] —
/// keeps the failing carrier identifiable when embedded in a wider metadata pipeline.
///
/// A failure raised while parsing a whole profile ([`crate::IccProfile::parse`],
/// [`crate::IccReader::parse`], [`crate::ProfileHeader::parse`]) is
/// [`MalformedAt`](IccError::MalformedAt): it also carries the byte offset of the structure at
/// fault and, when that structure is a tag element or tag-table row, the tag's signature. A failure
/// with no input position — a serialization error, or a standalone conversion such as
/// `DeviceClass::try_from` — is [`Malformed`](IccError::Malformed). Read the context through
/// [`message`](IccError::message), [`offset`](IccError::offset) and [`tag`](IccError::tag), which
/// answer the same way for either variant, rather than by matching.
///
/// Marked `#[non_exhaustive]` so a future failure category can be added without a breaking change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IccError {
    /// The profile was malformed, truncated, or violated an ICC.1:2022 invariant. The string
    /// names the specific structure at fault.
    #[error("{0}")]
    Malformed(&'static str),
    /// As [`Malformed`](IccError::Malformed), located in the parsed bytes.
    #[error("{message} ({})", Location { offset: *offset, tag: *tag })]
    MalformedAt {
        /// Names the specific structure at fault; the same text [`Malformed`](IccError::Malformed)
        /// carries for the same failure.
        message: &'static str,
        /// The byte offset from the start of the profile of the structure at fault: the header
        /// field, the tag-table row, or — for a failure inside a tag's element data — the
        /// element's first byte, as its tag-table row declares it. For a profile shorter than its
        /// 128-byte header, the input's length (where the missing bytes begin).
        offset: u64,
        /// The signature of the tag whose element data or tag-table row is at fault, or `None`
        /// for a failure in the header or the tag count.
        tag: Option<Signature>,
    },
}

/// Renders a [`IccError::MalformedAt`] location: `at byte N`, then `in tag 'sig '` when one applies.
struct Location {
    offset: u64,
    tag: Option<Signature>,
}

impl core::fmt::Display for Location {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "at byte {}", self.offset)?;
        if let Some(tag) = self.tag {
            write!(f, " in tag '{tag}'")?;
        }
        Ok(())
    }
}

impl IccError {
    /// The text naming the structure at fault, without any location.
    #[must_use]
    pub fn message(&self) -> &'static str {
        match self {
            IccError::Malformed(message) | IccError::MalformedAt { message, .. } => message,
        }
    }

    /// The byte offset from the start of the profile of the structure at fault, when the failure
    /// was raised against a position in parsed bytes (see [`IccError::MalformedAt`]).
    #[must_use]
    pub fn offset(&self) -> Option<u64> {
        match self {
            IccError::Malformed(_) => None,
            IccError::MalformedAt { offset, .. } => Some(*offset),
        }
    }

    /// The signature of the tag whose element data or tag-table row is at fault, when there is
    /// one.
    #[must_use]
    pub fn tag(&self) -> Option<Signature> {
        match self {
            IccError::Malformed(_) => None,
            IccError::MalformedAt { tag, .. } => *tag,
        }
    }

    /// Locates an unlocated error at `offset` (and `tag`); an error that already carries a
    /// location keeps it, because the narrower structure that raised it knew more.
    pub(crate) fn located(self, offset: usize, tag: Option<Signature>) -> Self {
        match self {
            IccError::Malformed(message) => IccError::MalformedAt {
                message,
                offset: offset as u64,
                tag,
            },
            located => located,
        }
    }
}

/// A [`Result`](core::result::Result) whose error is [`IccError`].
pub type Result<T> = core::result::Result<T, IccError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn located_attaches_offset_and_tag_to_an_unlocated_error() {
        let e = IccError::Malformed("icc: x").located(200, Some(Signature(*b"wtpt")));
        assert_eq!(e.message(), "icc: x");
        assert_eq!(e.offset(), Some(200));
        assert_eq!(e.tag(), Some(Signature(*b"wtpt")));
    }

    #[test]
    fn located_keeps_an_existing_location() {
        let inner = IccError::Malformed("icc: x").located(140, None);
        let e = inner.located(200, Some(Signature(*b"wtpt")));
        assert_eq!(e.offset(), Some(140));
        assert_eq!(e.tag(), None);
    }

    #[test]
    fn unlocated_error_has_no_context() {
        let e = IccError::Malformed("icc: x");
        assert_eq!(e.message(), "icc: x");
        assert_eq!(e.offset(), None);
        assert_eq!(e.tag(), None);
        assert_eq!(e.to_string(), "icc: x");
    }

    #[test]
    fn display_appends_the_location() {
        let header = IccError::Malformed("icc: x").located(12, None);
        assert_eq!(header.to_string(), "icc: x (at byte 12)");
        let tag = IccError::Malformed("icc: x").located(200, Some(Signature(*b"rTRC")));
        assert_eq!(tag.to_string(), "icc: x (at byte 200 in tag 'rTRC')");
    }
}
