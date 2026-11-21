//! EXIF → XMP: each row's converter from an EXIF tag's value to its XMP property value.

use ExifTag as T;
use WellKnownNs::Exif as ExifNs;
use gamut_exif::{ByteOrder, Exif, ExifTag, Value, as_text};
use gamut_xmp::{WellKnownNs, XmpArray, XmpItem, XmpProperty, XmpValue};

use super::table::Conv::{Comment, Date, Flash, GpsCoord, GpsTime, GpsVersion, Plain, Version};
use super::table::Row;
use super::table::Shape::{self, LangAlt, Seq, Text};

/// The XMP value of `row`'s tag in `exif`, or `None` when it is absent or does not convert.
pub(super) fn to_xmp(exif: &Exif, row: &Row) -> Option<XmpValue> {
    let value = exif.get_tag(row.tag)?;
    match row.conv {
        Plain(shape) => plain_to_xmp(value, shape),
        Date(subsec) => date_to_xmp(exif, value, subsec).map(XmpValue::Simple),
        Version => version_to_xmp(value).map(XmpValue::Simple),
        GpsVersion => Some(XmpValue::Simple(join(&numbers(value)?, "."))),
        Flash => flash_to_xmp(value),
        GpsCoord(reference) => coord_to_xmp(value, exif.get_tag(reference)?).map(XmpValue::Simple),
        // An empty comment (the all-NUL field cameras reserve) is no comment: exiv2 writes none.
        Comment => comment_to_xmp(value, exif.byte_order())
            .filter(|text| !text.is_empty())
            .map(|text| lang_alt(&text)),
        GpsTime => gps_time_to_xmp(exif, value).map(XmpValue::Simple),
    }
}

/// A plain value in `shape`: its text, its per-value entries, or an `x-default` alternative.
fn plain_to_xmp(value: &Value, shape: Shape) -> Option<XmpValue> {
    let parts = match as_text(value) {
        // Text ends at the first NUL: what follows is padding a writer reserved (`Software` of
        // a fixed width), not text.
        Some(text) => vec![text.split('\0').next().unwrap_or_default().to_owned()],
        None => numbers(value)?,
    };
    Some(match shape {
        Text => XmpValue::Simple(join(&parts, " ")),
        // An empty description or copyright notice says nothing; XMP has no empty alternative.
        LangAlt => Some(join(&parts, " "))
            .filter(|text| !text.is_empty())
            .map(|text| lang_alt(&text))?,
        Seq => XmpValue::Array(XmpArray::Seq(
            parts
                .into_iter()
                .filter(|part| !part.is_empty())
                .map(XmpItem::simple)
                .collect(),
        )),
    })
}

/// A one-entry `x-default` language alternative.
fn lang_alt(text: &str) -> XmpValue {
    XmpValue::Array(XmpArray::Alt(vec![XmpItem::lang_text("x-default", text)]))
}

/// Joins `parts` with `sep`.
fn join(parts: &[String], sep: &str) -> String {
    parts.join(sep)
}

/// Each component of a numeric (or byte) value as exiv2 writes it: integers in decimal, rationals
/// as `num/den`. `None` for text and for the value kinds no mapped tag holds.
fn numbers(value: &Value) -> Option<Vec<String>> {
    fn each<V: ToString>(values: &[V]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }
    Some(match value {
        Value::Byte(v) | Value::Undefined(v) => each(v),
        Value::Short(v) => each(v),
        Value::Long(v) => each(v),
        Value::SByte(v) => each(v),
        Value::SShort(v) => each(v),
        Value::SLong(v) => each(v),
        Value::Rational(v) => v.iter().map(|(n, d)| format!("{n}/{d}")).collect(),
        Value::SRational(v) => v.iter().map(|(n, d)| format!("{n}/{d}")).collect(),
        _ => return None,
    })
}

/// The first component of an integer value, widened.
fn first_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Byte(v) | Value::Undefined(v) => v.first().map(|&x| i64::from(x)),
        Value::Short(v) => v.first().map(|&x| i64::from(x)),
        Value::Long(v) => v.first().map(|&x| i64::from(x)),
        Value::SShort(v) => v.first().map(|&x| i64::from(x)),
        Value::SLong(v) => v.first().map(|&x| i64::from(x)),
        _ => None,
    }
}

/// `ExifVersion`/`FlashpixVersion`: each byte as a character (`0x30 0x32 0x33 0x32` → `0232`).
fn version_to_xmp(value: &Value) -> Option<String> {
    match value {
        Value::Undefined(bytes) | Value::Byte(bytes) => {
            Some(bytes.iter().map(|&b| char::from(b)).collect())
        }
        _ => None,
    }
}

