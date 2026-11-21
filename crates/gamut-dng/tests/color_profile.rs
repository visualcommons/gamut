//! The typed colour projections on `DecodedDng`: the camera profile's rendering tags
//! (`ColorProfileInfo`) and the sensor noise model (`NoiseProfile`).
//!
//! gamut's encoder does not write these tags, so the fixture is hand-built over `gamut-ifd`'s tree
//! writer with the same two-pass offset trick `subimages.rs` uses: IFD 0 is an RGB preview
//! carrying the profile tags, and the raw CFA image lives in a sub-IFD carrying `NoiseProfile` —
//! the placement DNG 1.7.1 prescribes for each.

use gamut_dng::{CalibrationIlluminant, DngDecoder, TableEncoding, tags};
use gamut_ifd::{ByteOrder, Ifd, TiffFile, Value, Variant, write};

/// A private maker tag: unmodelled content that must keep reaching the extras.
const PRIVATE_TAG: u16 = 0x9999;

fn identity_matrix() -> Value {
    Value::SRational(vec![
        (1, 1),
        (0, 1),
        (0, 1),
        (0, 1),
        (1, 1),
        (0, 1),
        (0, 1),
        (0, 1),
        (1, 1),
    ])
}

/// A 1 x 2 x 1 hue/saturation/value table (one hue division, two saturation divisions).
fn small_table() -> Value {
    Value::Float(vec![0.0, 1.0, 1.0, 10.0, 1.5, 0.5])
}

/// Builds a DNG whose IFD 0 is an RGB preview carrying the camera-profile colour tags, with the
/// 4x4 8-bit CFA raw in a sub-IFD carrying `noise_profile`.
fn build_profile_dng(noise_profile: Value) -> Vec<u8> {
    build_profile_dng_at(noise_profile, NoiseAt::RawIfd)
}

/// Which directory the `NoiseProfile` tag is written to.
#[derive(Clone, Copy, PartialEq)]
enum NoiseAt {
    /// Where the spec puts it.
    RawIfd,
    /// Where some writers put it instead, which the decoder falls back to.
    Ifd0,
}

