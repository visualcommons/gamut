//! Differential conformance for the AVIF decode surface (issue #250): the container view, the
//! `Av1StillDecoder` seam, and both presentation surfaces are checked against **libavif**
//! (`introspect`/`decode_rgba`, dav1d-backed) and against **dav1d directly** (`decode_obu` through
//! the same seam a platform decoder would use), over the libavif conformance corpus committed in
//! `third_party/libavif/tests/data` plus this crate's own encoder output.
//!
//! Building these tests requires the `third_party/libavif` + `third_party/dav1d` submodules and a
//! C toolchain (cmake/meson/ninja; nasm is vendored) — the same set the existing encoder oracle
//! tests already need.

use std::path::PathBuf;

use gamut_avif::{
    Av1Config, Av1StillDecoder, AvifContainer, AvifEncoder, AvifImage, ChromaFormat, DecodedFrame,
    Mirror, Rotation, SegmentKind, Subsampling,
};
use gamut_color::{ColorRange, MatrixCoefficients};
use gamut_core::{Dimensions, EncodeImage, Error, ErrorKind, ImageRef, Result, Rgb8};
use gamut_isobmff::{ColourInformation, PropertyKind};

// ---- the dav1d bridge: an Av1StillDecoder over the raw codestream oracle ---------------------

/// [`Av1StillDecoder`] implemented with the real dav1d decoder (`dav1d-oracle`), bridged exactly
/// the way a platform decoder would be: the typed `av1C` + raw item payload are assembled into
/// one self-contained temporal unit with [`Av1Config::full_stream`] and handed to a Section-5
/// decoder.
struct Dav1dDecoder;

impl Av1StillDecoder for Dav1dDecoder {
    fn decode_still(&mut self, config: &Av1Config, payload: &[u8]) -> Result<DecodedFrame> {
        let mut stream = Vec::new();
        config.full_stream(payload, &mut stream)?;
        let pic = dav1d_oracle::decode_obu(&stream)
            .map_err(|_| Error::InvalidInput("conformance: dav1d rejected the stream"))?;
        let chroma = config.chroma_format();
        let [y, u, v] = pic.planes;
        // `DecodedFrame::new` validates the plane lengths against the av1C-derived chroma, so a
        // config/codestream mismatch fails loudly here.
        DecodedFrame::new(pic.width, pic.height, pic.bit_depth, chroma, y, u, v)
    }
}

// ---- corpus ----------------------------------------------------------------------------------

fn corpus(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/libavif/tests/data")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read corpus fixture {}: {e}", path.display()))
}

/// A procedural RGB source whose every pixel is position-distinct.
fn source_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            rgb.extend_from_slice(&[
                (x * 7 + 13) as u8,
                (y * 11 + 29) as u8,
                (x * 3 + y * 5) as u8,
            ]);
        }
    }
    rgb
}

fn encode(enc: AvifEncoder, rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    enc.encode_image(
        ImageRef::<Rgb8>::new(
            rgb,
            Dimensions {
                width: w,
                height: h,
            },
        )
        .unwrap(),
        &mut out,
    )
    .unwrap();
    out
}

/// Pixel-diff statistics between two RGBA buffers.
struct Diff {
    max_rgb: u8,
    max_alpha: u8,
    mean: f64,
}

fn rgba_diff(a: &[u8], b: &[u8]) -> Diff {
    assert_eq!(a.len(), b.len(), "buffers must be the same size");
    let (mut max_rgb, mut max_alpha, mut sum) = (0u8, 0u8, 0u64);
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        for c in 0..3 {
            let d = pa[c].abs_diff(pb[c]);
            max_rgb = max_rgb.max(d);
            sum += u64::from(d);
        }
        max_alpha = max_alpha.max(pa[3].abs_diff(pb[3]));
    }
    Diff {
        max_rgb,
        max_alpha,
        mean: sum as f64 / (a.len() as f64 * 3.0 / 4.0),
    }
}

// ---- t1: container structure vs libavif introspection ----------------------------------------

