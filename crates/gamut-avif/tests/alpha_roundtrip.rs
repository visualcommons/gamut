//! The alpha and grayscale input surface: encode `Rgba8`/`Gray8`, then check both what the
//! container says and what a real AVIF reader makes of it.
//!
//! Two independent questions, so two kinds of assertion. *Structure* — the auxiliary item is
//! hidden, essential-`auxC`-typed, `auxl`-linked, monochrome and `colr`-free — is read back with
//! this crate's own parser, because it is the file layout AVIF v1.2.0 §4 prescribes. *Meaning* —
//! that a reader recovers the caller's alpha — is asked of **libavif** (dav1d backend), which
//! merges the auxiliary into its RGBA presentation exactly as a browser would.
//!
//! libavif is linked in from the `third_party/libavif` + `third_party/dav1d` submodules via the
//! `libavif-oracle` dev-dependency, so the check is hermetic. Building it needs cmake/meson/ninja
//! and the checked-out submodules (`git submodule update --init --recursive` plus `mise run
//! fetch-av1-oracles`).

use gamut_avif::{AlphaMode, AvifContainer, AvifEncoder, AvifItem, ChromaFormat};
use gamut_color::{ColourPrimaries, MatrixCoefficients, TransferCharacteristics};
use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef, Rgb8, Rgb16, Rgba8, Rgba16};

const W: u32 = 34;
const H: u32 = 18;

const DIMS: Dimensions = Dimensions {
    width: W,
    height: H,
};

/// The `aux_type` URN AVIF v1.2.0 §4 assigns an alpha auxiliary. Spelled out here rather than
/// imported so the test pins the wire string independently of the constant the encoder uses.
const ALPHA_URN: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha";

/// A source whose alpha is **structured and non-trivial**: a diagonal ramp with fully transparent
/// and fully opaque runs, so an encoder that dropped, replicated, or transposed the plane is
/// caught. Colour varies independently of alpha, so the two cannot be confused for one another.
fn rgba_at(x: u32, y: u32) -> [u8; 4] {
    [
        ((x * 7 + y * 3) & 0xff) as u8,
        ((x * x + y) & 0xff) as u8,
        ((x ^ (y * 5)) & 0xff) as u8,
        ((x * 11 + y * 29) % 256) as u8,
    ]
}

fn source_rgba() -> Vec<u8> {
    let mut px = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&rgba_at(x, y));
        }
    }
    px
}

fn gray_at(x: u32, y: u32) -> u8 {
    ((x * 13 + y * 7) & 0xff) as u8
}

fn source_gray() -> Vec<u8> {
    let mut px = vec![0u8; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            px[(y * W + x) as usize] = gray_at(x, y);
        }
    }
    px
}

fn encode_rgba(encoder: &AvifEncoder) -> Vec<u8> {
    let px = source_rgba();
    encoder
        .encode_to_vec(ImageRef::<Rgba8>::new(&px, DIMS).expect("buffer matches dimensions"))
        .expect("encode")
}

fn encode_gray(encoder: &AvifEncoder) -> Vec<u8> {
    let px = source_gray();
    encoder
        .encode_to_vec(ImageRef::<Gray8>::new(&px, DIMS).expect("buffer matches dimensions"))
        .expect("encode")
}

/// The items `item` points at with a `ref_type` item reference, if it owns one. Reads the raw
/// `iref` vector, so the *direction* of a reference is observable — the crate's own lenses answer
/// "is there one", which cannot distinguish the two ends.
fn targets<'a>(item: &'a gamut_isobmff::Item, ref_type: &[u8; 4]) -> Option<&'a [u32]> {
    item.references
        .iter()
        .find(|r| &r.reference_type == ref_type)
        .map(|r| &r.to_item_ids[..])
}

/// The `av1C` chroma format an item declares.
fn chroma(item: &AvifItem<'_>) -> ChromaFormat {
    item.av1_config()
        .expect("an av01 item has an av1C")
        .expect("the record parses")
        .chroma_format()
}

