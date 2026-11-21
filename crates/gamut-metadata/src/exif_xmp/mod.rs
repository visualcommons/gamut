//! EXIF ↔ XMP mapping: [`exif_to_xmp`] and [`xmp_to_exif`], with an [`ExifConflictPolicy`].
//!
//! A file can carry the same datum twice — once as an EXIF tag, once as an XMP property in the
//! `tiff:`/`exif:` namespaces (or the `dc:`/`xmp:`/`photoshop:` properties the Metadata Working
//! Group assigns to the descriptive tags). These two functions derive one carrier from the other,
//! row by row over one table, the way IPTC-IIM is reconciled with XMP in [`crate::iptc`].
//!
//! # Which tags
//!
//! The rows are exiv2's EXIF ↔ XMP conversion table (`Converter::conversion_` in exiv2 0.28's
//! `convert.cpp`, the table behind `copyExifToXmp` / `copyXmpToExif`), whose date rows follow the
//! MWG guidelines: `DateTime` ↔ `xmp:ModifyDate`, `DateTimeOriginal` ↔ `photoshop:DateCreated`,
//! `DateTimeDigitized` ↔ `xmp:CreateDate`, and `ImageDescription`/`Artist`/`Copyright` ↔
//! `dc:description`/`dc:creator`/`dc:rights`. Six of exiv2's rows are left out because exiv2
//! writes them in a form CIPA DC-010 does not define: `OECF`, `SpatialFrequencyResponse`,
//! `CFAPattern` and `DeviceSettingDescription` are XMP *structures* in DC-010 §7.9, which exiv2
//! flattens to a list of byte values, and `GPSProcessingMethod` and `GPSAreaInformation` are text,
//! which exiv2 writes with their character-code prefix spelled into it. Their conversion, the
//! `exifEX:` (Exif 2.21+) properties DC-010 adds beyond exiv2's table, and the `OffsetTime*`
//! zones the dates could carry, are not implemented yet.
//!
//! # Value forms
//!
//! Each row converts the way exiv2's does — dates to ISO 8601 with the `SubSecTime*` fraction
//! folded in, `ExifVersion` to its four characters, `GPSVersionID` to `2.2.0.0`, `Flash` to the
//! `exif:Flash` structure, GPS coordinates to `DDD,MM.mmmmmmmR`, `UserComment` decoded from its
//! character code — with one deliberate difference, taken from CIPA DC-010 §4: a numeric tag with
//! more than one value maps to an ordered array with **one entry per value** (`tiff:BitsPerSample`
//! = `8`, `8`, `8`), where exiv2 writes a single entry holding the values space-separated. Reading
//! XMP back, an array entry is split on whitespace, so both forms are accepted.
//!
//! Two smaller differences, each pinned by the exiv2 differential:
//!
//! - Converting an XMP date to EXIF keeps the wall-clock time the XMP value states and drops its
//!   zone designator: EXIF's date tags carry local time without a zone. exiv2 instead moves the
//!   time into the zone of the machine running it. A fractional second becomes the nine-digit
//!   `SubSecTime*` value with its leading zeros (`.06` → `060000000`), where exiv2 drops them.
//! - Non-ASCII text goes to a text tag as Exif 3.0's `UTF8` type where the tag allows it
//!   (DC-008 Table 6: `ImageDescription`, `Make`, `Model`, `Software`, `Artist`, `Copyright`);
//!   exiv2 writes the same bytes under `ASCII`.
//!
//! As in exiv2's output, EXIF text ends at its first NUL, and an empty description, copyright or
//! comment maps to no property at all.
//!
//! # Conflicts
//!
//! A conversion never touches a target the source does not map. Where the source maps a tag and
//! the target already holds it, the [`ExifConflictPolicy`] decides: the winning carrier's value
//! overwrites, the losing carrier's is kept. With [`ExifConflictPolicy::ExifWins`],
//! [`exif_to_xmp`] is exiv2's `copyExifToXmp`; with the default
//! [`ExifConflictPolicy::XmpWins`], [`xmp_to_exif`] is exiv2's `copyXmpToExif`.
//!
//! A companion tag ([`xmp_to_exif`] writes `SubSecTime*`, `GPSDateStamp` and the GPS `…Ref`
//! beside their targets) shares its target's conflict: an EXIF that holds either one keeps both
//! under [`ExifConflictPolicy::ExifWins`], and under [`ExifConflictPolicy::XmpWins`] a date with
//! no fraction removes the `SubSecTime*` the EXIF held. exiv2 forces the companions over EXIF in
//! both modes and leaves a stale `SubSecTime*` beside the new date.
//!
//! ```
//! use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
//! use gamut_metadata::xmp::{WellKnownNs, XmpMeta};
//! use gamut_metadata::{ExifConflictPolicy, exif_to_xmp, xmp_to_exif};
//!
//! let mut exif = Exif::new(ByteOrder::LittleEndian);
//! exif.set_tag(ExifTag::Make, Value::Ascii("gamut".into()));
//! exif.set_tag(ExifTag::DateTimeOriginal, Value::Ascii("2024:05:06 07:08:09".into()));
//!
//! let mut xmp = XmpMeta::new();
//! exif_to_xmp(&exif, &mut xmp, ExifConflictPolicy::ExifWins);
//! assert_eq!(xmp.get_text(WellKnownNs::Tiff.uri(), "Make"), Some("gamut"));
//! assert_eq!(
//!     xmp.get_text(WellKnownNs::Photoshop.uri(), "DateCreated"),
//!     Some("2024-05-06T07:08:09")
//! );
//!
//! // ...and back, into an empty EXIF.
//! let mut back = Exif::new(ByteOrder::LittleEndian);
//! xmp_to_exif(&xmp, &mut back, ExifConflictPolicy::XmpWins);
//! assert_eq!(back.datetime_original(), Some("2024:05:06 07:08:09"));
//! ```

