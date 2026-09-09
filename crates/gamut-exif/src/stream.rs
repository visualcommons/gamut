//! Reading EXIF from a positioned byte source rather than a slice.
//!
//! EXIF is usually small, but the file it is embedded in need not be: a raw `.NEF`/`.CR3` can be
//! hundreds of megabytes whose EXIF is a few kilobytes near the front. [`gamut_ifd`]'s
//! [`IfdReader`] already reads a TIFF stream through the positioned [`ReadAt`] trait, fetching only
//! the directory bodies and the values they reference, so this module lifts
//! [`ExifReader`](crate::ExifReader) onto the same source type: open a file, hand the reader a
//! [`gamut_ifd::StreamSource`], and pull the EXIF out without loading the image.
//!
//! This is the crate's **one** parse engine — [`ExifReader::parse`](crate::ExifReader::parse) is
//! the `&[u8]` case of it (`&[u8]` implements [`ReadAt`]), so the slice and streaming paths cannot
//! drift. It is deliberately synchronous: an async caller drives a [`ReadAt`] source itself, which
//! keeps a runtime dependency out of a crate that has none.

use gamut_ifd::{Ifd, IfdReader, RawIfd, ReadAt, tags as ifd_tags};

use crate::error::{ExifError, Result};
use crate::exif::{EXIF_IFD_POINTER, Exif, GPS_IFD_POINTER, INTEROP_IFD_POINTER, MARKER};
use crate::reader::ExifReader;
use crate::tag::ExifTag;
use crate::thumbnail::Thumbnail;

impl ExifReader {
    /// Parses EXIF from a positioned byte source, reading only the parts it needs.
    ///
    /// The streaming twin of [`parse`](Self::parse), which is this method over a `&[u8]`. `source`
    /// is taken by value; `&mut S` and `&mut dyn ReadAt` both implement [`ReadAt`], so a caller
    /// that must keep its source can pass a borrow, and a caller that needs dynamic dispatch can
    /// erase the type.
    ///
    /// ```no_run
    /// use std::fs::File;
    ///
    /// use gamut_exif::ExifReader;
    /// use gamut_ifd::StreamSource;
    ///
    /// // The EXIF of a large raw file, without reading the image.
    /// let mut file = File::open("capture.dng")?;
    /// let exif = ExifReader::new().parse_from(StreamSource::new(&mut file))?;
    /// println!("{:?}", exif.make());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ExifError::MissingMarker`] when the marker is required but absent, an
    /// [`ExifError::Ifd`] when the TIFF stream is malformed or the source fails, or (in
    /// [`strict`](Self::strict) mode) [`ExifError::InvalidIfd`] /
    /// [`ExifError::BadThumbnail`] when a sub-IFD pointer or thumbnail range is unusable.
    pub fn parse_from<S: ReadAt>(&self, mut source: S) -> Result<Exif> {
        let base = self.tiff_base(&mut source)?;
        // Everything below addresses the TIFF stream, so offsets read out of it — and the offsets
        // this crate hands back — stay in EXIF's own frame of reference.
        let mut reader = IfdReader::open(source.rebased(base))?;
        let order = reader.order();

        let file = reader.read_file()?;
        let mut ifds = file.ifds.into_iter();
        let mut image = ifds.next().ok_or(ExifError::Truncated)?;
        // The next-IFD chain's second entry is the thumbnail directory (1st IFD), if any.
        let thumbnail = match ifds.next() {
            Some(ifd) => Some(self.read_thumbnail(ifd, &mut reader)?),
            None => None,
        };

        // The Exif sub-IFD's own offset, captured before `follow` strips the pointer: the
        // maker-note pin needs the note value's absolute source position.
        let exif_ifd_at = image.get_u32(EXIF_IFD_POINTER).map(u64::from);
        let exif = self.follow(&mut image, &mut reader, EXIF_IFD_POINTER, "Exif")?;
        let gps = self.follow(&mut image, &mut reader, GPS_IFD_POINTER, "GPS")?;
        let maker_note_at = match (&exif, exif_ifd_at) {
            (Some(_), Some(at)) => maker_note_offset(&mut reader, at),
            _ => None,
        };

        // The Interoperability directory is reached from *inside* the Exif sub-IFD, not the 0th IFD.
        let (exif, interop) = match exif {
            Some(mut e) => {
                let interop = self.follow(&mut e, &mut reader, INTEROP_IFD_POINTER, "Interop")?;
                (Some(e), interop)
            }
            None => (None, None),
        };

        Ok(Exif::from_parts(
            order,
            image,
            exif,
            gps,
            interop,
            thumbnail,
            maker_note_at,
        ))
    }