// ---- structure --------------------------------------------------------------------------------

#[test]
fn alpha_is_a_conformant_auxiliary_image_item() {
    let avif = encode_rgba(&AvifEncoder::new());
    let container = AvifContainer::parse(&avif).expect("our own reader parses the file");
    let img = container.image();

    // Exactly two items: the colour image and its alpha. The primary is still id 1, so attaching
    // alpha does not renumber anything a metadata item already depended on.
    assert_eq!(img.items().count(), 2);
    let colour = img.primary_item();
    assert_eq!(colour.id(), 1);
    assert_eq!(colour.bits_per_channel(), Some(&[8u8, 8, 8][..]));
    assert_eq!(chroma(&colour), ChromaFormat::Yuv444);

    // The alpha auxiliary is reached the way a reader reaches it: an `auxl` reference to the
    // colour item, typed by the alpha URN.
    let alpha = img
        .alpha_auxiliary_of(colour.id())
        .expect("the colour item has an alpha auxiliary");
    assert_eq!(alpha.id(), 2);
    assert_eq!(alpha.auxiliary_type(), Some(ALPHA_URN));
    assert_eq!(
        alpha.dimensions(),
        Some(DIMS),
        "aux ispe matches the master"
    );

    // AVIF v1.2.0 §4.1: an AV1 auxiliary image item shall be monochrome, and its `pixi` describes
    // the one channel it actually codes.
    assert_eq!(chroma(&alpha), ChromaFormat::Monochrome);
    assert_eq!(alpha.bits_per_channel(), Some(&[8u8][..]));
    // §4.1: `colr` should be omitted — the samples are opacity, not colour.
    assert!(alpha.colour().is_none(), "no colr on an alpha item");

    let raw = alpha.as_isobmff_item();
    // Hidden, because an alpha plane is not independently displayable.
    assert!(raw.hidden);
    // The `auxC` type is *essential*: a reader that does not understand it must refuse the item
    // rather than display an opacity map as a picture.
    let aux_property = raw
        .properties
        .iter()
        .find(|p| matches!(p.kind, gamut_isobmff::PropertyKind::AuxiliaryType { .. }))
        .expect("auxC present");
    assert!(aux_property.essential, "auxC must be essential");
    // `auxl` runs auxiliary → master, so the reference lives on the auxiliary.
    assert_eq!(targets(raw, b"auxl"), Some(&[1u32][..]));
    assert_eq!(
        targets(colour.as_isobmff_item(), b"auxl"),
        None,
        "the colour item does not own the auxl reference"
    );
}

#[test]
fn premultiplication_is_declared_only_when_asked() {
    // `prem` runs the other way to `auxl` — colour image → alpha auxiliary — so getting the
    // direction wrong is invisible to a presence check on the wrong item.
    for premultiplied in [false, true] {
        let avif = encode_rgba(&AvifEncoder::new().with_premultiplied_alpha(premultiplied));
        let img = AvifContainer::parse(&avif).expect("parses");
        let img = img.image();
        assert_eq!(img.is_premultiplied(1), premultiplied);
        assert_eq!(
            targets(img.primary_item().as_isobmff_item(), b"prem"),
            premultiplied.then_some(&[2u32][..]),
            "prem targets the alpha item, from the colour item"
        );
        // libavif reads the same reference, so the declaration is not private to our own model.
        let oracle = libavif_oracle::introspect(&avif).expect("libavif parses");
        assert_eq!(oracle.premultiplied_alpha, premultiplied);
    }
    // Without an alpha channel there is nothing to premultiply, and the knob reaches no box.
    let rgb = vec![0u8; (W * H * 3) as usize];
    let avif = AvifEncoder::new()
        .with_premultiplied_alpha(true)
        .encode_to_vec(ImageRef::<Rgb8>::new(&rgb, DIMS).unwrap())
        .expect("encode");
    assert!(
        !AvifContainer::parse(&avif)
            .unwrap()
            .image()
            .is_premultiplied(1)
    );
}