/// `Flash` → the `exif:Flash` structure (DC-010 §7.9 / XMP Part 2): `Fired`, `Return`, `Mode`,
/// `Function` and `RedEyeMode` unpacked from the bit field.
fn flash_to_xmp(value: &Value) -> Option<XmpValue> {
    let bits = first_integer(value)?;
    let ns = ExifNs.uri();
    let field = |name: &str, text: String| XmpProperty::new(ns, name, XmpValue::Simple(text));
    let flag = |bit: i64| {
        if bits >> bit & 1 == 1 {
            "True"
        } else {
            "False"
        }
        .to_owned()
    };
    Some(XmpValue::Structured(vec![
        field("Fired", flag(0)),
        field("Return", (bits >> 1 & 3).to_string()),
        field("Mode", (bits >> 3 & 3).to_string()),
        field("Function", flag(5)),
        field("RedEyeMode", flag(6)),
    ]))
}

/// A GPS coordinate (three rationals: degrees, minutes, seconds) and its reference → the XMP
/// `GPSCoordinate` form `DDD,MM.mmmmmmmR` (XMP Part 2 / DC-010 §8.3): whole degrees, decimal
/// minutes to seven places, and the reference letter.
fn coord_to_xmp(value: &Value, reference: &Value) -> Option<String> {
    let Value::Rational(parts) = value else {
        return None;
    };
    let [deg, min, sec] = parts.as_slice() else {
        return None;
    };
    let quotient = |&(n, d): &(u32, u32)| (d != 0).then(|| f64::from(n) / f64::from(d));
    let mut minutes = quotient(deg)? * 60.0 + quotient(min)? + quotient(sec)? / 60.0;
    // Truncating toward zero, as exiv2's `static_cast<int>` does.
    let degrees = (minutes / 60.0) as i64;
    minutes -= (degrees * 60) as f64;
    let letter = as_text(reference)?.chars().next()?;
    Some(format!("{degrees},{minutes:.7}{letter}"))
}

/// `UserComment`: the eight-byte character code, then the text (Exif 3.0 §4.6.5 Table 9 / DC-008
/// `UserComment`). `UNICODE` text is UCS-2 in the stream's byte order unless a byte-order mark
/// says otherwise; `ASCII` and undefined text end at the first NUL; `JIS` text is passed through
/// as bytes, which decode only when they happen to be valid UTF-8.
fn comment_to_xmp(value: &Value, order: ByteOrder) -> Option<String> {
    let Value::Undefined(bytes) = value else {
        return None;
    };
    let (code, body) = bytes.split_at_checked(8)?;
    if code == b"UNICODE\0" {
        return utf16_to_string(body, order);
    }
    let body = if code == b"ASCII\0\0\0" || code == [0; 8] {
        body.split(|&b| b == 0).next().unwrap_or_default()
    } else {
        body
    };
    String::from_utf8(body.to_vec()).ok()
}

