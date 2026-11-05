//! The one error type every gamut codec returns, and the stable classification callers match on.
//!
//! [`Error`] is opaque: its representation is private, so a caller matches on [`Error::kind`] and
//! reads the diagnostic context through [`Error::origin`], [`Error::byte_offset`],
//! [`Error::detail`], [`Error::static_message`] and [`std::error::Error::source`]. There is exactly
//! one way to ask each question, whatever context a producer attached, so a match never depends on
//! whether the error was wrapped on its way up.

/// Stable classification of a gamut error, independent of any diagnostic context around it.
///
/// The discriminants are permanent and append-only so the classification remains mechanically
/// portable to the C status surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u32)]
pub enum ErrorKind {
    /// The input data was malformed, truncated, or otherwise invalid.
    InvalidInput = 1,
    /// The requested format, profile, or feature is unsupported.
    Unsupported = 2,
    /// An underlying I/O operation failed.
    Io = 3,
}

const _: () = {
    assert!(ErrorKind::InvalidInput as u32 == 1);
    assert!(ErrorKind::Unsupported as u32 == 2);
    assert!(ErrorKind::Io as u32 == 3);
};

/// Errors produced by gamut encoders and decoders.
///
/// Built with [`Error::invalid_input`], [`Error::unsupported`] or `From<std::io::Error>`, and
/// enriched with the first-write-wins [`Error::with_origin`], [`Error::with_byte_offset`] and
/// [`Error::with_detail`] builders. Matched through [`Error::kind`].
///
/// The value is a single boxed pointer, so `Result<T>` stays small on every success path; the one
/// allocation happens only when an error is constructed.
pub struct Error {
    inner: Box<Inner>,
}

#[derive(Debug)]
struct Inner {
    repr: Repr,
    origin: Option<&'static str>,
    byte_offset: Option<u64>,
    detail: Option<Box<str>>,
}

/// The classified payload; its `Display` is the head of [`Error`]'s, and its `source` is
/// [`Error`]'s.
#[derive(Debug, thiserror::Error)]
enum Repr {
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("i/o error: {0}")]
    Io(#[source] std::io::Error),
}

impl Error {
    fn from_repr(repr: Repr, origin: Option<&'static str>) -> Self {
        Self {
            inner: Box::new(Inner {
                repr,
                origin,
                byte_offset: None,
                detail: None,
            }),
        }
    }

    /// Constructs an invalid-input error: the bytes were malformed, truncated, or otherwise not
    /// valid for the format. `origin` names the Cargo package or format layer reporting it
    /// (conventionally `env!("CARGO_PKG_NAME")`).
    #[must_use]
    pub fn invalid_input(origin: &'static str, message: &'static str) -> Self {
        Self::from_repr(Repr::InvalidInput(message), Some(origin))
    }

    /// Constructs an unsupported-feature error: the requested format, profile, or feature is not
    /// supported. `origin` names the Cargo package or format layer reporting it.
    #[must_use]
    pub fn unsupported(origin: &'static str, message: &'static str) -> Self {
        Self::from_repr(Repr::Unsupported(message), Some(origin))
    }

    /// Returns the stable classification of this error.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self.inner.repr {
            Repr::InvalidInput(_) => ErrorKind::InvalidInput,
            Repr::Unsupported(_) => ErrorKind::Unsupported,
            Repr::Io(_) => ErrorKind::Io,
        }
    }

    /// Returns the static message an invalid-input or unsupported error was built with.
    ///
    /// Context does not change the message. An [`ErrorKind::Io`] error has no static message
    /// because its text belongs to the underlying [`std::io::Error`], reached through
    /// [`std::error::Error::source`].
    #[must_use]
    pub fn static_message(&self) -> Option<&'static str> {
        match self.inner.repr {
            Repr::InvalidInput(message) | Repr::Unsupported(message) => Some(message),
            Repr::Io(_) => None,
        }
    }

    /// Returns the Cargo package or format layer that produced the error, when known.
    #[must_use]
    pub fn origin(&self) -> Option<&'static str> {
        self.inner.origin
    }

    /// Returns the byte offset relative to the producing parser's immediate input, when known.
    #[must_use]
    pub fn byte_offset(&self) -> Option<u64> {
        self.inner.byte_offset
    }

    /// Returns dynamic diagnostic detail retained from an underlying parser or backend.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.inner.detail.as_deref()
    }

    /// Attaches the producing Cargo package or format layer if no origin is already present.
    #[must_use]
    pub fn with_origin(mut self, origin: &'static str) -> Self {
        self.inner.origin.get_or_insert(origin);
        self
    }

    /// Attaches a byte offset if no offset is already present.
    ///
    /// The offset is relative to the immediate input accepted by the parser that reports it.
    #[must_use]
    pub fn with_byte_offset(mut self, byte_offset: u64) -> Self {
        self.inner.byte_offset.get_or_insert(byte_offset);
        self
    }

    /// Attaches owned dynamic detail if no detail is already present.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<Box<str>>) -> Self {
        if self.inner.detail.is_none() {
            self.inner.detail = Some(detail.into());
        }
        self
    }
}