#[test]
fn structure_agrees_with_libavif() {
    for name in [
        "io/kodim03_yuv420_8bpc.avif",
        "paris_icc_exif_xmp.avif",
        "sofa_grid1x5_420.avif",
        "abc_color_irot_alpha_irot.avif",
        "white_1x1.avif",
        "extended_pixi.avif",
        "draw_points_idat.avif",
    ] {
        let data = corpus(name);
        let oracle = libavif_oracle::introspect(&data)
            .unwrap_or_else(|e| panic!("{name}: libavif introspect: {e}"));
        let container =
            AvifContainer::parse(&data).unwrap_or_else(|e| panic!("{name}: gamut parse: {e:?}"));
        let img = container.image();
        assert!(img.is_av1_still(), "{name}");
        let primary = img.primary_item();

        // Presentation dimensions: libavif reports the primary's ispe; a grid's ispe is its
        // output size, so the comparison holds for derived primaries too. (`alpha_noispe.avif`
        // is exercised separately — it has no ispe by construction.)
        let dims = primary
            .dimensions()
            .unwrap_or_else(|| panic!("{name}: ispe"));
        assert_eq!(
            (dims.width, dims.height),
            (oracle.width, oracle.height),
            "{name}: dims"
        );

        // av1C facts vs libavif's parsed depth/format. For a derived (grid) primary the config
        // lives on the tiles; resolve through the first derivation source then.
        let coded = if primary.av1_config().is_some() {
            primary
        } else {
            img.derivation_sources(primary.id())[0]
        };
        let config = coded.av1_config().expect("av1C present").unwrap();
        assert_eq!(
            u32::from(config.bit_depth()),
            u32::from(oracle.depth),
            "{name}: depth"
        );
        let oracle_chroma = match oracle.yuv_format {
            1 => ChromaFormat::Yuv444,
            2 => ChromaFormat::Yuv422,
            3 => ChromaFormat::Yuv420,
            _ => ChromaFormat::Monochrome,
        };
        assert_eq!(config.chroma_format(), oracle_chroma, "{name}: chroma");

        // CICP from the nclx colr (when the file's first colr is nclx; `paris` carries ICC too,
        // checked below via the property scan).
        let nclx = primary
            .as_isobmff_item()
            .properties
            .iter()
            .find_map(|p| match &p.kind {
                gamut_isobmff::PropertyKind::Colour(ColourInformation::Nclx(n)) => Some(n),
                _ => None,
            });
        if let Some(nclx) = nclx {
            assert_eq!(nclx.colour_primaries, oracle.color_primaries, "{name}: cp");
            assert_eq!(
                nclx.transfer_characteristics, oracle.transfer_characteristics,
                "{name}: tc"
            );
            assert_eq!(
                nclx.matrix_coefficients, oracle.matrix_coefficients,
                "{name}: mc"
            );
            assert_eq!(nclx.full_range, oracle.full_range, "{name}: range");
        }

        // Alpha presence via the auxiliary lens.
        assert_eq!(
            img.alpha_auxiliary_of(primary.id()).is_some(),
            oracle.alpha_present,
            "{name}: alpha presence"
        );

        // Transform values, wire-exact.
        assert_eq!(primary.rotation(), oracle.irot_angle, "{name}: irot");
        assert_eq!(primary.mirror(), oracle.imir_axis, "{name}: imir");
        let clap = primary.clean_aperture().map(|c| {
            [
                c.width_n,
                c.width_d,
                c.height_n,
                c.height_d,
                c.horiz_off_n,
                c.horiz_off_d,
                c.vert_off_n,
                c.vert_off_d,
            ]
        });
        assert_eq!(clap, oracle.clap, "{name}: clap");
    }
}

#[test]
fn metadata_payloads_agree_with_libavif() {
    let data = corpus("paris_icc_exif_xmp.avif");
    let oracle = libavif_oracle::introspect(&data).unwrap();
    let container = AvifContainer::parse(&data).unwrap();
    let img = container.image();

    // ICC: the raw profile bytes of the primary's ICC colr.
    let icc = img
        .primary_item()
        .as_isobmff_item()
        .properties
        .iter()
        .find_map(|p| match &p.kind {
            gamut_isobmff::PropertyKind::Colour(ColourInformation::RestrictedIcc(b))
            | gamut_isobmff::PropertyKind::Colour(ColourInformation::UnrestrictedIcc(b)) => {
                Some(b.clone())
            }
            _ => None,
        })
        .expect("ICC colr present");
    assert!(!oracle.icc.is_empty());
    assert_eq!(icc, oracle.icc, "ICC bytes verbatim");

    // Exif: gamut exposes the raw item payload including the 4-byte exif_tiff_header_offset
    // prefix; libavif returns the payload with the field stripped and the offset applied.
    let exif_payload = &img.exif().expect("Exif item").as_isobmff_item().payload;
    let offset = u32::from_be_bytes(exif_payload[0..4].try_into().unwrap()) as usize;
    assert!(!oracle.exif.is_empty());
    assert_eq!(
        &exif_payload[4 + offset..],
        &oracle.exif[..],
        "Exif TIFF stream"
    );

    // XMP: the mime item payload verbatim.
    let xmp_payload = &img.xmp().expect("XMP item").as_isobmff_item().payload;
    assert!(!oracle.xmp.is_empty());
    assert_eq!(xmp_payload, &oracle.xmp, "XMP packet verbatim");
}