mod table;
mod to_exif;
mod to_xmp;

use ExifTag as T;
use gamut_exif::{Exif, ExifTag};
use gamut_xmp::{XmpMeta, XmpProperty};
use table::Conv::{Comment, Date, Flash, GpsCoord, GpsTime, GpsVersion, Plain, Version};
use table::{ROWS, Row};
use to_exif::to_exif;
use to_xmp::to_xmp;

/// Which carrier wins when EXIF and XMP both hold a mapped datum with differing values.
///
/// Shaped like [`iptc::ConflictPolicy`](crate::iptc::ConflictPolicy): the MWG guidelines and
/// CIPA DC-010 call for keeping the carriers in sync but name no single winner for a library to
/// apply, so this is gamut's explicit knob. Marked `#[non_exhaustive]`: finer-grained policies may
/// be added later. `#[repr(u8)]` with permanent, append-only discriminants, so the value crosses a
/// C boundary unchanged; a new policy takes the next free discriminant.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ExifConflictPolicy {
    /// The XMP value wins (the default, as for IPTC: XMP is the modern carrier). [`exif_to_xmp`]
    /// fills only the XMP properties that are absent; [`xmp_to_exif`] overwrites.
    #[default]
    XmpWins = 0,
    /// The EXIF value wins. [`exif_to_xmp`] overwrites; [`xmp_to_exif`] fills only the EXIF tags
    /// that are absent.
    ExifWins = 1,
}

/// Writes the XMP form of every mapped EXIF tag `exif` holds into `xmp`.
///
/// A property already in `xmp` is overwritten only under [`ExifConflictPolicy::ExifWins`]; a tag
/// whose value cannot be converted (a date that does not parse, a GPS coordinate without its
/// reference, …) is skipped. Properties outside the mapping are never touched.
pub fn exif_to_xmp(exif: &Exif, xmp: &mut XmpMeta, policy: ExifConflictPolicy) {
    for row in ROWS {
        let ns = row.ns.uri();
        if policy == ExifConflictPolicy::XmpWins && xmp.get(ns, row.name).is_some() {
            continue;
        }
        if let Some(value) = to_xmp(exif, row) {
            xmp.set(XmpProperty::new(ns, row.name, value));
        }
    }
}

