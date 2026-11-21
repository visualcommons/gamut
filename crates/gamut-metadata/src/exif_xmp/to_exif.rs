//! XMP → EXIF: each row's converter from an XMP property to its EXIF tag (and companion tags).

use ExifTag as T;
use WellKnownNs::Exif as ExifNs;
use gamut_exif::{ByteOrder, ExifTag, FieldType, Value};
use gamut_xmp::{WellKnownNs, XmpArray, XmpItem, XmpProperty, XmpValue};

use super::table::Conv::{Comment, Date, Flash, GpsCoord, GpsTime, GpsVersion, Plain, Version};
use super::table::Row;
use super::to_xmp::parse_long;

/// The EXIF tags `prop` converts to under `row`: the target first, then any companion tags.
/// Empty when the value does not convert.
pub(super) fn to_exif(prop: &XmpProperty, row: &Row, order: ByteOrder) -> Vec<(ExifTag, Value)> {
    let value = match row.conv {
        Plain(_) => plain_to_exif(prop, row.tag),
        Date(subsec) => return date_to_exif(prop, row.tag, subsec),
        Version => version_to_exif(prop),
        GpsVersion => prop
            .text()
            .and_then(|text| bytes_of(text.replace('.', " ").split_whitespace()))
            .map(Value::Byte),
        Flash => flash_to_exif(prop),
        GpsCoord(reference) => return coord_to_exif(prop, row.tag, reference),
        Comment => xmp_text(prop).map(|text| comment_to_exif(&text, order)),
        GpsTime => return gps_time_to_exif(prop),
    };
    value
        .map(|value| vec![(row.tag, value)])
        .unwrap_or_default()
}

/// The text of a property the way exiv2 reads one: a simple value's text; a language
/// alternative's `x-default` entry (or its only entry); an array's entries joined with `", "`.
fn xmp_text(prop: &XmpProperty) -> Option<String> {
    match &prop.value {
        XmpValue::Simple(text) | XmpValue::Uri(text) => Some(text.clone()),
        XmpValue::Array(XmpArray::Alt(items)) => {
            let default = items.iter().find(|item| item.lang() == Some("x-default"));
            match (default, items.as_slice()) {
                (Some(item), _) | (None, [item]) => item.text().map(str::to_owned),
                (None, _) => None,
            }
        }
        XmpValue::Array(array) => {
            let texts: Vec<&str> = array.texts().collect();
            Some(texts.join(", "))
        }
        XmpValue::Structured(_) => None,
    }
}

/// The whitespace-separated tokens of a numeric property: every array entry is split, so both
/// DC-010's one-entry-per-value form and exiv2's single space-separated entry read the same.
fn tokens(prop: &XmpProperty) -> Option<Vec<String>> {
    let text = match &prop.value {
        XmpValue::Array(XmpArray::Seq(items) | XmpArray::Bag(items)) => {
            let texts: Vec<&str> = items.iter().filter_map(XmpItem::text).collect();
            texts.join(" ")
        }
        _ => xmp_text(prop)?,
    };
    Some(text.split_whitespace().map(str::to_owned).collect())
}

/// A plain property as a value of the type `tag` takes: text tags as `ASCII` (or `UTF8` where the
/// tag allows it and the text is not ASCII), numeric tags parsed token by token. `None` when a
/// token does not parse in that type.
fn plain_to_exif(prop: &XmpProperty, tag: ExifTag) -> Option<Value> {
    let types = tag.field_types();
    if types.contains(&FieldType::Ascii) {
        let text = xmp_text(prop)?;
        let utf8 = !text.is_ascii() && types.contains(&FieldType::Utf8);
        return Some(if utf8 {
            Value::Utf8(text)
        } else {
            Value::Ascii(text)
        });
    }
    let tokens = tokens(prop)?;
    if tokens.is_empty() {
        return None;
    }
    let tokens = tokens.iter().map(String::as_str);
    // `Rating` is catalogued from outside DC-008 with no mandated type; it is a SHORT (the type
    // exiv2 and the Microsoft photo tags give it).
    match types.first().copied().unwrap_or(FieldType::Short) {
        FieldType::Byte => bytes_of(tokens).map(Value::Byte),
        FieldType::Undefined => bytes_of(tokens).map(Value::Undefined),
        FieldType::Rational => tokens
            .map(parse_rational)
            .collect::<Option<_>>()
            .map(Value::Rational),
        FieldType::SRational => tokens
            .map(parse_srational)
            .collect::<Option<_>>()
            .map(Value::SRational),
        FieldType::Long => tokens
            .map(|t| t.parse().ok())
            .collect::<Option<_>>()
            .map(Value::Long),
        _ => short_or_long(tokens, types.contains(&FieldType::Long)),
    }
}