    /// The offset at which the TIFF stream starts in `source`: past the `Exif\0\0` marker when it
    /// is there, 0 when it is not.
    ///
    /// A source too short to hold the marker is treated as unmarked — the TIFF header that follows
    /// is longer than the marker, so such a source cannot parse either way.
    fn tiff_base<S: ReadAt>(&self, source: &mut S) -> Result<u64> {
        let mut head = [0u8; MARKER.len()];
        let marked = source.read_exact_at(0, &mut head).is_ok() && head.as_slice() == MARKER;
        if marked {
            Ok(MARKER.len() as u64)
        } else if self.require_marker {
            Err(ExifError::MissingMarker)
        } else {
            Ok(0)
        }
    }

    /// Reads pointer tag `ptr` from `parent`, removes it (the pointer is represented structurally,
    /// not as a data field), and parses the sub-IFD it addresses.
    ///
    /// Returns `Ok(None)` when the pointer is absent, or — in lenient mode — when the pointed-at
    /// directory is malformed.
    fn follow<S: ReadAt>(
        &self,
        parent: &mut Ifd,
        reader: &mut IfdReader<S>,
        ptr: u16,
        name: &'static str,
    ) -> Result<Option<Ifd>> {
        let Some(offset) = parent.get_u32(ptr) else {
            return Ok(None);
        };
        parent.remove(ptr);
        let followed = match reader.read_ifd(u64::from(offset)) {
            Ok(raw) => reader.decode_ifd(&raw),
            Err(e) => Err(e),
        };
        match followed {
            Ok(ifd) => Ok(Some(ifd)),
            Err(_) if !self.strict => Ok(None),
            Err(_) => Err(ExifError::InvalidIfd(name)),
        }
    }

    /// Builds a [`Thumbnail`] from the 1st IFD, fetching its JPEG bytes (from the
    /// `JPEGInterchangeFormat` offset / length) when the range is wholly inside the stream. In
    /// lenient mode an out-of-bounds range yields a thumbnail without bytes; in strict mode it
    /// errors.
    fn read_thumbnail<S: ReadAt>(&self, ifd: Ifd, reader: &mut IfdReader<S>) -> Result<Thumbnail> {
        let ptr = ExifTag::JpegInterchangeFormat.tag_id();
        let offset = ifd.get_u32(ptr);
        let length = ifd.get_u32(ExifTag::JpegInterchangeFormatLength.tag_id());
        let jpeg = match (offset, length) {
            (Some(offset), Some(length)) => match read_range(reader, offset, length)? {
                Some(bytes) => Some(bytes),
                None if self.strict => {
                    return Err(ExifError::BadThumbnail("JPEG offset out of bounds"));
                }
                None => None,
            },
            _ => None,
        };
        // The JPEGInterchangeFormat offset is structural — the bytes are captured above and the
        // writer re-synthesises the offset — so drop it from the stored directory (mirroring how the
        // sub-IFD pointer tags are stripped), leaving a value the model can't carry stale.
        let mut ifd = ifd;
        if jpeg.is_some() {
            ifd.remove(ptr);
        }
        Ok(Thumbnail::from_parts(ifd, jpeg))
    }
}

