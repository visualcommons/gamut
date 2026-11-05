//! Binding-drift guard and link smoke test.
//!
//! Asserts the statically linked libjxl reports exactly v0.12.0 — the version the `#[repr(C)]`
//! types and `extern "C"` signatures in this crate were transcribed from. If `jpegxl-src` is ever
//! bumped, this fails until the declarations are re-checked against the new headers. It also
//! exercises the encoder/decoder create+destroy paths and forces link-time resolution of every
//! declared symbol, so the `links = "jxl"` link lines are proven to resolve in CI. The decoder half
//! is declared only with the `oracle` feature, so its checks are compiled only then (the workspace
//! test run enables every feature).

#[cfg(feature = "oracle")]
use gamut_jxl_sys::decode::{JxlDecoderCreate, JxlDecoderDestroy, JxlDecoderVersion};
use gamut_jxl_sys::encode::{JxlEncoderCreate, JxlEncoderDestroy, JxlEncoderVersion};

/// libjxl v0.12.0 encoded as `MAJOR*1000000 + MINOR*1000 + PATCH` = 0*1000000 + 12*1000 + 0.
const LIBJXL_0_12_0: u32 = 12_000;

#[test]
fn encoder_version_matches_pinned_libjxl() {
    // SAFETY: takes no arguments and dereferences no pointers.
    let version = unsafe { JxlEncoderVersion() };
    assert_eq!(
        version, LIBJXL_0_12_0,
        "JxlEncoderVersion() = {version}; expected {LIBJXL_0_12_0} (libjxl 0.12.0). \
         If jpegxl-src was bumped, re-verify the FFI declarations against the new headers."
    );
}

#[cfg(feature = "oracle")]
#[test]
fn decoder_version_matches_pinned_libjxl() {
    // SAFETY: takes no arguments and dereferences no pointers.
    let version = unsafe { JxlDecoderVersion() };
    assert_eq!(
        version, LIBJXL_0_12_0,
        "JxlDecoderVersion() = {version}; expected {LIBJXL_0_12_0} (libjxl 0.12.0)."
    );
}

#[test]
fn encoder_create_and_destroy() {
    // SAFETY: a null memory manager selects the default allocator; the returned pointer is freed
    // exactly once with JxlEncoderDestroy and not used afterwards.
    unsafe {
        let enc = JxlEncoderCreate(core::ptr::null());
        assert!(!enc.is_null(), "JxlEncoderCreate returned null");
        JxlEncoderDestroy(enc);
    }
}

#[cfg(feature = "oracle")]
#[test]
fn decoder_create_and_destroy() {
    // SAFETY: a null memory manager selects the default allocator; the returned pointer is freed
    // exactly once with JxlDecoderDestroy and not used afterwards.
    unsafe {
        let dec = JxlDecoderCreate(core::ptr::null());
        assert!(!dec.is_null(), "JxlDecoderCreate returned null");
        JxlDecoderDestroy(dec);
    }
}

