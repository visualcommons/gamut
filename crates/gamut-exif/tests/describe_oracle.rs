//! Differential of the `describe` tables against exiv2's interpreted rendering
//! (`Exifdatum::print`, through `exiv2_oracle::exif_print`).
//!
//! The tables are transcribed from CIPA DC-008 and `src/describe.rs` checks them structurally,
//! which falsifies a transposed or mis-masked table but not a transcription that is wrong and
//! self-consistent. exiv2 is an independent reading of the same tags, so it can.
//!
//! exiv2 does **not** word its renderings the way DC-008 does (`Orientation` = 1 is `"top, left"`
//! there), and DC-008 wins where they differ: it is the specification this crate implements, and
//! exiv2 is the oracle, not the source — the rule `tests/oracle.rs` already applies to tag names.
//! So the assertions are two pinned sets rather than string equality:
//!
//! * **Domain.** For every tag with a table, each value the tag's field can hold is written and
//!   rendered by exiv2. The codes exiv2 interprets are compared with the codes DC-008 defines, and
//!   every disagreement is pinned in [`DOMAIN_DIVERGENCES`] with its reason. A transcription that
//!   invents or drops a code moves that set and fails.
//! * **Wording.** For every code both define, the two texts are compared ignoring ASCII case, and
//!   every disagreement is pinned in [`WORDING_DIVERGENCES`] with exiv2's text. A transcription
//!   that moves a text exiv2 agrees with onto another code or tag moves that set and fails. What
//!   it cannot see is a swap between two codes exiv2 *already* words differently (Orientation 1
//!   and 2, say): both stay divergent against the same exiv2 text. Pinning gamut's text as well
//!   would only restate `src/describe.rs`, so those rows rest on the transcription from DC-008.
//!
//! The sets are exact, so a divergence that goes away (gamut or exiv2 changing) fails too and the
//! entry is removed, rather than left pinning nothing.
//!
//! Requires the `third_party/exiv2` + `third_party/expat` submodules and a C++ toolchain.
#![cfg(feature = "describe")]

use exiv2_oracle::exif_print;
use gamut_exif::describe::described_values;
use gamut_exif::{ByteOrder, Exif, ExifTag, ExifWriter, FieldType, IfdKind, Value};

/// The `Exif.<group>` prefix exiv2 keys a directory's tags under. Every described tag lives in
/// the 0th IFD, the Exif sub-IFD or the GPS sub-IFD.
fn exiv2_group(ifd: IfdKind) -> &'static str {
    match ifd {
        IfdKind::Exif => "Exif.Photo",
        IfdKind::Gps => "Exif.GPSInfo",
        _ => "Exif.Image",
    }
}

/// Every code the tag's field can hold, so exiv2's whole domain is observed rather than a guess at
/// where it might end: each 7-bit character but NUL (the code is the letter before the
/// terminator), each byte, or each `SHORT`. No described tag is typed wider than `SHORT`.
fn every_code(tag: ExifTag) -> std::ops::RangeInclusive<u32> {
    match tag.field_types().first() {
        Some(FieldType::Ascii) => 0x01..=0x7F,
        Some(FieldType::Byte | FieldType::Undefined) => 0..=0xFF,
        _ => 0..=0xFFFF,
    }
}

/// The value DC-008 says carries `code` for `tag`, written the way the tag's type requires.
fn value_of(tag: ExifTag, code: u32) -> Value {
    let byte = || u8::try_from(code).expect("a byte-wide code");
    match tag.field_types().first() {
        Some(FieldType::Ascii) => Value::Ascii(char::from(byte()).to_string()),
        Some(FieldType::Byte) => Value::Byte(vec![byte()]),
        // Four channels; only the first carries the code. exiv2 renders a 0 channel as nothing,
        // so the other three add nothing to its output.
        Some(FieldType::Undefined) if tag == ExifTag::ComponentsConfiguration => {
            Value::Undefined(vec![byte(), 0, 0, 0])
        }
        Some(FieldType::Undefined) => Value::Undefined(vec![byte()]),
        _ => Value::Short(vec![u16::try_from(code).expect("a SHORT code")]),
    }
}