/// Decodes UCS-2/UTF-16 text, honouring a leading byte-order mark (and dropping it), else `order`.
fn utf16_to_string(body: &[u8], order: ByteOrder) -> Option<String> {
    if let Some(rest) = body.strip_prefix(b"\xEF\xBB\xBF") {
        return String::from_utf8(rest.to_vec()).ok();
    }
    let (body, little) = match body {
        [0xFF, 0xFE, rest @ ..] => (rest, true),
        [0xFE, 0xFF, rest @ ..] => (rest, false),
        _ => (body, order == ByteOrder::LittleEndian),
    };
    let units = body.as_chunks::<2>().0.iter().map(|&pair| {
        if little {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .ok()
}

/// An EXIF date (`YYYY:MM:DD HH:MM:SS`) → ISO 8601 (`YYYY-MM-DDTHH:MM:SS`), with the companion
/// `SubSecTime*` digits appended as the fraction when they are a number.
fn date_to_xmp(exif: &Exif, value: &Value, subsec: Option<ExifTag>) -> Option<String> {
    let [year, month, day, hour, min, sec] = scan_ints(as_text(value)?, b":: ::")?;
    let fraction = subsec
        .and_then(|tag| exif.get_tag(tag))
        .and_then(subsec_fraction)
        .unwrap_or_default();
    Some(iso_date(
        [year, month, day, hour, min, sec],
        truncate_fraction(fraction),
    ))
}

/// A `SubSecTime*` value as the `.digits` fraction exiv2 appends: only an ASCII value that reads
/// as a whole number qualifies.
fn subsec_fraction(value: &Value) -> Option<String> {
    let Value::Ascii(text) = value else {
        return None;
    };
    parse_long(text)?;
    Some(format!(".{text}"))
}

/// Caps a fraction (with its leading `.`) at nine digits, as exiv2 does.
fn truncate_fraction(mut fraction: String) -> String {
    fraction.truncate(10);
    fraction
}

/// `YYYY-MM-DDTHH:MM:SS` followed by `fraction` (empty, or `.digits`).
fn iso_date([year, month, day, hour, min, sec]: [i64; 6], fraction: String) -> String {
    format!("{year:4}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}{fraction}")
}

/// `GPSTimeStamp` (hours, minutes, seconds as rationals, UTC) on the date of `GPSDateStamp` —
/// or, failing that, of `DateTimeOriginal` or `DateTimeDigitized` — → ISO 8601, with the
/// fractional second always written to nine places (exiv2's form).
fn gps_time_to_xmp(exif: &Exif, value: &Value) -> Option<String> {
    let Value::Rational(parts) = value else {
        return None;
    };
    let [hour, min, sec] = parts.as_slice() else {
        return None;
    };
    if parts.iter().any(|&(_, d)| d == 0) {
        return None;
    }
    // exiv2 reads hours and minutes through `float` and the seconds through `double`; the same
    // widths keep the rounding of a fractional second identical.
    let single = |&(n, d): &(u32, u32)| f64::from(n as f32 / d as f32);
    let mut seconds =
        single(hour) * 3600.0 + single(min) * 60.0 + f64::from(sec.0) / f64::from(sec.1);
    // Whole hours, then minutes, then seconds, each truncated toward zero as exiv2's
    // `static_cast<int>` does.
    let mut take = |unit: f64| {
        let whole = (seconds / unit) as i64;
        seconds -= (whole as f64) * unit;
        whole
    };
    let (h, m, s) = (take(3600.0), take(60.0), take(1.0));
    let fraction = format!("{seconds:.9}");
    let fraction = fraction.get(1..).unwrap_or_default().to_owned();
    let date = [T::GpsDateStamp, T::DateTimeOriginal, T::DateTimeDigitized]
        .into_iter()
        .find_map(|tag| exif.get_tag(tag))?;
    let [year, month, day] = scan_ints(as_text(date)?, b"::")?;
    Some(iso_date(
        [year, month, day, h, m, s],
        truncate_fraction(fraction),
    ))
}

/// Reads `N` integers separated by `seps` (`N - 1` bytes) the way C's `sscanf("%d:%d…")` does:
/// each integer may have leading whitespace and a sign, a `' '` separator matches any run of
/// whitespace (including none), any other separator must match exactly, and whatever follows the
/// last integer is ignored.
fn scan_ints<const N: usize>(text: &str, seps: &[u8]) -> Option<[i64; N]> {
    let mut out = [0; N];
    let mut rest = text;
    for (i, slot) in out.iter_mut().enumerate() {
        let (value, tail) = scan_int(rest)?;
        *slot = value;
        rest = tail;
        if let Some(&sep) = seps.get(i) {
            rest = if sep == b' ' {
                rest.trim_start()
            } else {
                rest.strip_prefix(char::from(sep))?
            };
        }
    }
    Some(out)
}

/// One `%d`: optional leading whitespace, an optional sign, then at least one digit.
fn scan_int(text: &str) -> Option<(i64, &str)> {
    let text = text.trim_start();
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let digits = unsigned.bytes().take_while(u8::is_ascii_digit).count();
    let end = text.len() - unsigned.len() + digits;
    if digits == 0 {
        return None;
    }
    Some((text[..end].parse().ok()?, &text[end..]))
}

/// A whole number the way C++ `istream >> long` reads one when nothing but whitespace may follow.
pub(super) fn parse_long(text: &str) -> Option<i64> {
    let (value, rest) = scan_int(text)?;
    rest.trim().is_empty().then_some(value)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{ascii, exif};
    use super::super::to_exif::flash_to_exif;
    use super::*;

    #[test]
    fn a_multi_valued_tag_maps_to_one_seq_entry_per_value() {
        let value = plain_to_xmp(&Value::Short(vec![8, 8, 8]), Seq).unwrap();
        let XmpValue::Array(XmpArray::Seq(items)) = value else {
            panic!("not a Seq: {value:?}")
        };
        assert_eq!(
            items.iter().filter_map(XmpItem::text).collect::<Vec<_>>(),
            ["8", "8", "8"]
        );
    }

    #[test]
    fn a_text_tag_maps_to_one_seq_entry_and_rationals_to_num_over_den() {
        let creator = plain_to_xmp(&ascii("A, B"), Seq).unwrap();
        assert_eq!(
            creator,
            XmpValue::Array(XmpArray::Seq(vec![XmpItem::simple("A, B")]))
        );
        let text = plain_to_xmp(&Value::SRational(vec![(-1, 3), (2, 5)]), Text).unwrap();
        assert_eq!(text, XmpValue::Simple("-1/3 2/5".into()));
        assert_eq!(plain_to_xmp(&Value::Float(vec![1.0]), Text), None);
    }

    #[test]
    fn text_ends_at_the_first_nul_and_empty_text_maps_to_no_entry() {
        assert_eq!(
            plain_to_xmp(&ascii("DC-4300\0\0junk"), Text),
            Some(XmpValue::Simple("DC-4300".into()))
        );
        assert_eq!(plain_to_xmp(&ascii(""), LangAlt), None);
        assert_eq!(
            plain_to_xmp(&ascii("\0"), Seq),
            Some(XmpValue::Array(XmpArray::Seq(Vec::new())))
        );
        assert_eq!(
            plain_to_xmp(&ascii(""), Text),
            Some(XmpValue::Simple(String::new()))
        );
    }

    #[test]
    fn numbers_renders_every_integer_kind_in_decimal() {
        let cases = [
            (Value::Byte(vec![1, 255]), "1 255"),
            (Value::Undefined(vec![3]), "3"),
            (Value::Long(vec![70_000]), "70000"),
            (Value::SByte(vec![-1]), "-1"),
            (Value::SShort(vec![-2]), "-2"),
            (Value::SLong(vec![-3]), "-3"),
            (Value::Rational(vec![(72, 1)]), "72/1"),
        ];
        for (value, text) in cases {
            assert_eq!(numbers(&value).unwrap().join(" "), text, "{value:?}");
        }
    }

    #[test]
    fn version_bytes_become_characters() {
        assert_eq!(
            version_to_xmp(&Value::Undefined(b"0232".to_vec())).as_deref(),
            Some("0232")
        );
        assert_eq!(
            version_to_xmp(&Value::Byte(b"0100".to_vec())).as_deref(),
            Some("0100")
        );
        assert_eq!(version_to_xmp(&ascii("0232")), None);
    }

    #[test]
    fn flash_unpacks_each_field_of_the_bit_field() {
        // Fired, return 2 (bits 1-2 = 10), mode 3 (bits 3-4 = 11), no function, red-eye.
        let bits = 0b100_1101 | 0b0001_1000;
        let XmpValue::Structured(fields) = flash_to_xmp(&Value::Short(vec![bits])).unwrap() else {
            panic!("not a structure")
        };
        let get = |name: &str| {
            fields
                .iter()
                .find(|f| f.name == name)
                .and_then(XmpProperty::text)
        };
        assert_eq!(get("Fired"), Some("True"));
        assert_eq!(get("Return"), Some("2"));
        assert_eq!(get("Mode"), Some("3"));
        assert_eq!(get("Function"), Some("False"));
        assert_eq!(get("RedEyeMode"), Some("True"));
        // ...and back.
        let prop = XmpProperty::new(ExifNs.uri(), "Flash", XmpValue::Structured(fields));
        assert_eq!(flash_to_exif(&prop), Some(Value::Short(vec![bits])));
    }

    #[test]
    fn a_coordinate_maps_to_degrees_and_decimal_minutes_with_its_letter() {
        let coord = Value::Rational(vec![(40, 1), (26, 1), (4602, 100)]);
        assert_eq!(
            coord_to_xmp(&coord, &ascii("N")).as_deref(),
            Some("40,26.7670000N")
        );
        // Minutes past 60 carry into the degrees.
        let carry = Value::Rational(vec![(1, 1), (90, 1), (0, 1)]);
        assert_eq!(
            coord_to_xmp(&carry, &ascii("W")).as_deref(),
            Some("2,30.0000000W")
        );
        assert_eq!(
            coord_to_xmp(&Value::Rational(vec![(1, 0), (0, 1), (0, 1)]), &ascii("N")),
            None
        );
        assert_eq!(
            coord_to_xmp(&Value::Rational(vec![(1, 1)]), &ascii("N")),
            None
        );
        assert_eq!(coord_to_xmp(&coord, &ascii("")), None);
    }

    #[test]
    fn user_comment_decodes_each_character_code() {
        let le = ByteOrder::LittleEndian;
        let with = |code: &[u8], body: &[u8]| Value::Undefined([code, body].concat());
        assert_eq!(
            comment_to_xmp(&with(b"ASCII\0\0\0", b"hi\0pad"), le).as_deref(),
            Some("hi")
        );
        assert_eq!(
            comment_to_xmp(&with(&[0; 8], b"x\0"), le).as_deref(),
            Some("x")
        );
        assert_eq!(
            comment_to_xmp(&with(b"JIS\0\0\0\0\0", b"a\0b"), le).as_deref(),
            Some("a\0b")
        );
        assert_eq!(
            comment_to_xmp(&with(b"UNICODE\0", b"h\0i\0"), le).as_deref(),
            Some("hi")
        );
        assert_eq!(
            comment_to_xmp(&with(b"UNICODE\0", b"\0h\0i"), ByteOrder::BigEndian).as_deref(),
            Some("hi")
        );
        assert_eq!(
            comment_to_xmp(&with(b"UNICODE\0", b"\xFE\xFF\0h"), le).as_deref(),
            Some("h")
        );
        assert_eq!(
            comment_to_xmp(&with(b"UNICODE\0", b"\xFF\xFEh\0"), ByteOrder::BigEndian).as_deref(),
            Some("h")
        );
        assert_eq!(
            comment_to_xmp(&with(b"UNICODE\0", b"\xEF\xBB\xBFok"), le).as_deref(),
            Some("ok")
        );
        assert_eq!(comment_to_xmp(&with(b"UNICODE\0", b"\x00\xD8"), le), None);
        assert_eq!(
            comment_to_xmp(&Value::Undefined(b"short".to_vec()), le),
            None
        );
        assert_eq!(comment_to_xmp(&ascii("ASCII\0\0\0text"), le), None);
    }

    #[test]
    fn a_date_takes_its_subsec_digits_as_the_fraction() {
        let mut source = exif();
        source.set_tag(T::DateTimeOriginal, ascii("2020:01:02 03:04:05"));
        fn date(source: &Exif) -> Option<String> {
            let value = source.get_tag(T::DateTimeOriginal).unwrap();
            date_to_xmp(source, value, Some(T::SubSecTimeOriginal))
        }
        assert_eq!(date(&source).as_deref(), Some("2020-01-02T03:04:05"));
        source.set_tag(T::SubSecTimeOriginal, ascii("52"));
        assert_eq!(date(&source).as_deref(), Some("2020-01-02T03:04:05.52"));
        source.set_tag(T::SubSecTimeOriginal, ascii("1234567890123"));
        assert_eq!(
            date(&source).as_deref(),
            Some("2020-01-02T03:04:05.123456789")
        );
        source.set_tag(T::SubSecTimeOriginal, ascii("   "));
        assert_eq!(date(&source).as_deref(), Some("2020-01-02T03:04:05"));
        source.set_tag(T::SubSecTimeOriginal, Value::Short(vec![5]));
        assert_eq!(date(&source).as_deref(), Some("2020-01-02T03:04:05"));
        assert_eq!(
            date_to_xmp(&source, &ascii("    :  :     :  :  "), None),
            None
        );
    }

    #[test]
    fn scan_ints_follows_sscanf() {
        assert_eq!(
            scan_ints::<3>(" 2020: -1:+2 trailing", b"::"),
            Some([2020, -1, 2])
        );
        assert_eq!(scan_ints::<2>("1 2", b" "), Some([1, 2]));
        assert_eq!(scan_ints::<2>("12", b" "), None);
        assert_eq!(scan_ints::<2>("1-2", b":"), None);
        assert_eq!(scan_ints::<1>("x", b""), None);
        assert_eq!(parse_long(" 42 "), Some(42));
        assert_eq!(parse_long("4 2"), None);
    }

    #[test]
    fn gps_time_combines_the_date_stamp_and_a_nine_place_fraction() {
        let mut source = exif();
        let stamp = Value::Rational(vec![(13, 1), (58, 1), (155, 10)]);
        assert_eq!(
            gps_time_to_xmp(&source, &stamp),
            None,
            "no date to put it on"
        );
        source.set_tag(T::DateTimeDigitized, ascii("2001:02:03 00:00:00"));
        assert_eq!(
            gps_time_to_xmp(&source, &stamp).as_deref(),
            Some("2001-02-03T13:58:15.500000000")
        );
        source.set_tag(T::GpsDateStamp, ascii("2008:07:19"));
        assert_eq!(
            gps_time_to_xmp(&source, &stamp).as_deref(),
            Some("2008-07-19T13:58:15.500000000")
        );
        assert_eq!(
            gps_time_to_xmp(&source, &Value::Rational(vec![(1, 0), (0, 1), (0, 1)])),
            None
        );
        assert_eq!(
            gps_time_to_xmp(&source, &Value::Rational(vec![(1, 1)])),
            None
        );
    }
}
