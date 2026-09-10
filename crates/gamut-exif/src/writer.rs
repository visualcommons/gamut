//! Writing an [`Exif`] back to a valid EXIF blob.
//!
//! The writer hands the whole IFD tree to [`gamut_ifd::write`], whose two-pass offset layout does
//! the hard part: it places every directory, appends one value pool, and synthesises each sub-IFD
//! pointer field with the child's patched offset. This crate's job is only to shape the tree —
//! attach the Exif/GPS/Interop sub-IFDs under their pointer tags and chain the thumbnail as the 1st
//! IFD — so the round-trip `parse → write → parse` reproduces the directories with the source byte
//! order preserved.
//!
//! This module also holds the crate's one **conformance check**, [`set_tag_checked`]: the write
//! path is the only place gamut is the author of the bytes, so it is the only place a spec
//! violation is gamut's to refuse. It is one door among several, and the only checked one — every
//! other way to put a field into the tree stays lenient, because a caller reproducing a
//! non-conformant source file must be able to. Those are
//! [`Exif::set_tag`](crate::Exif::set_tag), [`Exif::set`](crate::Exif::set),
//! [`Exif::exif_ifd_mut`](crate::Exif::exif_ifd_mut),
//! [`Exif::gps_ifd_mut`](crate::Exif::gps_ifd_mut),
//! [`Exif::interop_ifd_mut`](crate::Exif::interop_ifd_mut) and
//! [`Exif::set_gps_ifd`](crate::Exif::set_gps_ifd), together with the `image_mut`,
//! `set_exif_ifd` and `set_interop_ifd` siblings that reach the same directories. Reading stays
//! lenient for the same reason — real files break the spec routinely and the crate's job there is
//! to surface what is present, not to judge it.

use gamut_ifd::{
    ByteOrder, FieldType, Ifd, TiffFile, Value, Variant, WriteOptions, align_word,
    tags as ifd_tags, write, write_with,
};

use crate::error::Result;
use crate::exif::{EXIF_IFD_POINTER, Exif, GPS_IFD_POINTER, INTEROP_IFD_POINTER, MARKER};
use crate::tag::{ExifTag, TagCount};
use crate::thumbnail::Thumbnail;

/// Why [`set_tag_checked`] refused a value.
///
/// Deliberately a type of its own rather than a variant of [`ExifError`](crate::ExifError), which
/// reports what is wrong with *data being read*: this reports what is wrong with a value a caller
/// asked gamut to write, so a caller that never writes never has to match it. That is the same
/// split `gps::GpsConversionError` makes. Marked `#[non_exhaustive]`
/// so further constraints can be checked without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TagConstraintError {
    /// The value's field type is not one CIPA DC-008 permits for the tag.
    #[error("{tag}: CIPA DC-008 requires {expected}, not {actual}")]
    FieldType {
        /// The tag's canonical name.
        tag: &'static str,
        /// The permitted type(s), as the spec writes them (e.g. `"SHORT or LONG"`).
        expected: String,
        /// The type the offered value would have been written as.
        actual: String,
    },
    /// The value has a component count CIPA DC-008 does not allow for the tag.
    #[error("{tag}: CIPA DC-008 requires a count of {expected}, not {actual}")]
    Count {
        /// The tag's canonical name.
        tag: &'static str,
        /// The permitted count(s), as the spec's `Count` column writes them.
        expected: TagCount,
        /// The component count of the offered value.
        actual: u64,
    },
}

/// The spec's own spelling of an on-disk field-type code, so the error message can be checked
/// against CIPA DC-008 without a lookup table.
///
/// Takes the code rather than a [`FieldType`] deliberately: `FieldType` grows three variants when
/// another workspace crate turns on `gamut-ifd`'s `bigtiff` feature, which Cargo unifies into this
/// build, so an exhaustive match over the *variants* would compile in one configuration and not
/// the other. A code is also what a [`Value::Unknown`] has to offer.
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