/// [`source_rgba`] with every alpha sample fully opaque. The colour still varies, so an opacity
/// test that read a colour channel instead of alpha would see non-maximal samples.
fn opaque_rgba() -> Vec<u8> {
    let mut px = source_rgba();
    for alpha in px.iter_mut().skip(3).step_by(4) {
        *alpha = u8::MAX;
    }
    px
}

/// The 16-bit twin of [`opaque_rgba`], each byte widened by replication.
fn opaque_rgba16() -> Vec<u16> {
    opaque_rgba()
        .iter()
        .map(|&s| u16::from(s) * 0x101)
        .collect()
}

/// Every pixel's first three samples: the colour of an RGBA buffer without its alpha.
fn colour_of<S: Copy>(rgba: &[S]) -> Vec<S> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, _]| [r, g, b])
        .collect()
}

/// Whether the file's primary item has an alpha auxiliary, as a reader would find it.
fn has_alpha_auxiliary(avif: &[u8]) -> bool {
    let container = AvifContainer::parse(avif).expect("our own reader parses the file");
    let img = container.image();
    img.alpha_auxiliary_of(img.primary_item().id()).is_some()
}

#[test]
fn an_opaque_rgba8_is_coded_as_its_rgb8() {
    let px = opaque_rgba();
    let rgb = colour_of(&px);
    // Lossy too, so the RGB path's 4:2:0 — which the RGBA path does not honour — is reached; and
    // a premultiplying mode, which on opaque pixels changes nothing and so declares nothing.
    for encoder in [
        AvifEncoder::new(),
        AvifEncoder::lossy(60),
        AvifEncoder::new().with_alpha_mode(AlphaMode::PremultiplyOnEncode),
    ] {
        let from_rgba = encoder
            .encode_to_vec(ImageRef::<Rgba8>::new(&px, DIMS).unwrap())
            .expect("encode RGBA");
        let from_rgb = encoder
            .encode_to_vec(ImageRef::<Rgb8>::new(&rgb, DIMS).unwrap())
            .expect("encode RGB");
        assert_eq!(from_rgba, from_rgb, "{encoder:?}");
    }
}

#[test]
fn an_opaque_rgba16_is_coded_as_its_rgb16() {
    let px = opaque_rgba16();
    let rgb = colour_of(&px);
    let encoder = AvifEncoder::new();
    let from_rgba = encoder
        .encode_to_vec(ImageRef::<Rgba16>::new(&px, DIMS).unwrap())
        .expect("encode RGBA");
    let from_rgb = encoder
        .encode_to_vec(ImageRef::<Rgb16>::new(&rgb, DIMS).unwrap())
        .expect("encode RGB");
    assert_eq!(from_rgba, from_rgb);
}

#[test]
fn one_non_opaque_alpha_sample_keeps_the_alpha_auxiliary() {
    // The last pixel, so a scan that stopped early would miss it; one code below the maximum, so
    // the test is opacity and not merely transparency.
    let mut px = opaque_rgba();
    let last = px.len() - 1;
    px[last] = u8::MAX - 1;
    let avif = AvifEncoder::new()
        .encode_to_vec(ImageRef::<Rgba8>::new(&px, DIMS).unwrap())
        .expect("encode");
    assert!(has_alpha_auxiliary(&avif));
}

#[test]
fn a_16_bit_alpha_below_full_scale_keeps_the_alpha_auxiliary() {
    // 65534 truncates to the 12-bit maximum, so the *coded* alpha is opaque — but opacity is
    // judged on the caller's samples, and the caller's are not all opaque.
    let mut px = opaque_rgba16();
    let last = px.len() - 1;
    px[last] = u16::MAX - 1;
    let avif = AvifEncoder::new()
        .encode_to_vec(ImageRef::<Rgba16>::new(&px, DIMS).unwrap())
        .expect("encode");
    assert!(has_alpha_auxiliary(&avif));
}

