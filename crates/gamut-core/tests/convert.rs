//! Public-API tests for [`gamut_core::convert`].
//!
//! The inline unit tests exercise the engine's internals; this drives it through the **public
//! surface only**, the way a format crate or an application does, so the module is proven usable
//! without reaching for anything crate-private.

use gamut_core::convert::{
    AlphaPolicy, ConvertPolicy, DepthPolicy, LumaPolicy, RawImage, convert, convert_from_raw,
    convert_from_raw_into, premultiply_alpha, unpremultiply_alpha,
};
use gamut_core::{
    Dimensions, ErrorKind, Gray8, Gray16, GrayAlpha8, ImageBuf, ImageRef, PixelFormat, Rgb8, Rgb16,
    Rgba8,
};

fn dims(width: u32, height: u32) -> Dimensions {
    Dimensions::new(width, height).expect("non-empty dimensions")
}

/// The lossless direction needs no policy at all, in one chain a caller would plausibly write:
/// a greyscale image widening all the way to 16-bit RGBA.
#[test]
fn widening_needs_no_policy() {
    let d = dims(2, 1);
    let grey = [10u8, 200];

    let rgba: ImageBuf<Rgba8> = convert(
        ImageRef::<Gray8>::new(&grey, d).unwrap(),
        ConvertPolicy::lossless(),
    )
    .unwrap();
    assert_eq!(rgba.as_samples(), &[10, 10, 10, 255, 200, 200, 200, 255]);

    let wide: ImageBuf<Rgb16> = convert(
        ImageRef::<Gray8>::new(&grey, d).unwrap(),
        ConvertPolicy::lossless(),
    )
    .unwrap();
    // 8 -> 16 replicates the byte, so 200 becomes 0xC8C8 rather than 0xC800.
    assert_eq!(
        wide.as_samples(),
        &[0x0A0A, 0x0A0A, 0x0A0A, 0xC8C8, 0xC8C8, 0xC8C8]
    );
}

