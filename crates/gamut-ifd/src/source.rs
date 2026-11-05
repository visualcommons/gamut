//! Random-access byte sources for IFD parsing.
//!
//! TIFF structure is offset-driven: a parse touches the header, each directory body, and each
//! out-of-line value — a tiny, scattered fraction of a multi-hundred-MB camera file. [`ReadAt`]
//! abstracts positioned reads so [`IfdReader`](crate::IfdReader) can walk that structure from a
//! borrowed slice, a seekable stream ([`StreamSource`]), or an offset-rebased view ([`Rebased`],
//! the maker-note primitive) without loading the file into memory.

use gamut_core::{Error, Result};

/// A random-access byte source: positioned exact reads plus a total length.
///
/// Offsets are absolute within the source, matching how TIFF stores them. The methods take
/// `&mut self` so a seekable stream can implement the trait; to share one source between
/// readers, pass `&mut source` (covered by the blanket `&mut S` impl).
pub trait ReadAt {
    /// Reads exactly `buf.len()` bytes starting at `offset`.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// range lies outside the source, or an [`ErrorKind::Io`](gamut_core::ErrorKind::Io) error
    /// if an underlying stream operation fails.
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// The total length of the source in bytes.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::Io`](gamut_core::ErrorKind::Io) error if the length cannot be
    /// determined from an underlying stream.
    fn len(&mut self) -> Result<u64>;

    /// Whether the source is empty.
    ///
    /// # Errors
    ///
    /// Propagates [`ReadAt::len`]'s errors.
    fn is_empty(&mut self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Adapts this source so logical offset `o` reads physical offset `base + o`.
    ///
    /// This is the offset-rebasing primitive for maker notes — vendor mini-IFDs whose internal
    /// offsets are relative to the maker-note start or the enclosing TIFF header — and for a
    /// TIFF stream embedded at an offset inside a larger container. Rebasing composes:
    /// `s.rebased(a).rebased(b)` reads like `s.rebased(a + b)`.
    fn rebased(self, base: u64) -> Rebased<Self>
    where
        Self: Sized,
    {
        Rebased::new(self, base)
    }
}

impl ReadAt for &[u8] {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let start = usize::try_from(offset)
            .ok()
            .filter(|&s| s <= <[u8]>::len(self))
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: read out of bounds")
                    .with_byte_offset(offset)
            })?;
        let bytes = start
            .checked_add(buf.len())
            .and_then(|end| self.get(start..end))
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: read out of bounds")
                    .with_byte_offset(offset)
            })?;
        buf.copy_from_slice(bytes);
        Ok(())
    }

    fn len(&mut self) -> Result<u64> {
        Ok(<[u8]>::len(self) as u64)
    }
}

impl<S: ReadAt + ?Sized> ReadAt for &mut S {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_exact_at(offset, buf)
    }

    fn len(&mut self) -> Result<u64> {
        (**self).len()
    }
}

/// Adapts any [`Read`](std::io::Read) + [`Seek`](std::io::Seek) stream (a [`std::fs::File`], a
/// [`Cursor`](std::io::Cursor), …) into a [`ReadAt`] source.
///
/// Each read seeks to the offset then reads exactly; the length is measured once (a seek to the
/// end) and cached. A short read at a validated offset means the stream ended early — that is
/// reported as the same [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error a
/// truncated slice produces, while any other stream failure surfaces as an
/// [`ErrorKind::Io`](gamut_core::ErrorKind::Io) error.
///
/// Note for `BufReader` users: every positioned read seeks, which discards the buffer — for the
/// directory-walk access pattern, pass the [`File`](std::fs::File) directly.
#[derive(Debug)]
pub struct StreamSource<R> {
    inner: R,
    len: Option<u64>,
}

impl<R: std::io::Read + std::io::Seek> StreamSource<R> {
    /// Wraps a seekable stream. No I/O happens until the first read or length query.
    #[must_use]
    pub fn new(inner: R) -> Self {
        Self { inner, len: None }
    }

