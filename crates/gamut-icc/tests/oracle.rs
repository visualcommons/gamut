//! Differential conformance tests for `gamut-icc` against the reference CMM (Little-CMS), via the
//! dev-only [`lcms2_oracle`] crate. Profiles are synthesized in memory by lcms2, so no binary
//! `.icc` fixtures are committed; gamut-icc decodes the same bytes and the decoded values are
//! asserted equal to what lcms2 reports.

use gamut_color::transfer::{pq_eotf, pq_oetf, srgb_oetf};
use gamut_icc::{
    BuiltinProfile, IccProfile, KnownTag, ProfileHeader, ProfileId, S15Fixed16, Signature, TagData,
};
use lcms2_oracle::{
    FLAGS_NOOPTIMIZE, INTENT_ABSOLUTE_COLORIMETRIC, INTENT_PERCEPTUAL,
    INTENT_RELATIVE_COLORIMETRIC, TYPE_RGB_DBL, TYPE_XYZ_DBL, Transform, tag,
};

/// The Rec.709/sRGB primaries and D65 white point, for synthesizing matrix/TRC profiles.
const D65: [f64; 2] = [0.3127, 0.3290];
const REC709_PRIMARIES: [[f64; 2]; 3] = [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06]];

/// The header's PCS illuminant (ICC.1:2022 §7.2.16) is mandated to be D50; lcms2 writes exactly
/// that. Decoding those three `s15Fixed16` fields from a real lcms2-produced profile exercises the
/// fixed-point conversion against an independent reference.
#[test]
fn pcs_illuminant_is_d50() {
    let bytes = lcms2_oracle::srgb().to_bytes();
    assert!(bytes.len() >= 128, "profile shorter than its header");

    // The PCS illuminant lives at byte offset 68: X, Y, Z as consecutive s15Fixed16 values.
    let component = |offset: usize| -> f64 {
        let raw = i32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]);
        S15Fixed16(raw).to_f64()
    };
    let (x, y, z) = (component(68), component(72), component(76));

    // D50 ≈ (0.9642, 1.0000, 0.8249) — within one s15Fixed16 quantum.
    assert!((x - 0.9642).abs() < 1.0e-3, "illuminant X = {x}");
    assert!((y - 1.0).abs() < 1.0e-3, "illuminant Y = {y}");
    assert!((z - 0.8249).abs() < 1.0e-3, "illuminant Z = {z}");
}

/// Header fields decoded by gamut-icc match what the reference CMM reports, across a spread of
/// synthesized profiles. Comparing against lcms's own getters (rather than hard-coded values) keeps
/// the test honest and pins every modelled header field's offset and decoder.
#[test]
fn header_fields_match_lcms() {
    let profiles = [
        ("srgb", lcms2_oracle::srgb()),
        (
            "rgb",
            lcms2_oracle::rgb_matrix_shaper(
                [0.3127, 0.3290],
                [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06]],
                [2.2, 2.2, 2.2],
            ),
        ),
        ("gray", lcms2_oracle::gray([0.3127, 0.3290], 2.2)),
        ("xyz", lcms2_oracle::xyz()),
        ("lab4", lcms2_oracle::lab4()),
    ];
    for (label, profile) in profiles {
        let bytes = profile.to_bytes();
        let h = ProfileHeader::parse(&bytes).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        assert_eq!(
            u32::from(Signature::from(h.device_class)),
            profile.device_class(),
            "{label}: device class"
        );
        assert_eq!(
            u32::from(Signature::from(h.data_color_space)),
            profile.color_space(),
            "{label}: data colour space"
        );
        assert_eq!(
            u32::from(Signature::from(h.pcs)),
            profile.pcs(),
            "{label}: pcs"
        );
        assert_eq!(
            u32::from(h.rendering_intent),
            profile.rendering_intent(),
            "{label}: rendering intent"
        );
        let version = f64::from(h.version.major) + f64::from(h.version.minor) / 10.0;
        assert!(
            (version - profile.version()).abs() < 0.05,
            "{label}: version {version} vs lcms {}",
            profile.version()
        );
    }
}