/// exiv2's rendering of `tag` holding `code`, or `None` if exiv2 has no interpretation of it.
///
/// exiv2 renders a value it has no table entry for as the numeric code in parentheses — for a
/// character-coded tag too, where the code is the letter's byte — and a tag it has no interpreter
/// for at all as the bare number; both are "no interpretation".
fn exiv2_reading(tag: ExifTag, code: u32) -> Option<String> {
    let mut exif = Exif::new(ByteOrder::LittleEndian);
    exif.set_tag(tag, value_of(tag, code));
    let bytes = ExifWriter::new()
        .marker(false)
        .write(&exif)
        .expect("gamut writes the tag");
    let key = format!("{}.{}", exiv2_group(tag.ifd()), tag.name());
    let printed =
        exif_print(&bytes, &key).unwrap_or_else(|e| panic!("exiv2 reads {key} = {code}: {e}"));

    let raw = code.to_string();
    (printed != raw && printed != format!("({raw})")).then_some(printed)
}

/// Every tag `describe` has a table for.
fn described_tags() -> impl Iterator<Item = ExifTag> {
    ExifTag::ALL
        .iter()
        .copied()
        .filter(|&t| !described_values(t).is_empty())
}

/// The tags whose domains differ: the tag, the codes only exiv2 interprets, and the codes only
/// DC-008 defines. Each sweep is over every value the tag's field can hold.
const DOMAIN_DIVERGENCES: &[(&str, &[u32], &[u32])] = &[
    // §4.6.5.1.4 restricts Exif to uncompressed and JPEG; exiv2 reads the tag as TIFF's, with
    // TIFF's other codecs (CCITT, LZW, deflate, JBIG), DNG's (lossy JPEG, JPEG XL) and vendor raw
    // compressions.
    (
        "Compression",
        &[
            2, 3, 4, 5, 7, 8, 9, 10, 32766, 32767, 32769, 32770, 32771, 32773, 32809, 32895, 32896,
            32897, 32898, 32908, 32909, 32946, 32947, 34661, 34676, 34677, 34712, 34713, 34892,
            52546, 65000, 65535,
        ],
        &[],
    ),
    // §4.6.5.1.5 restricts Exif to RGB and YCbCr; exiv2 reads the tag as TIFF's, with DNG's
    // additions (Linear Raw, Depth Map, Photometric Mask).
    (
        "PhotometricInterpretation",
        &[
            0, 1, 3, 4, 5, 8, 9, 10, 32803, 32844, 32845, 34892, 51177, 52527,
        ],
        &[],
    ),
    // §4.6.5.1.11 reserves 1; exiv2 renders it "none", as TIFF 6.0 defines it. The focal-plane
    // unit (below) shares the table (§4.6.6.7.28), and exiv2 shares it too.
    ("ResolutionUnit", &[1], &[]),
    // DC-008 §4.6.6.7.7 assigns these; exiv2 has no interpretation of the tag at all and prints
    // the raw number.
    ("SensitivityType", &[], &[0, 1, 2, 3, 4, 5, 6, 7]),
    // "Warm white fluorescent" (§4.6.6.7.20), present in both vendored editions (2.32 and 3.0);
    // exiv2's table stops at 15.
    ("LightSource", &[], &[16]),
    // §4.6.6.2.1 defines sRGB and Uncalibrated only; exiv2 also names 2 "Adobe RGB", which
    // neither vendored edition assigns.
    ("ColorSpace", &[2], &[]),
    ("FocalPlaneResolutionUnit", &[1], &[]),
    // "others" (§4.6.6.7.32), present in both vendored editions; exiv2's table starts at 1.
    ("FileSource", &[], &[0]),
    // The sea-level references (§4.6.7.1.6) are new in Exif 3.0 — 2.32 has only 0 and 1 — and
    // exiv2 0.28 has not added them.
    ("GPSAltitudeRef", &[], &[2, 3]),
];

