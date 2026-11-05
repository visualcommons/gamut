//! Round-trip and serialization conformance for `gamut-icc`.
//!
//! Parsing then re-serializing must preserve the model, and the reference CMM (Little-CMS) must
//! accept gamut-icc's serialization as an equivalent profile — the conformance gate the crate
//! documents. The corpus is synthesized by lcms in memory (no committed binary fixtures).

use gamut_icc::{
    ColorSpace, DeviceClass, IccProfile, IccReader, IccWriter, ProfileHeader, ProfileId, Signature,
    Tag, TagData, XyzNumber,
};

const D65: [f64; 2] = [0.3127, 0.3290];
const REC709_PRIMARIES: [[f64; 2]; 3] = [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06]];

/// A spread of profile shapes: matrix/TRC RGB and grey, the PCS profiles, and a v2 CMYK device link
/// (a CLUT-bearing LUT profile).
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let cmyk = lcms2_oracle::cmyk_ink_limiting_devicelink(250.0);
    cmyk.set_version(2.1);
    vec![
        ("srgb", lcms2_oracle::srgb().to_bytes()),
        (
            "rgb_d65",
            lcms2_oracle::rgb_matrix_shaper(D65, REC709_PRIMARIES, [2.2, 2.2, 2.2]).to_bytes(),
        ),
        ("gray", lcms2_oracle::gray(D65, 2.2).to_bytes()),
        ("xyz", lcms2_oracle::xyz().to_bytes()),
        ("lab4", lcms2_oracle::lab4().to_bytes()),
        ("lab2", lcms2_oracle::lab2().to_bytes()),
        ("cmyk_v2_devicelink", cmyk.to_bytes()),
    ]
}

/// Parsing, re-serializing, and re-parsing preserves the decoded model: the tags are identical and
/// the header matches (apart from the `size` field, which a re-layout may legitimately change).
///
/// The `to_bytes` expectations double as the fallible-write contract across the whole corpus: the
/// decoder establishes every invariant serialization checks, so a *parsed* profile never fails to
/// serialize.
#[test]
fn round_trip_preserves_the_model() {
    for (label, bytes) in corpus() {
        let parsed = IccProfile::parse(&bytes).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        let serialized = parsed
            .to_bytes()
            .unwrap_or_else(|e| panic!("{label}: a parsed profile must serialize: {e:?}"));
        let reparsed =
            IccProfile::parse(&serialized).unwrap_or_else(|e| panic!("{label} reparse: {e:?}"));

        assert_eq!(parsed.tags, reparsed.tags, "{label}: tags changed");

        let mut a = parsed.header.clone();
        let mut b = reparsed.header.clone();
        a.size = 0;
        b.size = 0;
        assert_eq!(a, b, "{label}: header changed");
    }
}

/// The serialized bytes are structurally well-formed: the `size` field equals the length, the total
/// is 4-byte aligned, and the `acsp` magic is in place.
#[test]
fn serialization_is_well_formed() {
    for (label, bytes) in corpus() {
        let out = IccProfile::parse(&bytes).unwrap().to_bytes().unwrap();
        let size = u32::from_be_bytes([out[0], out[1], out[2], out[3]]) as usize;
        assert_eq!(size, out.len(), "{label}: size field");
        assert!(out.len().is_multiple_of(4), "{label}: 4-byte aligned");
        assert_eq!(&out[36..40], b"acsp", "{label}: acsp magic");

        // The first tag's data lands exactly at the (4-byte-aligned) end of the tag table, pinning
        // the writer's offset arithmetic.
        let count = u32::from_be_bytes([out[128], out[129], out[130], out[131]]) as usize;
        if count > 0 {
            let first_offset =
                u32::from_be_bytes([out[136], out[137], out[138], out[139]]) as usize;
            assert_eq!(
                first_offset,
                (128 + 4 + 12 * count).next_multiple_of(4),
                "{label}: first tag data offset"
            );
        }
    }
}