/// A v2 profile decodes its major/minor version correctly (the legacy path lcms emits when the
/// version is forced down).
#[test]
fn parses_v2_profile_version() {
    let profile = lcms2_oracle::srgb();
    profile.set_version(2.1);
    let bytes = profile.to_bytes();
    let h = ProfileHeader::parse(&bytes).unwrap();
    assert_eq!(h.version.major, 2);
    assert_eq!(h.version.minor, 1);
}

/// gamut-icc finds exactly the tags the reference CMM reports, and decodes each (currently as `Raw`
/// with its type signature matching the element's leading four bytes).
#[test]
fn tag_set_matches_lcms() {
    let profiles = [
        ("srgb", lcms2_oracle::srgb()),
        ("gray", lcms2_oracle::gray([0.3127, 0.3290], 2.2)),
        ("lab4", lcms2_oracle::lab4()),
    ];
    for (label, profile) in profiles {
        let bytes = profile.to_bytes();
        let parsed = IccProfile::parse(&bytes).unwrap_or_else(|e| panic!("{label}: {e:?}"));

        let mut got: Vec<u32> = parsed.tags.iter().map(|(s, _)| u32::from(*s)).collect();
        let mut want: Vec<u32> = (0..profile.tag_count())
            .map(|i| profile.tag_signature(i))
            .collect();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want, "{label}: tag signature set");

        for (_, data) in &parsed.tags {
            if let TagData::Raw { type_sig, bytes } = data {
                assert_eq!(&type_sig.0, &bytes[0..4], "{label}: raw type signature");
            }
        }
    }
}

/// The XYZ colorant/white-point tags decode to the same tristimulus values lcms reads back.
#[test]
fn xyz_tags_match_lcms() {
    let profile = lcms2_oracle::srgb();
    let parsed = IccProfile::parse(&profile.to_bytes()).unwrap();
    for tagsig in [
        tag::MEDIA_WHITE_POINT,
        tag::RED_COLORANT,
        tag::GREEN_COLORANT,
        tag::BLUE_COLORANT,
    ] {
        let data = parsed.get(Signature::from(tagsig)).expect("tag present");
        let TagData::Xyz(values) = data else {
            panic!("expected XYZ for {tagsig:#010x}");
        };
        let ours = values[0].to_f64();
        let theirs = profile.read_xyz(tagsig).expect("lcms reads XYZ");
        for k in 0..3 {
            assert!(
                (ours[k] - theirs[k]).abs() < 2.0 / 65536.0,
                "{tagsig:#010x}[{k}]: {} vs {}",
                ours[k],
                theirs[k]
            );
        }
    }
}

/// Decoded tone curves evaluate to the same values as lcms, for both a parametric (sRGB) and a
/// pure-gamma TRC. Sampling `eval` at points across [0, 1] pins the curve formulas.
#[test]
fn tone_curves_match_lcms() {
    let cases = [
        ("srgb", lcms2_oracle::srgb()),
        (
            "gamma2.2",
            lcms2_oracle::rgb_matrix_shaper(D65, REC709_PRIMARIES, [2.2, 2.2, 2.2]),
        ),
    ];
    for (label, profile) in cases {
        let parsed = IccProfile::parse(&profile.to_bytes()).unwrap();
        let data = parsed
            .get(Signature::from(tag::RED_TRC))
            .expect("rTRC present");
        for i in 0..=16 {
            let x = f64::from(i) / 16.0;
            let ours = eval_curve(data, x);
            let theirs = f64::from(
                profile
                    .eval_tone_curve(tag::RED_TRC, x as f32)
                    .expect("lcms evaluates the curve"),
            );
            assert!(
                (ours - theirs).abs() < 2.0e-3,
                "{label} @ {x}: {ours} vs {theirs}"
            );
        }
    }
}

/// A non-D50 (here D65) matrix profile carries a chromatic-adaptation matrix, stored as an
/// `s15Fixed16ArrayType` of nine values (the 3×3 matrix).
#[test]
fn chad_decodes_as_s15fixed16_array() {
    let profile = lcms2_oracle::rgb_matrix_shaper(D65, REC709_PRIMARIES, [2.2, 2.2, 2.2]);
    let parsed = IccProfile::parse(&profile.to_bytes()).unwrap();
    let data = parsed
        .get(Signature::from(tag::CHROMATIC_ADAPTATION))
        .expect("chad present");
    let TagData::S15Fixed16Array(values) = data else {
        panic!("expected sf32 chad");
    };
    assert_eq!(values.len(), 9);
}

