//! Low-level ISOBMFF box serialization ([`BoxBuilder`]) and parsing ([`BoxReader`]).
//!
//! All boxes here are byte-aligned big-endian, so these work on a plain byte buffer rather than a
//! bit-level writer. [`BoxBuilder::begin_box`] returns the position of the box's size field; the
//! matching [`BoxBuilder::end_box`] back-patches it once the body is written.
//! [`BoxBuilder::reserve_u32`] / [`BoxBuilder::patch_u32`] support the `iloc` `extent_offset`, which
//! can only be filled once the `mdat` payload position is known. [`BoxReader`] is the read dual: a
//! bounds-checked cursor whose [`BoxReader::next_box`] walks a box list, never trusting a length
//! from the stream without checking it against the remaining bytes.
//!
//! Visibility: [`BoxBuilder`] is an internal writer detail and stays crate-private. [`BoxReader`]
//! and [`RawBox`] are re-exported from the crate root as the box-walk primitive a byte-accounting
//! consumer (e.g. `gamut-heic`) uses to map every byte of a file to a box; their public surface is
//! deliberately the walk only — [`BoxReader::new`], [`BoxReader::next_box`],
//! [`BoxReader::position`], [`BoxReader::remaining`], and [`RawBox`]'s fields and
//! [`RawBox::payload`]. The scalar
//! big-endian field readers ([`BoxReader::u8`], `u16`, `u32`, `u64`, [`BoxReader::fourcc`],
//! [`BoxReader::take`]) stay crate-private: they decode box *bodies*, which is this crate's job,
//! not a consumer's.

use gamut_core::{Error, Result};

/// Append-only big-endian box writer.
pub(crate) struct BoxBuilder {
    buf: Vec<u8>,
}

impl BoxBuilder {
    /// Creates an empty builder.
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Opens a box of type `box_type`, writing a placeholder 32-bit size; returns the size field's
    /// position to pass to [`BoxBuilder::end_box`].
    pub(crate) fn begin_box(&mut self, box_type: &[u8; 4]) -> usize {
        let start = self.buf.len();
        self.buf.extend_from_slice(&[0, 0, 0, 0]);
        self.buf.extend_from_slice(box_type);
        start
    }

    /// Closes the box opened at `start`, back-patching its 32-bit size.
    pub(crate) fn end_box(&mut self, start: usize) {
        let size = (self.buf.len() - start) as u32;
        self.buf[start..start + 4].copy_from_slice(&size.to_be_bytes());
    }

    /// Writes a `FullBox` header (1-byte version, 3-byte flags).
    pub(crate) fn full_box(&mut self, version: u8, flags: u32) {
        self.buf.push(version);
        self.buf.extend_from_slice(&flags.to_be_bytes()[1..]);
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub(crate) fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn bytes(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Writes a 4-byte placeholder and returns its position for a later [`BoxBuilder::patch_u32`].
    pub(crate) fn reserve_u32(&mut self) -> usize {
        let pos = self.buf.len();
        self.buf.extend_from_slice(&[0, 0, 0, 0]);
        pos
    }

    /// Overwrites the 4 bytes at `pos` with `value` (big-endian).
    pub(crate) fn patch_u32(&mut self, pos: usize, value: u32) {
        self.buf[pos..pos + 4].copy_from_slice(&value.to_be_bytes());
    }

    /// Current length of the buffer (also the absolute file offset of the next byte written).
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    /// Consumes the builder, returning the serialized bytes.
    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// A bounds-checked big-endian cursor over a byte slice, used to walk a box list.
///
/// Every read is checked against the remaining bytes, so a truncated or malformed stream yields a
/// typed [`Error`] rather than a panic. It is the box-walk primitive re-exported for byte-accounting
/// consumers: [`new`](Self::new) opens a cursor over a slice, [`next_box`](Self::next_box) yields
/// each [`RawBox`] in turn (with its [`offset`](RawBox::offset) within that slice),
/// [`position`](Self::position) is the current cursor, and [`remaining`](Self::remaining) is the
/// unconsumed tail. To descend into a box, open a fresh reader over its [`body`](RawBox::body), or
/// over [`payload`](RawBox::payload) when a `uuid` box's user type is framing rather than payload.
///
/// ```
/// use gamut_isobmff::BoxReader;
/// // Two consecutive boxes: an empty 8-byte `free`, then a `skip` with a 2-byte body.
/// let data = [
///     0, 0, 0, 8, b'f', b'r', b'e', b'e', // free: size 8, empty body
///     0, 0, 0, 10, b's', b'k', b'i', b'p', 0xAA, 0xBB, // skip: size 10, body {AA, BB}
/// ];
/// let mut r = BoxReader::new(&data);
///
/// let free = r.next_box().unwrap().expect("first box");
/// assert_eq!(&free.ty, b"free");
/// assert_eq!(free.offset, 0); // header starts at the slice start
/// assert!(free.body.is_empty());
/// assert_eq!(r.position(), 8); // cursor now past the first box
///
/// let skip = r.next_box().unwrap().expect("second box");
/// assert_eq!(&skip.ty, b"skip");
/// assert_eq!(skip.offset, 8); // header starts right after `free`; body is at 8 + 8 = 16
/// assert_eq!(skip.body, &[0xAA, 0xBB]);
/// assert_eq!(r.position(), 18);
///
/// assert_eq!(r.remaining(), 0);
/// assert!(r.next_box().unwrap().is_none()); // clean end of the slice
/// ```
pub struct BoxReader<'a> {
    data: &'a [u8],
    pos: usize,
}

/// One box yielded by [`BoxReader::next_box`]: its four-character type, a borrow of its body, and
/// the absolute offset of its header within the slice the reader was created over.
pub struct RawBox<'a> {
    /// The box type (its four-character code).
    pub ty: [u8; 4],
    /// The box body — the bytes after the complete size/type header. For a `uuid` box this starts
    /// with the 16-byte user type; [`payload`](Self::payload) omits that prefix.
    pub body: &'a [u8],
    /// The 16-byte user type for a `uuid` box, or `None` for every other box type.
    pub user_type: Option<[u8; 16]>,
    /// Absolute offset of this box's header within the slice the [`BoxReader`] was created over
    /// (the size field). After yielding this box, [`BoxReader::position`] is its authoritative end
    /// offset, including a 64-bit size header when present.
    pub offset: usize,
}

impl RawBox<'_> {
    /// Returns the box payload, excluding the 16-byte user type prefix of a `uuid` box.
    ///
    /// For every other box type this is identical to [`body`](Self::body).
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        if self.user_type.is_some() {
            self.body.get(16..).unwrap_or_default()
        } else {
            self.body
        }
    }
}