#[test]
fn keep_opaque_alpha_keeps_the_alpha_auxiliary_of_an_opaque_rgba8() {
    let px = opaque_rgba();
    let avif = AvifEncoder::new()
        .with_keep_opaque_alpha(true)
        .encode_to_vec(ImageRef::<Rgba8>::new(&px, DIMS).unwrap())
        .expect("encode");
    assert!(has_alpha_auxiliary(&avif));
}

#[test]
fn grayscale_is_one_monochrome_item() {
    let avif = encode_gray(&AvifEncoder::new());
    let container = AvifContainer::parse(&avif).expect("parses");
    let img = container.image();
    assert_eq!(img.items().count(), 1, "no replicated chroma, no auxiliary");
    let primary = img.primary_item();
    assert_eq!(chroma(&primary), ChromaFormat::Monochrome);
    // One channel declared, because one is coded — three would claim colour the item does not hold.
    assert_eq!(primary.bits_per_channel(), Some(&[8u8][..]));
    assert!(img.alpha_auxiliary_of(primary.id()).is_none());
}

#[test]
fn a_monochrome_item_costs_the_advanced_profile_brand() {
    // AVIF v1.2.0 §8.2 and §8.3 both constrain "all AV1 Image Items", not just the primary, and
    // each names one AV1 profile: Main for `MA1B`, High for `MA1A`. A monochrome item is Main
    // (profile 0), which decides all three cases below.
    //
    // - `rgb`: one 4:4:4 High item — `MA1A`.
    // - `rgba`: a 4:4:4 High colour item *beside* a monochrome Main alpha auxiliary. The file is
    //   High and Main at once, so it fits neither profile and §8.1 blesses signalling only the
    //   general brands. This is the case the alpha auxiliary introduces.
    // - `gray`: a single monochrome item, uniformly Main — so `MA1B`, which §8.2 constrains on the
    //   AV1 profile and level alone (its other requirements are self-containment, which this
    //   writer satisfies, and four `should`s about sequences and tracks that a still cannot reach).
    let rgb = vec![0u8; (W * H * 3) as usize];
    let colour_only = AvifEncoder::new()
        .encode_to_vec(ImageRef::<Rgb8>::new(&rgb, DIMS).unwrap())
        .expect("encode");
    for (name, avif, advanced, baseline) in [
        ("rgb", colour_only, true, false),
        ("rgba", encode_rgba(&AvifEncoder::new()), false, false),
        ("gray", encode_gray(&AvifEncoder::new()), false, true),
    ] {
        let container = AvifContainer::parse(&avif).expect("parses");
        let brands = container.image().compatible_brands().to_vec();
        assert!(
            brands.starts_with(&[*b"avif", *b"mif1", *b"miaf"]),
            "{name}: general brands are always signalled"
        );
        assert_eq!(
            brands.contains(b"MA1A"),
            advanced,
            "{name}: MA1A claim, brands {brands:?}"
        );
        assert_eq!(
            brands.contains(b"MA1B"),
            baseline,
            "{name}: MA1B claim, brands {brands:?}"
        );
    }
}

// ---- meaning, per libavif ---------------------------------------------------------------------

#[test]
fn lossless_rgba_round_trips_through_libavif() {
    let avif = encode_rgba(&AvifEncoder::new());
    let oracle = libavif_oracle::introspect(&avif).expect("libavif parses");
    assert!(oracle.alpha_present);
    assert!(!oracle.premultiplied_alpha);

    // libavif's own RGBA presentation: it decodes both items and merges the auxiliary. Lossless
    // identity coding means every channel — alpha included — must come back exactly.
    let (w, h, rgba) = libavif_oracle::decode_rgba(&avif).expect("libavif decodes");
    assert_eq!((w, h), (W, H));
    assert_eq!(rgba, source_rgba());

    // `decode_avif` leaves libavif in its **strict** mode, unlike `introspect`/`decode_rgba` — so
    // this also puts the file through the reader's own conformance checks on an alpha item, the
    // `ispe` requirement among them.
    let strict = libavif_oracle::decode_avif(&avif).expect("strict libavif decode");
    assert!(!strict.monochrome(), "the colour item stays 4:4:4");
}