/// The profile description decodes to the same text lcms reads back, in both the v4
/// (`multiLocalizedUnicode`) and v2 (`textDescription`) representations.
#[test]
fn descriptions_match_lcms() {
    // v4: 'desc' is a multiLocalizedUnicode element.
    let profile = lcms2_oracle::srgb();
    let parsed = IccProfile::parse(&profile.to_bytes()).unwrap();
    let ours = match parsed
        .get(Signature::from(tag::PROFILE_DESCRIPTION))
        .expect("desc present")
    {
        TagData::MultiLocalizedUnicode(mluc) => mluc
            .text(b"en", b"US")
            .or_else(|| mluc.first())
            .expect("a description record")
            .to_owned(),
        other => panic!("expected mluc, got {other:?}"),
    };
    let theirs = profile
        .read_mlu_ascii(tag::PROFILE_DESCRIPTION, b"en", b"US")
        .expect("lcms reads desc");
    assert_eq!(ours, theirs);

    // v2: the same description is stored as a textDescription element.
    let profile = lcms2_oracle::srgb();
    profile.set_version(2.1);
    let parsed = IccProfile::parse(&profile.to_bytes()).unwrap();
    let ours = match parsed
        .get(Signature::from(tag::PROFILE_DESCRIPTION))
        .expect("desc present")
    {
        TagData::TextDescription(desc) => desc.ascii.clone(),
        other => panic!("expected textDescription, got {other:?}"),
    };
    let theirs = profile
        .read_mlu_ascii(tag::PROFILE_DESCRIPTION, b"en", b"US")
        .expect("lcms reads desc");
    assert_eq!(ours, theirs);
}

/// A complete matrix/TRC RGB profile decodes end to end: every tag a baseline display profile
/// carries resolves to its modelled element type (none falls back to `Raw`), reached via the
/// ergonomic [`KnownTag`] API.
#[test]
fn matrix_trc_profile_decodes_every_tag() {
    let profile = lcms2_oracle::rgb_matrix_shaper(D65, REC709_PRIMARIES, [2.2, 2.2, 2.2]);
    let parsed = IccProfile::parse(&profile.to_bytes()).unwrap();
    let get = |t: KnownTag| parsed.get(t);

    for t in [
        KnownTag::MediaWhitePoint,
        KnownTag::RedColorant,
        KnownTag::GreenColorant,
        KnownTag::BlueColorant,
    ] {
        assert!(
            matches!(get(t), Some(TagData::Xyz(_))),
            "{t:?} should be XYZ"
        );
    }
    for t in [KnownTag::RedTrc, KnownTag::GreenTrc, KnownTag::BlueTrc] {
        assert!(
            matches!(
                get(t),
                Some(TagData::Curve(_) | TagData::ParametricCurve(_))
            ),
            "{t:?} should be a tone curve"
        );
    }
    assert!(
        matches!(get(KnownTag::ChromaticAdaptation), Some(TagData::S15Fixed16Array(v)) if v.len() == 9)
    );
    for t in [KnownTag::ProfileDescription, KnownTag::Copyright] {
        assert!(
            matches!(get(t), Some(TagData::MultiLocalizedUnicode(_))),
            "{t:?} should be mluc"
        );
    }

    // chromaticityType decodes semantically: lcms writes three phosphor channels here.
    assert!(matches!(
        get(KnownTag::Chromaticity),
        Some(TagData::Chromaticity(chrm)) if chrm.channels.len() == 3
    ));

    // Every tag the profile carries is now a modelled (non-Raw) element.
    let undecoded: Vec<String> = parsed
        .tags
        .iter()
        .filter(|(_, data)| matches!(data, TagData::Raw { .. }))
        .map(|(sig, _)| sig.to_string())
        .collect();
    assert!(
        undecoded.is_empty(),
        "unexpected undecoded tags: {undecoded:?}"
    );
}

