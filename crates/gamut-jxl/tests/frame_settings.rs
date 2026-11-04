//! Differential tests for [`FrameSettings`] against a **direct libjxl C-API encode**: for every
//! value of every setting, in both lossless and lossy mode, gamut's stream must be byte-identical to
//! the one libjxl produces when a plain C client sets the same frame setting (issue #1018). Each
//! setting is also shown to be observable — some value changes the stream — so the equality is not
//! vacuous, and the all-default settings must match a direct encode that sets nothing. The direct
//! side names each option by its literal `encode.h` id, so the comparison also pins
//! gamut-jxl-sys's constants.
//!
//! Cost: about 70 libjxl encodes of a 300×280 image at the default effort, a few seconds per run
//! (1.7 s measured on a loaded 16-core host), paid by every gamut-jxl mutant.
//!
//! Encoder-only (no decoder involved); compiled wherever the libjxl encoder is.
#![cfg(all(
    feature = "encode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]

mod common;

use common::{encode_direct_rgb8, gen_u8};
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_jxl::{
    DecodingSpeed, Distance, Effort, FrameSettings, JxlEncoder, ProgressiveDc, Toggle,
};
use gamut_jxl_sys::encode::JxlEncoderFrameSettingId as Id;

// The option ids of libjxl 0.12.0's `jxl/encode.h` (`JXL_ENC_FRAME_SETTING_*`), written as
// literals rather than taken from gamut-jxl-sys's constants: the gamut side forwards through those
// constants, so a mistyped one would otherwise cancel out of the comparison.
/// `JXL_ENC_FRAME_SETTING_DECODING_SPEED`.
const DECODING_SPEED: Id = Id(1);
/// `JXL_ENC_FRAME_SETTING_RESPONSIVE`.
const RESPONSIVE: Id = Id(16);
/// `JXL_ENC_FRAME_SETTING_PROGRESSIVE_AC`.
const PROGRESSIVE_AC: Id = Id(17);
/// `JXL_ENC_FRAME_SETTING_QPROGRESSIVE_AC`.
const QPROGRESSIVE_AC: Id = Id(18);
/// `JXL_ENC_FRAME_SETTING_PROGRESSIVE_DC`.
const PROGRESSIVE_DC: Id = Id(19);

/// Large enough to span several 256×256 groups and a non-trivial DC image, so progression has
/// passes to split and the decoding-speed tiers have groups to resize.
const W: u32 = 300;
/// See [`W`].
const H: u32 = 280;

/// The two modes every setting is checked in: lossless (Modular) and lossy at distance 1 (VarDCT).
const MODES: [Option<f32>; 2] = [None, Some(1.0)];

/// The image every test encodes.
fn pixels() -> Vec<u8> {
    gen_u8(W, H, 3)
}

/// Encodes through gamut with the given mode and frame settings at the default effort.
fn gamut(pixels: &[u8], distance: Option<f32>, settings: FrameSettings) -> Vec<u8> {
    let image = ImageRef::<Rgb8>::new(pixels, Dimensions::new(W, H).unwrap()).unwrap();
    let encoder = match distance {
        None => JxlEncoder::lossless(),
        Some(d) => JxlEncoder::lossy(Distance::new(d).unwrap()),
    };
    encoder
        .with_frame_settings(settings)
        .encode_to_vec(image)
        .unwrap()
}

/// Encodes directly through libjxl with the given mode and frame settings at the same effort.
fn direct(pixels: &[u8], distance: Option<f32>, options: &[(Id, i64)]) -> Vec<u8> {
    let effort = i64::from(Effort::default().level());
    encode_direct_rgb8(W, H, pixels, distance, effort, options)
}