#[test]
fn premultiply_on_encode_round_trips_through_libavif_within_rounding() {
    // The caller's colour is unassociated; the encoder premultiplies it and declares `prem`.
    // libavif must see the declaration and un-premultiply on its own, giving back the source:
    // alpha exactly (premultiplying never touches it), opaque pixels exactly, and every other
    // colour within the rounding premultiplication costs — round-to-nearest both ways leaves at
    // most ⌊(a + 255) / 2a⌋ code values at alpha `a`. A fully transparent pixel keeps no colour.
    let avif = encode_rgba(&AvifEncoder::new().with_alpha_mode(AlphaMode::PremultiplyOnEncode));
    assert!(
        libavif_oracle::introspect(&avif)
            .expect("libavif parses")
            .premultiplied_alpha
    );

    let (w, h, rgba) = libavif_oracle::decode_rgba(&avif).expect("libavif decodes");
    assert_eq!((w, h), (W, H));
    let source = source_rgba();
    let mut opaque = 0;
    for (i, (got, want)) in rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(source.as_chunks::<4>().0)
        .enumerate()
    {
        let a = want[3];
        assert_eq!(got[3], a, "alpha of pixel {i}");
        let bound = match a {
            0 => continue,
            255 => {
                opaque += 1;
                0
            }
            a => (u16::from(a) + 255) / (2 * u16::from(a)),
        };
        for c in 0..3 {
            let err = u16::from(got[c]).abs_diff(u16::from(want[c]));
            assert!(
                err <= bound,
                "pixel {i} channel {c} at alpha {a}: {got:?} vs {want:?}"
            );
        }
    }
    assert!(opaque > 0, "the source has opaque pixels to hold exact");
}

#[test]
fn lossless_grayscale_round_trips_through_libavif() {
    let avif = encode_gray(&AvifEncoder::new());
    let decoded = libavif_oracle::decode_avif(&avif).expect("libavif decodes");
    assert_eq!((decoded.width, decoded.height), (W, H));
    assert!(decoded.monochrome(), "libavif reports one coded plane");
    let expected: Vec<u16> = source_gray().into_iter().map(u16::from).collect();
    assert_eq!(decoded.planes[0], expected);
    assert!(decoded.planes[1].is_empty() && decoded.planes[2].is_empty());
}

#[test]
fn a_monochrome_item_still_carries_the_primaries_and_transfer_tags() {
    // `with_primaries`/`with_transfer` only *tag* samples, so they apply to a monochrome item just
    // as they do to a colour one — and they reach both the AV1 sequence header and `colr`, which
    // §2.3.4 requires to agree. What does *not* apply is the matrix: there is no chroma to
    // describe, and AV1 §6.4.2 forbids `MC_IDENTITY` on a single-plane stream, so the encoder
    // signals `Unspecified` however `with_matrix` was set.
    let avif = encode_gray(
        &AvifEncoder::new()
            .with_primaries(ColourPrimaries::Bt2020)
            .with_transfer(TransferCharacteristics::Pq)
            .with_matrix(MatrixCoefficients::Bt709),
    );
    let oracle = libavif_oracle::introspect(&avif).expect("libavif parses");
    assert_eq!(oracle.color_primaries, ColourPrimaries::Bt2020.code_point());
    assert_eq!(
        oracle.transfer_characteristics,
        TransferCharacteristics::Pq.code_point()
    );
    assert_eq!(
        oracle.matrix_coefficients,
        MatrixCoefficients::Unspecified.code_point()
    );
    // AVIF v1.2.0 §4.1 makes full range a *shall* for an auxiliary item, and a monochrome AV1
    // stream codes the bit explicitly rather than inferring it.
    assert!(oracle.full_range);
}

