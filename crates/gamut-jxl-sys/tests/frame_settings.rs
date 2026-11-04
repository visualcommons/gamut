//! Drift guard for the declared `JxlEncoderFrameSettingId` values.
//!
//! The ids are hand-transcribed from libjxl 0.12.0's `jxl/encode.h`, and a wrong one would not fail
//! to link: it would silently set a different option. libjxl validates each option's value range
//! per id inside `JxlEncoderFrameSettingsSetOption`, so the accepted range narrows down which
//! option an id names: it singles out `DECODING_SPEED` and `PROGRESSIVE_DC`, but only places
//! `RESPONSIVE` / `PROGRESSIVE_AC` / `QPROGRESSIVE_AC` in the shared tri-state class. Those three
//! are told apart by gamut-jxl's `tests/frame_settings.rs`, whose direct libjxl encodes name each
//! option by its literal `encode.h` id.

use gamut_jxl_sys::encode::{
    JxlEncoder, JxlEncoderCreate, JxlEncoderDestroy, JxlEncoderFrameSettingId,
    JxlEncoderFrameSettings, JxlEncoderFrameSettingsCreate, JxlEncoderFrameSettingsSetOption,
    JxlEncoderStatus,
};

/// Whether libjxl accepts `value` for `id` on a fresh encoder's default frame settings.
fn accepts(id: JxlEncoderFrameSettingId, value: i64) -> bool {
    // SAFETY: a null memory manager selects the default allocator. The frame settings are owned by
    // the encoder, which is destroyed exactly once after its last use.
    unsafe {
        let enc: *mut JxlEncoder = JxlEncoderCreate(core::ptr::null());
        assert!(!enc.is_null(), "JxlEncoderCreate returned null");
        let settings: *mut JxlEncoderFrameSettings =
            JxlEncoderFrameSettingsCreate(enc, core::ptr::null());
        assert!(
            !settings.is_null(),
            "JxlEncoderFrameSettingsCreate returned null"
        );
        let status = JxlEncoderFrameSettingsSetOption(settings, id, value);
        JxlEncoderDestroy(enc);
        status == JxlEncoderStatus::SUCCESS
    }
}

#[test]
fn decoding_speed_is_the_zero_to_four_tier_option() {
    let id = JxlEncoderFrameSettingId::DECODING_SPEED;
    assert!(accepts(id, 0));
    assert!(accepts(id, 4));
    // Unlike the tri-state options there is no `-1` "encoder default" value.
    assert!(!accepts(id, -1));
    assert!(!accepts(id, 5));
}

#[test]
fn progressive_dc_is_the_minus_one_to_two_option() {
    let id = JxlEncoderFrameSettingId::PROGRESSIVE_DC;
    assert!(accepts(id, -1));
    assert!(accepts(id, 2));
    assert!(!accepts(id, -2));
    assert!(!accepts(id, 3));
}

#[test]
fn responsive_and_both_ac_progressions_are_tri_state_options() {
    for id in [
        JxlEncoderFrameSettingId::RESPONSIVE,
        JxlEncoderFrameSettingId::PROGRESSIVE_AC,
        JxlEncoderFrameSettingId::QPROGRESSIVE_AC,
    ] {
        assert!(accepts(id, -1), "{id:?} must accept the encoder default");
        assert!(accepts(id, 1), "{id:?} must accept on");
        assert!(!accepts(id, -2), "{id:?} must refuse -2");
        assert!(!accepts(id, 2), "{id:?} must refuse 2");
    }
}