/// Integers as `SHORT`, or as `LONG` when one does not fit a `SHORT` and the tag allows `LONG`.
fn short_or_long<'a>(tokens: impl Iterator<Item = &'a str>, long: bool) -> Option<Value> {
    let values: Vec<u32> = tokens.map(|t| t.parse().ok()).collect::<Option<_>>()?;
    let shorts: Option<Vec<u16>> = values.iter().map(|&v| u16::try_from(v).ok()).collect();
    match shorts {
        Some(shorts) => Some(Value::Short(shorts)),
        None if long => Some(Value::Long(values)),
        None => None,
    }
}

/// Byte tokens (`0`–`255`).
fn bytes_of<'a>(tokens: impl Iterator<Item = &'a str>) -> Option<Vec<u8>> {
    tokens.map(|t| t.parse().ok()).collect()
}

/// An unsigned rational written `num/den`.
fn parse_rational(token: &str) -> Option<(u32, u32)> {
    let (n, d) = token.split_once('/')?;
    Some((n.parse().ok()?, d.parse().ok()?))
}

/// A signed rational written `num/den`.
fn parse_srational(token: &str) -> Option<(i32, i32)> {
    let (n, d) = token.split_once('/')?;
    Some((n.parse().ok()?, d.parse().ok()?))
}

/// `exif:ExifVersion`/`exif:FlashpixVersion`: the first four characters as bytes.
fn version_to_exif(prop: &XmpProperty) -> Option<Value> {
    let text = prop.text()?;
    let bytes = text.as_bytes().get(..4)?;
    Some(Value::Undefined(bytes.to_vec()))
}

/// The `exif:Flash` structure → the `Flash` bit field. Requires `Fired`; a missing or unreadable
/// other field contributes nothing.
pub(super) fn flash_to_exif(prop: &XmpProperty) -> Option<Value> {
    let XmpValue::Structured(fields) = &prop.value else {
        return None;
    };
    let ns = ExifNs.uri();
    let field = |name: &str| {
        fields
            .iter()
            .find(|f| f.namespace == ns && f.name == name)
            .and_then(XmpProperty::text)
            .and_then(parse_flag)
    };
    if !fields
        .iter()
        .any(|f| f.namespace == ns && f.name == "Fired")
    {
        return None;
    }
    let mut bits = 0;
    for (name, mask, shift) in [
        ("Fired", 1, 0),
        ("Return", 3, 1),
        ("Mode", 3, 3),
        ("Function", 1, 5),
        ("RedEyeMode", 1, 6),
    ] {
        if let Some(v) = field(name) {
            bits |= (v & mask) << shift;
        }
    }
    Some(Value::Short(vec![bits]))
}

/// A `Flash` field: a whole number, or `True`/`False` (any case).
fn parse_flag(text: &str) -> Option<u16> {
    if let Some(v) = parse_long(text) {
        return u16::try_from(v & 0xFF).ok();
    }
    match text.trim().to_ascii_lowercase().as_str() {
        "true" => Some(1),
        "false" => Some(0),
        _ => None,
    }
}