#[test]
fn content_light_level_agrees_with_libavif() {
    // libavif-written HDR files carrying a `clli` box. (None in the corpus carries `mdcv`: the
    // pinned libavif never writes one, and it skips the box on read, so it cannot vouch for it.)
    for name in ["colors_hdr_rec2020.avif", "seine_hdr_rec2020.avif"] {
        let data = corpus(name);
        let oracle = libavif_oracle::introspect(&data).unwrap();
        let container = AvifContainer::parse(&data).unwrap();
        let clli = container
            .image()
            .primary_item()
            .content_light_level()
            .unwrap_or_else(|| panic!("{name}: clli present"));
        assert_ne!(oracle.clli, [0, 0], "{name}: libavif reports a clli");
        assert_eq!(
            [
                clli.max_content_light_level,
                clli.max_pic_average_light_level
            ],
            oracle.clli,
            "{name}: clli"
        );
    }
}

// ---- t2: presentation pixels vs libavif ------------------------------------------------------

#[test]
fn presentation_pixels_agree_with_libavif() {
    // Files whose colour configuration the RGBA surface supports (BT.601/unspecified/identity,
    // 8-bit), including a real multi-tile grid, an alpha-merged image, and an idat-backed one.
    // Both readers use nearest-neighbour chroma upsampling, so only conversion rounding may
    // differ; alpha must be exact.
    for name in [
        "io/kodim03_yuv420_8bpc.avif",
        "sofa_grid1x5_420.avif",
        "white_1x1.avif",
        "extended_pixi.avif",
        "draw_points_idat.avif",
    ] {
        let data = corpus(name);
        let (ow, oh, oracle) = libavif_oracle::decode_rgba(&data)
            .unwrap_or_else(|e| panic!("{name}: libavif decode_rgba: {e}"));
        let container = AvifContainer::parse(&data).unwrap();
        let ours = container
            .decode_item_rgba8(container.image().primary_item().id(), &mut Dav1dDecoder)
            .unwrap_or_else(|e| panic!("{name}: gamut rgba decode: {e:?}"));
        // libavif does not apply irot/imir/clap; compare pre-transform by decoding the item
        // directly (the primary of every file above carries no transform except
        // abc_..._NOirot, whose colour item is transform-free).
        assert_eq!((ours.width(), ours.height()), (ow, oh), "{name}: dims");
        let diff = rgba_diff(ours.as_samples(), &oracle);
        assert!(
            diff.max_rgb <= 3 && diff.mean <= 0.5,
            "{name}: rgb diff too large (max {}, mean {:.3})",
            diff.max_rgb,
            diff.mean
        );
        assert_eq!(diff.max_alpha, 0, "{name}: alpha must be exact");
    }
}

// ---- t2b/t4: the crate's own encoder output --------------------------------------------------

#[test]
fn self_encoded_lossless_is_bit_exact_end_to_end() {
    // encode (lossless identity 4:4:4) → parse → RGBA via the dav1d seam → the exact source.
    let (w, h) = (23u32, 17u32); // odd dims exercise the ceiling-division paths
    let rgb = source_rgb(w, h);
    let data = encode(AvifEncoder::lossless(), &rgb, w, h);
    let container = AvifContainer::parse(&data).unwrap();
    assert!(container.image().is_av1_still());
    let rgba = container.decode_primary_rgba8(&mut Dav1dDecoder).unwrap();
    assert_eq!((rgba.width(), rgba.height()), (w, h));
    for (px, src) in rgba
        .as_samples()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(rgb.as_chunks::<3>().0)
    {
        assert_eq!(&px[0..3], src);
        assert_eq!(px[3], 255);
    }
}