    /// Unwraps the source, returning the inner stream (at an unspecified position).
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: std::io::Read + std::io::Seek> ReadAt for StreamSource<R> {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner
            .seek(std::io::SeekFrom::Start(offset))
            .map_err(|error| {
                Error::from(error)
                    .with_origin(env!("CARGO_PKG_NAME"))
                    .with_byte_offset(offset)
            })?;
        self.inner.read_exact(buf).map_err(|e| {
            // A stream that ends before `buf` is filled is a bounds violation (the offsets
            // promised more bytes than exist), not a transport failure.
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: read out of bounds")
                    .with_byte_offset(offset)
            } else {
                Error::from(e)
                    .with_origin(env!("CARGO_PKG_NAME"))
                    .with_byte_offset(offset)
            }
        })
    }

    fn len(&mut self) -> Result<u64> {
        if let Some(len) = self.len {
            return Ok(len);
        }
        let len = self
            .inner
            .seek(std::io::SeekFrom::End(0))
            .map_err(|error| Error::from(error).with_origin(env!("CARGO_PKG_NAME")))?;
        self.len = Some(len);
        Ok(len)
    }
}

/// A [`ReadAt`] view whose offsets are rebased: logical offset `o` reads `base + o` in the inner
/// source, and the length shrinks accordingly. See [`ReadAt::rebased`].
///
/// ```
/// use gamut_ifd::ReadAt;
///
/// let data: &[u8] = &[0xAA, 0xBB, 0xCC, 0xDD];
/// let mut view = data.rebased(2);
/// assert_eq!(view.len().unwrap(), 2);
/// let mut byte = [0u8; 1];
/// view.read_exact_at(0, &mut byte).unwrap();
/// assert_eq!(byte, [0xCC]);
/// ```
#[derive(Debug)]
pub struct Rebased<S> {
    inner: S,
    base: u64,
}

impl<S: ReadAt> Rebased<S> {
    /// Wraps `inner` so logical offset `o` reads physical offset `base + o`.
    #[must_use]
    pub fn new(inner: S, base: u64) -> Self {
        Self { inner, base }
    }