/// Forces link-time resolution of EVERY extern declaration in the crate by taking each function's
/// address (nothing is called, so no unsafe contract is invoked). A misspelled symbol name in any
/// declaration fails this test's link step here instead of surfacing later in gamut-jxl.
///
/// KEEP EXHAUSTIVE: any extern fn added to `src/encode.rs` or `src/decode.rs` MUST be added to
/// this table.
#[test]
fn every_declared_symbol_resolves() {
    #[cfg(feature = "oracle")]
    use gamut_jxl_sys::decode;
    use gamut_jxl_sys::encode;
    #[cfg_attr(not(feature = "oracle"), expect(unused_mut))]
    let mut symbols: Vec<(&str, *const ())> = vec![
        // encode.rs (23)
        ("JxlEncoderVersion", encode::JxlEncoderVersion as *const ()),
        ("JxlEncoderCreate", encode::JxlEncoderCreate as *const ()),
        ("JxlEncoderReset", encode::JxlEncoderReset as *const ()),
        ("JxlEncoderDestroy", encode::JxlEncoderDestroy as *const ()),
        (
            "JxlEncoderFrameSettingsCreate",
            encode::JxlEncoderFrameSettingsCreate as *const (),
        ),
        (
            "JxlEncoderSetBasicInfo",
            encode::JxlEncoderSetBasicInfo as *const (),
        ),
        (
            "JxlEncoderInitBasicInfo",
            encode::JxlEncoderInitBasicInfo as *const (),
        ),
        (
            "JxlEncoderSetColorEncoding",
            encode::JxlEncoderSetColorEncoding as *const (),
        ),
        (
            "JxlEncoderSetICCProfile",
            encode::JxlEncoderSetICCProfile as *const (),
        ),
        (
            "JxlEncoderSetFrameDistance",
            encode::JxlEncoderSetFrameDistance as *const (),
        ),
        (
            "JxlEncoderSetFrameLossless",
            encode::JxlEncoderSetFrameLossless as *const (),
        ),
        (
            "JxlEncoderFrameSettingsSetOption",
            encode::JxlEncoderFrameSettingsSetOption as *const (),
        ),
        (
            "JxlEncoderAddImageFrame",
            encode::JxlEncoderAddImageFrame as *const (),
        ),
        (
            "JxlEncoderAddJPEGFrame",
            encode::JxlEncoderAddJPEGFrame as *const (),
        ),
        (
            "JxlEncoderCloseInput",
            encode::JxlEncoderCloseInput as *const (),
        ),
        (
            "JxlEncoderProcessOutput",
            encode::JxlEncoderProcessOutput as *const (),
        ),
        (
            "JxlEncoderGetError",
            encode::JxlEncoderGetError as *const (),
        ),
        (
            "JxlEncoderUseContainer",
            encode::JxlEncoderUseContainer as *const (),
        ),
        (
            "JxlEncoderStoreJPEGMetadata",
            encode::JxlEncoderStoreJPEGMetadata as *const (),
        ),
        (
            "JxlEncoderUseBoxes",
            encode::JxlEncoderUseBoxes as *const (),
        ),
        ("JxlEncoderAddBox", encode::JxlEncoderAddBox as *const ()),
        (
            "JxlColorEncodingSetToSRGB",
            encode::JxlColorEncodingSetToSRGB as *const (),
        ),
        (
            "JxlColorEncodingSetToLinearSRGB",
            encode::JxlColorEncodingSetToLinearSRGB as *const (),
        ),
    ];
    #[cfg(feature = "oracle")]
    symbols.extend_from_slice(&[
        // decode.rs (18)
        ("JxlDecoderVersion", decode::JxlDecoderVersion as *const ()),
        ("JxlSignatureCheck", decode::JxlSignatureCheck as *const ()),
        ("JxlDecoderCreate", decode::JxlDecoderCreate as *const ()),
        ("JxlDecoderReset", decode::JxlDecoderReset as *const ()),
        ("JxlDecoderDestroy", decode::JxlDecoderDestroy as *const ()),
        (
            "JxlDecoderSubscribeEvents",
            decode::JxlDecoderSubscribeEvents as *const (),
        ),
        (
            "JxlDecoderSetInput",
            decode::JxlDecoderSetInput as *const (),
        ),
        (
            "JxlDecoderReleaseInput",
            decode::JxlDecoderReleaseInput as *const (),
        ),
        (
            "JxlDecoderCloseInput",
            decode::JxlDecoderCloseInput as *const (),
        ),
        (
            "JxlDecoderProcessInput",
            decode::JxlDecoderProcessInput as *const (),
        ),
        (
            "JxlDecoderGetBasicInfo",
            decode::JxlDecoderGetBasicInfo as *const (),
        ),
        (
            "JxlDecoderImageOutBufferSize",
            decode::JxlDecoderImageOutBufferSize as *const (),
        ),
        (
            "JxlDecoderSetImageOutBuffer",
            decode::JxlDecoderSetImageOutBuffer as *const (),
        ),
        (
            "JxlDecoderSetJPEGBuffer",
            decode::JxlDecoderSetJPEGBuffer as *const (),
        ),
        (
            "JxlDecoderReleaseJPEGBuffer",
            decode::JxlDecoderReleaseJPEGBuffer as *const (),
        ),
        (
            "JxlDecoderGetColorAsEncodedProfile",
            decode::JxlDecoderGetColorAsEncodedProfile as *const (),
        ),
        (
            "JxlDecoderGetICCProfileSize",
            decode::JxlDecoderGetICCProfileSize as *const (),
        ),
        (
            "JxlDecoderGetColorAsICCProfile",
            decode::JxlDecoderGetColorAsICCProfile as *const (),
        ),
    ]);
    for (name, addr) in symbols {
        assert!(!addr.is_null(), "{name} resolved to a null address");
    }
}