#[test]
fn self_encoded_orientation_round_trips_through_the_decoder() {
    // The encoder's irot/imir land on the wire (libavif agrees on the values) and the decode
    // surface applies them in the direction the 2022 imir semantics prescribe.
    let (w, h) = (8u32, 6u32);
    let rgb = source_rgb(w, h);
    let data = encode(
        AvifEncoder::lossless()
            .with_rotation(Rotation::Ccw90)
            .with_mirror(Mirror::LeftRight),
        &rgb,
        w,
        h,
    );

    // Wire agreement: libavif sees irot angle 1 and imir axis 1 (left↔right per 23008-12:2022).
    let oracle = libavif_oracle::introspect(&data).unwrap();
    assert_eq!(oracle.irot_angle, Some(1));
    assert_eq!(oracle.imir_axis, Some(1));
    let container = AvifContainer::parse(&data).unwrap();
    assert_eq!(container.image().primary_item().rotation(), Some(1));
    assert_eq!(container.image().primary_item().mirror(), Some(1));

    // Pixel direction: rotate the source 90° CCW, then mirror left↔right — reference transforms
    // written independently of the pipeline's.
    let rgba = container.decode_primary_rgba8(&mut Dav1dDecoder).unwrap();
    assert_eq!((rgba.width(), rgba.height()), (h, w));
    let (nw, nh) = (h as usize, w as usize);
    let (wu, _) = (w as usize, h as usize);
    for oy in 0..nh {
        for ox in 0..nw {
            // Inverse of mirror(left↔right): pre-mirror x = nw-1-ox. Inverse of rotate90ccw at
            // (x', oy): source (sx, sy) = (wu-1-oy, x').
            let px = nw - 1 - ox;
            let (sx, sy) = (wu - 1 - oy, px);
            let src = &rgb[(sy * wu + sx) * 3..(sy * wu + sx) * 3 + 3];
            let got = &rgba.as_samples()[(oy * nw + ox) * 4..(oy * nw + ox) * 4 + 3];
            assert_eq!(got, src, "pixel ({ox},{oy})");
        }
    }
}

// ---- t3: planar bit-exactness through the seam -----------------------------------------------

#[test]
fn planar_decode_is_bit_exact_against_raw_dav1d_and_libavif() {
    // The 10-bit 4:4:4 corpus file: (a) the pipeline's planar output equals dav1d fed the
    // manually assembled temporal unit — proving payload extraction and config delivery do not
    // corrupt a byte — and (b) equals libavif's own decode (two independent container readers
    // over the same normative decoder output).
    let data = corpus("io/cosmos1650_yuv444_10bpc_p3pq.avif");
    let container = AvifContainer::parse(&data).unwrap();
    let img = container.image();
    let primary = img.primary_item();
    let frame = img
        .decode_item_planar(primary.id(), &mut Dav1dDecoder)
        .unwrap();
    assert_eq!(frame.bit_depth(), 10);
    assert_eq!(frame.chroma(), ChromaFormat::Yuv444);

    // (a) direct dav1d over the raw item payload.
    let config = primary.av1_config().unwrap().unwrap();
    let mut stream = Vec::new();
    config
        .full_stream(&primary.as_isobmff_item().payload, &mut stream)
        .unwrap();
    let direct = dav1d_oracle::decode_obu(&stream).unwrap();
    assert_eq!(
        (frame.width(), frame.height()),
        (direct.width, direct.height)
    );
    assert_eq!(frame.y(), &direct.planes[0][..]);
    assert_eq!(frame.cb(), &direct.planes[1][..]);
    assert_eq!(frame.cr(), &direct.planes[2][..]);

    // (b) libavif's full container decode.
    let via_libavif = libavif_oracle::decode_avif(&data).unwrap();
    assert_eq!(frame.y(), &via_libavif.planes[0][..]);
    assert_eq!(frame.cb(), &via_libavif.planes[1][..]);
    assert_eq!(frame.cr(), &via_libavif.planes[2][..]);

    // The 8-bit RGBA surface still declines the 10-bit frame rather than narrowing it.
    assert!(matches!(
        container.decode_item_rgba8(primary.id(), &mut Dav1dDecoder),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));

    // This file's `colr` carries matrix_coefficients = 12 (chromaticity-derived non-constant
    // luminance), which is *not* a modeled Kr/Kb de-matrixing — deriving its weights from the
    // primaries is a separate feature. So even the wide surface declines it, and it declines for
    // that reason rather than for its bit depth: the same file's monochrome/identity paths would
    // present fine. An unmodeled matrix must be refused, never silently approximated.
    let matrix = match primary.colour() {
        Some(gamut_isobmff::ColourInformation::Nclx(nclx)) => nclx.matrix_coefficients,
        other => panic!("expected an nclx colr on the corpus file, got {other:?}"),
    };
    assert_eq!(
        matrix, 12,
        "corpus file's matrix changed; revisit this test"
    );
    assert!(matches!(
        container.decode_primary_rgba16(&mut Dav1dDecoder),
        Err(error) if error.kind() == ErrorKind::Unsupported
    ));
}