/// Fetches `length` bytes at `offset`, or `None` when that range is not wholly inside the stream.
///
/// The bound is checked against the source's length *before* anything is allocated, so a hostile
/// `JPEGInterchangeFormatLength` cannot make the reader reserve more than the stream can hold.
fn read_range<S: ReadAt>(
    reader: &mut IfdReader<S>,
    offset: u32,
    length: u32,
) -> Result<Option<Vec<u8>>> {
    // Widened to 64 bits first: the sum of two `u32`s cannot overflow a `u64`.
    let end = u64::from(offset) + u64::from(length);
    if end > reader.source_mut().len()? {
        return Ok(None);
    }
    let mut buf = vec![0u8; length as usize];
    reader
        .source_mut()
        .read_exact_at(u64::from(offset), &mut buf)?;
    Ok(Some(buf))
}

/// The absolute offset of the Exif sub-IFD's out-of-line `MakerNote` value in the TIFF stream, or
/// `None` if the note is absent or inline.
fn maker_note_offset<S: ReadAt>(reader: &mut IfdReader<S>, exif_ifd_at: u64) -> Option<u64> {
    let raw: RawIfd = reader.read_ifd(exif_ifd_at).ok()?;
    let entry = raw.entry(ifd_tags::MAKER_NOTE)?;
    reader.value_offset(entry)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use gamut_ifd::{ByteOrder, StreamSource, TiffFile, Value, Variant, write};

    use super::*;

    /// A minimal marked EXIF blob whose 0th IFD carries `Make`.
    fn blob() -> Vec<u8> {
        let mut image = Ifd::new();
        image.set(0x010F, Value::Ascii("Canon".into()));
        let tiff = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image],
        })
        .expect("write");
        let mut out = MARKER.to_vec();
        out.extend(tiff);
        out
    }

    /// The streaming entry point reads a `Read + Seek` source that is not a slice at all — the
    /// capability the slice-only API could not offer.
    #[test]
    fn parse_from_reads_a_seekable_stream() {
        let source = StreamSource::new(Cursor::new(blob()));
        let exif = ExifReader::new().parse_from(source).expect("parse_from");
        assert_eq!(exif.make(), Some("Canon"));
    }

    /// The marker is detected through the source, not by slicing: the same `require_marker`
    /// contract `parse` has must hold for a stream, or the two entry points disagree.
    #[test]
    fn the_marker_is_detected_through_the_source() {
        let marked = blob();
        let bare = marked[MARKER.len()..].to_vec();

        assert!(
            ExifReader::new()
                .require_marker(true)
                .parse_from(StreamSource::new(Cursor::new(marked)))
                .is_ok()
        );
        let err = ExifReader::new()
            .require_marker(true)
            .parse_from(StreamSource::new(Cursor::new(bare.clone())))
            .expect_err("a bare TIFF stream must be rejected");
        assert!(matches!(err, ExifError::MissingMarker), "{err:?}");
        // ...and without the requirement the same bare stream parses.
        assert!(
            ExifReader::new()
                .parse_from(StreamSource::new(Cursor::new(bare)))
                .is_ok()
        );
    }

    /// A source shorter than the 6-byte marker cannot be read for one; it is unmarked, and
    /// `require_marker` says so rather than reporting a torn read.
    #[test]
    fn a_source_too_short_for_the_marker_is_unmarked() {
        let err = ExifReader::new()
            .require_marker(true)
            .parse_from(&b"Exi"[..])
            .expect_err("three bytes cannot carry the marker");
        assert!(matches!(err, ExifError::MissingMarker), "{err:?}");
    }

    /// A range is fetched only when it ends inside the stream — the bound that stops a hostile
    /// length from being allocated. The boundary case (a range ending exactly at the end) is the
    /// one an off-by-one would get wrong.
    #[test]
    fn a_range_is_fetched_only_when_it_ends_inside_the_stream() {
        let data = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut reader =
            IfdReader::with_layout(&data[..], ByteOrder::LittleEndian, Variant::Classic);
        assert_eq!(
            read_range(&mut reader, 4, 4).expect("read"),
            Some(vec![5, 6, 7, 8]),
            "a range ending exactly at the end is inside"
        );
        assert_eq!(
            read_range(&mut reader, 4, 5).expect("read"),
            None,
            "one byte past the end is not"
        );
        assert_eq!(
            read_range(&mut reader, 0, u32::MAX).expect("read"),
            None,
            "a hostile length is refused before it is allocated"
        );
    }
}
