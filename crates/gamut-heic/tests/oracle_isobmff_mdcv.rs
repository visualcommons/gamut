//! Differential check of `gamut_isobmff::read`'s typed `mdcv` property against an independent
//! writer (issue #699, remainder of #645).
//!
//! The test sits in gamut-heic rather than gamut-isobmff because this crate already has a normal
//! edge to gamut-isobmff and owns the libheif oracle as a dev-dependency, so `check-release-deps`
//! holds without giving the container crate an oracle. Its reach is gamut-isobmff's reader and
//! libheif's writer; no gamut-heic code runs. Because the mutation gate sets
//! `test_workspace = false`, this test does not score gamut-isobmff's mutants: the reader's
//! inline exact-byte tests do that, and this one guards against a layout defect symmetric across
//! gamut's own reader and writer, which those tests cannot see.
//!
//! The pinned libavif does not write `mdcv` (`src/write.c` hard-codes `hasMdcv = AVIF_FALSE`); the
//! pinned libheif does (`heif_image_set_mastering_display_colour_volume` →
//! `ImageDescription::create_mdcv_box` → `Box_mdcv::write`).

use gamut_isobmff::PropertyKind;
use libheif_oracle::{EncodeOpts, MasteringDisplay};

/// Every field differs from every other, so a swapped, transposed or misaligned read cannot
/// reproduce the same values. Indices are passed to libheif in its `[0]`, `[1]`, `[2]` order.
const MDCV: MasteringDisplay = MasteringDisplay {
    display_primaries_x: [13250, 7500, 34000],
    display_primaries_y: [34500, 3000, 16000],
    white_point_x: 15635,
    white_point_y: 16450,
    max_display_mastering_luminance: 10_000_000,
    min_display_mastering_luminance: 50,
};

#[test]
fn read_types_libheif_written_mdcv_with_the_values_libheif_was_given() {
    let (w, h) = (16u32, 16u32);
    let rgba = vec![128u8; (w * h * 4) as usize];
    let heic = libheif_oracle::encode_rgba_to_heic(
        w,
        h,
        &rgba,
        &EncodeOpts {
            quality: 50,
            mdcv: Some(MDCV),
            ..Default::default()
        },
    )
    .expect("libheif encodes the mdcv fixture");

    let image = gamut_isobmff::read(&heic).expect("gamut_isobmff reads the libheif file");
    let primary = image
        .items
        .iter()
        .find(|item| item.id == image.primary_item_id)
        .expect("primary item present");
    let typed: Vec<&PropertyKind> = primary
        .properties
        .iter()
        .map(|p| &p.kind)
        .filter(|k| matches!(k, PropertyKind::MasteringDisplayColourVolume { .. }))
        .collect();

    assert_eq!(
        typed,
        [&PropertyKind::MasteringDisplayColourVolume {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 10_000_000,
            min_display_mastering_luminance: 50,
        }],
        "exactly one typed mdcv, carrying the input values in stored order"
    );
}