/// `UserComment` text → the `UNICODE` character code and UCS-2 text in the stream's byte order
/// (exiv2's choice, which no text can fail).
fn comment_to_exif(text: &str, order: ByteOrder) -> Value {
    let mut bytes = b"UNICODE\0".to_vec();
    for unit in text.encode_utf16() {
        let pair = if order == ByteOrder::LittleEndian {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        bytes.extend_from_slice(&pair);
    }
    Value::Undefined(bytes)
}

/// The XMP `GPSCoordinate` form `DDD,MM,SSk` or `DDD,MM.mmk` → three rationals plus the `…Ref`
/// letter.
fn coord_to_exif(prop: &XmpProperty, tag: ExifTag, reference: ExifTag) -> Vec<(ExifTag, Value)> {
    let parse = || -> Option<Vec<(ExifTag, Value)>> {
        let text = prop.text()?;
        let letter = text
            .chars()
            .last()
            .filter(|c| matches!(c, 'N' | 'S' | 'E' | 'W'))?;
        let body = &text[..text.len() - 1];
        let parts: Vec<f64> = body
            .split(',')
            .map(|p| p.trim().parse().ok())
            .collect::<Option<_>>()?;
        let (deg, min, sec) = match *parts.as_slice() {
            [deg, min] => (deg, min.trunc(), min.fract() * 60.0),
            [deg, min, sec] => (deg, min, sec),
            _ => return None,
        };
        let rationals = [deg, min, sec]
            .into_iter()
            .map(float_to_rational)
            .collect::<Option<_>>()?;
        Some(vec![
            (tag, Value::Rational(rationals)),
            (reference, Value::Ascii(letter.to_string())),
        ])
    };
    parse().unwrap_or_default()
}

/// exiv2's `floatToRationalCast`: the value rounded to single precision, then over a power-of-ten
/// denominator as large as keeps the numerator in an `i32`, reduced. Negative and out-of-range
/// values do not fit an unsigned rational.
fn float_to_rational(value: f64) -> Option<(u32, u32)> {
    let d = f64::from(value as f32);
    let den: u32 = match d.abs() {
        a if a <= 2_147.0 => 1_000_000,
        a if a <= 214_748.0 => 10_000,
        a if a <= 21_474_836.0 => 100,
        a if a <= 2_147_483_647.0 => 1,
        _ => return None,
    };
    // Rounded half away from zero, as `std::lround`; the denominator keeps it within an `i32`.
    let num = u32::try_from((d * f64::from(den)).round() as i64).ok()?;
    let g = gcd(num, den);
    Some((num / g, den / g))
}

/// Greatest common divisor (`den` is never zero, so neither is the result).
fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// A date in the XMP `Date` form (Part 1 §8.2.1.1): `YYYY[-MM[-DD[Thh:mm[:ss[.s…]][TZD]]]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct XmpDate {
    fields: [i64; 6],
    /// The fractional second's digits, at most nine, as written (empty when there are none).
    fraction: String,
}

impl XmpDate {
    /// The fraction as nanoseconds (its digits right-padded to nine).
    fn nanos(&self) -> u32 {
        format!("{:0<9}", self.fraction).parse().unwrap_or_default()
    }
}

/// Parses an XMP date; the zone designator is checked for form and then dropped.
fn parse_xmp_date(text: &str) -> Option<XmpDate> {
    let text = text.trim();
    let (date, time) = match text.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };
    let mut fields = [0i64; 6];
    let mut date_parts = date.split('-');
    for (slot, width) in fields.iter_mut().zip([4, 2, 2]) {
        let Some(part) = date_parts.next() else { break };
        *slot = fixed_digits(part, width)?;
    }
    if date_parts.next().is_some() {
        return None;
    }
    let mut fraction = String::new();
    if let Some(time) = time {
        let zone_at = time.find(['Z', '+', '-']).unwrap_or(time.len());
        let (clock, zone) = time.split_at(zone_at);
        check_zone(zone)?;
        let (clock, frac) = match clock.split_once('.') {
            Some((clock, frac)) => (clock, Some(frac)),
            None => (clock, None),
        };
        let mut clock_parts = clock.split(':');
        for slot in &mut fields[3..] {
            let Some(part) = clock_parts.next() else {
                break;
            };
            *slot = fixed_digits(part, 2)?;
        }
        if clock_parts.next().is_some() || !clock.contains(':') {
            return None;
        }
        if let Some(frac) = frac {
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            fraction = frac.chars().take(9).collect();
        }
    }
    Some(XmpDate { fields, fraction })
}