#[test]
fn lossy_alpha_is_the_av1_reconstruction_of_the_alpha_plane() {
    // Lossy alpha is coded, not dropped or forced opaque — and coded at the *colour item's*
    // `base_q_idx`, as a monochrome still. The exact assertion is available: quantized output is
    // not the source, but it must be the AV1 encoder's own reconstruction of the same plane, which
    // is what a conformant decoder is obliged to reproduce. A tolerance would pass just as well
    // for an alpha plane quantized at the wrong index, or coded from the wrong channel.
    const QUALITY: u8 = 80;
    let avif = encode_rgba(&AvifEncoder::lossy(QUALITY));
    assert!(libavif_oracle::introspect(&avif).unwrap().alpha_present);

    let px = source_rgba();
    let alpha_planes =
        gamut_color::Planar8::from_rgba8_alpha_view(ImageRef::<Rgba8>::new(&px, DIMS).unwrap());
    let (_, recon) = gamut_av1::encode_still_intra_with(
        &alpha_planes,
        quality_to_quant(QUALITY),
        gamut_av1::Av1Colour::monochrome(),
    )
    .expect("the alpha plane encodes");

    let (_, _, rgba) = libavif_oracle::decode_rgba(&avif).expect("libavif decodes");
    let merged: Vec<u16> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| u16::from(px[3]))
        .collect();
    assert_eq!(merged, recon.planes[0]);
    // And it is genuinely varying alpha, not a constant plane that would match trivially.
    assert!(merged.iter().any(|&a| a < 200) && merged.iter().any(|&a| a > 200));
}

/// The colour and alpha items' coded AV1 payloads, in that order.
fn item_payloads(avif: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let container = AvifContainer::parse(avif).expect("parses");
    let img = container.image();
    let colour = img.primary_item();
    let alpha = img
        .alpha_auxiliary_of(colour.id())
        .expect("alpha auxiliary");
    (
        colour.as_isobmff_item().payload.clone(),
        alpha.as_isobmff_item().payload.clone(),
    )
}

/// The built-in encoder's monochrome stream for the source's alpha plane at `base_q_idx` — what
/// the alpha item must carry, byte for byte, when it is coded at that index.
fn alpha_stream_at(base_q_idx: u8) -> Vec<u8> {
    let px = source_rgba();
    let planes =
        gamut_color::Planar8::from_rgba8_alpha_view(ImageRef::<Rgba8>::new(&px, DIMS).unwrap());
    gamut_av1::encode_still_intra_with(&planes, base_q_idx, gamut_av1::Av1Colour::monochrome())
        .expect("the alpha plane encodes")
        .0
        .obus
}

