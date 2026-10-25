//! fuzz · robustness — `gamut_jxl::JxlDecoder`, the entry point a JPEG XL file from the network hits.
//!
//! `docs/testing.md`'s per-crate table names `decode` as this crate's untrusted-input surface. The
//! decoder is gamut's wrapper over the pure-Rust `jxl` crate (jxl-rs): gamut-jxl's own `unsafe` is
//! confined to its `ffi` module, which wraps the libjxl **encoder** and is not built here — the
//! dependency is taken with `default-features = false, features = ["decode"]`, so this target needs
//! no cmake and no C++ toolchain. Its hostile-input bound is the wrapper's `PIXEL_LIMIT` (`1 << 28`
//! samples), applied to every decode it starts.
//!
//! What it searches is therefore the wrapper *and* the decoder underneath, together: a panic, a
//! hang or an allocation past libFuzzer's malloc limit in either is a defect, and one inside jxl-rs
//! is reported upstream and pinned by a named case in `gamut-jxl`'s own suite that reaches it
//! through gamut's API.
//!
//! Four calls, each its own path through the wrapper:
//!
//! - [`JxlDecoder::info`] — the header parse alone;
//! - [`JxlDecoder::embedded_icc_profile`] — the header parse through the colour encoding, which
//!   decompresses an embedded ICC profile;
//! - [`JxlDecoder::metadata_boxes`] — gamut-jxl's own walk of the ISOBMFF container's top-level
//!   boxes (`Exif`, `xml `, and the `brob` wrapper it refuses), which never touches the codestream;
//! - `decode_image` to `Rgba8`, with the `permissive` policy so every coded layout converts rather
//!   than being refused at the layout check — and, only where that full decode **fails**,
//!   [`DecodePartialImage::decode_partial_image`], the truncation-tolerant path, which is
//!   reached by exactly the inputs the full decode refuses.
//!
//! This target lists **no check beyond the crash oracle**, like `dng_decode`.

#![no_main]

use gamut_core::convert::ConvertPolicy;
use gamut_core::{DecodeImage, Rgba8};
use gamut_jxl::{DecodePartialImage, JxlDecoder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let decoder = JxlDecoder::new().with_convert_policy(ConvertPolicy::permissive());
    let _ = decoder.info(data);
    let _ = decoder.embedded_icc_profile(data);
    let _ = decoder.metadata_boxes(data);
    if DecodeImage::<Rgba8>::decode_image(&decoder, data).is_err() {
        let _ = DecodePartialImage::<Rgba8>::decode_partial_image(&decoder, data);
    }
});