/// An I/O failure while reading from a stream-backed source, classified [`ErrorKind::Io`].
///
/// Distinct from [`ErrorKind::InvalidInput`]: it reports the *transport* failing (a disk error, an
/// interrupted read), not the bytes being malformed, so batch pipelines can retry or surface it
/// instead of misclassifying the file as corrupt. The I/O error is the [`source`] of the result.
///
/// [`source`]: std::error::Error::source
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::from_repr(Repr::Io(error), None)
    }
}

impl core::fmt::Debug for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let inner = &*self.inner;
        f.debug_struct("Error")
            .field("repr", &inner.repr)
            .field("origin", &inner.origin)
            .field("byte_offset", &inner.byte_offset)
            .field("detail", &inner.detail)
            .finish()
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let inner = &*self.inner;
        write!(f, "{}", inner.repr)?;
        match (inner.origin, inner.byte_offset) {
            (Some(origin), Some(offset)) => {
                write!(f, " [origin: {origin}, byte offset: {offset}]")?
            }
            (Some(origin), None) => write!(f, " [origin: {origin}]")?,
            (None, Some(offset)) => write!(f, " [byte offset: {offset}]")?,
            (None, None) => {}
        }
        if let Some(detail) = &inner.detail {
            write!(f, ": {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        std::error::Error::source(&self.inner.repr)
    }
}

/// Convenience result type for gamut operations.
pub type Result<T> = core::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn invalid_input_carries_kind_message_and_origin() {
        let err = Error::invalid_input("gamut-test", "bad bytes");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.static_message(), Some("bad bytes"));
        assert_eq!(err.origin(), Some("gamut-test"));
        assert_eq!(err.byte_offset(), None);
        assert_eq!(err.detail(), None);
        assert!(err.source().is_none());
        assert_eq!(
            err.to_string(),
            "invalid input: bad bytes [origin: gamut-test]"
        );
    }

    #[test]
    fn unsupported_carries_kind_message_and_origin() {
        let err = Error::unsupported("gamut-test", "future profile");
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert_eq!(err.static_message(), Some("future profile"));
        assert_eq!(err.origin(), Some("gamut-test"));
        assert!(err.source().is_none());
        assert_eq!(
            err.to_string(),
            "unsupported: future profile [origin: gamut-test]"
        );
    }

    #[test]
    fn io_error_converts_keeps_its_source_and_displays() {
        let io = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short read");
        let err: Error = io.into();
        assert_eq!(err.kind(), ErrorKind::Io);
        assert_eq!(err.static_message(), None);
        assert_eq!(err.origin(), None);
        assert_eq!(err.to_string(), "i/o error: short read");
        let source = err.source().expect("io source");
        assert_eq!(source.to_string(), "short read");
    }

    #[test]
    fn context_is_first_write_wins() {
        let err = Error::invalid_input("gamut-isobmff", "bad box")
            .with_origin("gamut-avif")
            .with_byte_offset(17)
            .with_byte_offset(99)
            .with_detail("declared size exceeds input")
            .with_detail("replacement");

        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.static_message(), Some("bad box"));
        assert_eq!(err.origin(), Some("gamut-isobmff"));
        assert_eq!(err.byte_offset(), Some(17));
        assert_eq!(err.detail(), Some("declared size exceeds input"));
        assert_eq!(
            err.to_string(),
            "invalid input: bad box [origin: gamut-isobmff, byte offset: 17]: declared size exceeds input"
        );
    }

    #[test]
    fn with_origin_fills_an_absent_origin() {
        let err = Error::from(std::io::Error::other("disk on fire"))
            .with_origin("gamut-ifd")
            .with_byte_offset(4096);
        assert_eq!(err.kind(), ErrorKind::Io);
        assert_eq!(err.origin(), Some("gamut-ifd"));
        assert_eq!(err.byte_offset(), Some(4096));
        assert_eq!(
            err.to_string(),
            "i/o error: disk on fire [origin: gamut-ifd, byte offset: 4096]"
        );
        assert_eq!(err.source().expect("io source").to_string(), "disk on fire");
    }

    #[test]
    fn partial_context_displays_without_empty_separators() {
        assert_eq!(
            Error::from(std::io::Error::other("eof"))
                .with_byte_offset(3)
                .to_string(),
            "i/o error: eof [byte offset: 3]"
        );
        assert_eq!(
            Error::from(std::io::Error::other("eof"))
                .with_detail("decoder rejected marker")
                .to_string(),
            "i/o error: eof: decoder rejected marker"
        );
    }

    #[test]
    fn debug_names_every_field() {
        let rendered = format!(
            "{:?}",
            Error::unsupported("gamut-test", "m")
                .with_byte_offset(5)
                .with_detail("d")
        );
        assert_eq!(
            rendered,
            "Error { repr: Unsupported(\"m\"), origin: Some(\"gamut-test\"), byte_offset: Some(5), \
             detail: Some(\"d\") }"
        );
    }

    #[test]
    fn errors_are_send_sync_and_one_pointer_wide() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
        assert_eq!(size_of::<Error>(), size_of::<usize>());
    }
}