fn build_profile_dng_at(noise_profile: Value, at: NoiseAt) -> Vec<u8> {
    let preview_pixels: Vec<u8> = (0..48).collect();
    let raw_pixels: Vec<u8> = (0..16).collect();

    let mut ifd0 = Ifd::new();
    ifd0.set(tags::NEW_SUBFILE_TYPE, Value::Long(vec![1]));
    ifd0.set(tags::IMAGE_WIDTH, Value::Short(vec![4]));
    ifd0.set(tags::IMAGE_LENGTH, Value::Short(vec![4]));
    ifd0.set(tags::BITS_PER_SAMPLE, Value::Short(vec![8, 8, 8]));
    ifd0.set(tags::COMPRESSION, Value::Short(vec![1]));
    ifd0.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![2]));
    ifd0.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![3]));
    ifd0.set(tags::ROWS_PER_STRIP, Value::Short(vec![4]));
    ifd0.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
    ifd0.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![48]));
    ifd0.set(tags::DNG_VERSION, Value::Byte(vec![1, 6, 0, 0]));
    ifd0.set(
        tags::UNIQUE_CAMERA_MODEL,
        Value::Ascii("gamut ProfileCam".into()),
    );

    // The calibration `CameraProfile` already models.
    ifd0.set(tags::COLOR_MATRIX1, identity_matrix());
    ifd0.set(tags::CALIBRATION_ILLUMINANT1, Value::Short(vec![21]));
    ifd0.set(
        tags::AS_SHOT_NEUTRAL,
        Value::Rational(vec![(1, 2), (1, 1), (2, 3)]),
    );

    // The third calibration set (DNG 1.6).
    ifd0.set(tags::COLOR_MATRIX3, identity_matrix());
    ifd0.set(tags::CALIBRATION_ILLUMINANT3, Value::Short(vec![17]));
    ifd0.set(tags::CAMERA_CALIBRATION3, identity_matrix());
    ifd0.set(tags::FORWARD_MATRIX3, identity_matrix());
    // A four-plane reduction matrix (3 x 4 entries).
    ifd0.set(tags::REDUCTION_MATRIX1, Value::SRational(vec![(1, 2); 12]));

    // The rendering tables and curve.
    ifd0.set(tags::PROFILE_HUE_SAT_MAP_DIMS, Value::Long(vec![1, 2, 1]));
    ifd0.set(tags::PROFILE_HUE_SAT_MAP_DATA1, small_table());
    ifd0.set(tags::PROFILE_HUE_SAT_MAP_DATA2, small_table());
    ifd0.set(tags::PROFILE_HUE_SAT_MAP_ENCODING, Value::Long(vec![1]));
    // A third table whose entry count contradicts the shared dims: unusable, so it must stay
    // visible in the extras rather than be silently swallowed.
    ifd0.set(
        tags::PROFILE_HUE_SAT_MAP_DATA3,
        Value::Float(vec![0.0, 1.0, 1.0]),
    );
    ifd0.set(tags::PROFILE_LOOK_TABLE_DIMS, Value::Long(vec![1, 2, 1]));
    ifd0.set(tags::PROFILE_LOOK_TABLE_DATA, small_table());
    ifd0.set(
        tags::PROFILE_TONE_CURVE,
        Value::Float(vec![0.0, 0.0, 0.5, 0.6, 1.0, 1.0]),
    );
    ifd0.set(
        tags::BASELINE_EXPOSURE_OFFSET,
        Value::SRational(vec![(-7, 10)]),
    );
    ifd0.set(PRIVATE_TAG, Value::Ascii("maker secret".into()));

    let mut raw = Ifd::new();
    raw.set(tags::NEW_SUBFILE_TYPE, Value::Long(vec![0]));
    raw.set(tags::IMAGE_WIDTH, Value::Short(vec![4]));
    raw.set(tags::IMAGE_LENGTH, Value::Short(vec![4]));
    raw.set(tags::BITS_PER_SAMPLE, Value::Short(vec![8]));
    raw.set(tags::COMPRESSION, Value::Short(vec![1]));
    raw.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803]));
    raw.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
    raw.set(tags::ROWS_PER_STRIP, Value::Short(vec![4]));
    raw.set(tags::CFA_REPEAT_PATTERN_DIM, Value::Short(vec![2, 2]));
    raw.set(tags::CFA_PATTERN, Value::Byte(vec![0, 1, 1, 2]));
    raw.set(tags::STRIP_OFFSETS, Value::Long(vec![0]));
    raw.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![16]));
    match at {
        NoiseAt::RawIfd => raw.set(tags::NOISE_PROFILE, noise_profile),
        NoiseAt::Ifd0 => ifd0.set(tags::NOISE_PROFILE, noise_profile),
    }
    ifd0.set_sub_ifd(tags::SUB_IFDS, vec![raw]);

    // Two-pass layout: learn the tree length, patch real offsets, re-write, append pixels.
    let file = TiffFile::new(ByteOrder::LittleEndian, Variant::Classic, vec![ifd0]);
    let base = write(&file).expect("first pass").len();
    let mut file = file;
    let ifd0 = &mut file.ifds[0];
    ifd0.set(tags::STRIP_OFFSETS, Value::Long(vec![base as u32]));
    let mut raw = ifd0.sub_ifds()[0].ifds.clone();
    raw[0].set(tags::STRIP_OFFSETS, Value::Long(vec![(base + 48) as u32]));
    ifd0.set_sub_ifd(tags::SUB_IFDS, raw);

    let mut bytes = write(&file).expect("second pass");
    assert_eq!(bytes.len(), base, "two-pass layout must be byte-stable");
    bytes.extend_from_slice(&preview_pixels);
    bytes.extend_from_slice(&raw_pixels);
    bytes
}

/// A plausible three-plane noise profile: (scale, offset) per plane.
fn noise_parameters() -> Value {
    Value::Double(vec![2e-5, 4.5e-7, 3e-5, 5.0e-7, 4e-5, 6.0e-7])
}

#[test]
fn camera_profile_colour_tags_decode_typed() {
    let dng = build_profile_dng(noise_parameters());
    let decoded = DngDecoder::new().decode(&dng).expect("decode");

    let info = decoded.color_profile.expect("colour-profile tags present");
    let identity = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    assert_eq!(info.color_matrix3, Some(identity));
    assert_eq!(
        info.calibration_illuminant3,
        Some(CalibrationIlluminant::StandardLightA)
    );
    assert_eq!(info.camera_calibration3, Some(identity));
    assert_eq!(info.forward_matrix3, Some(identity));
    assert_eq!(info.reduction_matrix1, Some(vec![0.5; 12]));
    assert_eq!(info.reduction_matrix2, None);
    assert_eq!(info.reduction_matrix3, None);

    let map1 = info.hue_sat_map1.as_ref().expect("first hue/sat table");
    assert_eq!(
        (
            map1.hue_divisions,
            map1.saturation_divisions,
            map1.value_divisions
        ),
        (1, 2, 1)
    );
    assert_eq!(map1.encoding, TableEncoding::Srgb);
    assert_eq!(map1.entry(0, 1, 0).map(|e| e.hue_shift_degrees), Some(10.0));
    assert_eq!(map1.entry(0, 1, 0).map(|e| e.saturation_scale), Some(1.5));
    assert_eq!(map1.entry(0, 1, 0).map(|e| e.value_scale), Some(0.5));
    assert!(info.hue_sat_map2.is_some());
    assert_eq!(
        info.hue_sat_map3, None,
        "a table contradicting the shared dims is not projected"
    );

    let look = info.look_table.as_ref().expect("look table");
    assert_eq!(
        look.encoding,
        TableEncoding::Linear,
        "an absent encoding tag means the spec's default"
    );
    assert_eq!(look.entries.len(), 2);

    assert_eq!(
        info.tone_curve,
        Some(vec![(0.0, 0.0), (0.5, 0.6), (1.0, 1.0)])
    );
    assert_eq!(info.baseline_exposure_offset, Some(-0.7));

    // The calibration `CameraProfile` models is untouched by the new projection.
    let profile = decoded.profile.as_ref().expect("camera profile");
    assert_eq!(profile.unique_camera_model(), "gamut ProfileCam");
    assert_eq!(profile.color_matrix1(), &identity);
}