/// A v2 CMYK device link stores its transform as a `lut16` (`mft2`) `A2B0` tag, which decodes with
/// the expected 4→4 channel geometry.
#[test]
fn v2_devicelink_decodes_as_lut() {
    let profile = lcms2_oracle::cmyk_ink_limiting_devicelink(250.0);
    profile.set_version(2.1);
    let parsed = IccProfile::parse(&profile.to_bytes()).unwrap();
    match parsed
        .get(Signature::from(tag::A_TO_B0))
        .expect("A2B0 present")
    {
        TagData::Lut16(lut) => {
            assert_eq!(lut.input_channels, 4);
            assert_eq!(lut.output_channels, 4);
        }
        TagData::Lut8(lut) => {
            assert_eq!(lut.input_channels, 4);
            assert_eq!(lut.output_channels, 4);
        }
        other => panic!("expected a lut8/lut16 A2B0, got {other:?}"),
    }
}

/// A v4 RGB device link stores its transform as a `lutAToB` (`mAB `). (Some lcms builds emit a
/// `multiProcessElements` instead, which is intentionally not modelled and round-trips as Raw.)
#[test]
fn v4_devicelink_decodes_as_lut_a_to_b() {
    let profile = lcms2_oracle::rgb_linearization_devicelink();
    let parsed = IccProfile::parse(&profile.to_bytes()).unwrap();
    match parsed
        .get(Signature::from(tag::A_TO_B0))
        .expect("A2B0 present")
    {
        TagData::LutAToB(lut) => {
            assert_eq!(lut.input_channels, 3);
            assert_eq!(lut.output_channels, 3);
            assert_eq!(lut.b_curves.len(), 3);
        }
        TagData::Raw { type_sig, .. } => assert_eq!(&type_sig.0, b"mpet"),
        other => panic!("unexpected A2B0 element: {other:?}"),
    }
}

/// The profile ID gamut-icc computes (§7.2.18 MD5) matches the one the reference CMM computes over
/// the same serialized bytes.
#[test]
fn profile_id_matches_lcms() {
    let profile = lcms2_oracle::srgb();
    let id_lcms = profile.compute_md5_id(); // lcms computes and stores the ID
    let bytes = profile.to_bytes(); // serialization now carries that ID in the header
    let id_ours = ProfileId::compute(&bytes);
    assert_eq!(id_ours.0, id_lcms);
}

/// The `IccReader` façade parses through both its lenient default and strict mode, and strict mode
/// actually tightens parsing: a non-conformant profile (a nonzero reserved header byte) that the
/// lenient default accepts is rejected under strict.
#[test]
fn reader_facade_lenient_and_strict() {
    let bytes = lcms2_oracle::srgb().to_bytes();
    assert!(gamut_icc::IccReader::new().parse(&bytes).is_ok());
    assert!(
        gamut_icc::IccReader::new()
            .strict(true)
            .parse(&bytes)
            .is_ok()
    );

    let mut poked = bytes.clone();
    poked[100] = 1; // a reserved header byte (offset 100..128)
    assert!(gamut_icc::IccReader::new().parse(&poked).is_ok());
    assert!(
        gamut_icc::IccReader::new()
            .strict(true)
            .parse(&poked)
            .is_err()
    );
}

/// Evaluates whichever tone-curve element a tag holds.
fn eval_curve(data: &TagData, x: f64) -> f64 {
    match data {
        TagData::Curve(curve) => curve.eval(x),
        TagData::ParametricCurve(curve) => curve.eval(x),
        other => panic!("expected a tone curve, got {other:?}"),
    }
}