/// The codes both define but word differently, ignoring ASCII case: the tag, the code, and exiv2's
/// text. gamut's is DC-008's, in `src/describe.rs`.
const WORDING_DIVERGENCES: &[(&str, u32, &str)] = &[
    // exiv2 names the TIFF codec where DC-008 says what Exif uses it for.
    ("Compression", 6, "JPEG (old-style)"),
    // exiv2 names the corner the 0th row and column meet at; DC-008 §4.6.5.1.6 states it in full.
    ("Orientation", 1, "top, left"),
    ("Orientation", 2, "top, right"),
    ("Orientation", 3, "bottom, right"),
    ("Orientation", 4, "bottom, left"),
    ("Orientation", 5, "left, top"),
    ("Orientation", 6, "right, top"),
    ("Orientation", 7, "right, bottom"),
    ("Orientation", 8, "left, bottom"),
    // exiv2 drops DC-008's "format".
    ("PlanarConfiguration", 1, "Chunky"),
    ("PlanarConfiguration", 2, "Planar"),
    // exiv2 abbreviates the unit, for both tags that share the table.
    ("ResolutionUnit", 2, "inch"),
    ("ResolutionUnit", 3, "cm"),
    // exiv2 drops DC-008's parenthesised gloss, and names program 2 "Auto".
    ("ExposureProgram", 2, "Auto"),
    ("ExposureProgram", 5, "Creative program"),
    ("ExposureProgram", 6, "Action program"),
    ("ExposureProgram", 7, "Portrait mode"),
    ("ExposureProgram", 8, "Landscape mode"),
    // exiv2 renders an absent channel as nothing at all.
    ("ComponentsConfiguration", 0, ""),
    // exiv2 spaces DC-008's identifier, and names pattern metering "Multi-segment".
    ("MeteringMode", 2, "Center weighted average"),
    ("MeteringMode", 5, "Multi-segment"),
    // exiv2's colour-temperature ranges match neither vendored edition: 2.32 and 3.0 both print
    // the ranges gamut carries.
    ("LightSource", 13, "Day white fluorescent (N 4600 - 5400K)"),
    ("LightSource", 14, "Cool white fluorescent (W 3900 - 4500K)"),
    ("LightSource", 15, "White fluorescent (WW 3200 - 3700K)"),
    // ResolutionUnit's abbreviations again, through the shared table.
    ("FocalPlaneResolutionUnit", 2, "inch"),
    ("FocalPlaneResolutionUnit", 3, "cm"),
    // exiv2 drops DC-008's "sensor".
    ("SensingMethod", 2, "One-chip color area"),
    ("SensingMethod", 3, "Two-chip color area"),
    ("SensingMethod", 4, "Three-chip color area"),
    ("SensingMethod", 5, "Color sequential area"),
    ("SensingMethod", 8, "Color sequential linear"),
    // exiv2 names the device; DC-008 §4.6.6.7.32 describes it.
    ("FileSource", 1, "Film scanner"),
    ("FileSource", 2, "Reflexion print scanner"),
    ("FileSource", 3, "Digital still camera"),
    ("SceneType", 1, "Directly photographed"),
    // exiv2 drops the noun DC-008 qualifies.
    ("ExposureMode", 0, "Auto"),
    ("ExposureMode", 1, "Manual"),
    ("WhiteBalance", 0, "Auto"),
    ("WhiteBalance", 1, "Manual"),
    ("Saturation", 1, "Low"),
    ("Saturation", 2, "High"),
    // exiv2 renders a CamelCase identifier rather than DC-008 §4.6.6.7.47's phrase.
    ("CompositeImage", 1, "NonComposite"),
    ("CompositeImage", 2, "GeneralComposite"),
    ("CompositeImage", 3, "CompositeCapturedWhenShooting"),
    // exiv2 drops "latitude"/"longitude", for both the shooting location and the destination.
    ("GPSLatitudeRef", 78, "North"),
    ("GPSLatitudeRef", 83, "South"),
    ("GPSLongitudeRef", 69, "East"),
    ("GPSLongitudeRef", 87, "West"),
    // exiv2 keeps Exif 2.32's sea-level reading of 0 and 1; Exif 3.0 §4.6.7.1.6 made both
    // ellipsoidal and moved sea level to 2 and 3 (the domain divergence above).
    ("GPSAltitudeRef", 0, "Above sea level"),
    ("GPSAltitudeRef", 1, "Below sea level"),
    // exiv2 abbreviates the units.
    ("GPSSpeedRef", 75, "km/h"),
    ("GPSSpeedRef", 77, "mph"),
    // The destination's references, read as the shooting location's are above.
    ("GPSDestLatitudeRef", 78, "North"),
    ("GPSDestLatitudeRef", 83, "South"),
    ("GPSDestLongitudeRef", 69, "East"),
    ("GPSDestLongitudeRef", 87, "West"),
    // exiv2 abbreviates kilometres; its "miles" and "nautical miles" match ignoring case.
    ("GPSDestDistanceRef", 75, "km"),
    // exiv2 drops "differential" and "measurement".
    ("GPSDifferential", 0, "Without correction"),
    ("GPSDifferential", 1, "Correction applied"),
];

