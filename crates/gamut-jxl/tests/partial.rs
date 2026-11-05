//! The best-effort decode path (`JxlDecoder::decode_partial`, issue #256): a truncated codestream returns
//! whatever pixels it supports plus a `JxlPartialReport`, instead of the flat `InvalidInput` the
//! `DecodeImage` path still gives.
//!
//! Two things are pinned here, and they are deliberately different in kind.
//!
//! **Identity.** On a *complete* stream the partial path must be bit-identical to
//! [`DecodeImage::decode_image`] and report [`JxlRender::Complete`]. That is the anchor: it chains
//! the new surface to the existing three-way libjxl oracle in `oracle.rs`/`features.rs` without
//! duplicating a byte of it, and it is what makes the shared `decode_raw` refactor safe.
//!
//! **Coverage.** On a *truncated* stream the assertions are exactly as strong as jxl-rs actually
//! promises and no stronger. How much it will draw from an incomplete frame is an internal
//! heuristic, so the "it does something" claim is asserted **existentially over a whole sweep**,
//! never per-prefix, and no pixel value, threshold length, or pass count is pinned. What *is*
//! pinned is the contract: dimensions, the render/hint agreement, blankness of a header-only
//! render, and that nothing panics.
//!
//! The truncation fixtures are deliberately large. A JPEG XL frame small enough to be coded as a
//! single group is one TOC section, and jxl-rs never hands a partially-received section to a
//! decoder — so a 16×16 image (what `robustness.rs` uses) has no partially-decodable structure at
//! all and would make this file assert nothing.
//!
//! Uses both codec halves, so it is compiled only when both are available.
#![cfg(all(
    feature = "encode",
    feature = "decode",
    any(not(target_arch = "wasm32"), target_os = "emscripten")
))]

mod common;

use common::{gen_u8, gen_u16};
use gamut_core::{
    DecodeImage, Dimensions, EncodeImage, ErrorKind, Gray8, Gray16, GrayAlpha8, GrayAlpha16,
    ImageBuf, ImageRef, Pixel, Rgb8, Rgb16, Rgba8, Rgba16,
};
use gamut_jxl::{
    BitDepth, Container, Distance, Effort, JxlDecoder, JxlEncoder, JxlPartialImage, JxlRender,
};

/// The identity fixture's size: small and odd, matching `features.rs` — identity does not depend on
/// the group structure, so there is no reason to pay for a big encode eight times over.
const SMALL: (u32, u32) = (33, 29);

/// The truncation fixture's size. At 1024×768 the frame spans many 256×256 groups, which is the
/// only regime in which a truncated stream has anything to render (see the module docs).
const BIG: (u32, u32) = (1024, 768);

/// The container framings swept.
const CONTAINERS: [Container; 2] = [Container::Codestream, Container::IsoBmff];

/// Emits the identity test for one 8-bit layout, over both framings.
macro_rules! identity_u8 {
    ($name:ident, $pixel:ty) => {
        #[test]
        fn $name() {
            let (w, h) = SMALL;
            let px = gen_u8(w, h, <$pixel as Pixel>::CHANNELS);
            let dims = Dimensions::new(w, h).unwrap();
            let img = ImageRef::<$pixel>::new(&px, dims).unwrap();
            for container in CONTAINERS {
                let bytes = JxlEncoder::lossless()
                    .with_container(container)
                    .encode_to_vec(img)
                    .unwrap();
                assert_identical::<$pixel>(&JxlDecoder::new(), &bytes, &px);
            }
        }
    };
}

/// Emits the identity test for one 16-bit layout, over both framings.
macro_rules! identity_u16 {
    ($name:ident, $pixel:ty) => {
        #[test]
        fn $name() {
            let (w, h) = SMALL;
            let px = gen_u16(w, h, <$pixel as Pixel>::CHANNELS);
            let dims = Dimensions::new(w, h).unwrap();
            let img = ImageRef::<$pixel>::new(&px, dims).unwrap();
            for container in CONTAINERS {
                let bytes = JxlEncoder::lossless()
                    .with_container(container)
                    .encode_to_vec(img)
                    .unwrap();
                assert_identical::<$pixel>(&JxlDecoder::new(), &bytes, &px);
            }
        }
    };
}