/// A `measurementType` tag written by lcms2 decodes to the same fields in gamut-icc — an independent
/// check of the §10.14 field layout (observer, backing, geometry, flare, illuminant).
#[test]
fn measurement_tag_matches_lcms() {
    let bytes = lcms2_oracle::measurement(1, [0.72, 0.75, 0.62], 1, 0.01, 1).to_bytes();
    let profile = IccProfile::parse(&bytes).expect("parse lcms measurement profile");
    let Some(TagData::Measurement(meas)) = profile.get(Signature(*b"meas")) else {
        panic!("expected a decoded measurement tag");
    };
    assert_eq!(meas.observer, 1); // CIE 1931
    assert_eq!(meas.geometry, 1); // 0°:45°/45°:0°
    assert_eq!(meas.illuminant, 1); // D50
    assert!(
        (meas.flare.to_f64() - 0.01).abs() < 1.0e-3,
        "flare = {}",
        meas.flare.to_f64()
    );
    let backing = meas.backing.to_f64();
    assert!(
        (backing[1] - 0.75).abs() < 1.0e-3,
        "backing Y = {}",
        backing[1]
    );
}

/// A `viewingConditionsType` tag written by lcms2 decodes to the same illuminant/surround/type
/// (§10.30).
#[test]
fn viewing_conditions_tag_matches_lcms() {
    let bytes = lcms2_oracle::viewing_conditions([19.0, 20.0, 21.0], [3.0, 3.5, 4.0], 1).to_bytes();
    let profile = IccProfile::parse(&bytes).expect("parse lcms viewing profile");
    let Some(TagData::ViewingConditions(view)) = profile.get(Signature(*b"view")) else {
        panic!("expected a decoded viewingConditions tag");
    };
    let illuminant = view.illuminant.to_f64();
    let surround = view.surround.to_f64();
    assert!(
        (illuminant[0] - 19.0).abs() < 1.0e-2,
        "illuminant = {illuminant:?}"
    );
    assert!(
        (surround[2] - 4.0).abs() < 1.0e-2,
        "surround = {surround:?}"
    );
    assert_eq!(view.illuminant_type, 1);
}

/// A `cicpType` tag written by lcms2 decodes to the same four H.273 code points (§10.3).
#[test]
fn cicp_tag_matches_lcms() {
    // 9-16-0-1 — PQ Rec.2100 full-range R'G'B'.
    let bytes = lcms2_oracle::cicp(9, 16, 0, 1).to_bytes();
    let profile = IccProfile::parse(&bytes).expect("parse lcms cicp profile");
    let Some(TagData::Cicp(cicp)) = profile.get(Signature(*b"cicp")) else {
        panic!("expected a decoded cicp tag");
    };
    assert_eq!(cicp.colour_primaries, 9);
    assert_eq!(cicp.transfer_characteristics, 16);
    assert_eq!(cicp.matrix_coefficients, 0);
    assert_eq!(cicp.video_full_range_flag, 1);
}

/// Every synthesizer added for the transform-oracle work (issue #322) produces a profile that
/// gamut-icc parses without error and round-trips: re-serializing the parsed model yields bytes
/// that parse to the same tag set and that lcms2 itself re-accepts.
#[test]
fn transform_oracle_corpus_parses_and_round_trips() {
    lcms2_oracle::set_quiet_log_handler();
    let profiles = [
        (
            "scnr_matrix_shaper",
            lcms2_oracle::scnr_matrix_shaper(D65, REC709_PRIMARIES, [2.2, 2.2, 2.2]),
        ),
        ("scnr_lut", lcms2_oracle::scnr_lut(9)),
        ("cmyk_prtr_v4", lcms2_oracle::cmyk_prtr_v4(9)),
        ("cmyk_prtr_v2", lcms2_oracle::cmyk_prtr_v2(9)),
        ("display_p3_srgb_trc", lcms2_oracle::display_p3_srgb_trc()),
        (
            "rgb_matrix_shaper_v2_chad",
            lcms2_oracle::rgb_matrix_shaper_v2(true, D65, REC709_PRIMARIES, [2.2, 2.2, 2.2]),
        ),
        (
            "rgb_matrix_shaper_v2_no_chad",
            lcms2_oracle::rgb_matrix_shaper_v2(false, D65, REC709_PRIMARIES, [2.2, 2.2, 2.2]),
        ),
        (
            "rgb_matrix_shaper_d65_wtpt",
            lcms2_oracle::rgb_matrix_shaper_d65_wtpt(D65, REC709_PRIMARIES, [2.2, 2.2, 2.2]),
        ),
    ];
    for (label, profile) in profiles {
        let bytes = profile.to_bytes();
        let parsed = IccProfile::parse(&bytes).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        let rewritten = parsed
            .to_bytes()
            .unwrap_or_else(|e| panic!("{label}: serialize: {e:?}"));
        let reparsed =
            IccProfile::parse(&rewritten).unwrap_or_else(|e| panic!("{label}: reparse: {e:?}"));
        let sigs =
            |p: &IccProfile| -> Vec<u32> { p.tags.iter().map(|(s, _)| u32::from(*s)).collect() };
        assert_eq!(sigs(&parsed), sigs(&reparsed), "{label}: tag set survives");
        assert!(
            lcms2_oracle::Profile::from_bytes(&rewritten).is_some(),
            "{label}: lcms2 rejects gamut-icc's re-serialization"
        );
    }
}