/// Writes the EXIF form of every mapped XMP property `xmp` holds into `exif`.
///
/// A tag already in `exif` is overwritten only under [`ExifConflictPolicy::XmpWins`]; a property
/// whose value cannot be converted is skipped. The companion tags a conversion writes alongside
/// its target — `SubSecTime*` beside a date, `GPSDateStamp` beside `GPSTimeStamp`, the `…Ref` beside
/// a GPS coordinate — follow their target, so the conflict is decided for the target and its
/// companion together: under [`ExifConflictPolicy::ExifWins`] the row is skipped when `exif`
/// holds either of them, and under [`ExifConflictPolicy::XmpWins`] a written date with no
/// fraction removes the `SubSecTime*` tag `exif` held, which would otherwise pair the new date
/// with the old fraction. Tags outside the mapping are never touched.
pub fn xmp_to_exif(xmp: &XmpMeta, exif: &mut Exif, policy: ExifConflictPolicy) {
    for row in ROWS {
        if policy == ExifConflictPolicy::ExifWins
            && [Some(row.tag), companion(row)]
                .into_iter()
                .flatten()
                .any(|tag| exif.get_tag(tag).is_some())
        {
            continue;
        }
        let Some(prop) = xmp.get(row.ns.uri(), row.name) else {
            continue;
        };
        let writes = to_exif(prop, row, exif.byte_order());
        if writes.is_empty() {
            continue;
        }
        // Every `SubSecTime*` tag lives in the Exif IFD. A fraction the date carries is written
        // back below; checking first keeps a missing Exif IFD from being created empty.
        if let Date(Some(subsec)) = row.conv
            && exif.get_tag(subsec).is_some()
        {
            exif.exif_ifd_mut().remove(subsec.tag_id());
        }
        for (tag, value) in writes {
            exif.set_tag(tag, value);
        }
    }
}

/// The companion tag `row`'s conversion writes beside its target, if any.
fn companion(row: &Row) -> Option<ExifTag> {
    match row.conv {
        Date(subsec) => subsec,
        GpsCoord(reference) => Some(reference),
        GpsTime => Some(T::GpsDateStamp),
        Plain(_) | Version | GpsVersion | Flash | Comment => None,
    }
}

/// Fixtures the submodules' inline tests share.
#[cfg(test)]
mod test_support {
    use gamut_exif::{ByteOrder, Exif, ExifTag, Value};

    use super::table::{ROWS, Row};

    pub(super) fn exif() -> Exif {
        Exif::new(ByteOrder::LittleEndian)
    }

    pub(super) fn ascii(text: &str) -> Value {
        Value::Ascii(text.to_owned())
    }

    pub(super) fn row_for(tag: ExifTag) -> &'static Row {
        ROWS.iter().find(|r| r.tag == tag).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use gamut_exif::Value;
    use gamut_xmp::WellKnownNs::{Exif as ExifNs, Photoshop, Tiff, Xmp};

    use super::test_support::{ascii, exif};
    use super::*;