/// Asserts that a complete stream decodes bit-identically through both entry points, reports
/// itself complete, and that every diagnostic field takes its documented complete-decode value.
fn assert_identical<P>(dec: &JxlDecoder, bytes: &[u8], source: &[P::Sample])
where
    P: Pixel,
    P::Sample: PartialEq + core::fmt::Debug,
    JxlDecoder: DecodeImage<P>,
{
    let plain: ImageBuf<P> = dec.decode_image(bytes).expect("decode_image");
    let JxlPartialImage {
        image: partial,
        report,
        ..
    } = dec.decode_partial::<P>(bytes).expect("decode_partial");

    assert!(report.is_complete(), "a whole stream must report complete");
    assert_eq!(report.render, JxlRender::Complete);
    assert_eq!(report.additional_bytes_hint, None);
    assert_eq!(
        report.completed_passes, 0,
        "documented complete-decode value"
    );
    assert_eq!(partial.dimensions(), plain.dimensions());
    assert_eq!(
        partial.as_samples(),
        plain.as_samples(),
        "the partial path diverged from decode_image on a complete stream"
    );
    // Lossless, so both are also bit-exact to the source — which is what ties this to the oracle.
    assert_eq!(partial.as_samples(), source);
}

identity_u8!(identity_gray8, Gray8);
identity_u8!(identity_gray_alpha8, GrayAlpha8);
identity_u8!(identity_rgb8, Rgb8);
identity_u8!(identity_rgba8, Rgba8);
identity_u16!(identity_gray16, Gray16);
identity_u16!(identity_gray_alpha16, GrayAlpha16);
identity_u16!(identity_rgb16, Rgb16);
identity_u16!(identity_rgba16, Rgba16);

#[test]
fn the_codestream_bit_depth_policy_flows_through_the_partial_path() {
    // The decoder's one configuration knob must reach the new entry point, not just the old one.
    let (w, h) = SMALL;
    let px = gen_u16(w, h, Rgb16::CHANNELS)
        .iter()
        .map(|v| v >> 6) // 10-bit code values.
        .collect::<Vec<u16>>();
    let dims = Dimensions::new(w, h).unwrap();
    let img = ImageRef::<Rgb16>::new(&px, dims).unwrap();
    let bytes = JxlEncoder::lossless()
        .with_bit_depth(BitDepth::Ten)
        .encode_to_vec(img)
        .unwrap();

    let dec = JxlDecoder::new().with_codestream_bit_depth(true);
    assert_identical::<Rgb16>(&dec, &bytes, &px);

    // And the knob genuinely changes the answer, so the assertion above is not vacuous.
    let scaled = JxlDecoder::new()
        .decode_partial::<Rgb16>(&bytes)
        .expect("full-range decode")
        .image;
    assert_ne!(
        scaled.as_samples(),
        px.as_slice(),
        "full-range output should not equal the coded 10-bit values"
    );
}

/// The prefix lengths swept over `len` bytes: a dense band across the head, where the image and
/// frame headers land, plus 5% steps across the body.
fn prefixes(len: usize) -> Vec<usize> {
    let mut out: Vec<usize> = (0..512.min(len)).step_by(8).collect();
    out.extend((0..=20).map(|pct| len * pct / 20));
    out.sort_unstable();
    out.dedup();
    out
}