/// A `NoiseProfile` written to IFD 0 instead of the raw IFD is still found.
///
/// The decoder falls back to IFD 0 when the raw IFD carries none, guarded by `raw_index != 0` so
/// it does not re-read the same directory. Every fixture put the tag where the spec says, so
/// inverting that guard to `== 0` disabled the fallback in exactly the case it exists for and no
/// test noticed (#110). This is the writer quirk the fallback was added for.
#[test]
fn the_noise_profile_falls_back_to_ifd0() {
    let dng = build_profile_dng_at(noise_parameters(), NoiseAt::Ifd0);
    let decoded = DngDecoder::new().decode(&dng).expect("decode");

    let noise = decoded
        .noise_profile
        .expect("a NoiseProfile in IFD 0 must still be found");
    assert_eq!(noise.planes.len(), 3);
    assert_eq!(noise.for_plane(0).map(|m| m.scale), Some(2e-5));
}

#[test]
fn the_noise_profile_decodes_from_the_raw_ifd() {
    let dng = build_profile_dng(noise_parameters());
    let decoded = DngDecoder::new().decode(&dng).expect("decode");

    let noise = decoded.noise_profile.expect("noise profile");
    assert_eq!(noise.planes.len(), 3);
    assert_eq!(noise.for_plane(0).map(|m| m.scale), Some(2e-5));
    assert_eq!(noise.for_plane(2).map(|m| m.offset), Some(6.0e-7));
    assert_eq!(noise.for_plane(3), None);
    assert!(
        decoded.raw_extra.is_empty(),
        "the raw IFD's NoiseProfile is consumed, not surfaced: {:?}",
        decoded.raw_extra
    );
}

#[test]
fn typed_tags_leave_the_extras_and_unusable_ones_stay() {
    let dng = build_profile_dng(noise_parameters());
    let decoded = DngDecoder::new().decode(&dng).expect("decode");

    assert_eq!(
        decoded.ifd0_extra.iter().map(|t| t.tag).collect::<Vec<_>>(),
        vec![PRIVATE_TAG, tags::PROFILE_HUE_SAT_MAP_DATA3],
        "only the private maker tag and the unusable third table remain (tag order)"
    );
    assert_eq!(
        decoded.ifd0_extra[1].value,
        Value::Float(vec![0.0, 1.0, 1.0]),
        "the unusable table keeps its verbatim typed value"
    );
}

#[test]
fn an_unusable_noise_profile_surfaces_verbatim() {
    // The scale term must be positive: this value is outside the model's domain.
    let broken = Value::Double(vec![0.0, 1.0]);
    let dng = build_profile_dng(broken.clone());
    let decoded = DngDecoder::new().decode(&dng).expect("decode");

    assert_eq!(decoded.noise_profile, None);
    assert_eq!(
        decoded
            .raw_extra
            .iter()
            .map(|t| (t.tag, t.value.clone()))
            .collect::<Vec<_>>(),
        vec![(tags::NOISE_PROFILE, broken)],
        "a NoiseProfile the model cannot use is kept verbatim"
    );
}

#[test]
fn a_file_without_the_colour_tags_projects_nothing() {
    let mut dng = Vec::new();
    let raw = gamut_dng::RawImage::new_cfa(
        gamut_dng::Dimensions::new(4, 4).expect("dimensions"),
        8,
        (2, 2),
        vec![0, 1, 1, 2],
        vec![0u16; 16],
    )
    .expect("raw image")
    .with_white_level(255.0)
    .expect("white level");
    let profile = gamut_dng::CameraProfile::new(
        "gamut PlainCam",
        [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        CalibrationIlluminant::D65,
        [0.5, 1.0, 0.6],
    )
    .expect("profile");
    gamut_dng::DngEncoder::new()
        .encode(&raw, &profile, &mut dng)
        .expect("encode");

    let decoded = DngDecoder::new().decode(&dng).expect("decode");
    assert_eq!(decoded.color_profile, None);
    assert_eq!(decoded.noise_profile, None);
}