/// Checks `value` against the field type and component count CIPA DC-008 mandates for `tag`.
///
/// This is the **write-side** conformance check, deliberately absent from the read path: a parsed
/// file is reported as it is, however non-conformant. A tag CIPA DC-008 does not itself define
/// (empty [`ExifTag::field_types`]) constrains nothing and always passes — see the
/// [tag module docs](crate::tag).
///
/// # Errors
///
/// Returns [`TagConstraintError::FieldType`] if the value would be written with a type the spec
/// does not list for the tag, or [`TagConstraintError::Count`] if its component count is not one
/// the spec allows. For `ASCII` and `UTF8` the component count is the byte count *including* the
/// terminating NUL, matching [`gamut_ifd::Value::count`].
pub fn check_tag(tag: ExifTag, value: &Value) -> core::result::Result<(), TagConstraintError> {
    let types = tag.field_types();
    if !types.is_empty() && !types.iter().any(|&t| Some(t) == value.field_type()) {
        return Err(TagConstraintError::FieldType {
            tag: tag.name(),
            expected: spec_type_list(types),
            actual: spec_type_name(value.type_code()),
        });
    }

    let count = tag.component_count();
    if !count.allows(value.count()) {
        return Err(TagConstraintError::Count {
            tag: tag.name(),
            expected: count,
            actual: value.count(),
        });
    }
    Ok(())
}

/// Sets `tag` on `exif` only if `value` conforms to CIPA DC-008's field type and component count
/// for that tag.
///
/// The conformant alternative to [`Exif::set_tag`](crate::Exif::set_tag), which writes whatever it
/// is given — deliberately, since a caller reproducing a non-conformant source file must be able
/// to. Reach for this one when gamut is authoring the metadata.
///
/// ```
/// use gamut_exif::{ByteOrder, Exif, ExifTag, Value, set_tag_checked};
///
/// let mut exif = Exif::new(ByteOrder::LittleEndian);
/// // FNumber is RATIONAL with one component.
/// assert!(set_tag_checked(&mut exif, ExifTag::FNumber, Value::Rational(vec![(28, 10)])).is_ok());
///
/// let wrong = set_tag_checked(&mut exif, ExifTag::FNumber, Value::Short(vec![28]));
/// assert_eq!(
///     wrong.unwrap_err().to_string(),
///     "FNumber: CIPA DC-008 requires RATIONAL, not SHORT",
/// );
/// // The rejected write left the conformant value in place.
/// assert_eq!(exif.get_tag(ExifTag::FNumber), Some(&Value::Rational(vec![(28, 10)])));
/// ```
///
/// # Where this refuses what a table literally says
///
/// `GainControl` (0xA407) is the one tag CIPA DC-008 describes twice and differently: Table 9's
/// `Type` column says RATIONAL, while the tag's own §4.6.6.7.41 says SHORT and enumerates five
/// integer codes. gamut follows the section — a fraction cannot carry an enumeration, and exiv2
/// and ExifTool read it as SHORT — so this function rejects a RATIONAL `GainControl` even though
/// Table 9 permits one. A caller reproducing a Table-9-conformant file writes it through
/// [`Exif::set_tag`](crate::Exif::set_tag), which is unchecked by design.
///
/// # Errors
///
/// Returns the [`TagConstraintError`] from [`check_tag`]; `exif` is then left untouched.
pub fn set_tag_checked(
    exif: &mut Exif,
    tag: ExifTag,
    value: Value,
) -> core::result::Result<(), TagConstraintError> {
    check_tag(tag, &value)?;
    exif.set_tag(tag, value);
    Ok(())
}

/// Serialises an [`Exif`] back to an EXIF blob, with options for the marker and byte order.
///
/// By default it emits the `Exif\0\0` marker (as a JPEG `APP1` segment needs) and preserves the
/// [`Exif`]'s byte order.
#[derive(Debug, Clone)]
pub struct ExifWriter {
    marker: bool,
    byte_order: Option<ByteOrder>,
}

impl Default for ExifWriter {
    fn default() -> Self {
        Self {
            marker: true,
            byte_order: None,
        }
    }
}

impl ExifWriter {
    /// A writer with default options (emit the marker; keep the source byte order).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether to prefix the output with the `Exif\0\0` marker.
    ///
    /// Pass `false` for a bare TIFF stream — what the PNG `eXIf` and WebP `EXIF` chunks carry.
    #[must_use]
    pub fn marker(mut self, yes: bool) -> Self {
        self.marker = yes;
        self
    }