impl<'a> BoxReader<'a> {
    /// Creates a cursor at the start of `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The current cursor position: the number of bytes consumed from the start of the slice, i.e.
    /// the absolute offset at which the next [`next_box`](Self::next_box) would read a header.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not yet consumed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Consumes `n` bytes, returning them, or an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if fewer remain.
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let offset = self.pos as u64;
        let end = self.pos.checked_add(n).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: length overflow")
                .with_byte_offset(offset)
        })?;
        let slice = self.data.get(self.pos..end).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: unexpected end of box")
                .with_byte_offset(offset)
        })?;
        self.pos = end;
        Ok(slice)
    }

    /// Reads one byte.
    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Reads a big-endian `u16`.
    pub(crate) fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    /// Reads a big-endian `u32`.
    pub(crate) fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads a big-endian `u64`.
    pub(crate) fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Reads a four-character code.
    pub(crate) fn fourcc(&mut self) -> Result<[u8; 4]> {
        let b = self.take(4)?;
        Ok([b[0], b[1], b[2], b[3]])
    }

    /// Reads the next box header and body, advancing past it. Returns `Ok(None)` at a clean end of
    /// the slice.
    ///
    /// The returned [`RawBox::offset`] records where this box's header began within the slice.
    ///
    /// A 32-bit `size` of 1 selects the following 64-bit `largesize`; a size of 0 extends the box
    /// through the end of this reader's slice. The writer continues to emit only 32-bit sizes.
    ///
    /// # Errors
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// header or body is truncated, the declared size is smaller than its header, a size cannot fit the platform address space, or a `uuid` body does
    /// not contain its complete 16-byte user type.
    pub fn next_box(&mut self) -> Result<Option<RawBox<'a>>> {
        if self.remaining() == 0 {
            return Ok(None);
        }
        let offset = self.pos;
        let size = self.u32()?;
        let ty = self.fourcc()?;
        let (size, header_size) = match size {
            0 => (self.data.len() - offset, 8),
            1 => {
                let large_size = usize::try_from(self.u64()?).map_err(|_| {
                    Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "ISOBMFF: box size exceeds address space",
                    )
                    .with_byte_offset(offset as u64)
                })?;
                (large_size, 16)
            }
            size => (
                usize::try_from(size).map_err(|_| {
                    Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "ISOBMFF: box size exceeds address space",
                    )
                    .with_byte_offset(offset as u64)
                })?,
                8,
            ),
        };
        if size < header_size {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: box size smaller than header",
            )
            .with_byte_offset(offset as u64));
        }
        let body = self.take(size - header_size)?;
        let user_type = if &ty == b"uuid" {
            if body.len() < 16 {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "ISOBMFF: truncated uuid user type",
                )
                .with_byte_offset(offset as u64));
            }
            let mut user_type = [0; 16];
            user_type.copy_from_slice(&body[..16]);
            Some(user_type)
        } else {
            None
        };
        Ok(Some(RawBox {
            ty,
            body,
            user_type,
            offset,
        }))
    }
}