/// A described tag and the codes exiv2 interprets for it, each with exiv2's text.
type Reading = (ExifTag, Vec<(u32, String)>);

/// Every described tag's exiv2 reading, swept once and shared by the two tests.
fn readings() -> &'static [Reading] {
    static READINGS: std::sync::OnceLock<Vec<Reading>> = std::sync::OnceLock::new();
    READINGS.get_or_init(|| {
        described_tags()
            .map(|tag| {
                let interpreted = every_code(tag)
                    .filter_map(|code| exiv2_reading(tag, code).map(|text| (code, text)))
                    .collect();
                (tag, interpreted)
            })
            .collect()
    })
}

#[test]
fn exiv2_interprets_the_codes_dc008_defines_up_to_the_pinned_divergences() {
    let mut divergences = Vec::new();
    for (tag, interpreted) in readings() {
        let ours: Vec<u32> = described_values(*tag).iter().map(|&(c, _)| c).collect();
        let only_exiv2: Vec<u32> = interpreted
            .iter()
            .map(|&(c, _)| c)
            .filter(|c| !ours.contains(c))
            .collect();
        let only_ours: Vec<u32> = ours
            .iter()
            .copied()
            .filter(|c| !interpreted.iter().any(|(theirs, _)| theirs == c))
            .collect();
        if !only_exiv2.is_empty() || !only_ours.is_empty() {
            divergences.push((tag.name(), only_exiv2, only_ours));
        }
    }

    let pinned: Vec<(&str, Vec<u32>, Vec<u32>)> = DOMAIN_DIVERGENCES
        .iter()
        .map(|&(tag, theirs, ours)| (tag, theirs.to_vec(), ours.to_vec()))
        .collect();
    assert_eq!(
        divergences, pinned,
        "the codes exiv2 and DC-008 disagree on changed: (tag, exiv2 only, DC-008 only)"
    );
}

#[test]
fn exiv2_words_each_shared_code_as_dc008_does_up_to_the_pinned_divergences() {
    let mut divergences = Vec::new();
    for (tag, interpreted) in readings() {
        for &(code, ours) in described_values(*tag) {
            let Some((_, theirs)) = interpreted.iter().find(|&&(c, _)| c == code) else {
                continue; // a domain divergence, pinned by the other test
            };
            if !theirs.eq_ignore_ascii_case(ours) {
                divergences.push((tag.name(), code, theirs.as_str()));
            }
        }
    }

    assert_eq!(
        divergences, WORDING_DIVERGENCES,
        "the codes exiv2 words differently from DC-008 changed: (tag, code, exiv2's text)"
    );
}