    #[test]
    fn exif_wins_overwrites_and_xmp_wins_keeps_an_existing_property() {
        let mut source = exif();
        source.set_tag(T::Make, ascii("Canon"));
        let tiff = Tiff.uri();

        let mut xmp = XmpMeta::new();
        xmp.set_text(tiff, "Make", "Nikon");
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::XmpWins);
        assert_eq!(xmp.get_text(tiff, "Make"), Some("Nikon"));
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::ExifWins);
        assert_eq!(xmp.get_text(tiff, "Make"), Some("Canon"));
    }

    #[test]
    fn exif_wins_keeps_a_row_whose_companion_alone_the_exif_holds() {
        let mut xmp = XmpMeta::new();
        let ns = ExifNs.uri();
        xmp.set_text(Photoshop.uri(), "DateCreated", "2024-05-06T07:08:09.5");
        xmp.set_text(ns, "GPSLatitude", "37,46.5N");
        xmp.set_text(ns, "GPSTimeStamp", "2008-07-19T12:34:56Z");

        let mut target = exif();
        target.set_tag(T::SubSecTimeOriginal, ascii("25"));
        target.set_tag(T::GpsLatitudeRef, ascii("S"));
        target.set_tag(T::GpsDateStamp, ascii("2001:02:03"));
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::ExifWins);
        assert_eq!(target.get_tag(T::DateTimeOriginal), None);
        assert_eq!(target.get_tag(T::SubSecTimeOriginal), Some(&ascii("25")));
        assert_eq!(target.get_tag(T::GpsLatitude), None);
        assert_eq!(target.get_tag(T::GpsLatitudeRef), Some(&ascii("S")));
        assert_eq!(target.get_tag(T::GpsTimeStamp), None);
        assert_eq!(target.get_tag(T::GpsDateStamp), Some(&ascii("2001:02:03")));
    }

    #[test]
    fn xmp_wins_removes_the_old_fraction_beside_a_date_without_one() {
        let mut xmp = XmpMeta::new();
        xmp.set_text(Photoshop.uri(), "DateCreated", "2024-05-06T07:08:09");

        let mut target = exif();
        target.set_tag(T::DateTimeOriginal, ascii("2001:02:03 04:05:06"));
        target.set_tag(T::SubSecTimeOriginal, ascii("25"));
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::XmpWins);
        assert_eq!(target.datetime_original(), Some("2024:05:06 07:08:09"));
        assert_eq!(target.get_tag(T::SubSecTimeOriginal), None);
    }

    #[test]
    fn a_date_that_does_not_convert_keeps_the_old_fraction() {
        let mut xmp = XmpMeta::new();
        xmp.set_text(Photoshop.uri(), "DateCreated", "not a date");

        let mut target = exif();
        target.set_tag(T::SubSecTimeOriginal, ascii("25"));
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::XmpWins);
        assert_eq!(target.get_tag(T::SubSecTimeOriginal), Some(&ascii("25")));
    }

    #[test]
    fn a_date_without_a_fraction_creates_no_exif_ifd() {
        let mut xmp = XmpMeta::new();
        xmp.set_text(Xmp.uri(), "ModifyDate", "2024-05-06T07:08:09");

        let mut target = exif();
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::XmpWins);
        assert_eq!(
            target.get_tag(T::DateTime),
            Some(&ascii("2024:05:06 07:08:09"))
        );
        assert!(target.exif_ifd().is_none());
    }

    #[test]
    fn xmp_wins_overwrites_and_exif_wins_keeps_an_existing_tag() {
        let mut xmp = XmpMeta::new();
        xmp.set_text(Tiff.uri(), "Make", "Canon");

        let mut target = exif();
        target.set_tag(T::Make, ascii("Nikon"));
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::ExifWins);
        assert_eq!(target.make(), Some("Nikon"));
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::XmpWins);
        assert_eq!(target.make(), Some("Canon"));
    }

    #[test]
    fn the_default_policy_is_xmp_wins_with_discriminants_zero_and_one() {
        assert_eq!(ExifConflictPolicy::default(), ExifConflictPolicy::XmpWins);
        assert_eq!(ExifConflictPolicy::XmpWins as u8, 0);
        assert_eq!(ExifConflictPolicy::ExifWins as u8, 1);
    }

    #[test]
    fn an_empty_user_comment_maps_to_no_property() {
        let mut source = exif();
        source.set_tag(T::UserComment, Value::Undefined(vec![0; 16]));
        let mut xmp = XmpMeta::new();
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::ExifWins);
        assert_eq!(xmp.get(ExifNs.uri(), "UserComment"), None);
        source.set_tag(T::UserComment, Value::Undefined(b"ASCII\0\0\0hi".to_vec()));
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::ExifWins);
        assert_eq!(
            xmp.get_lang_alt(ExifNs.uri(), "UserComment", "x-default"),
            Some("hi")
        );
    }

    #[test]
    fn conversion_skips_absent_and_unconvertible_sources() {
        let mut source = exif();
        source.set_tag(T::Orientation, Value::Float(vec![1.0]));
        let mut xmp = XmpMeta::new();
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::ExifWins);
        assert!(xmp.properties.is_empty());

        xmp.set_text(Tiff.uri(), "Orientation", "sideways");
        let mut target = exif();
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::XmpWins);
        assert_eq!(target.orientation(), None);
    }

    #[test]
    fn a_gps_coordinate_round_trips_with_its_reference() {
        let mut source = exif();
        source.set_tag(
            T::GpsLatitude,
            Value::Rational(vec![(40, 1), (26, 1), (46, 1)]),
        );
        source.set_tag(T::GpsLatitudeRef, ascii("N"));
        let mut xmp = XmpMeta::new();
        exif_to_xmp(&source, &mut xmp, ExifConflictPolicy::default());
        assert_eq!(
            xmp.get_text(ExifNs.uri(), "GPSLatitude"),
            Some("40,26.7666667N")
        );
        let mut target = exif();
        xmp_to_exif(&xmp, &mut target, ExifConflictPolicy::default());
        assert_eq!(target.get_tag(T::GpsLatitudeRef), Some(&ascii("N")));
        assert!(target.get_tag(T::GpsLatitude).is_some());
    }
}