#[test]
fn alpha_quality_sets_the_alpha_items_coded_base_q_idx() {
    // The coded index is pinned by the stream itself: the alpha item's payload is exactly the
    // built-in encoder's stream at the alpha quality's `base_q_idx` — `gamut-av1`'s own decoder
    // does not parse monochrome streams yet, so the header field is not read directly — and the
    // colour item is exactly what the colour quality codes without the knob.
    for (colour_q, alpha_q) in [(30u8, 90u8), (90, 30), (50, 0), (0, 100)] {
        let (colour, alpha) = item_payloads(&encode_rgba(
            &AvifEncoder::lossy(colour_q).with_alpha_quality(alpha_q),
        ));
        let (plain_colour, plain_alpha) =
            item_payloads(&encode_rgba(&AvifEncoder::lossy(colour_q)));
        let label = format!("lossy({colour_q}).with_alpha_quality({alpha_q})");
        assert_eq!(alpha, alpha_stream_at(quality_to_quant(alpha_q)), "{label}");
        assert_ne!(alpha, plain_alpha, "{label}: the alpha quantizer moved");
        assert_eq!(colour, plain_colour, "{label}: colour untouched");
    }
    // Unset, alpha follows the colour quality — and setting it to that same quality changes no
    // byte, so the knob is inert until it differs.
    let follows = encode_rgba(&AvifEncoder::lossy(40));
    assert_eq!(
        item_payloads(&follows).1,
        alpha_stream_at(quality_to_quant(40))
    );
    assert_eq!(
        encode_rgba(&AvifEncoder::lossy(40).with_alpha_quality(40)),
        follows
    );
    // Lossless ignores it: alpha stays on the lossless path (`base_q_idx` 0), byte-for-byte.
    let lossless = encode_rgba(&AvifEncoder::lossless());
    assert_eq!(item_payloads(&lossless).1, alpha_stream_at(0));
    assert_eq!(
        encode_rgba(&AvifEncoder::lossless().with_alpha_quality(10)),
        lossless
    );
}

#[test]
fn a_finer_alpha_quality_decodes_closer_to_the_source_through_libavif() {
    // Coarse colour, fine alpha. libavif's merged alpha must be the AV1 reconstruction at the
    // *alpha* quantizer (a conformant decoder is obliged to reproduce it exactly), and its error
    // against the source must sit inside the bound that quantizer earns — well below what the
    // same plane costs when it follows the coarse colour quality.
    const COLOUR_Q: u8 = 10;
    const ALPHA_Q: u8 = 95;
    let px = source_rgba();
    let alpha_planes =
        gamut_color::Planar8::from_rgba8_alpha_view(ImageRef::<Rgba8>::new(&px, DIMS).unwrap());
    let source_alpha: Vec<u16> = px
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| u16::from(p[3]))
        .collect();
    let decoded_alpha = |avif: &[u8]| -> Vec<u16> {
        let (_, _, rgba) = libavif_oracle::decode_rgba(avif).expect("libavif decodes");
        rgba.as_chunks::<4>()
            .0
            .iter()
            .map(|p| u16::from(p[3]))
            .collect()
    };
    let max_error = |alpha: &[u16]| -> u16 {
        alpha
            .iter()
            .zip(&source_alpha)
            .map(|(&a, &s)| a.abs_diff(s))
            .max()
            .unwrap_or(0)
    };

    let fine = decoded_alpha(&encode_rgba(
        &AvifEncoder::lossy(COLOUR_Q).with_alpha_quality(ALPHA_Q),
    ));
    let (_, recon) = gamut_av1::encode_still_intra_with(
        &alpha_planes,
        quality_to_quant(ALPHA_Q),
        gamut_av1::Av1Colour::monochrome(),
    )
    .expect("the alpha plane encodes");
    assert_eq!(fine, recon.planes[0]);

    let coarse = decoded_alpha(&encode_rgba(&AvifEncoder::lossy(COLOUR_Q)));
    let (fine_err, coarse_err) = (max_error(&fine), max_error(&coarse));
    assert!(
        fine_err <= FINE_ALPHA_MAX_ERROR,
        "alpha at quality {ALPHA_Q}: max error {fine_err}"
    );
    assert!(
        fine_err < coarse_err,
        "alpha at quality {ALPHA_Q} ({fine_err}) vs following colour quality {COLOUR_Q} ({coarse_err})"
    );
}

/// The largest per-sample alpha error libavif's decode may show at alpha quality 95
/// (`base_q_idx` 12) on [`source_rgba`]. Measured, not derived: the source's alpha wraps from 255
/// to 0 along its diagonal, and those hard edges are where the residual concentrates (44 at the
/// time of writing; the same plane following colour quality 10 shows 171). A drift guard on
/// fidelity — the exact match against the encoder's reconstruction is the correctness check.
const FINE_ALPHA_MAX_ERROR: u16 = 48;