/// The `prtr` synthesizers' A2B0 element type follows the profile version the tag was written
/// under: `lut16` for the v2 build, `lutAToB` for the v4 build.
#[test]
fn prtr_lut_element_type_follows_version() {
    lcms2_oracle::set_quiet_log_handler();
    let v2 = IccProfile::parse(&lcms2_oracle::cmyk_prtr_v2(9).to_bytes()).unwrap();
    match v2.get(Signature::from(tag::A_TO_B0)).expect("A2B0 present") {
        TagData::Lut16(lut) => {
            assert_eq!(lut.input_channels, 4);
            assert_eq!(lut.output_channels, 3);
        }
        other => panic!("expected a lut16 A2B0 in the v2 prtr, got {other:?}"),
    }

    let v4 = IccProfile::parse(&lcms2_oracle::cmyk_prtr_v4(9).to_bytes()).unwrap();
    match v4.get(Signature::from(tag::A_TO_B0)).expect("A2B0 present") {
        TagData::LutAToB(lut) => {
            assert_eq!(lut.input_channels, 4);
            assert_eq!(lut.output_channels, 3);
        }
        other => panic!("expected a lutAToB A2B0 in the v4 prtr, got {other:?}"),
    }
}

/// A v2 device link written by lcms2 carries `profileSequenceDescType` and
/// `profileSequenceIdentifierType` tags whose descriptions are the legacy `textDescriptionType`;
/// gamut-icc decodes both (walking the 4-byte-aligned embedded descriptions) rather than failing.
#[test]
fn devicelink_sequence_tags_decode() {
    let profile = lcms2_oracle::cmyk_ink_limiting_devicelink(250.0);
    profile.set_version(2.1);
    let parsed = IccProfile::parse(&profile.to_bytes()).expect("parse lcms v2 device link");

    let Some(TagData::ProfileSequenceDesc(pseq)) = parsed.get(Signature(*b"pseq")) else {
        panic!("expected a decoded profileSequenceDesc tag");
    };
    assert!(
        !pseq.entries.is_empty(),
        "pseq should have at least one entry"
    );

    let Some(TagData::ProfileSequenceIdentifier(psid)) = parsed.get(Signature(*b"psid")) else {
        panic!("expected a decoded profileSequenceIdentifier tag");
    };
    assert!(
        !psid.entries.is_empty(),
        "psid should have at least one entry"
    );
}

/// The BT.2408 HDR reference white, 203 cd/m², as the ST 2084 signal that encodes it.
fn pq_diffuse_white_signal() -> f64 {
    pq_oetf(203.0)
}

/// The built-in BT.2100 PQ profile, opened by lcms2 from the bytes this crate serializes.
fn builtin_pq_opened_by_lcms() -> lcms2_oracle::Profile {
    let bytes = IccProfile::builtin(BuiltinProfile::Bt2020Pq)
        .expect("a modelled space")
        .to_bytes()
        .expect("serializes");
    lcms2_oracle::Profile::from_bytes(&bytes).expect("lcms2 opens the built-in PQ profile")
}

