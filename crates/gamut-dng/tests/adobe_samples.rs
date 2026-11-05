//! Decode conformance against **Adobe's official sample DNGs** (shipped inside the DNG SDK ZIP,
//! surfaced by `gamut_dng_oracle::sample_file`): gamut-dng's decoder must reproduce exactly what
//! the SDK itself reads from Adobe's own files. These are the ProRAW-shaped inputs — tiled,
//! JPEG-XL-compressed linear and Bayer raws.

use gamut_core::ErrorKind;
use gamut_dng::DngDecoder;

/// Decodes an Adobe sample with both gamut and the SDK and requires pixel-exact agreement.
fn assert_matches_adobe(name: &str) {
    let bytes = gamut_dng_oracle::sample_file(name).expect("sample DNG present");
    let gamut = DngDecoder::new().decode(&bytes).expect("gamut decode");
    let adobe = gamut_dng_oracle::read_raw_dng(&bytes).expect("adobe decode");
    assert_eq!(
        (
            gamut.raw.dimensions().width(),
            gamut.raw.dimensions().height(),
            u32::from(gamut.raw.samples_per_pixel())
        ),
        (adobe.width, adobe.height, adobe.planes),
        "{name}: geometry must agree"
    );
    // Adobe encoded these samples lossily (VarDCT), and JPEG XL conformance permits a tiny
    // per-sample tolerance between conforming decoders — jxl-rs and libjxl round the float
    // reconstruction independently. Lossless streams decode bit-exact, so this tolerance is
    // only ever consumed by lossy input; anything structural (interleave, tiling, depth) shifts
    // values by orders of magnitude more than one code.
    let mut diverging = 0usize;
    for (i, (&ours, &theirs)) in gamut.raw.samples().iter().zip(&adobe.samples).enumerate() {
        let diff = (i64::from(ours) - i64::from(theirs)).abs();
        assert!(
            diff <= 1,
            "{name}: sample {i} diverges beyond lossy rounding — gamut {ours} vs Adobe {theirs}"
        );
        diverging += usize::from(diff != 0);
    }
    // The two decoders must still agree almost everywhere (a systematic offset would not).
    assert!(
        diverging * 50 < adobe.samples.len(),
        "{name}: {diverging} of {} samples differ — beyond decoder rounding",
        adobe.samples.len()
    );
}

#[test]
fn decodes_adobe_jxl_linear_raw_integer_sample() {
    assert_matches_adobe("01_jxl_linear_raw_integer.dng");
    // Adobe's own file also passes the digest-enforcing validation (its stored
    // NewRawImageDigest is the lossy-compressed chunk digest the SDK recomputes).
    let bytes =
        gamut_dng_oracle::sample_file("01_jxl_linear_raw_integer.dng").expect("sample present");
    gamut_dng_oracle::validate_dng(&bytes).expect("Adobe sample must validate digest-clean");
}

#[test]
fn decodes_adobe_jxl_bayer_raw_integer_sample() {
    assert_matches_adobe("03_jxl_bayer_raw_integer.dng");
}

/// The fp16 JPEG XL sample must be rejected cleanly (float samples are deferred), naming the
/// reason — not panic, and not silently misdecode.
#[test]
fn rejects_adobe_jxl_float_sample_cleanly() {
    let bytes =
        gamut_dng_oracle::sample_file("02_jxl_linear_raw_float.dng").expect("sample DNG present");
    let err = DngDecoder::new().decode(&bytes).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(
        err.static_message()
            .is_some_and(|message| message.contains("floating-point")),
        "expected a floating-point rejection, got {err:?}"
    );
}

