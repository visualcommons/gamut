//! The embedded thumbnail — the 1st IFD and the image bytes it addresses.
//!
//! Virtually all EXIF thumbnails are JPEG (`Compression` = 6, with `JPEGInterchangeFormat` /
//! `JPEGInterchangeFormatLength` pointing at the byte range). The rest are uncompressed
//! (`Compression` = 1), their pixels held in strips addressed by the `StripOffsets` /
//! `StripByteCounts` arrays (Exif 3.0 §4.6.9.2 Table 21's uncompressed columns). Both are
//! extracted on read and re-embedded on write, the writer re-synthesising each offset; the model
//! never stores an offset, since it is an address into one particular layout of the blob.

use gamut_ifd::{Ifd, Value};

use crate::tag::ExifTag;

/// The image's embedded thumbnail: the 1st IFD's directory plus the image bytes it addresses — a
/// JPEG thumbnail's compressed bytes, an uncompressed thumbnail's strips, or neither.
#[derive(Debug, Clone, PartialEq)]
pub struct Thumbnail {
    ifd: Ifd,
    jpeg: Option<Vec<u8>>,
    strips: Option<Vec<Vec<u8>>>,
}

impl Thumbnail {
    /// Assembles a thumbnail from an already-parsed 1st IFD and the bytes extracted from it: the
    /// JPEG range and the strips, each `None` when the directory addresses none (or none readable).
    pub(crate) fn from_parts(
        ifd: Ifd,
        jpeg: Option<Vec<u8>>,
        strips: Option<Vec<Vec<u8>>>,
    ) -> Self {
        Self { ifd, jpeg, strips }
    }

    /// Builds a JPEG thumbnail from its compressed bytes, synthesising the 1st IFD tags
    /// (`Compression` = 6 and `JPEGInterchangeFormatLength`).
    ///
    /// The `JPEGInterchangeFormat` offset is *not* stored — it is structural (the bytes are held
    /// separately) and the writer synthesises it, so the model never carries a value that changes on
    /// every re-layout.
    #[must_use]
    pub fn from_jpeg(jpeg: Vec<u8>) -> Self {
        let mut ifd = Ifd::new();
        ifd.set(ExifTag::Compression.tag_id(), Value::Short(vec![6]));
        ifd.set(
            ExifTag::JpegInterchangeFormatLength.tag_id(),
            Value::Long(vec![jpeg.len() as u32]),
        );
        Self {
            ifd,
            jpeg: Some(jpeg),
            strips: None,
        }
    }

    /// The thumbnail's directory (the 1st IFD).
    #[must_use]
    pub fn ifd(&self) -> &Ifd {
        &self.ifd
    }

    /// The thumbnail's directory, mutably (used when vivifying via [`Exif`](crate::Exif) accessors).
    pub(crate) fn ifd_mut(&mut self) -> &mut Ifd {
        &mut self.ifd
    }

    /// The compressed JPEG bytes, for a JPEG thumbnail; `None` for an uncompressed one.
    #[must_use]
    pub fn jpeg(&self) -> Option<&[u8]> {
        self.jpeg.as_deref()
    }

    /// The image strips of an uncompressed thumbnail, in `StripOffsets` order, each exactly the
    /// bytes its `StripByteCounts` entry sized; `None` when the 1st IFD addresses no strips, or
    /// when any strip range could not be read (the directory then keeps its source `StripOffsets`).
    #[must_use]
    pub fn strips(&self) -> Option<&[Vec<u8>]> {
        self.strips.as_deref()
    }

    /// The thumbnail's `Compression` tag (6 = JPEG, 1 = uncompressed), if present.
    #[must_use]
    pub fn compression(&self) -> Option<u16> {
        self.ifd
            .get_u32(ExifTag::Compression.tag_id())
            .and_then(|v| u16::try_from(v).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_jpeg_sets_the_1st_ifd_tags() {
        let thumb = Thumbnail::from_jpeg(vec![0xFF, 0xD8, 0xFF, 0xD9]);
        assert_eq!(thumb.jpeg(), Some(&[0xFF, 0xD8, 0xFF, 0xD9][..]));
        assert_eq!(thumb.compression(), Some(6));
        assert_eq!(
            thumb
                .ifd()
                .get_u32(ExifTag::JpegInterchangeFormatLength.tag_id()),
            Some(4)
        );
    }

    #[test]
    fn ifd_mut_edits_the_thumbnail_directory_in_place() {
        // Same gap as `Exif::image_mut`: nothing observed an edit made through the mutable
        // accessor, so handing back a detached directory passed every test (#110).
        let mut thumb = Thumbnail::from_jpeg(vec![0xFF, 0xD8, 0xFF, 0xD9]);
        thumb
            .ifd_mut()
            .set(ExifTag::Orientation.tag_id(), Value::Short(vec![3]));

        assert_eq!(
            thumb.ifd().get(ExifTag::Orientation.tag_id()),
            Some(&Value::Short(vec![3])),
            "the edit landed in the directory this Thumbnail owns"
        );
    }

    #[test]
    fn uncompressed_thumbnail_has_no_jpeg() {
        let mut ifd = Ifd::new();
        ifd.set(ExifTag::Compression.tag_id(), Value::Short(vec![1]));
        let strips = vec![vec![1, 2, 3], vec![4]];
        let thumb = Thumbnail::from_parts(ifd, None, Some(strips.clone()));
        assert_eq!(thumb.jpeg(), None);
        assert_eq!(thumb.strips(), Some(strips.as_slice()));
        assert_eq!(thumb.compression(), Some(1));
    }
}