/// Asserts gamut matches the direct encode for each `(settings, libjxl value)` case of option
/// `id`, in both modes, and that at least one case changes the stream in the mode named
/// `observable_in`.
fn assert_matches_direct(id: Id, cases: &[(FrameSettings, i64)], observable_in: Option<f32>) {
    let px = pixels();
    for distance in MODES {
        for &(settings, value) in cases {
            assert!(
                gamut(&px, distance, settings) == direct(&px, distance, &[(id, value)]),
                "{settings:?} (libjxl {id:?} = {value}) diverged from the direct encode at \
                 distance {distance:?}"
            );
        }
    }
    let baseline = direct(&px, observable_in, &[]);
    assert!(
        cases
            .iter()
            .any(|&(_, value)| direct(&px, observable_in, &[(id, value)]) != baseline),
        "no value of {id:?} changed the stream at distance {observable_in:?}; the comparison \
         above could not tell a dropped setting from a forwarded one"
    );
}

#[test]
fn default_settings_match_a_direct_encode_that_sets_nothing() {
    let px = pixels();
    for distance in MODES {
        assert!(
            gamut(&px, distance, FrameSettings::default()) == direct(&px, distance, &[]),
            "default frame settings diverged from an untouched libjxl at distance {distance:?}"
        );
    }
}

#[test]
fn progressive_ac_matches_a_direct_encode() {
    let base = FrameSettings::default();
    assert_matches_direct(
        PROGRESSIVE_AC,
        &[
            (base.with_progressive_ac(Toggle::Off), 0),
            (base.with_progressive_ac(Toggle::On), 1),
        ],
        Some(1.0),
    );
}

#[test]
fn qprogressive_ac_matches_a_direct_encode() {
    let base = FrameSettings::default();
    assert_matches_direct(
        QPROGRESSIVE_AC,
        &[
            (base.with_qprogressive_ac(Toggle::Off), 0),
            (base.with_qprogressive_ac(Toggle::On), 1),
        ],
        Some(1.0),
    );
}

#[test]
fn progressive_dc_matches_a_direct_encode() {
    let base = FrameSettings::default();
    assert_matches_direct(
        PROGRESSIVE_DC,
        &[
            (base.with_progressive_dc(ProgressiveDc::Off), 0),
            (base.with_progressive_dc(ProgressiveDc::OneLevel), 1),
            (base.with_progressive_dc(ProgressiveDc::TwoLevels), 2),
        ],
        Some(1.0),
    );
}

#[test]
fn responsive_matches_a_direct_encode() {
    let base = FrameSettings::default();
    assert_matches_direct(
        RESPONSIVE,
        &[
            (base.with_responsive(Toggle::Off), 0),
            (base.with_responsive(Toggle::On), 1),
        ],
        None,
    );
}

#[test]
fn decoding_speed_matches_a_direct_encode() {
    let base = FrameSettings::default();
    assert_matches_direct(
        DECODING_SPEED,
        &[
            (base.with_decoding_speed(DecodingSpeed::Tier0), 0),
            (base.with_decoding_speed(DecodingSpeed::Tier1), 1),
            (base.with_decoding_speed(DecodingSpeed::Tier2), 2),
            (base.with_decoding_speed(DecodingSpeed::Tier3), 3),
            (base.with_decoding_speed(DecodingSpeed::Tier4), 4),
        ],
        Some(1.0),
    );
}

#[test]
fn all_settings_together_match_a_direct_encode_setting_all_of_them() {
    // One encode with every field set at once, so a setting forwarded under another's id — which
    // a one-at-a-time comparison could miss if the two happened to coincide — still diverges. Both
    // AC progressions stay off: libjxl turns responsive on (and, lossless, the quantized
    // progression too) whenever one is on, which would overwrite the other values sent here.
    let px = pixels();
    let settings = FrameSettings::default()
        .with_progressive_ac(Toggle::Off)
        .with_qprogressive_ac(Toggle::Off)
        .with_progressive_dc(ProgressiveDc::TwoLevels)
        .with_responsive(Toggle::On)
        .with_decoding_speed(DecodingSpeed::Tier3);
    let options = [
        (PROGRESSIVE_AC, 0),
        (QPROGRESSIVE_AC, 0),
        (PROGRESSIVE_DC, 2),
        (RESPONSIVE, 1),
        (DECODING_SPEED, 3),
    ];
    for distance in MODES {
        assert!(
            gamut(&px, distance, settings) == direct(&px, distance, &options),
            "combined frame settings diverged from the direct encode at distance {distance:?}"
        );
    }
}