/// Mirrors `gamut-avif`'s documented quality→`base_q_idx` mapping, as `decode_roundtrip.rs` does:
/// the test needs the exact index the encoder selected in order to reproduce its reconstruction.
fn quality_to_quant(quality: u8) -> u8 {
    debug_assert!(quality <= 100, "quality must be 0..=100, got {quality}");
    (((100 - u32::from(quality)) * 255 / 100) as u8).max(1)
}

#[test]
fn odd_and_tiny_sizes_round_trip_with_alpha() {
    // Sizes that exercise edge padding and a single-superblock frame, where a plane-geometry
    // mistake in the monochrome auxiliary would show up first.
    for (w, h) in [(1, 1), (3, 7), (17, 13), (64, 64)] {
        let dims = Dimensions {
            width: w,
            height: h,
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                px[i..i + 4].copy_from_slice(&rgba_at(x, y));
            }
        }
        let avif = AvifEncoder::new()
            .encode_to_vec(ImageRef::<Rgba8>::new(&px, dims).unwrap())
            .unwrap_or_else(|e| panic!("encode {w}x{h}: {e}"));
        let (dw, dh, rgba) = libavif_oracle::decode_rgba(&avif)
            .unwrap_or_else(|e| panic!("libavif decode {w}x{h}: {e}"));
        assert_eq!((dw, dh), (w, h));
        assert_eq!(rgba, px, "{w}x{h}");
    }
}

/// A transparent-edge fixture: a smooth opaque gradient on the left, a soft three-pixel alpha
/// edge, and a fully transparent right half hiding high-frequency noise — the case clean alpha
/// exists for.
fn transparent_edge_rgba() -> Vec<u8> {
    const SIDE: u32 = 64;
    let mut px = Vec::with_capacity((SIDE * SIDE * 4) as usize);
    let mut noise = 0x1234_5678u32;
    for y in 0..SIDE {
        for x in 0..SIDE {
            noise ^= noise << 13;
            noise ^= noise >> 17;
            noise ^= noise << 5;
            let alpha = match x {
                0..30 => 255,
                30 => 170,
                31 => 85,
                32 => 20,
                _ => 0,
            };
            let [r, g, b, _] = noise.to_le_bytes();
            let (r, g, b) = if alpha == 0 {
                (r, g, b)
            } else {
                ((x * 4) as u8, (y * 4) as u8, 128)
            };
            px.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    px
}

#[test]
fn clean_alpha_shrinks_a_lossy_transparent_edge_and_libavif_decodes_it() {
    const SIDE: u32 = 64;
    let dims = Dimensions {
        width: SIDE,
        height: SIDE,
    };
    let px = transparent_edge_rgba();
    let encode = |enc: AvifEncoder| {
        enc.encode_to_vec(ImageRef::<Rgba8>::new(&px, dims).expect("buffer matches dimensions"))
            .expect("encode")
    };
    let dirty = encode(AvifEncoder::lossy(60));
    let clean = encode(AvifEncoder::lossy(60).with_clean_alpha(true));

    // Reported, not just asserted: the size the hidden noise was costing.
    eprintln!(
        "transparent-edge 64x64 lossy(60): dirty {} B, clean {} B",
        dirty.len(),
        clean.len()
    );
    assert!(
        clean.len() < dirty.len(),
        "clean {} B is not smaller than dirty {} B",
        clean.len(),
        dirty.len()
    );
    // Cleaning touches colour only: the alpha item is the same stream either way.
    assert_eq!(item_payloads(&clean).1, item_payloads(&dirty).1);

    let (w, h, _) = libavif_oracle::decode_rgba(&clean).expect("libavif decodes");
    assert_eq!((w, h), (SIDE, SIDE));
    assert!(libavif_oracle::introspect(&clean).unwrap().alpha_present);
}