#[test]
fn wide_surface_widens_a_real_eight_bit_codestream_exactly() {
    // The widening invariant against a genuine dav1d-decoded frame rather than a synthetic one:
    // 8-bit content reaches the 16-bit surface losslessly, since 65535 == 255 · 257.
    //
    // The corpus has no 10-bit file with a modeled matrix (the only 10-bit file is
    // chromaticity-derived, see above), so the high-bit-depth colour maths is validated against an
    // independent f64 reference in `tests/decode.rs` instead of a reference codec here.
    let data = corpus("io/kodim03_yuv420_8bpc.avif");
    let container = AvifContainer::parse(&data).unwrap();
    let narrow = container
        .decode_primary_rgba8(&mut Dav1dDecoder)
        .expect("8-bit RGBA decode");
    let wide = container
        .decode_primary_rgba16(&mut Dav1dDecoder)
        .expect("16-bit RGBA decode");
    assert_eq!(
        (narrow.width(), narrow.height()),
        (wide.width(), wide.height())
    );
    for (i, (&n, &w)) in narrow
        .as_samples()
        .iter()
        .zip(wide.as_samples())
        .enumerate()
    {
        assert_eq!(u16::from(n) * 257, w, "sample {i}");
    }
}

// ---- t5: byte accounting on a real file + appended stream ------------------------------------

#[test]
fn appended_stream_preserves_decode_and_accounts_every_byte() {
    let clean = corpus("io/kodim03_yuv420_8bpc.avif");
    let mut appended = clean.clone();
    // A second whole "file" (second top-level ftyp + opaque bytes), phone-motion-photo style.
    appended.extend_from_slice(&{
        let mut ftyp = (16u32).to_be_bytes().to_vec();
        ftyp.extend_from_slice(b"ftypmp42");
        ftyp.extend_from_slice(&[0, 0, 0, 0]);
        ftyp
    });
    appended.extend_from_slice(b"opaque-vendor-stream");

    let c_clean = AvifContainer::parse(&clean).unwrap();
    let c_appended = AvifContainer::parse(&appended).unwrap();
    // Every byte accounted: contiguous segments covering 0..len.
    let segs = c_appended.segments();
    assert!(
        gamut_core::segment::covers(segs, appended.len()),
        "segments must tile the file: {:?}",
        segs.iter().map(|s| s.range.clone()).collect::<Vec<_>>()
    );
    assert_eq!(
        c_appended.appended_stream().map(<[u8]>::len),
        Some(appended.len() - clean.len())
    );
    // The appended stream changes nothing about the primary decode.
    let a = c_clean.decode_primary_rgba8(&mut Dav1dDecoder).unwrap();
    let b = c_appended.decode_primary_rgba8(&mut Dav1dDecoder).unwrap();
    assert_eq!(a.as_samples(), b.as_samples());
}

// ---- t6: av1C coherence with the decoded stream ----------------------------------------------

#[test]
fn av1c_agrees_with_the_decoded_frames() {
    for name in [
        "io/kodim03_yuv420_8bpc.avif",
        "io/cosmos1650_yuv444_10bpc_p3pq.avif",
        "abc_color_irot_alpha_irot.avif",
    ] {
        let data = corpus(name);
        let container = AvifContainer::parse(&data).unwrap();
        let img = container.image();
        for item in img.items() {
            let Some(config) = item.av1_config() else {
                continue;
            };
            let config = config.unwrap();
            // The pipeline validates the payload, decodes through dav1d, and `DecodedFrame::new`
            // re-checks the plane geometry against the av1C-derived chroma — so a mismatch
            // between the record and the actual codestream cannot pass.
            let frame = img
                .decode_item_planar(item.id(), &mut Dav1dDecoder)
                .unwrap();
            assert_eq!(
                frame.bit_depth(),
                config.bit_depth(),
                "{name}: item {}",
                item.id()
            );
            assert_eq!(
                frame.chroma(),
                config.chroma_format(),
                "{name}: item {}",
                item.id()
            );
        }
    }
}

// ---- alpha merge against libavif -------------------------------------------------------------