    /// Overrides the byte order the stream is written in (default: the [`Exif`]'s own order).
    #[must_use]
    pub fn byte_order(mut self, order: ByteOrder) -> Self {
        self.byte_order = Some(order);
        self
    }

    /// Serialises `exif` to bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ExifError::Ifd`](crate::ExifError::Ifd) if the model is not representable in
    /// classic-TIFF widths (a directory of more than `u16::MAX` entries, or a stream past the
    /// 4 GiB offset limit) — far beyond what any EXIF carrier accepts.
    pub fn write(&self, exif: &Exif) -> Result<Vec<u8>> {
        let order = self.byte_order.unwrap_or_else(|| exif.byte_order());
        let image = build_image(exif);
        // Pin the maker note at its source offset when the model still carries one there —
        // vendor notes encode absolute internal offsets, so keeping the byte range in place
        // keeps them valid.
        let pin = exif.maker_note_offset().filter(|_| {
            exif.exif_ifd()
                .is_some_and(|e| e.get(ifd_tags::MAKER_NOTE).is_some())
        });
        let tiff = write_with_thumbnail(order, image, exif.thumbnail(), pin)?;

        Ok(if self.marker {
            let mut out = MARKER.to_vec();
            out.extend(tiff);
            out
        } else {
            tiff
        })
    }
}

/// Rebuilds the 0th IFD: drops any hand-set pointer tags and re-attaches the Exif/GPS sub-IFDs (the
/// Exif sub-IFD itself nesting the Interop sub-IFD) so [`gamut_ifd::write`] synthesises correct
/// pointer offsets. An Interop directory implies an Exif directory to hold its pointer.
fn build_image(exif: &Exif) -> Ifd {
    let exif_sub = if exif.exif_ifd().is_some() || exif.interop_ifd().is_some() {
        let mut e = exif.exif_ifd().cloned().unwrap_or_default();
        e.remove(INTEROP_IFD_POINTER);
        if let Some(interop) = exif.interop_ifd() {
            e.set_sub_ifd(INTEROP_IFD_POINTER, vec![interop.clone()]);
        }
        Some(e)
    } else {
        None
    };

    let mut image = exif.image().clone();
    image.remove(EXIF_IFD_POINTER);
    image.remove(GPS_IFD_POINTER);
    if let Some(e) = exif_sub {
        image.set_sub_ifd(EXIF_IFD_POINTER, vec![e]);
    }
    if let Some(gps) = exif.gps_ifd() {
        image.set_sub_ifd(GPS_IFD_POINTER, vec![gps.clone()]);
    }
    image
}

/// Serialises the TIFF stream, chaining the thumbnail as the 1st IFD.
///
/// A JPEG thumbnail is re-embedded with a deterministic double-write: lay out the directories with a
/// placeholder `JPEGInterchangeFormat` offset to learn where the bytes will land, patch the offset
/// (an inline `LONG`, so the layout and total length are unchanged), then append the JPEG. An
/// uncompressed strip thumbnail's directory is preserved but its pixel bytes are **not** re-embedded
/// (a documented v1 limitation).
fn write_with_thumbnail(
    order: ByteOrder,
    image: Ifd,
    thumbnail: Option<&Thumbnail>,
    pin: Option<u64>,
) -> Result<Vec<u8>> {
    let Some(thumb) = thumbnail else {
        return write_tree(order, vec![image], pin);
    };
    let Some(jpeg) = thumb.jpeg() else {
        return write_tree(order, vec![image, thumb.ifd().clone()], pin);
    };

    let mut thumb_ifd = thumb.ifd().clone();
    thumb_ifd.set(
        ExifTag::JpegInterchangeFormatLength.tag_id(),
        Value::Long(vec![jpeg.len() as u32]),
    );
    thumb_ifd.set(
        ExifTag::JpegInterchangeFormat.tag_id(),
        Value::Long(vec![0]),
    );

    // Pass 1: lay out the directories to learn where the JPEG will start (word-aligned).
    let planned = write_tree(order, vec![image.clone(), thumb_ifd.clone()], pin)?;
    let jpeg_offset = align_word(planned.len() as u64) as usize;

    // Pass 2: patch the now-known offset. Changing an inline LONG moves nothing, so the byte length
    // is identical to pass 1 and `jpeg_offset` still points just past the directories.
    thumb_ifd.set(
        ExifTag::JpegInterchangeFormat.tag_id(),
        Value::Long(vec![jpeg_offset as u32]),
    );
    let mut bytes = write_tree(order, vec![image, thumb_ifd], pin)?;
    debug_assert_eq!(jpeg_offset, align_word(bytes.len() as u64) as usize);
    bytes.resize(jpeg_offset, 0);
    bytes.extend_from_slice(jpeg);
    Ok(bytes)
}