/// Issue #557's measurement: lcms2 transforms a diffuse-white PQ signal through
/// `builtin(Bt2020Pq)` into lcms2's own XYZ identity profile and lands it at
/// `pq_eotf(V) / pq_eotf(1)` = 203 / 10 000 = 0.0203 of lcms2's own D50 media white — the
/// peak-referred normalization the issue decided to keep.
///
/// The intent is stated, and the answer is the same under every intent tried. **Media-relative
/// colorimetric** is the one the measurement is about. **ICC-absolute colorimetric** agrees because
/// the profile's `mediaWhitePointTag` *is* the PCS D50, so absolute scaling is the identity — the
/// peak is media white in either reading. **Perceptual**, the header's own default, agrees because
/// a matrix/TRC profile carries no perceptual table and lcms2 falls back to the colorimetric
/// matrix/TRC. A diffuse-white-referred profile would instead land this signal on 1.0, so the
/// test tells the two normalizations apart by a factor of 49.
///
/// The tolerance is two `uInt16` quanta of `Y`: the sampled tone curve interpolates to within one
/// (see `SAMPLED_TRC_POINTS` in `src/builtin.rs`), and the `s15Fixed16` colorants round on top.
/// Measured: `Y` = 0.020294, 5.8e-6 (0.38 quanta) under 0.0203, identically under all three
/// intents.
#[test]
fn oracle_pq_diffuse_white_lands_at_its_share_of_the_peak() {
    let ours = builtin_pq_opened_by_lcms();
    let pcs = lcms2_oracle::xyz();
    let white = lcms2_oracle::srgb()
        .read_xyz(tag::MEDIA_WHITE_POINT)
        .expect("lcms2's own D50 media white");
    let signal = pq_diffuse_white_signal();
    let share = pq_eotf(signal) / pq_eotf(1.0);
    assert!((share - 0.0203).abs() < 1e-9, "share of the peak: {share}");
    for (label, intent) in [
        ("media-relative colorimetric", INTENT_RELATIVE_COLORIMETRIC),
        ("ICC-absolute colorimetric", INTENT_ABSOLUTE_COLORIMETRIC),
        ("perceptual", INTENT_PERCEPTUAL),
    ] {
        let xyz = Transform::new(
            &ours,
            TYPE_RGB_DBL,
            &pcs,
            TYPE_XYZ_DBL,
            intent,
            FLAGS_NOOPTIMIZE,
        )
        .apply_f64(&[signal; 3], 1, 3);
        for axis in 0..3 {
            assert!(
                (xyz[axis] - share * white[axis]).abs() < 2.0 / 65535.0,
                "{label} [{axis}]: {xyz:?} vs {share} × {white:?}"
            );
        }
    }
}

/// What issue #557's peak-referred profile means for an SDR CMM: lcms2 transforms the same
/// diffuse-white PQ signal through `builtin(Bt2020Pq)` into lcms2's own sRGB profile, at
/// media-relative colorimetric intent, and writes the neutral sRGB value that encodes 0.0203 of
/// sRGB white — `srgb_oetf(0.0203)` ≈ 0.153, code 39 of 255, a dark grey where SDR-referred
/// content expects white. That is why the documented route to an SDR rendering is a tone map and
/// an SDR profile, not this one.
///
/// The tolerance is 1e-4 in the sRGB signal: lcms2 inverts its own `s15Fixed16`-rounded
/// parametric sRGB curve rather than `gamut-color`'s closed form, and that difference plus the
/// first test's 0.38 quanta measure 2.5e-5 here (0.152965 against 0.152990), while the
/// 0.0203-versus-1.0 question moves this value by 0.85.
#[test]
fn oracle_pq_diffuse_white_is_dark_grey_in_lcms_srgb() {
    let ours = builtin_pq_opened_by_lcms();
    let srgb = lcms2_oracle::srgb();
    let signal = pq_diffuse_white_signal();
    let want = srgb_oetf(pq_eotf(signal) / pq_eotf(1.0));
    let rgb = Transform::new(
        &ours,
        TYPE_RGB_DBL,
        &srgb,
        TYPE_RGB_DBL,
        INTENT_RELATIVE_COLORIMETRIC,
        FLAGS_NOOPTIMIZE,
    )
    .apply_f64(&[signal; 3], 1, 3);
    for (channel, got) in ["r", "g", "b"].into_iter().zip(rgb) {
        assert!(
            (got - want).abs() < 1e-4,
            "{channel}: {got} vs srgb_oetf(0.0203) = {want}"
        );
    }
}