/// Adobe's gain-table-map samples (05–08) cover every gain `DataType`. Each must parse into the
/// typed model with the expected representation, and re-serialise **byte-exactly** — the
/// strongest possible layout gate (any field-order, width, or byte-order slip changes the
/// bytes). Note 08: float32 gains are v1-representable, and Adobe indeed stored that one as the
/// older ProfileGainTableMap (52525) tag.
#[test]
fn adobe_gain_map_samples_parse_typed_and_reserialise_byte_exact() {
    use gamut_dng::GainValues;
    type TypeCheck = fn(&GainValues) -> bool;
    let cases: [(&str, TypeCheck); 4] = [
        ("05_PGTM2_unsigned8.dng", |g| matches!(g, GainValues::U8(_))),
        ("06_PGTM2_unsigned16.dng", |g| {
            matches!(g, GainValues::U16(_))
        }),
        ("07_PGTM2_float16.dng", |g| matches!(g, GainValues::F16(_))),
        ("08_PGTM2_float32.dng", |g| matches!(g, GainValues::F32(_))),
    ];
    for (name, is_expected_type) in cases {
        let bytes = gamut_dng_oracle::sample_file(name).expect("sample DNG present");
        let decoded = DngDecoder::new().decode(&bytes).expect("decode");
        let (map, tag, v2) = match (&decoded.gain_table_map2, &decoded.gain_table_map) {
            (Some(map), _) => (map, 52544u16, true),
            (None, Some(map)) => (map, 52525u16, false),
            (None, None) => panic!("{name}: a gain-table map must be surfaced"),
        };
        assert!(is_expected_type(&map.gains), "{name}: wrong gain data type");
        assert_eq!(
            map.gains.len() as u32,
            map.points_v * map.points_h * map.points_n,
            "{name}: gain count"
        );

        // Byte-exact against the original tag payload (these samples keep the raw image in
        // IFD 0 itself, so both tags are found there).
        let file = gamut_ifd::read(&bytes).expect("parse");
        let payload = file.ifds[0]
            .get(tag)
            .and_then(gamut_ifd::Value::as_bytes)
            .expect("gain-map tag present");
        let serialised = if v2 {
            map.to_bytes_v2(file.order)
        } else {
            map.to_bytes_v1(file.order)
        }
        .expect("serialise");
        assert_eq!(
            serialised, payload,
            "{name}: re-serialisation must be byte-exact"
        );
    }
}

/// Adobe's ImageSequenceInfo (09–11) and ImageStats (12–13) samples decode, and those DNG 1.7
/// tags — which have no typed surface yet — appear verbatim in the extras with typed values
/// (issue #109's "all metadata explicitly represented, even if unexpected").
#[test]
fn adobe_sequence_and_stats_samples_surface_untyped_tags() {
    for (name, tag) in [
        ("09_ImageSequenceInfo_1_of_3.dng", 52548u16),
        ("10_ImageSequenceInfo_2_of_3.dng", 52548),
        ("11_ImageSequenceInfo_3_of_3.dng", 52548),
        ("12_ImageStats_WeightedAverage.dng", 52550),
        ("13_ImageStats_Several.dng", 52550),
    ] {
        let bytes = gamut_dng_oracle::sample_file(name).expect("sample DNG present");
        let decoded = DngDecoder::new().decode(&bytes).expect("decode");
        let extra = decoded
            .ifd0_extra
            .iter()
            .find(|t| t.tag == tag)
            .unwrap_or_else(|| panic!("{name}: tag {tag} must surface in ifd0_extra"));
        assert!(
            matches!(&extra.value, gamut_dng::Value::Undefined(b) if !b.is_empty()),
            "{name}: tag {tag} carries its typed UNDEFINED payload"
        );
    }
}

/// The JXL samples carry a declared DNG 1.7 version.
#[test]
fn adobe_jxl_sample_declares_dng_17() {
    let bytes =
        gamut_dng_oracle::sample_file("01_jxl_linear_raw_integer.dng").expect("sample present");
    let decoded = DngDecoder::new().decode(&bytes).expect("decode");
    assert_eq!(decoded.dng_version[0], 1);
    assert!(decoded.dng_version[1] >= 7, "JXL requires DNG >= 1.7");
}