/// Independent 90° CCW rotation of an interleaved RGBA buffer (forward scatter).
fn ref_rotate_ccw(src: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    let (wu, hu) = (w as usize, h as usize);
    let (nw, nh) = (hu, wu);
    let mut out = vec![0u8; nw * nh * 4];
    for y in 0..hu {
        for x in 0..wu {
            let (ox, oy) = (y, wu - 1 - x);
            out[(oy * nw + ox) * 4..(oy * nw + ox) * 4 + 4]
                .copy_from_slice(&src[(y * wu + x) * 4..(y * wu + x) * 4 + 4]);
        }
    }
    (out, h, w)
}

#[test]
fn alpha_merge_agrees_with_libavif() {
    // The alpha-carrying abc file. libavif merges the alpha but leaves the colour item's `irot`
    // unapplied; our surface applies it — so rotate libavif's output by the introspected angle
    // with an independent reference transform before comparing. Alpha must be exact; colour is
    // within conversion-rounding tolerance.
    let data = corpus("abc_color_irot_alpha_NOirot.avif");
    let oracle = libavif_oracle::introspect(&data).unwrap();
    assert!(oracle.alpha_present);
    let (ow, oh, mut oracle_rgba) = libavif_oracle::decode_rgba(&data).unwrap();
    let (mut rw, mut rh) = (ow, oh);
    for _ in 0..oracle.irot_angle.unwrap_or(0) {
        let rotated = ref_rotate_ccw(&oracle_rgba, rw, rh);
        oracle_rgba = rotated.0;
        rw = rotated.1;
        rh = rotated.2;
    }
    let container = AvifContainer::parse(&data).unwrap();
    let ours = container
        .decode_item_rgba8(container.image().primary_item().id(), &mut Dav1dDecoder)
        .unwrap();
    assert_eq!((ours.width(), ours.height()), (rw, rh));
    let diff = rgba_diff(ours.as_samples(), &oracle_rgba);
    assert_eq!(diff.max_alpha, 0, "alpha must be exact");
    assert!(
        diff.max_rgb <= 3 && diff.mean <= 0.5,
        "rgb diff too large (max {}, mean {:.3})",
        diff.max_rgb,
        diff.mean
    );
    // And the file genuinely has non-trivial alpha (guards against an all-opaque comparison).
    assert!(
        ours.as_samples()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|px| px[3] != 255),
        "fixture must carry real alpha"
    );
}

#[test]
fn non_essential_transforms_diverge_documented() {
    // `clap_irot_imir_non_essential.avif` violates the MIAF SHALL that transformative properties
    // be essential. libavif rejects the file outright (its own test suite pins that); gamut's
    // parse is deliberately permissive — the container still parses, the lens surfaces the wire
    // values, and MIAF conformance is reported, not enforced.
    let data = corpus("clap_irot_imir_non_essential.avif");
    assert!(libavif_oracle::introspect(&data).is_err());
    let container = AvifContainer::parse(&data).unwrap();
    let primary = container.image().primary_item();
    assert_eq!(primary.rotation(), Some(1));
    assert!(primary.mirror().is_some());
    assert!(primary.clean_aperture().is_some());
    assert!(primary.is_miaf_transform_ordered());
}

// ---- out-of-scope and hostile shapes ---------------------------------------------------------

/// The libavif `avis` fixtures: each carries a `meta` primary still beside its `moov` track.
const SEQUENCE_FIXTURES: [&str; 6] = [
    "colors-animated-8bpc.avif",
    "colors-animated-8bpc-audio.avif",
    "colors-animated-8bpc-alpha-exif-xmp.avif",
    "colors-animated-8bpc-depth-exif-xmp.avif",
    "colors-animated-12bpc-keyframes-0-2-3.avif",
    "poc_b_506387278.avif",
];

#[test]
fn animated_sequence_parses_its_still_and_accounts_the_track() {
    // Every libavif `avis` fixture carries a `meta` primary still beside its `moov` (AVIF v1.2.0
    // §7). Sequence decode stays out of scope, but the still parses (#646): the track is skipped
    // by the model and accounted as one opaque top-level box, so the segments still tile the file.
    for name in SEQUENCE_FIXTURES {
        let data = corpus(name);
        let container = AvifContainer::parse(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            container.image().primary_item().kind().is_av1(),
            "{name}: primary still is an av01 item"
        );
        // `avis` is not a still brand, but each fixture also lists `avif`/`mif1`, so the
        // still-image predicate keeps its meaning and holds (#652).
        assert!(container.image().is_av1_still(), "{name}: is an AV1 still");
        assert!(
            gamut_core::segment::covers(container.segments(), data.len()),
            "{name}: segments tile the file"
        );
        assert!(
            container
                .segments()
                .iter()
                .any(|s| matches!(s.kind, SegmentKind::Box { ty, .. } if ty == *b"moov")),
            "{name}: moov accounted as a top-level box"
        );
    }
}