/// Each lossy axis is refused on its own, and permitted only by the policy that names it — so a
/// caller cannot accidentally authorise more loss than it meant to.
#[test]
fn each_lossy_axis_needs_its_own_policy() {
    let d = dims(1, 1);
    let strict = ConvertPolicy::lossless();

    // Alpha: an alpha policy alone permits it, a depth policy does not.
    let rgba = [1u8, 2, 3, 128];
    let src = || ImageRef::<Rgba8>::new(&rgba, d).unwrap();
    assert_eq!(
        convert::<Rgba8, Rgb8>(src(), strict).unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(
        convert::<Rgba8, Rgb8>(src(), strict.with_depth(DepthPolicy::Rescale))
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(
        convert::<Rgba8, Rgb8>(src(), strict.with_alpha(AlphaPolicy::Drop))
            .unwrap()
            .as_samples(),
        &[1, 2, 3]
    );

    // Depth: only a depth policy permits narrowing.
    let wide = [0xFFFFu16];
    let src16 = || ImageRef::<Gray16>::new(&wide, d).unwrap();
    assert_eq!(
        convert::<Gray16, Gray8>(src16(), strict)
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(
        convert::<Gray16, Gray8>(src16(), strict.with_depth(DepthPolicy::Rescale))
            .unwrap()
            .as_samples(),
        &[255]
    );

    // Colour: only a luma policy permits reduction to grey.
    let rgb = [255u8, 0, 0];
    let src_rgb = || ImageRef::<Rgb8>::new(&rgb, d).unwrap();
    assert_eq!(
        convert::<Rgb8, Gray8>(src_rgb(), strict)
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(
        convert::<Rgb8, Gray8>(src_rgb(), strict.with_luma(LumaPolicy::Bt709))
            .unwrap()
            .as_samples(),
        &[54] // round(0.2126 * 255)
    );
}

/// The decoder-facing door: a runtime `PixelFormat` in, a branded buffer out, reaching the same
/// result as the typed door.
#[test]
fn the_raw_door_matches_the_typed_door() {
    let d = dims(2, 2);
    let grey: Vec<u8> = (0..4).map(|i| i * 40).collect();
    let policy = ConvertPolicy::lossless();

    let raw = RawImage::new(&grey, PixelFormat::Gray8, d).unwrap();
    assert_eq!(raw.format(), PixelFormat::Gray8);
    assert_eq!(raw.dimensions(), d);

    let via_raw: ImageBuf<Rgb8> = convert_from_raw(raw, policy).unwrap();
    let via_typed: ImageBuf<Rgb8> =
        convert(ImageRef::<Gray8>::new(&grey, d).unwrap(), policy).unwrap();
    assert_eq!(via_raw, via_typed);

    // And the borrowed-destination form fills caller storage with the same samples.
    let mut dst = ImageBuf::<Rgb8>::zeroed(d).unwrap();
    convert_from_raw_into::<_, Rgb8>(
        RawImage::new(&grey, PixelFormat::Gray8, d).unwrap(),
        policy,
        dst.as_mut_samples(),
    )
    .unwrap();
    assert_eq!(dst, via_typed);
}

/// Compositing is the alternative to dropping, and its endpoints must be exact: a fully opaque
/// pixel reproduces the source and a fully transparent one reproduces the background.
#[test]
fn compositing_over_a_background_is_exact_at_the_endpoints() {
    let d = dims(3, 1);
    // Opaque, half-transparent, fully transparent.
    let rgba = [10u8, 20, 30, 255, 10, 20, 30, 128, 10, 20, 30, 0];
    let over_white = ConvertPolicy::lossless()
        .with_alpha(AlphaPolicy::CompositeOver)
        .with_background([u16::MAX; 3]);

    let out: ImageBuf<Rgb8> =
        convert(ImageRef::<Rgba8>::new(&rgba, d).unwrap(), over_white).unwrap();
    let samples = out.as_samples();
    assert_eq!(&samples[0..3], &[10, 20, 30], "opaque must be untouched");
    assert_eq!(
        &samples[6..9],
        &[255, 255, 255],
        "clear must be the background"
    );
    // The middle pixel lands strictly between the two.
    assert!(samples[3] > 10 && samples[3] < 255, "got {}", samples[3]);
}

/// The layouts the module deliberately does not convert report that fact, rather than guessing —
/// and no policy overrides it, because the missing machinery is not a policy question.
#[test]
fn palette_and_cmyk_are_refused_under_every_policy() {
    let d = dims(1, 1);
    let indices = [7u8];
    for policy in [ConvertPolicy::lossless(), ConvertPolicy::permissive()] {
        let err = convert_from_raw::<_, Rgb8>(
            RawImage::new(&indices, PixelFormat::Indexed8, d).unwrap(),
            policy,
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert_eq!(err.origin(), Some("gamut-core"));

        let inks = [1u8, 2, 3, 4];
        let err = convert_from_raw::<_, Rgb8>(
            RawImage::new(&inks, PixelFormat::Cmyk8, d).unwrap(),
            policy,
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
    }
}

/// A rejected conversion must not have written anything, so a caller can keep using its buffer.
#[test]
fn a_refused_conversion_leaves_the_destination_untouched() {
    let d = dims(1, 1);
    let rgba = [1u8, 2, 3, 128];
    let mut dst = [9u8; 3];
    let err = convert_from_raw_into::<_, Rgb8>(
        RawImage::new(&rgba, PixelFormat::Rgba8, d).unwrap(),
        ConvertPolicy::lossless(),
        &mut dst,
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(dst, [9; 3]);
}

/// `RawImage::new` is the validation boundary for a runtime-tagged buffer, standing in for the
/// brand a typed buffer carries.
#[test]
fn raw_image_rejects_a_description_that_does_not_match_its_samples() {
    let samples = [0u8; 6];
    assert!(RawImage::new(&samples, PixelFormat::Rgb8, dims(2, 1)).is_ok());
    // Wrong sample count for the dimensions.
    assert!(RawImage::new(&samples, PixelFormat::Rgb8, dims(3, 1)).is_err());
    // 8-bit samples described as a 16-bit layout, and the reverse.
    assert!(RawImage::new(&samples, PixelFormat::Rgb16, dims(2, 1)).is_err());
    assert!(RawImage::new(&[0u16; 6], PixelFormat::Rgb8, dims(2, 1)).is_err());
}

/// The policy is plain, inspectable data: what a caller sets is what a decoder reads back.
#[test]
fn policy_round_trips_through_its_accessors() {
    let policy = ConvertPolicy::lossless()
        .with_alpha(AlphaPolicy::CompositeOver)
        .with_depth(DepthPolicy::Rescale)
        .with_luma(LumaPolicy::Bt2020)
        .with_background([1, 2, 3])
        .with_threshold(4);

    assert_eq!(policy.alpha(), AlphaPolicy::CompositeOver);
    assert_eq!(policy.depth(), DepthPolicy::Rescale);
    assert_eq!(policy.luma(), LumaPolicy::Bt2020);
    assert_eq!(policy.background(), [1, 2, 3]);
    assert_eq!(policy.threshold(), 4);

    // The two named starting points differ in every axis a decoder branches on.
    assert_eq!(ConvertPolicy::default(), ConvertPolicy::lossless());
    assert_eq!(ConvertPolicy::lossless().alpha(), AlphaPolicy::Reject);
    assert_eq!(ConvertPolicy::lossless().depth(), DepthPolicy::Reject);
    assert_eq!(ConvertPolicy::lossless().luma(), LumaPolicy::Reject);
    assert_eq!(ConvertPolicy::permissive().alpha(), AlphaPolicy::Drop);
    assert_eq!(ConvertPolicy::permissive().depth(), DepthPolicy::Rescale);
    assert_eq!(ConvertPolicy::permissive().luma(), LumaPolicy::Bt709);
}

/// GrayAlpha sits in both families at once — one colour channel plus alpha — so it is the layout
/// most likely to be mishandled by a rule written for RGB.
#[test]
fn gray_alpha_widens_and_narrows_on_the_axes_it_should() {
    let d = dims(2, 1);
    let ga = [10u8, 255, 200, 128];
    let src = || ImageRef::<GrayAlpha8>::new(&ga, d).unwrap();

    // Widening into RGBA keeps the alpha and replicates the luma.
    let rgba: ImageBuf<Rgba8> = convert(src(), ConvertPolicy::lossless()).unwrap();
    assert_eq!(rgba.as_samples(), &[10, 10, 10, 255, 200, 200, 200, 128]);

    // Dropping to plain grey is a loss on the alpha axis only; the luma passes through exactly.
    assert_eq!(
        convert::<GrayAlpha8, Gray8>(src(), ConvertPolicy::lossless())
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(
        convert::<GrayAlpha8, Gray8>(
            src(),
            ConvertPolicy::lossless().with_alpha(AlphaPolicy::Drop)
        )
        .unwrap()
        .as_samples(),
        &[10, 200]
    );
}

/// Associated alpha is an explicit in-place step on an owned buffer, not a `ConvertPolicy`
/// conversion: a caller that wants to keep the unassociated image premultiplies a clone, and the
/// layout brand is unchanged either way.
#[test]
fn premultiplying_a_clone_keeps_the_unassociated_image() {
    let d = dims(2, 1);
    // Opaque red, and half-transparent white.
    let straight = ImageBuf::<Rgba8>::new(vec![255, 0, 0, 255, 255, 255, 255, 128], d).unwrap();

    let mut associated = straight.clone();
    premultiply_alpha(&mut associated);
    assert_eq!(
        associated.as_samples(),
        &[255, 0, 0, 255, 128, 128, 128, 128]
    );
    assert_eq!(straight.as_samples(), &[255, 0, 0, 255, 255, 255, 255, 128]);

    unpremultiply_alpha(&mut associated);
    assert_eq!(associated, straight);
}