/// Exactly `width` ASCII digits.
fn fixed_digits(text: &str, width: usize) -> Option<i64> {
    (text.len() == width && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// An empty zone, `Z`, or `±hh:mm`.
fn check_zone(zone: &str) -> Option<()> {
    match zone.as_bytes() {
        [] | [b'Z'] => Some(()),
        [b'+' | b'-', ..] => {
            let (h, m) = zone[1..].split_once(':')?;
            fixed_digits(h, 2)?;
            fixed_digits(m, 2)?;
            Some(())
        }
        _ => None,
    }
}

/// An XMP date → the EXIF `YYYY:MM:DD HH:MM:SS` form, plus the nine-digit `SubSecTime*` value
/// when the fraction is non-zero.
fn date_to_exif(
    prop: &XmpProperty,
    tag: ExifTag,
    subsec: Option<ExifTag>,
) -> Vec<(ExifTag, Value)> {
    let Some(date) = prop.text().and_then(parse_xmp_date) else {
        return Vec::new();
    };
    let [year, month, day, hour, min, sec] = date.fields;
    let text = format!("{year:4}:{month:02}:{day:02} {hour:02}:{min:02}:{sec:02}");
    let mut out = vec![(tag, Value::Ascii(text))];
    let nanos = date.nanos();
    if let Some(subsec) = subsec
        && nanos != 0
    {
        out.push((subsec, Value::Ascii(format!("{nanos:09}"))));
    }
    out
}

/// An XMP date → `GPSTimeStamp` (hours, minutes, seconds; a fractional second moves the whole
/// seconds into the minutes so the nanoseconds fit the seconds' rational) plus `GPSDateStamp`.
/// The zone is ignored: the XMP value is taken to be UTC, as `GPSTimeStamp` is.
fn gps_time_to_exif(prop: &XmpProperty) -> Vec<(ExifTag, Value)> {
    let Some(date) = prop.text().and_then(parse_xmp_date) else {
        return Vec::new();
    };
    let [year, month, day, hour, min, sec] = date.fields;
    let narrow = |v: i64| u32::try_from(v).unwrap_or_default();
    let (hour, min, sec) = (narrow(hour), narrow(min), narrow(sec));
    let nanos = date.nanos();
    let (rmin, rsec) = match (nanos, sec) {
        (0, _) => ((min, 1), (sec, 1)),
        (_, 0) => ((min, 1), (nanos, 1_000_000_000)),
        _ => ((min * 60 + sec, 60), (nanos, 1_000_000_000)),
    };
    vec![
        (
            T::GpsTimeStamp,
            Value::Rational(vec![(hour, 1), rmin, rsec]),
        ),
        (
            T::GpsDateStamp,
            Value::Ascii(format!("{year:4}:{month:02}:{day:02}")),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use WellKnownNs::{DublinCore as Dc, Photoshop, Tiff};

    use super::super::test_support::{ascii, row_for};
    use super::*;

    #[test]
    fn flash_back_needs_fired_and_reads_numbers_or_booleans() {
        let ns = ExifNs.uri();
        let field =
            |name: &str, text: &str| XmpProperty::new(ns, name, XmpValue::Simple(text.into()));
        let flash = |fields| XmpProperty::new(ns, "Flash", XmpValue::Structured(fields));
        assert_eq!(flash_to_exif(&flash(vec![field("Mode", "1")])), None);
        assert_eq!(
            flash_to_exif(&flash(vec![field("Fired", "true"), field("Function", "1")])),
            Some(Value::Short(vec![0b10_0001]))
        );
        assert_eq!(
            flash_to_exif(&flash(vec![
                field("Fired", "maybe"),
                field("RedEyeMode", "TRUE")
            ])),
            Some(Value::Short(vec![0b100_0000]))
        );
        assert_eq!(flash_to_exif(&field("Flash", "1")), None);
    }

    #[test]
    fn a_coordinate_reads_back_in_either_xmp_form() {
        let prop = |text: &str| {
            XmpProperty::new(ExifNs.uri(), "GPSLatitude", XmpValue::Simple(text.into()))
        };
        let decimal = coord_to_exif(&prop("40,26.767N"), T::GpsLatitude, T::GpsLatitudeRef);
        assert_eq!(
            decimal,
            [
                // 0.767 minutes is 46.02 seconds, reduced.
                (
                    T::GpsLatitude,
                    Value::Rational(vec![(40, 1), (26, 1), (2301, 50)])
                ),
                (T::GpsLatitudeRef, ascii("N")),
            ]
        );
        let sexagesimal = coord_to_exif(&prop("40,26,46S"), T::GpsLatitude, T::GpsLatitudeRef);
        assert_eq!(
            sexagesimal[0].1,
            Value::Rational(vec![(40, 1), (26, 1), (46, 1)])
        );
        assert_eq!(sexagesimal[1].1, ascii("S"));
        for bad in ["40,26.767X", "40N", "40,26,1,2N", "4x,26N", "-1,0N"] {
            assert_eq!(
                coord_to_exif(&prop(bad), T::GpsLatitude, T::GpsLatitudeRef),
                [],
                "{bad}"
            );
        }
    }

    #[test]
    fn float_to_rational_picks_the_largest_power_of_ten_that_fits() {
        assert_eq!(float_to_rational(0.5), Some((1, 2)));
        assert_eq!(float_to_rational(2_147.0), Some((2_147, 1)));
        assert_eq!(float_to_rational(2_148.5), Some((4_297, 2)));
        assert_eq!(float_to_rational(214_749.5), Some((429_499, 2)));
        assert_eq!(float_to_rational(21_474_837.0), Some((21_474_836, 1)));
        assert_eq!(float_to_rational(3e9), None);
        assert_eq!(float_to_rational(-1.0), None);
    }

    #[test]
    fn user_comment_encodes_as_ucs2_in_the_stream_byte_order() {
        assert_eq!(
            comment_to_exif("hé", ByteOrder::LittleEndian),
            Value::Undefined(b"UNICODE\0h\0\xE9\0".to_vec())
        );
        assert_eq!(
            comment_to_exif("hé", ByteOrder::BigEndian),
            Value::Undefined(b"UNICODE\0\0h\0\xE9".to_vec())
        );
    }

    #[test]
    fn an_xmp_date_parses_every_precision_and_drops_its_zone() {
        let fields = |text: &str| parse_xmp_date(text).map(|d| (d.fields, d.fraction));
        assert_eq!(fields("2020"), Some(([2020, 0, 0, 0, 0, 0], String::new())));
        assert_eq!(
            fields("2020-05-06"),
            Some(([2020, 5, 6, 0, 0, 0], String::new()))
        );
        assert_eq!(
            fields("2020-05-06T07:08"),
            Some(([2020, 5, 6, 7, 8, 0], String::new()))
        );
        assert_eq!(
            fields("2020-05-06T07:08:09.0123456789+02:00"),
            Some(([2020, 5, 6, 7, 8, 9], "012345678".into()))
        );
        assert_eq!(
            fields("2020-05-06T07:08:09Z").unwrap().0,
            [2020, 5, 6, 7, 8, 9]
        );
        for bad in [
            "20",
            "2020-5",
            "2020-05-06T07",
            "2020-05-06T07:08:09.",
            "2020-05-06T07:08+2",
            "2020-05-06-07",
            "2020-05-06T07:08:09:10",
            "2020-05-06T07:08Q",
        ] {
            assert_eq!(parse_xmp_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_xmp_date_writes_the_exif_date_and_a_nine_digit_subsec() {
        let prop = |text: &str| {
            XmpProperty::new(
                Photoshop.uri(),
                "DateCreated",
                XmpValue::Simple(text.into()),
            )
        };
        let row = row_for(T::DateTimeOriginal);
        assert_eq!(
            to_exif(
                &prop("2020-05-06T07:08:09.05-05:00"),
                row,
                ByteOrder::LittleEndian
            ),
            [
                (T::DateTimeOriginal, ascii("2020:05:06 07:08:09")),
                (T::SubSecTimeOriginal, ascii("050000000")),
            ]
        );
        assert_eq!(
            to_exif(
                &prop("2020-05-06T07:08:09.000"),
                row,
                ByteOrder::LittleEndian
            ),
            [(T::DateTimeOriginal, ascii("2020:05:06 07:08:09"))]
        );
        assert_eq!(
            to_exif(&prop("not a date"), row, ByteOrder::LittleEndian),
            []
        );
    }

    #[test]
    fn an_xmp_date_writes_gps_time_and_date_stamps() {
        let prop = |text: &str| {
            XmpProperty::new(ExifNs.uri(), "GPSTimeStamp", XmpValue::Simple(text.into()))
        };
        let stamps = |text: &str| gps_time_to_exif(&prop(text));
        assert_eq!(
            stamps("2008-07-19T13:58:15Z"),
            [
                (
                    T::GpsTimeStamp,
                    Value::Rational(vec![(13, 1), (58, 1), (15, 1)])
                ),
                (T::GpsDateStamp, ascii("2008:07:19")),
            ]
        );
        assert_eq!(
            stamps("2008-07-19T13:58:15.5")[0].1,
            Value::Rational(vec![
                (13, 1),
                (58 * 60 + 15, 60),
                (500_000_000, 1_000_000_000)
            ])
        );
        assert_eq!(
            stamps("2008-07-19T13:58:00.5")[0].1,
            Value::Rational(vec![(13, 1), (58, 1), (500_000_000, 1_000_000_000)])
        );
        assert_eq!(stamps("garbage"), []);
    }

    #[test]
    fn xmp_text_reads_simple_alternative_and_array_values() {
        let p = |value| XmpProperty::new(Dc.uri(), "x", value);
        let alt = |items| p(XmpValue::Array(XmpArray::Alt(items)));
        assert_eq!(
            xmp_text(&p(XmpValue::Uri("u".into()))).as_deref(),
            Some("u")
        );
        assert_eq!(
            xmp_text(&alt(vec![
                XmpItem::lang_text("fr", "b"),
                XmpItem::lang_text("x-default", "a")
            ]))
            .as_deref(),
            Some("a")
        );
        assert_eq!(
            xmp_text(&alt(vec![XmpItem::lang_text("fr", "b")])).as_deref(),
            Some("b")
        );
        assert_eq!(
            xmp_text(&alt(vec![
                XmpItem::lang_text("fr", "b"),
                XmpItem::lang_text("de", "c")
            ])),
            None
        );
        let seq = p(XmpValue::Array(XmpArray::Seq(vec![
            XmpItem::simple("A"),
            XmpItem::simple("B"),
        ])));
        assert_eq!(xmp_text(&seq).as_deref(), Some("A, B"));
        assert_eq!(xmp_text(&p(XmpValue::Structured(Vec::new()))), None);
    }

    #[test]
    fn numeric_properties_read_one_value_per_token_in_the_tags_type() {
        let p = |value| XmpProperty::new(Tiff.uri(), "x", value);
        let seq = |items: &[&str]| {
            p(XmpValue::Array(XmpArray::Seq(
                items.iter().map(|&t| XmpItem::simple(t)).collect(),
            )))
        };
        let simple = |text: &str| p(XmpValue::Simple(text.into()));
        assert_eq!(
            plain_to_exif(&seq(&["8", "8", "8"]), T::BitsPerSample),
            Some(Value::Short(vec![8, 8, 8]))
        );
        assert_eq!(
            plain_to_exif(&seq(&["8 8 8"]), T::BitsPerSample),
            Some(Value::Short(vec![8, 8, 8]))
        );
        assert_eq!(
            plain_to_exif(&simple("70000"), T::ImageWidth),
            Some(Value::Long(vec![70_000]))
        );
        assert_eq!(
            plain_to_exif(&simple("640"), T::ImageWidth),
            Some(Value::Short(vec![640]))
        );
        assert_eq!(plain_to_exif(&simple("70000"), T::Orientation), None);
        assert_eq!(
            plain_to_exif(&simple("3"), T::Rating),
            Some(Value::Short(vec![3]))
        );
        assert_eq!(
            plain_to_exif(&simple("72/1"), T::XResolution),
            Some(Value::Rational(vec![(72, 1)]))
        );
        assert_eq!(plain_to_exif(&simple("72"), T::XResolution), None);
        assert_eq!(
            plain_to_exif(&simple("-1/3"), T::ExposureBiasValue),
            Some(Value::SRational(vec![(-1, 3)]))
        );
        assert_eq!(plain_to_exif(&simple("-1"), T::ExposureBiasValue), None);
        assert_eq!(
            plain_to_exif(&simple("3"), T::FileSource),
            Some(Value::Undefined(vec![3]))
        );
        assert_eq!(
            plain_to_exif(&simple("1"), T::GpsAltitudeRef),
            Some(Value::Byte(vec![1]))
        );
        assert_eq!(plain_to_exif(&simple("256"), T::GpsAltitudeRef), None);
        assert_eq!(plain_to_exif(&simple(""), T::Orientation), None);
    }

    #[test]
    fn text_properties_write_ascii_or_utf8_where_the_tag_allows_it() {
        let p = |text: &str| XmpProperty::new(Tiff.uri(), "x", XmpValue::Simple(text.into()));
        assert_eq!(plain_to_exif(&p("Canon"), T::Make), Some(ascii("Canon")));
        assert_eq!(
            plain_to_exif(&p("Café"), T::Make),
            Some(Value::Utf8("Café".into()))
        );
        assert_eq!(
            plain_to_exif(&p("Café"), T::GpsMapDatum),
            Some(ascii("Café"))
        );
    }

    #[test]
    fn versions_read_back_from_their_first_four_characters() {
        let prop = |text: &str| {
            XmpProperty::new(ExifNs.uri(), "ExifVersion", XmpValue::Simple(text.into()))
        };
        assert_eq!(
            version_to_exif(&prop("02321")),
            Some(Value::Undefined(b"0232".to_vec()))
        );
        assert_eq!(version_to_exif(&prop("023")), None);
        let gps = row_for(T::GpsVersionId);
        let gps_prop = XmpProperty::new(
            ExifNs.uri(),
            "GPSVersionID",
            XmpValue::Simple("2.2.0.0".into()),
        );
        assert_eq!(
            to_exif(&gps_prop, gps, ByteOrder::LittleEndian),
            [(T::GpsVersionId, Value::Byte(vec![2, 2, 0, 0]))]
        );
    }
}