/// Serialises the tree, pinning the maker note at `pin` when possible: a pin the new layout
/// cannot satisfy (the offset now collides with the directory region) falls back to an ordinary
/// relocating write — the note's *bytes* still round-trip exactly.
fn write_tree(order: ByteOrder, ifds: Vec<Ifd>, pin: Option<u64>) -> Result<Vec<u8>> {
    let file = tiff_file(order, ifds);
    if let Some(at) = pin
        && let Ok((bytes, _map)) = write_with(
            &file,
            &WriteOptions::default().pin(ifd_tags::MAKER_NOTE, at),
        )
    {
        return Ok(bytes);
    }
    Ok(write(&file)?)
}

/// A classic-TIFF [`TiffFile`] in `order`.
fn tiff_file(order: ByteOrder, ifds: Vec<Ifd>) -> TiffFile {
    TiffFile {
        order,
        variant: Variant::Classic,
        ifds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExifTag, IfdKind, Value};

    /// A model with tags spread across the 0th IFD, Exif, GPS and Interop sub-IFDs, plus a
    /// thumbnail directory — so the round-trip exercises the whole pointer tree.
    fn sample(order: ByteOrder) -> Exif {
        let mut exif = Exif::new(order);
        exif.set_tag(ExifTag::Make, Value::Ascii("Fujifilm".into()));
        exif.set_tag(ExifTag::Model, Value::Ascii("X-T5".into()));
        exif.set_tag(ExifTag::Orientation, Value::Short(vec![1]));
        exif.set_tag(ExifTag::FNumber, Value::Rational(vec![(20, 10)]));
        exif.set_tag(ExifTag::ExposureTime, Value::Rational(vec![(1, 250)]));
        exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![160]));
        exif.set_tag(ExifTag::ExifVersion, Value::Undefined(b"0300".to_vec()));
        // Exif 3.0 UTF-8 text must survive the round-trip.
        exif.set_tag(ExifTag::LensModel, Value::Utf8("XF16-80mm ƒ4".into()));
        exif.set_tag(ExifTag::GpsVersionId, Value::Byte(vec![2, 3, 0, 0]));
        exif.set_tag(ExifTag::GpsLatitudeRef, Value::Ascii("N".into()));
        exif.set_tag(
            ExifTag::GpsLatitude,
            Value::Rational(vec![(48, 1), (51, 1), (0, 1)]),
        );
        exif.set_tag(ExifTag::InteroperabilityIndex, Value::Ascii("R98".into()));
        exif
    }

    fn assert_round_trips(order: ByteOrder) {
        let original = sample(order);
        let bytes = original.to_bytes().expect("write");
        let parsed = Exif::parse(&bytes).expect("round-trip parse");
        assert_eq!(parsed, original, "value-level round-trip in {order:?}");
        assert_eq!(parsed.byte_order(), order, "byte order preserved");
    }

    #[test]
    fn round_trips_both_byte_orders() {
        assert_round_trips(ByteOrder::LittleEndian);
        assert_round_trips(ByteOrder::BigEndian);
    }

    #[test]
    fn emits_and_omits_the_marker() {
        let exif = sample(ByteOrder::LittleEndian);
        let with = ExifWriter::new().write(&exif).expect("write");
        assert_eq!(&with[..6], MARKER);
        let bare = ExifWriter::new().marker(false).write(&exif).expect("write");
        assert_ne!(&bare[..2], MARKER);
        // A bare stream begins with the TIFF byte-order mark and re-parses.
        assert_eq!(&bare[..2], b"II");
        assert_eq!(Exif::parse(&bare).expect("bare re-parse"), exif);
    }

    #[test]
    fn byte_order_override_rewrites_endianness() {
        let exif = sample(ByteOrder::LittleEndian);
        let be = ExifWriter::new()
            .byte_order(ByteOrder::BigEndian)
            .write(&exif)
            .expect("write");
        let parsed = Exif::parse(&be).expect("parse");
        assert_eq!(parsed.byte_order(), ByteOrder::BigEndian);
        // Values are unchanged despite the re-encoding.
        assert_eq!(parsed.f_number(), exif.f_number());
        assert_eq!(
            parsed.get_tag(ExifTag::LensModel),
            exif.get_tag(ExifTag::LensModel)
        );
    }

    #[test]
    fn preserves_the_thumbnail_directory_on_round_trip() {
        // Build a blob with a 1st IFD (thumbnail) via gamut_ifd, parse it, then re-serialise: the
        // writer must chain the thumbnail directory back as the 1st IFD.
        let mut image = Ifd::new();
        image.set(ExifTag::Make.tag_id(), Value::Ascii("Canon".into()));
        let mut thumb = Ifd::new();
        thumb.set(ExifTag::Compression.tag_id(), Value::Short(vec![6]));
        thumb.set(
            ExifTag::JpegInterchangeFormatLength.tag_id(),
            Value::Long(vec![123]),
        );
        let blob = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![image, thumb],
        })
        .expect("write");

        let parsed = Exif::parse(&blob).expect("parse");
        assert!(parsed.thumbnail_ifd().is_some());
        let reparsed = Exif::parse(&parsed.to_bytes().expect("write")).expect("re-parse");
        assert_eq!(
            reparsed, parsed,
            "thumbnail directory survives the round-trip"
        );
        assert_eq!(
            reparsed
                .thumbnail_ifd()
                .and_then(|t| t.get_u32(ExifTag::Compression.tag_id())),
            Some(6)
        );
    }

    #[test]
    fn re_embeds_a_jpeg_thumbnail_round_trip() {
        // Two JPEG lengths, one even and one odd, to exercise the word-alignment padding before the
        // appended bytes.
        for jpeg in [
            vec![0xFFu8, 0xD8, 0xFF, 0xD9],       // 4 bytes (even)
            vec![0xFFu8, 0xD8, 0xFF, 0xE0, 0xD9], // 5 bytes (odd)
        ] {
            let mut exif = sample(ByteOrder::LittleEndian);
            exif.set_thumbnail(jpeg.clone());

            let parsed = Exif::parse(&exif.to_bytes().expect("write")).expect("round-trip parse");
            assert_eq!(parsed.thumbnail_bytes(), Some(jpeg.as_slice()));
            assert_eq!(parsed.thumbnail().and_then(Thumbnail::compression), Some(6));
            // The rest of the model survives alongside the thumbnail.
            assert_eq!(parsed.make(), exif.make());
            assert_eq!(parsed.f_number(), exif.f_number());
        }
    }

    #[test]
    fn maker_note_bytes_survive_round_trip_verbatim() {
        // A MakerNote long enough to be stored out of line; its bytes must be byte-exact after a
        // round-trip even though v1 does not decode or rebase its internal offsets.
        let blob: Vec<u8> = (0..64u16).map(|b| b as u8).collect();
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set_tag(ExifTag::Make, Value::Ascii("NIKON CORPORATION".into()));
        exif.set_tag(ExifTag::MakerNote, Value::Undefined(blob.clone()));

        let parsed = Exif::parse(&exif.to_bytes().expect("write")).expect("parse");
        let maker = parsed.maker_note().expect("maker note present");
        assert_eq!(maker.bytes, blob, "MakerNote bytes preserved verbatim");
        assert_eq!(maker.vendor, crate::MakerNoteVendor::Nikon);
    }

    /// The maker-note pin (issue #263): after a parse, a rewrite keeps the note's byte range at
    /// its exact source offset even when an edit shifts every directory — so vendor-internal
    /// absolute offsets stay valid.
    #[test]
    fn maker_note_pins_at_its_source_offset_across_rewrites() {
        let blob: Vec<u8> = (0..64u16).map(|b| b as u8).collect();
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set_tag(ExifTag::Make, Value::Ascii("NIKON CORPORATION".into()));
        exif.set_tag(ExifTag::MakerNote, Value::Undefined(blob.clone()));
        let bytes1 = ExifWriter::new().marker(false).write(&exif).expect("write");

        let mut parsed = Exif::parse(&bytes1).expect("parse");
        let at = parsed.maker_note_offset().expect("offset recorded") as usize;
        assert_eq!(&bytes1[at..at + blob.len()], &blob[..], "offset is real");

        // An edit that grows the 0th IFD, shifting everything after it.
        parsed.set_tag(
            ExifTag::ImageDescription,
            Value::Ascii("a description long enough to move every following directory".into()),
        );
        let bytes2 = ExifWriter::new()
            .marker(false)
            .write(&parsed)
            .expect("write");
        assert_eq!(
            &bytes2[at..at + blob.len()],
            &blob[..],
            "maker-note byte range untouched at its source offset"
        );
        // Reparsing records the same (pinned) offset, so pinning is stable across generations.
        assert_eq!(
            Exif::parse(&bytes2).expect("reparse").maker_note_offset(),
            Some(at as u64)
        );
    }

    /// A pin the new layout cannot honor (the directories now reach past the note's old offset)
    /// falls back to relocation — the bytes still round-trip exactly.
    #[test]
    fn unsatisfiable_pin_falls_back_to_relocation() {
        let blob: Vec<u8> = (0..32u16).map(|b| b as u8).collect();
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set_tag(ExifTag::MakerNote, Value::Undefined(blob.clone()));
        let bytes1 = ExifWriter::new().marker(false).write(&exif).expect("write");
        let mut parsed = Exif::parse(&bytes1).expect("parse");
        // Balloon the 0th IFD so the directory region engulfs the note's old offset.
        for tag in 0x8000..0x8060u16 {
            parsed.image_mut().set(tag, Value::Short(vec![1]));
        }
        let bytes2 = ExifWriter::new()
            .marker(false)
            .write(&parsed)
            .expect("write");
        let reparsed = Exif::parse(&bytes2).expect("reparse");
        assert_eq!(
            reparsed.maker_note().expect("note").bytes,
            blob,
            "bytes exact despite relocation"
        );
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

    #[test]
    fn check_tag_accepts_every_type_the_spec_lists() {
        // "SHORT or LONG" must accept both, not just the first.
        assert!(check_tag(ExifTag::ImageWidth, &Value::Short(vec![640])).is_ok());
        assert!(check_tag(ExifTag::ImageWidth, &Value::Long(vec![640])).is_ok());
        // "ASCII or UTF-8" likewise, so Exif 3.0 text is not refused.
        assert!(check_tag(ExifTag::LensModel, &Value::Ascii("XF16mm".into())).is_ok());
        assert!(check_tag(ExifTag::LensModel, &Value::Utf8("XF16mm ƒ1.4".into())).is_ok());
    }

    #[test]
    fn check_tag_rejects_a_type_the_spec_does_not_list() {
        let err = check_tag(ExifTag::FNumber, &Value::Short(vec![28])).expect_err("wrong type");
        assert_eq!(
            err.to_string(),
            "FNumber: CIPA DC-008 requires RATIONAL, not SHORT"
        );
        // An alternation names both permitted types.
        let err = check_tag(ExifTag::ImageWidth, &Value::Ascii("640".into()))
            .expect_err("ASCII is not a dimension");
        assert_eq!(
            err.to_string(),
            "ImageWidth: CIPA DC-008 requires SHORT or LONG, not ASCII"
        );
    }

    #[test]
    fn check_tag_rejects_a_count_the_spec_does_not_allow() {
        // GPSLatitude is three RATIONALs: degrees, minutes, seconds.
        let err = check_tag(
            ExifTag::GpsLatitude,
            &Value::Rational(vec![(48, 1), (51, 1)]),
        )
        .expect_err("two components is not a coordinate");
        assert_eq!(
            err.to_string(),
            "GPSLatitude: CIPA DC-008 requires a count of 3, not 2"
        );
        // The alternating count names each alternative.
        let err = check_tag(ExifTag::SubjectArea, &Value::Short(vec![1, 2, 3, 4, 5]))
            .expect_err("five components is not a subject area");
        assert_eq!(
            err.to_string(),
            "SubjectArea: CIPA DC-008 requires a count of 2 or 3 or 4, not 5"
        );
        for n in 2..=4 {
            assert!(check_tag(ExifTag::SubjectArea, &Value::Short(vec![0; n])).is_ok());
        }
    }

    #[test]
    fn a_string_count_includes_the_terminating_nul() {
        // DateTime's count of 20 is 19 characters plus the NUL, so the 19-character form passes
        // and a 20-character one does not.
        assert!(
            check_tag(
                ExifTag::DateTime,
                &Value::Ascii("2024:01:01 12:00:00".into())
            )
            .is_ok()
        );
        let err = check_tag(
            ExifTag::DateTime,
            &Value::Ascii("2024:01:01 12:00:000".into()),
        )
        .expect_err("one character too many");
        assert_eq!(
            err.to_string(),
            "DateTime: CIPA DC-008 requires a count of 20, not 21"
        );
    }

    #[test]
    fn check_tag_constrains_nothing_the_spec_does_not_define() {
        // ApplicationNotes (XMP) and the DCF-era Interoperability tags have no DC-008 row, so
        // the check must pass anything rather than invent a constraint.
        assert!(check_tag(ExifTag::Xmp, &Value::Byte(vec![1, 2, 3])).is_ok());
        assert!(check_tag(ExifTag::Xmp, &Value::Ascii("<x:xmpmeta/>".into())).is_ok());
        assert!(check_tag(ExifTag::RelatedImageWidth, &Value::Long(vec![1])).is_ok());
    }

    #[test]
    fn set_tag_checked_writes_only_a_conforming_value() {
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        set_tag_checked(&mut exif, ExifTag::FNumber, Value::Rational(vec![(28, 10)]))
            .expect("conformant");
        assert_eq!(
            exif.get_tag(ExifTag::FNumber),
            Some(&Value::Rational(vec![(28, 10)]))
        );

        // A rejected write must not disturb what is already there.
        let err = set_tag_checked(&mut exif, ExifTag::FNumber, Value::Short(vec![56]))
            .expect_err("wrong type");
        assert_eq!(
            err.to_string(),
            "FNumber: CIPA DC-008 requires RATIONAL, not SHORT"
        );
        assert_eq!(
            exif.get_tag(ExifTag::FNumber),
            Some(&Value::Rational(vec![(28, 10)])),
            "the refused value was not written"
        );
    }

    #[test]
    fn the_unchecked_setter_stays_lenient() {
        // Exif::set_tag is the deliberate escape hatch for reproducing a non-conformant source
        // file; adding the checked setter must not have changed it.
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set_tag(ExifTag::FNumber, Value::Short(vec![28]));
        assert_eq!(
            exif.get_tag(ExifTag::FNumber),
            Some(&Value::Short(vec![28]))
        );
        // And such a model still serialises and re-parses.
        let parsed = Exif::parse(&exif.to_bytes().expect("write")).expect("parse");
        assert_eq!(
            parsed.get_tag(ExifTag::FNumber),
            Some(&Value::Short(vec![28]))
        );
    }

    #[test]
    fn hand_set_pointer_tags_do_not_corrupt_layout() {
        // A caller wrongly writes a raw ExifIFD pointer field; the writer must drop it and
        // synthesise the real one from the typed sub-IFD.
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set_tag(ExifTag::Make, Value::Ascii("Canon".into()));
        exif.set_tag(ExifTag::FNumber, Value::Rational(vec![(28, 10)]));
        exif.image_mut()
            .set(EXIF_IFD_POINTER, Value::Long(vec![0xDEAD]));

        let parsed = Exif::parse(&exif.to_bytes().expect("write")).expect("parse");
        assert_eq!(
            parsed.f_number(),
            Some(crate::Rational { num: 28, den: 10 })
        );
        // The bogus pointer value is gone; the pointer is represented structurally.
        assert_eq!(parsed.get(IfdKind::Image, EXIF_IFD_POINTER), None);
    }
}