    /// Unwraps the view, returning the inner source.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: ReadAt> ReadAt for Rebased<S> {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let physical = self.base.checked_add(offset).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: rebased offset overflow")
        })?;
        self.inner.read_exact_at(physical, buf)
    }

    fn len(&mut self) -> Result<u64> {
        // A base past the end of the inner source is an empty view, not an error: the view is
        // only ever *read through*, and every read then fails its own bounds check.
        Ok(self.inner.len()?.saturating_sub(self.base))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_reads_exactly_and_bounds_checks() {
        let mut data: &[u8] = &[1, 2, 3, 4, 5];
        let mut buf = [0u8; 3];
        data.read_exact_at(1, &mut buf).expect("in bounds");
        assert_eq!(buf, [2, 3, 4]);
        // A zero-length read at the very end is in bounds; one byte past is not.
        data.read_exact_at(5, &mut []).expect("empty at end");
        assert!(data.read_exact_at(6, &mut []).is_err());
        assert!(data.read_exact_at(3, &mut buf).is_err()); // runs past the end
        assert!(data.read_exact_at(u64::MAX, &mut buf).is_err()); // offset overflows usize/len
        // UFCS: the inherent slice `len`/`is_empty` shadow the trait methods on `&[u8]`.
        assert_eq!(ReadAt::len(&mut data).expect("len"), 5);
        assert!(!ReadAt::is_empty(&mut data).expect("is_empty"));
        let mut empty: &[u8] = &[];
        assert!(ReadAt::is_empty(&mut empty).expect("is_empty"));
    }

    /// Drives a source through a generic bound, so `&mut data` exercises the blanket `&mut S`
    /// impl (a direct method call would reborrow down to the `&[u8]` impl instead).
    fn read_generic<S: ReadAt>(mut source: S, offset: u64) -> (u8, u64) {
        let mut byte = [0u8; 1];
        source.read_exact_at(offset, &mut byte).expect("read");
        (byte[0], source.len().expect("len"))
    }

    #[test]
    fn mut_ref_delegates() {
        let mut data: &[u8] = &[9, 8, 7];
        assert_eq!(read_generic(&mut data, 1), (8, 3));
    }

    #[test]
    fn stream_source_reads_positioned_in_both_directions() {
        let mut src = StreamSource::new(std::io::Cursor::new(vec![10, 20, 30, 40]));
        let mut buf = [0u8; 2];
        src.read_exact_at(2, &mut buf).expect("forward");
        assert_eq!(buf, [30, 40]);
        src.read_exact_at(0, &mut buf).expect("backward");
        assert_eq!(buf, [10, 20]);
        assert_eq!(src.len().expect("len"), 4);
        // The length is cached: it stays correct when queried again after reads moved the
        // stream position.
        src.read_exact_at(1, &mut buf).expect("reposition");
        assert_eq!(src.len().expect("cached len"), 4);
        assert_eq!(src.into_inner().into_inner(), vec![10, 20, 30, 40]);
    }

    #[test]
    fn stream_source_short_read_is_invalid_input() {
        let mut src = StreamSource::new(std::io::Cursor::new(vec![1, 2]));
        let mut buf = [0u8; 4];
        match src.read_exact_at(0, &mut buf) {
            Err(error) if error.kind() == gamut_core::ErrorKind::InvalidInput => {
                assert_eq!(error.static_message(), Some("TIFF: read out of bounds"));
                assert_eq!(error.byte_offset(), Some(0));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// A stream whose reads fail with a non-EOF error, proving transport failures surface as
    /// [`ErrorKind::Io`](gamut_core::ErrorKind::Io) rather than being branded malformed input.
    struct FailingStream;

    impl std::io::Read for FailingStream {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk on fire"))
        }
    }

    impl std::io::Seek for FailingStream {
        fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    #[test]
    fn stream_source_transport_failure_is_io() {
        let mut src = StreamSource::new(FailingStream);
        let mut buf = [0u8; 1];
        match src.read_exact_at(0, &mut buf) {
            Err(error) if error.kind() == gamut_core::ErrorKind::Io => {
                assert!(error.to_string().contains("disk on fire"));
                assert_eq!(error.byte_offset(), Some(0));
            }
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn rebased_maps_offsets_and_shrinks_len() {
        let data: &[u8] = &[0, 1, 2, 3, 4, 5];
        let mut view = data.rebased(4);
        assert_eq!(view.len().expect("len"), 2);
        let mut buf = [0u8; 2];
        view.read_exact_at(0, &mut buf).expect("read at base");
        assert_eq!(buf, [4, 5]);
        assert!(view.read_exact_at(2, &mut buf).is_err()); // past the inner end
        assert_eq!(view.into_inner(), data);
    }

    #[test]
    fn rebased_composes_additively() {
        let data: &[u8] = &[0, 1, 2, 3, 4, 5];
        let mut nested = data.rebased(2).rebased(3);
        assert_eq!(nested.len().expect("len"), 1);
        let mut byte = [0u8; 1];
        nested.read_exact_at(0, &mut byte).expect("read");
        assert_eq!(byte, [5]);
    }

    #[test]
    fn rebased_guards_base_arithmetic() {
        let data: &[u8] = &[1, 2, 3];
        // base + offset overflowing u64 is a typed error, not a wrap-around read.
        let mut view = data.rebased(u64::MAX);
        let mut byte = [0u8; 1];
        assert!(view.read_exact_at(1, &mut byte).is_err());
        // A base past the inner end yields an empty view whose reads all fail.
        let mut past = data.rebased(10);
        assert_eq!(past.len().expect("len"), 0);
        assert!(past.is_empty().expect("is_empty"));
        assert!(past.read_exact_at(0, &mut byte).is_err());
    }
}