/// The conformance gate: the reference CMM re-opens gamut-icc's serialization and reports the same
/// header colour spaces, device class and rendering intent.
#[test]
fn lcms_accepts_our_serialization() {
    for (label, bytes) in corpus() {
        let original = lcms2_oracle::Profile::from_bytes(&bytes).expect("lcms reads the original");
        let reserialized = IccProfile::parse(&bytes).unwrap().to_bytes().unwrap();
        let reopened = lcms2_oracle::Profile::from_bytes(&reserialized)
            .unwrap_or_else(|| panic!("{label}: lcms rejected our serialization"));

        assert_eq!(
            reopened.color_space(),
            original.color_space(),
            "{label}: colour space"
        );
        assert_eq!(reopened.pcs(), original.pcs(), "{label}: pcs");
        assert_eq!(
            reopened.device_class(),
            original.device_class(),
            "{label}: device class"
        );
        assert_eq!(
            reopened.rendering_intent(),
            original.rendering_intent(),
            "{label}: rendering intent"
        );
    }
}

/// An unmodelled tag (here a `multiProcessElementsType` element, which is out of scope) is
/// re-emitted byte-for-byte through a round-trip.
#[test]
fn raw_tags_round_trip_byte_exact() {
    // Start from a real profile, then inject an element of a type gamut-icc does not decode.
    let mut parsed = IccProfile::parse(&lcms2_oracle::srgb().to_bytes()).unwrap();
    let element = b"mpet\x00\x00\x00\x00opaque multiProcessElements payload".to_vec();
    parsed.tags.push(Tag::new(
        *b"mpet",
        TagData::Raw {
            type_sig: Signature(*b"mpet"),
            bytes: element.clone(),
        },
    ));

    let reparsed = IccProfile::parse(&parsed.to_bytes().unwrap()).unwrap();
    match reparsed.get(Signature(*b"mpet")) {
        Some(TagData::Raw { bytes, .. }) => assert_eq!(*bytes, element),
        other => panic!("expected a Raw mpet tag, got {other:?}"),
    }
}

/// A profile built from scratch — `ProfileHeader::new` plus a white-point tag — serializes to
/// spec-valid bytes that a *strict* re-parse accepts and decodes to the same model.
#[test]
fn from_scratch_profile_round_trips_strictly() {
    let profile = IccProfile {
        header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
        tags: vec![Tag::new(*b"wtpt", TagData::Xyz(vec![XyzNumber::D50]))],
    };
    let bytes = profile.to_bytes().expect("a fresh header must serialize");
    let reparsed = IccReader::new()
        .strict(true)
        .parse(&bytes)
        .expect("strict parse of a from-scratch profile");

    assert_eq!(reparsed.tags, profile.tags);
    let mut expected = profile.header.clone();
    expected.size = u32::try_from(bytes.len()).unwrap(); // the writer patches the size field
    assert_eq!(reparsed.header, expected);
}

/// A hand-built duplicate tag signature is rejected on write, mirroring the parser's §7.3 check —
/// serializing it would produce a profile that cannot be re-read.
#[test]
fn duplicate_tags_are_rejected_on_write() {
    let mut parsed = IccProfile::parse(&lcms2_oracle::srgb().to_bytes()).unwrap();
    let duplicate = parsed.tags[0].clone();
    parsed.tags.push(duplicate);
    assert!(parsed.to_bytes().is_err());
}

/// `IccWriter::recompute_profile_id` stamps a non-zero, self-consistent MD5 ID into the output.
#[test]
fn writer_recomputes_profile_id() {
    let parsed = IccProfile::parse(&lcms2_oracle::srgb().to_bytes()).unwrap();
    let out = IccWriter::new()
        .recompute_profile_id(true)
        .write(&parsed)
        .unwrap();

    let stamped: [u8; 16] = out[84..100].try_into().unwrap();
    assert_ne!(stamped, [0u8; 16], "the ID should be set");
    assert_eq!(stamped, ProfileId::compute(&out).0);
}