/// Drives the partial path over every prefix of `stream`, checking every invariant the contract
/// states, and returns how many prefixes produced a best-effort render carrying an actual sample.
///
/// Returning the count rather than asserting inside lets the caller make the "it does something"
/// claim existentially, over the whole sweep — jxl-rs promises nothing about any single prefix.
fn sweep<P>(stream: &[u8], full: &ImageBuf<P>) -> usize
where
    P: Pixel<Sample = u8>,
    JxlDecoder: DecodeImage<P>,
{
    let dec = JxlDecoder::new();
    let mut rendered = 0usize;

    for len in prefixes(stream.len()) {
        let prefix = &stream[..len];

        // A prefix the header parser rejects outright cannot yield an image either. (Only this
        // direction holds: some truncations are indistinguishable from corruption to jxl-rs, so a
        // readable header does not guarantee a best-effort decode succeeds.)
        let header_ok = dec.info(prefix).is_ok();

        let Ok(JxlPartialImage { image, report, .. }) = dec.decode_partial::<P>(prefix) else {
            assert!(
                !header_ok || len < stream.len(),
                "the whole stream must decode"
            );
            continue;
        };

        assert!(header_ok, "pixels came back from a prefix info() rejected");
        assert_eq!(
            image.dimensions(),
            full.dimensions(),
            "dimensions come from the file header and never vary with truncation ({len} bytes)"
        );
        assert_eq!(
            report.additional_bytes_hint.is_none(),
            report.is_complete(),
            "the byte hint is present exactly when the decode is incomplete ({len} bytes)"
        );

        match report.render {
            JxlRender::Complete => assert_eq!(
                image.as_samples(),
                full.as_samples(),
                "a complete render must equal the full decode ({len} bytes)"
            ),
            JxlRender::HeaderOnly => {
                assert_eq!(report.completed_passes, 0, "({len} bytes)");
                assert!(
                    image.as_samples().iter().all(|&s| s == 0),
                    "a header-only render must be blank ({len} bytes)"
                );
            }
            JxlRender::BestEffort => {
                if image.as_samples().iter().any(|&s| s != 0) {
                    rendered += 1;
                }
            }
            // `JxlRender` is `#[non_exhaustive]`; a future variant needs its own contract here.
            other => panic!("unhandled render {other:?} ({len} bytes)"),
        }
    }

    rendered
}

#[test]
fn truncated_lossless_streams_render_the_groups_that_arrived() {
    // Modular (lossless) has no sub-frame preview, but groups that did arrive decode exactly and
    // the remainder stays zero — so a mid-stream truncation carries real pixels.
    let (w, h) = BIG;
    let px = gen_u8(w, h, Rgb8::CHANNELS);
    let dims = Dimensions::new(w, h).unwrap();
    let img = ImageRef::<Rgb8>::new(&px, dims).unwrap();
    let bytes = JxlEncoder::lossless()
        .with_effort(Effort::Lightning)
        .encode_to_vec(img)
        .unwrap();
    let full: ImageBuf<Rgb8> = JxlDecoder::new().decode_image(&bytes).expect("full decode");

    let rendered = sweep::<Rgb8>(&bytes, &full);
    assert!(
        rendered > 0,
        "no truncation of a {w}x{h} lossless stream rendered a single sample"
    );
}

#[test]
fn truncated_lossy_streams_render_a_coarse_preview() {
    // VarDCT groups with no detail pass are drawn from the upsampled DC image, so a truncated
    // lossy stream is the case that yields the most: a full-size coarse preview.
    let (w, h) = BIG;
    let px = gen_u8(w, h, Rgba8::CHANNELS);
    let dims = Dimensions::new(w, h).unwrap();
    let img = ImageRef::<Rgba8>::new(&px, dims).unwrap();
    let bytes = JxlEncoder::lossy(Distance::new(1.0).unwrap())
        .with_effort(Effort::Lightning)
        .with_container(Container::IsoBmff)
        .encode_to_vec(img)
        .unwrap();
    let full: ImageBuf<Rgba8> = JxlDecoder::new().decode_image(&bytes).expect("full decode");

    let rendered = sweep::<Rgba8>(&bytes, &full);
    assert!(
        rendered > 0,
        "no truncation of a {w}x{h} lossy stream rendered a single sample"
    );
}

#[test]
fn truncation_before_the_image_headers_is_still_an_error() {
    // Without dimensions there is no buffer to hand back, so the best-effort policy is no more
    // permissive than the rejecting one. This is the one stage that cannot be relaxed.
    let dec = JxlDecoder::new();
    for prefix in [&[][..], &[0xFF][..], &[0xFF, 0x0A][..]] {
        let result = dec.decode_partial::<Rgb8>(prefix);
        let error = result.expect_err("no image headers, no image");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }
}