#[test]
fn sequence_primary_still_decodes_bit_exact_against_libavif_primary_item() {
    // #652: the still an `avis` file carries is the image a still-only reader shows. libavif,
    // pinned to its primary item (`AVIF_DECODER_SOURCE_PRIMARY_ITEM` — its default would decode
    // the track), is the oracle; the coded planes must agree exactly.
    for name in SEQUENCE_FIXTURES {
        let data = corpus(name);
        let container = AvifContainer::parse(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
        let frame = container
            .image()
            .decode_item_planar(container.image().primary_item().id(), &mut Dav1dDecoder)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let oracle = libavif_oracle::decode_primary_item(&data)
            .unwrap_or_else(|e| panic!("{name}: libavif: {e}"));
        assert_eq!(
            (frame.width(), frame.height(), frame.bit_depth()),
            (oracle.width, oracle.height, oracle.bit_depth),
            "{name}: geometry"
        );
        assert_eq!(frame.y(), &oracle.planes[0][..], "{name}: Y");
        assert_eq!(frame.cb(), &oracle.planes[1][..], "{name}: Cb");
        assert_eq!(frame.cr(), &oracle.planes[2][..], "{name}: Cr");
    }
}

#[test]
fn has_image_sequence_reports_only_a_primary_stream_moov() {
    // True for every `avis` fixture's top-level `moov` …
    for name in SEQUENCE_FIXTURES {
        let data = corpus(name);
        assert!(
            AvifContainer::parse(&data).unwrap().has_image_sequence(),
            "{name}"
        );
    }
    // … false for a plain still …
    let still = corpus("io/kodim03_yuv420_8bpc.avif");
    assert!(!AvifContainer::parse(&still).unwrap().has_image_sequence());
    // … and false when the only `moov` belongs to an appended foreign stream (a motion-photo
    // MP4 after a second top-level `ftyp`), which is not this file's image.
    let mut appended = still;
    for (ty, len) in [(*b"ftyp", 16u32), (*b"moov", 8)] {
        appended.extend_from_slice(&len.to_be_bytes());
        appended.extend_from_slice(&ty);
        appended.resize(appended.len() + len as usize - 8, 0);
    }
    let container = AvifContainer::parse(&appended).unwrap();
    assert!(container.appended_stream().is_some());
    assert!(!container.has_image_sequence());
}

#[test]
fn no_ispe_file_still_parses_and_decodes() {
    // `alpha_noispe.avif` omits the (mandatory) ispe on the alpha item; parsing is permissive,
    // the lens reports no dimensions, and the coded frames still decode through the seam.
    let data = corpus("alpha_noispe.avif");
    let container = AvifContainer::parse(&data).unwrap();
    let img = container.image();
    let primary = img.primary_item();
    let frame = img
        .decode_item_planar(primary.id(), &mut Dav1dDecoder)
        .unwrap();
    let oracle = libavif_oracle::introspect(&data).unwrap();
    assert_eq!(
        (frame.width(), frame.height()),
        (oracle.width, oracle.height)
    );
}

// ---- t7: the AV1 sequence header, and the colour of a colr-less item (issue #651) ------------

/// `bytes` with every `colr` property removed from every item: per AVIF v1.2.0 §2.2.1 each item's
/// colour now comes from its AV1 sequence header.
fn strip_colr(bytes: &[u8]) -> Vec<u8> {
    let mut model = gamut_isobmff::read(bytes).unwrap();
    for item in &mut model.items {
        item.properties
            .retain(|p| !matches!(p.kind, PropertyKind::Colour(_)));
    }
    gamut_isobmff::write(&model).unwrap()
}

/// The coded item whose sequence header governs the primary's colour: the primary itself, or a
/// grid primary's first tile (the one libavif harvests CICP from).
fn colour_source_id(img: &AvifImage) -> u32 {
    let primary = img.primary_item().id();
    img.derivation_sources(primary)
        .first()
        .map_or(primary, |tile| tile.id())
}

/// libavif's raw `avifPixelFormat` for a sequence header's chroma layout.
fn avif_pixel_format(subsampling: Subsampling) -> u32 {
    match subsampling {
        Subsampling::Yuv444 => 1,
        Subsampling::Yuv422 => 2,
        Subsampling::Yuv420 => 3,
        Subsampling::Monochrome => 4,
    }
}

/// This crate's lossy encodes at both ends of the colour space: the BT.709 full-range default, and
/// BT.601 at studio range.
fn self_encoded_lossy() -> [(&'static str, Vec<u8>); 2] {
    let (w, h) = (24u32, 16u32);
    let rgb = source_rgb(w, h);
    [
        (
            "lossy BT.709 full",
            encode(AvifEncoder::lossy(50), &rgb, w, h),
        ),
        (
            "lossy BT.601 limited",
            encode(
                AvifEncoder::lossy(50)
                    .with_matrix(MatrixCoefficients::Bt601)
                    .with_color_range(ColorRange::Limited),
                &rgb,
                w,
                h,
            ),
        ),
    ]
}

#[test]
fn sequence_header_fields_agree_with_libavif() {
    // With every colr stripped, libavif reports the primary's colour from the AV1 sequence header,
    // so its depth, pixel format, CICP and range are the header's fields as libavif parsed them —
    // checked here against the header gamut-av1 parses.
    let mut files: Vec<(String, Vec<u8>)> = [
        "io/kodim03_yuv420_8bpc.avif",
        "io/cosmos1650_yuv444_10bpc_p3pq.avif",
        "paris_icc_exif_xmp.avif",
        "sofa_grid1x5_420.avif",
        "abc_color_irot_alpha_irot.avif",
        "white_1x1.avif",
        "extended_pixi.avif",
        "draw_points_idat.avif",
        "alpha_noispe.avif",
    ]
    .into_iter()
    .map(|name| (name.to_string(), corpus(name)))
    .collect();
    files.extend(
        self_encoded_lossy()
            .into_iter()
            .map(|(label, data)| (label.to_string(), data)),
    );
    for (name, data) in files {
        let data = strip_colr(&data);
        let oracle = libavif_oracle::introspect(&data)
            .unwrap_or_else(|e| panic!("{name}: libavif introspect: {e}"));
        let container = AvifContainer::parse(&data).unwrap();
        let img = container.image();
        let colour = img
            .av1_sequence_header(colour_source_id(img))
            .unwrap_or_else(|e| panic!("{name}: sequence header: {e:?}"))
            .color;
        assert_eq!(colour.bit_depth, u32::from(oracle.depth), "{name}: depth");
        assert_eq!(
            avif_pixel_format(colour.subsampling),
            oracle.yuv_format,
            "{name}: pixel format"
        );
        assert_eq!(
            (
                u16::from(colour.color_primaries),
                u16::from(colour.transfer_characteristics),
                u16::from(colour.matrix_coefficients),
            ),
            (
                oracle.color_primaries,
                oracle.transfer_characteristics,
                oracle.matrix_coefficients,
            ),
            "{name}: CICP"
        );
        assert_eq!(colour.full_range, oracle.full_range, "{name}: range");
    }
}

#[test]
fn colr_stripped_file_presents_like_the_original_and_like_libavif() {
    // This crate's encoder stamps the same CICP into colr and the sequence header, so once colr is
    // stripped the header alone must reproduce the original presentation exactly — the BT.709
    // full-range default included, which the old BT.601-limited fallback could not. libavif reads
    // the stripped file the same way, within conversion rounding.
    let decode = |bytes: &[u8]| {
        AvifContainer::parse(bytes)
            .unwrap()
            .decode_primary_rgba8(&mut Dav1dDecoder)
            .unwrap()
            .into_samples()
    };
    for (label, data) in self_encoded_lossy() {
        let stripped = strip_colr(&data);
        let container = AvifContainer::parse(&stripped).unwrap();
        assert!(
            container.image().primary_item().colour().is_none(),
            "{label}: the stripped file carries no colr"
        );
        let ours = decode(&stripped);
        assert_eq!(ours, decode(&data), "{label}: stripped vs original");

        let (_, _, oracle_stripped) = libavif_oracle::decode_rgba(&stripped)
            .unwrap_or_else(|e| panic!("{label}: libavif decode_rgba: {e}"));
        let (_, _, oracle_original) = libavif_oracle::decode_rgba(&data).unwrap();
        assert_eq!(
            oracle_stripped, oracle_original,
            "{label}: libavif reads the stripped fixture as the original"
        );
        let diff = rgba_diff(&ours, &oracle_stripped);
        assert!(
            diff.max_rgb <= 3 && diff.mean <= 0.5,
            "{label}: rgb diff too large (max {}, mean {:.3})",
            diff.max_rgb,
            diff.mean
        );
    }
}
