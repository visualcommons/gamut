//! libheif differential-conformance suite for `gamut-heic` (issue #238, slice S4).
//!
//! This is the issue-#238 story made real: a third-party HEVC decoder (libde265, reached through the
//! `libheif-oracle` dev crate) is plugged into `gamut-heic`'s pure-Rust container pipeline via the
//! crate's pluggable [`gamut_heic::HevcDecoder`] seam, and the whole path is checked differentially
//! against the de-facto reference reader (libheif). Fixtures are generated at test time with
//! libheif + kvazaar (`encode_rgba_to_heic`), so **no binary fixtures are committed**.
//!
//! The oracle statically builds libheif/libde265/kvazaar from the `third_party/` submodules on first
//! run (several minutes); building the tests needs cmake/ninja, a C/C++ toolchain, and the checked-out
//! submodules (`git submodule update --init --recursive`).
//!
//! ## Kvazaar / libheif domain caveat
//!
//! kvazaar encodes 4:2:0 YCbCr, and libheif's RGB↔YCbCr conversion is not bit-exact, so an RGB
//! round-trip is only *near*-exact — bit-exact comparisons live in the YCbCr/planar domain (test 3).
//! The presentation-pixel tests (2, 4) therefore use a measured tolerance: our nearest co-sited
//! chroma upsample + BT.601 integer conversion differs slightly from libheif's (bilinear upsample).
//! Measured maxima are documented at each assertion.

use std::sync::OnceLock;

use gamut_core::{Error, ErrorKind};
use gamut_heic::{
    ChromaFormat, DecodedFrame, HeifContainer, HevcConfig, HevcDecoder, HevcDecoders, NO_BACKEND,
    iter_nal_units,
};
use libheif_oracle::{EncodeOpts, NclxProfile, OracleChroma};

/// An explicit BT.601 (matrix 6), full-range nclx colour profile. Writing it into the fixture's
/// `colr` box makes gamut-heic and libheif agree on the colour policy — without an explicit `colr`
/// libheif falls back to full-range sRGB internally while gamut-heic's colr-less default is BT.601
/// *limited* range, a documented divergence that would otherwise swamp the presentation-pixel diff.
fn nclx_bt601_full() -> NclxProfile {
    NclxProfile {
        colour_primaries: 1,          // BT.709 primaries
        transfer_characteristics: 13, // sRGB
        matrix_coefficients: 6,       // BT.601 — gamut-heic's supported RGBA matrix
        full_range: true,
    }
}

// ============================================================================================
//   De265Decoder — the third-party HEVC-intra decoder plugged into the pure-Rust pipeline
// ============================================================================================

/// A [`HevcDecoder`] that bridges `gamut-heic`'s container pipeline to libde265 (via the
/// `libheif-oracle` crate's `decode_hevc_intra`). It splits the item payload into NAL units with the
/// crate's own [`iter_nal_units`], delivers the `hvcC` parameter sets as libde265 config NALs, and
/// maps the reconstructed YUV back into a [`DecodedFrame`]. Plugging a real HEVC decoder in behind
/// this trait is exactly what issue #238's decoder seam exists for.
struct De265Decoder;

impl HevcDecoder for De265Decoder {
    fn decode_intra(
        &mut self,
        config: &HevcConfig,
        payload: &[u8],
    ) -> gamut_core::Result<DecodedFrame> {
        let cfg = config_nals(config);
        let pics = picture_nals(payload, config.nal_length_size())?;
        let yuv = libheif_oracle::decode_hevc_intra(&cfg, &pics).map_err(|e| {
            eprintln!("De265Decoder: libde265 decode failed: {e}");
            Error::InvalidInput("De265Decoder: libde265 decode failed")
        })?;
        let chroma = match yuv.chroma {
            OracleChroma::Mono => ChromaFormat::Monochrome,
            OracleChroma::Yuv420 => ChromaFormat::Yuv420,
            OracleChroma::Yuv422 => ChromaFormat::Yuv422,
            OracleChroma::Yuv444 => ChromaFormat::Yuv444,
        };
        let [y, cb, cr] = yuv.planes;
        DecodedFrame::new(yuv.width, yuv.height, yuv.bit_depth, chroma, y, cb, cr)
    }
}

/// The parameter-set NAL units of an `hvcC` record as libde265 config NALs — VPS, then SPS, then
/// PPS, then any non-parameter-set arrays (e.g. SEI), in file order. Mirrors the ordering
/// [`HevcConfig::annex_b`] emits, so the direct-decode oracle in test 3 sees an identical NAL stream.
fn config_nals(config: &HevcConfig) -> Vec<&[u8]> {
    let mut nals: Vec<&[u8]> = Vec::new();
    nals.extend(config.vps());
    nals.extend(config.sps());
    nals.extend(config.pps());
    for array in &config.arrays {
        if !array.nal_unit_type.is_parameter_set() {
            nals.extend(array.nal_units.iter().map(Vec::as_slice));
        }
    }
    nals
}

/// Splits a length-prefixed `hvc1`/`hev1` payload into its picture NAL units with the crate's own
/// [`iter_nal_units`] — the exact split the container pipeline performs before the decoder hook.
fn picture_nals(payload: &[u8], len_size: usize) -> gamut_core::Result<Vec<&[u8]>> {
    iter_nal_units(payload, len_size).collect()
}

// ============================================================================================
//   Fixtures — generated once, reused across tests
// ============================================================================================

/// A smooth RGBA gradient (smooth so nearest vs bilinear chroma upsampling differ minimally). R
/// ramps with x, G with y, B with x+y; alpha ramps diagonally when `alpha_ramp`, else opaque.
fn gradient_rgba(w: u32, h: u32, alpha_ramp: bool) -> Vec<u8> {
    let (wu, hu) = (w as usize, h as usize);
    let mut v = vec![0u8; wu * hu * 4];
    for y in 0..hu {
        for x in 0..wu {
            let i = (y * wu + x) * 4;
            v[i] = ((x * 255) / wu.max(1)) as u8;
            v[i + 1] = ((y * 255) / hu.max(1)) as u8;
            v[i + 2] = (((x + y) * 255) / (wu + hu).max(1)) as u8;
            v[i + 3] = if alpha_ramp {
                (((x + y) * 255) / (wu + hu).max(1)) as u8
            } else {
                255
            };
        }
    }
    v
}

/// A minimal Exif block that begins at a TIFF header (`II*\0`), so libheif's Exif writer computes a
/// zero `exif_tiff_header_offset`. The trailing bytes are arbitrary — they are stored verbatim.
fn exif_input() -> Vec<u8> {
    let mut v = b"II*\0".to_vec();
    v.extend_from_slice(&8u32.to_le_bytes()); // IFD offset (arbitrary)
    v.extend_from_slice(b" gamut-heic conformance exif payload ");
    v
}

/// A tiny XMP packet, stored verbatim as a `mime` / `application/rdf+xml` item.
fn xmp_input() -> &'static [u8] {
    br#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"/></x:xmpmeta><?xpacket end="w"?>"#
}

/// Dimensions of the fully-featured fixture.
const FULL_W: u32 = 64;
const FULL_H: u32 = 64;

/// The fully-featured fixture: 64×64, alpha + thumbnail + Exif + XMP, lossy q90. Used by the
/// structure, presentation-pixel, planar-bit-exact, hvcC-sanity, and motion-photo tests.
fn full_fixture() -> &'static [u8] {
    static FIXTURE: OnceLock<Vec<u8>> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let src = gradient_rgba(FULL_W, FULL_H, true);
        libheif_oracle::encode_rgba_to_heic(
            FULL_W,
            FULL_H,
            &src,
            &EncodeOpts {
                quality: 90,
                with_alpha: true,
                thumbnail_bbox: Some(32),
                exif: Some(exif_input()),
                xmp: Some(xmp_input().to_vec()),
                nclx: Some(nclx_bt601_full()),
                ..Default::default()
            },
        )
        .expect("encode full fixture")
    })
}

/// Coded dimensions of the orientation fixture (pre-transform).
const ORI_W: u32 = 64;
const ORI_H: u32 = 48;

/// The orientation fixture: 64×48, no alpha, lossy q90, EXIF orientation 6 (rotate 90° CW ⇒ libheif
/// stores irot 270° CCW). Decoding applies the transform, swapping dimensions to 48×64.
fn orientation_fixture() -> &'static [u8] {
    static FIXTURE: OnceLock<Vec<u8>> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let src = gradient_rgba(ORI_W, ORI_H, false);
        libheif_oracle::encode_rgba_to_heic(
            ORI_W,
            ORI_H,
            &src,
            &EncodeOpts {
                quality: 90,
                orientation: 6,
                nclx: Some(nclx_bt601_full()),
                ..Default::default()
            },
        )
        .expect("encode orientation fixture")
    })
}

/// Builds the same asymmetric source with an EXIF mirror orientation. libheif serializes
/// orientation 2 as `imir` axis 1 (left↔right) and orientation 4 as axis 0 (top↔bottom).
fn mirror_orientation_fixture(orientation: u8) -> Vec<u8> {
    let src = gradient_rgba(ORI_W, ORI_H, false);
    libheif_oracle::encode_rgba_to_heic(
        ORI_W,
        ORI_H,
        &src,
        &EncodeOpts {
            quality: 90,
            orientation,
            nclx: Some(nclx_bt601_full()),
            ..Default::default()
        },
    )
    .expect("encode mirror orientation fixture")
}

/// A lossless GRAY-ish fixture: 32×32 solid mid-gray, encoded lossless — a tight-bound identity case.
fn lossless_gray_fixture() -> &'static [u8] {
    static FIXTURE: OnceLock<Vec<u8>> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let (w, h) = (32u32, 32u32);
        let mut src = vec![0u8; (w * h * 4) as usize];
        for px in src.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[128, 128, 128, 255]);
        }
        libheif_oracle::encode_rgba_to_heic(
            w,
            h,
            &src,
            &EncodeOpts {
                lossless: true,
                nclx: Some(nclx_bt601_full()),
                ..Default::default()
            },
        )
        .expect("encode lossless gray fixture")
    })
}

// ============================================================================================
//   Diff helpers
// ============================================================================================

/// Per-channel comparison of two equal-length interleaved RGBA buffers.
struct Diff {
    max_rgb: u8,
    mean_rgb: f64,
    max_a: u8,
    /// Fraction of pixels whose every RGB channel is within ±2.
    frac_rgb_within_2: f64,
}

fn rgba_diff(a: &[u8], b: &[u8]) -> Diff {
    assert_eq!(a.len(), b.len(), "buffers differ in length");
    let (mut sum, mut n) = (0u64, 0u64);
    let (mut max_rgb, mut max_a) = (0u8, 0u8);
    let mut within = 0u64;
    let mut pixels = 0u64;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let mut ok = true;
        for c in 0..3 {
            let d = pa[c].abs_diff(pb[c]);
            sum += u64::from(d);
            n += 1;
            max_rgb = max_rgb.max(d);
            ok &= d <= 2;
        }
        max_a = max_a.max(pa[3].abs_diff(pb[3]));
        within += u64::from(ok);
        pixels += 1;
    }
    Diff {
        max_rgb,
        mean_rgb: sum as f64 / n as f64,
        max_a,
        frac_rgb_within_2: within as f64 / pixels as f64,
    }
}

/// A libheif four-character-code rendering matching the oracle's `introspect` (printable ASCII or
/// space kept, anything else `?`), so item-type strings compare exactly.
fn fourcc(ty: [u8; 4]) -> String {
    ty.iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

// ============================================================================================
//   Test 1 — Structure conformance (container parse vs libheif introspection)
// ============================================================================================

#[test]
fn t1_structure_conformance_matches_libheif() {
    let heic = full_fixture();
    let container = HeifContainer::parse(heic).expect("gamut-heic parses the fixture");
    let image = container.image();
    let st = libheif_oracle::introspect(heic).expect("libheif introspects the fixture");

    // 1a. Primary item id.
    assert_eq!(
        image.as_isobmff().primary_item_id,
        st.primary_item_id,
        "primary item id disagrees"
    );
    assert_eq!(image.primary_item().id(), st.primary_item_id);
    let primary_id = st.primary_item_id;

    // 1b. Per-item ids + 4cc types agree (as sets, sorted by id).
    let mut ours: Vec<(u32, String)> = image
        .items()
        .map(|it| (it.id(), fourcc(it.as_isobmff_item().item_type)))
        .collect();
    let mut theirs: Vec<(u32, String)> = st
        .items
        .iter()
        .map(|i| (i.id, i.item_type.clone()))
        .collect();
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs, "item id+type lists disagree");

    // 1c. Primary dimensions (ispe) == oracle's decoded dims.
    let oracle_primary = st
        .images
        .iter()
        .find(|i| i.is_primary)
        .expect("libheif reports a primary image");
    let dims = image
        .primary_item()
        .dimensions()
        .expect("primary has ispe dimensions");
    assert_eq!(
        (dims.width, dims.height),
        (oracle_primary.width, oracle_primary.height),
        "ispe dims disagree with libheif"
    );
    assert_eq!((dims.width, dims.height), (FULL_W, FULL_H));

    // 1d. Alpha lens presence == oracle has_alpha.
    assert_eq!(
        image.alpha_auxiliary_of(primary_id).is_some(),
        oracle_primary.has_alpha,
        "alpha-auxiliary presence disagrees with libheif"
    );
    assert!(oracle_primary.has_alpha, "fixture was built with alpha");

    // 1e. Thumbnails of primary agree (as sorted id sets).
    let mut our_thumbs: Vec<u32> = image
        .thumbnails_of(primary_id)
        .iter()
        .map(|t| t.id())
        .collect();
    let mut their_thumbs = oracle_primary.thumbnail_ids.clone();
    our_thumbs.sort_unstable();
    their_thumbs.sort_unstable();
    assert_eq!(our_thumbs, their_thumbs, "thumbnail ids disagree");
    assert_eq!(our_thumbs.len(), 1, "fixture has exactly one thumbnail");

    // 1f. Exif bytes == oracle's Exif block, and the exif_tiff_header_offset convention.
    let our_exif = image
        .exif()
        .expect("gamut-heic finds the Exif item")
        .as_isobmff_item()
        .payload
        .clone();
    let oracle_exif = st
        .primary_metadata
        .iter()
        .find(|m| m.item_type == "Exif")
        .expect("libheif reports an Exif block");
    assert_eq!(
        our_exif, oracle_exif.data,
        "Exif item payload disagrees with libheif's metadata block"
    );
    // Convention (references/heif §9): libheif KEEPS the 4-byte big-endian exif_tiff_header_offset as
    // the first four bytes of the stored ExifDataBlock, and so does gamut-heic's item payload — so
    // the two are byte-identical (asserted above). The offset value is the byte position of the TIFF
    // header inside the supplied Exif data; our input starts at `II*\0`, so it is 0 and the remaining
    // bytes are the Exif input verbatim.
    let offset = u32::from_be_bytes([our_exif[0], our_exif[1], our_exif[2], our_exif[3]]);
    assert_eq!(
        offset, 0,
        "exif_tiff_header_offset should be 0 (TIFF header at start)"
    );
    assert_eq!(
        &our_exif[4..],
        exif_input().as_slice(),
        "exif payload after offset"
    );

    // 1g. XMP bytes == oracle's XMP block (raw packet, no offset prefix).
    let our_xmp = image
        .xmp()
        .expect("gamut-heic finds the XMP item")
        .as_isobmff_item()
        .payload
        .clone();
    let oracle_xmp = st
        .primary_metadata
        .iter()
        .find(|m| m.content_type == "application/rdf+xml")
        .expect("libheif reports an XMP block");
    assert_eq!(
        our_xmp, oracle_xmp.data,
        "XMP payload disagrees with libheif"
    );
    assert_eq!(our_xmp, xmp_input(), "XMP stored verbatim");

    eprintln!(
        "t1: primary={primary_id} items={} thumbs={:?} exif_offset={offset}",
        ours.len(),
        their_thumbs
    );
}

// ============================================================================================
//   Test 2 — Presentation-pixel conformance (our RGBA decode vs libheif RGBA)
// ============================================================================================

#[test]
fn t2_presentation_pixels_match_libheif() {
    let heic = full_fixture();
    let container = HeifContainer::parse(heic).expect("parse");
    let ours = container
        .decode_primary_rgba8(&mut De265Decoder)
        .expect("gamut-heic decodes primary to RGBA");
    let (w, h, theirs) =
        libheif_oracle::decode_primary_rgba(heic).expect("libheif decodes primary");

    assert_eq!(
        (ours.width(), ours.height()),
        (w, h),
        "decoded dims disagree"
    );
    assert_eq!((w, h), (FULL_W, FULL_H));

    let d = rgba_diff(ours.as_samples(), &theirs);
    eprintln!(
        "t2 presentation diff: max_rgb={} mean_rgb={:.3} max_a={} frac_within2={:.4}",
        d.max_rgb, d.mean_rgb, d.max_a, d.frac_rgb_within_2
    );

    // The wide surface carries the same real, libde265-decoded frame losslessly: 8-bit content
    // widens by exactly 257 (65535 == 255 · 257). This is the widening invariant against a genuine
    // codestream rather than the mock decoder.
    let wide = container
        .decode_primary_rgba16(&mut De265Decoder)
        .expect("gamut-heic decodes primary to RGBA16");
    assert_eq!((wide.width(), wide.height()), (w, h));
    for (i, (&narrow, &widened)) in ours.as_samples().iter().zip(wide.as_samples()).enumerate() {
        assert_eq!(u16::from(narrow) * 257, widened, "sample {i}");
    }

    // Measured (libheif 1.23.1 + kvazaar 2.3.2, q90 4:2:0, explicit BT.601-full colr): max_rgb=1,
    // mean_rgb≈0.010, max_a=0, frac_within2=1.0000. With both readers on the same colour policy the
    // only residual is chroma-upsampling/rounding (our nearest co-sited + BT.601 integer conversion
    // vs libheif's bilinear) over a smooth gradient. Bounds keep a little headroom over those maxima.
    assert!(d.max_rgb <= 3, "RGB max diff too high: {}", d.max_rgb);
    assert!(
        d.mean_rgb <= 0.5,
        "RGB mean diff too high: {:.3}",
        d.mean_rgb
    );
    assert!(d.max_a <= 1, "alpha max diff too high: {}", d.max_a);
    assert!(
        d.frac_rgb_within_2 >= 0.99,
        "fewer than 99% of pixels within ±2: {:.4}",
        d.frac_rgb_within_2
    );
}

#[test]
fn t2b_lossless_gray_tight_bound() {
    let heic = lossless_gray_fixture();
    let container = HeifContainer::parse(heic).expect("parse");
    let ours = container
        .decode_primary_rgba8(&mut De265Decoder)
        .expect("decode primary");
    let (w, h, theirs) = libheif_oracle::decode_primary_rgba(heic).expect("libheif decode");
    assert_eq!((ours.width(), ours.height()), (w, h));

    let d = rgba_diff(ours.as_samples(), &theirs);
    eprintln!(
        "t2b lossless gray diff: max_rgb={} mean_rgb={:.3} max_a={}",
        d.max_rgb, d.mean_rgb, d.max_a
    );
    // A flat, lossless, full-range image: both readers reconstruct the same constant (Y=Cb=Cr=128,
    // upsampling is a no-op on a constant chroma field), so the presentation pixels are *bit-exact*.
    // Measured: max_rgb=0, max_a=0 — the tight identity bound the task asks for on a lossless fixture.
    assert_eq!(
        d.max_rgb, 0,
        "lossless flat gray must be bit-exact, got {}",
        d.max_rgb
    );
    assert_eq!(d.max_a, 0, "opaque alpha must be exact");
}

// ============================================================================================
//   Test 3 — Planar bit-exact conformance (container plumbing proof)
// ============================================================================================

#[test]
fn t3_planar_is_bit_exact_with_direct_decode() {
    let heic = full_fixture();
    let container = HeifContainer::parse(heic).expect("parse");
    let image = container.image();
    let primary = image.primary_item();

    // Decode the primary through the full container pipeline (NAL split + config delivery).
    let frame = container
        .decode_item_planar(primary.id(), &mut De265Decoder)
        .expect("planar decode of primary");

    // Decode the exact same payload directly, bypassing the container plumbing entirely.
    let config = primary
        .hevc_config()
        .expect("primary carries hvcC")
        .expect("hvcC parses");
    let payload = &primary.as_isobmff_item().payload;
    let cfg = config_nals(&config);
    let pics = picture_nals(payload, config.nal_length_size()).expect("split payload");
    let direct = libheif_oracle::decode_hevc_intra(&cfg, &pics).expect("direct libde265 decode");

    // Identical decoder underneath ⇒ byte-identical planes ⇒ the container split/config delivery is
    // faithful and does not corrupt the payload.
    assert_eq!(
        (frame.width(), frame.height()),
        (direct.width, direct.height)
    );
    assert_eq!(frame.bit_depth(), direct.bit_depth);
    let expect_chroma = match direct.chroma {
        OracleChroma::Mono => ChromaFormat::Monochrome,
        OracleChroma::Yuv420 => ChromaFormat::Yuv420,
        OracleChroma::Yuv422 => ChromaFormat::Yuv422,
        OracleChroma::Yuv444 => ChromaFormat::Yuv444,
    };
    assert_eq!(frame.chroma(), expect_chroma);
    assert_eq!(frame.y(), direct.planes[0].as_slice(), "luma plane differs");
    assert_eq!(frame.cb(), direct.planes[1].as_slice(), "Cb plane differs");
    assert_eq!(frame.cr(), direct.planes[2].as_slice(), "Cr plane differs");

    eprintln!(
        "t3 planar bit-exact: {}x{} {:?} {}-bit — planes identical",
        frame.width(),
        frame.height(),
        frame.chroma(),
        frame.bit_depth()
    );

    // NOTE (grid): libheif + kvazaar do not emit a multi-tile `grid` derived image for a 64×64
    // input — the whole image is a single `hvc1` item, and the oracle's `encode_rgba_to_heic`
    // exposes no knob to force a tiled `grid`. A real-grid differential is therefore not cheaply
    // constructible via the oracle API; the crate's own synthetic grid-assembly unit tests cover
    // the tile reassembly path. (Reported in the suite summary.)
}

// ============================================================================================
//   Test 4 — Orientation conformance (irot/imir direction vs the reference)
// ============================================================================================

#[test]
fn t4_orientation_matches_libheif() {
    let heic = orientation_fixture();
    let container = HeifContainer::parse(heic).expect("parse");
    let image = container.image();

    // The primary carries an essential irot (270° CCW). Confirm the container sees a transform.
    let props = image.primary_item().transformative_properties();
    assert!(
        !props.is_empty(),
        "orientation fixture should carry a transformative property"
    );

    let ours = container
        .decode_primary_rgba8(&mut De265Decoder)
        .expect("decode primary with transform applied");
    let (w, h, theirs) = libheif_oracle::decode_primary_rgba(heic).expect("libheif decode");

    // Both apply the stored transform ⇒ dims swap from 64×48 to 48×64.
    assert_eq!(
        (ours.width(), ours.height()),
        (w, h),
        "oriented dims disagree"
    );
    assert_eq!((w, h), (ORI_H, ORI_W), "90° rotation swaps dimensions");

    let d = rgba_diff(ours.as_samples(), &theirs);
    eprintln!(
        "t4 oriented diff: {}x{} max_rgb={} mean_rgb={:.3} frac_within2={:.4}",
        w, h, d.max_rgb, d.mean_rgb, d.frac_rgb_within_2
    );
    // Same tolerance as test 2 — matching pixels confirm our irot/imir direction convention agrees
    // with the reference implementation (a direction flip would misalign nearly every pixel).
    // Measured: max_rgb=1, mean_rgb≈0.032, frac_within2=1.0000.
    assert!(
        d.max_rgb <= 3,
        "oriented RGB max diff too high: {}",
        d.max_rgb
    );
    assert!(
        d.mean_rgb <= 0.5,
        "oriented RGB mean diff too high: {:.3}",
        d.mean_rgb
    );
    assert!(
        d.frac_rgb_within_2 >= 0.99,
        "fewer than 99% of oriented pixels within ±2: {:.4}",
        d.frac_rgb_within_2
    );
}

#[test]
fn t4_mirror_axes_match_libheif() {
    for (orientation, axis) in [(2, 1), (4, 0)] {
        let heic = mirror_orientation_fixture(orientation);
        let container = HeifContainer::parse(&heic).expect("parse");
        assert_eq!(
            container.image().primary_item().transformative_properties(),
            vec![gamut_heic::TransformativeProperty::Mirror(axis)],
            "EXIF orientation {orientation}"
        );

        let ours = container
            .decode_primary_rgba8(&mut De265Decoder)
            .expect("decode primary with mirror applied");
        let (w, h, theirs) = libheif_oracle::decode_primary_rgba(&heic).expect("libheif decode");
        assert_eq!((ours.width(), ours.height()), (ORI_W, ORI_H));
        assert_eq!((w, h), (ORI_W, ORI_H));

        let d = rgba_diff(ours.as_samples(), &theirs);
        assert!(
            d.max_rgb <= 3,
            "orientation {orientation} RGB max diff too high: {}",
            d.max_rgb
        );
        assert!(
            d.mean_rgb <= 0.5,
            "orientation {orientation} RGB mean diff too high: {:.3}",
            d.mean_rgb
        );
        assert!(
            d.frac_rgb_within_2 >= 0.99,
            "orientation {orientation}: fewer than 99% of RGB samples within ±2: {:.4}",
            d.frac_rgb_within_2
        );
    }
}

// ============================================================================================
//   Test 5 — Motion-photo overlay (real-world files decode identically to the pristine still)
// ============================================================================================

/// Wraps `data` in a top-level box of `ty` (8-byte size+type header).
fn wrap_box(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let size = (8 + data.len()) as u32;
    let mut v = Vec::with_capacity(size as usize);
    v.extend_from_slice(&size.to_be_bytes());
    v.extend_from_slice(ty);
    v.extend_from_slice(data);
    v
}

#[test]
fn t5_motion_photo_overlay_decodes_identically() {
    let pristine = full_fixture();

    // (i) An mpvd box wrapping arbitrary "video" bytes, then (ii) a second ftyp beginning an MP4
    // stub, then garbage trailing bytes — the shape of a Google/Samsung motion-photo HEIC.
    let mpvd_payload = b"\x00\x01\x02\x03 arbitrary motion-photo video data \xff\xfe";
    let mpvd = wrap_box(b"mpvd", mpvd_payload);
    let mp4_ftyp = wrap_box(b"ftyp", b"mp42\x00\x00\x00\x00mp42isom");
    let mp4_garbage = b"\xde\xad\xbe\xef SEF trailer-ish garbage \x00\x11\x22";
    let appended: Vec<u8> = mp4_ftyp.iter().chain(mp4_garbage).copied().collect();

    let mut file = pristine.to_vec();
    file.extend_from_slice(&mpvd);
    file.extend_from_slice(&appended);

    let container = HeifContainer::parse(&file).expect("parse motion-photo file");

    // The mpvd box is surfaced verbatim as a top-level Box segment.
    let mpvd_body = container
        .boxes()
        .find(|(ty, _)| ty == b"mpvd")
        .map(|(_, body)| body)
        .expect("mpvd box surfaced");
    assert_eq!(mpvd_body, mpvd_payload, "mpvd body must be byte-exact");

    // The appended foreign stream (second ftyp to EOF) is retained opaque and byte-exact.
    let stream = container
        .appended_stream()
        .expect("appended stream surfaced");
    assert_eq!(
        stream,
        appended.as_slice(),
        "appended stream must be byte-exact"
    );
    // With a second ftyp the whole tail is one AppendedStream, so there is no separate Trailer here.
    assert!(
        container.trailer().is_none(),
        "no trailer when a second ftyp is present"
    );

    // Every byte is accounted for (the crate's every-byte invariant) and the last segment reaches EOF.
    let segs = container.segments();
    assert!(
        gamut_core::segment::covers(segs, file.len()),
        "segments must tile the file: {:?}",
        segs.iter().map(|s| s.range.clone()).collect::<Vec<_>>()
    );

    // The still decodes identically to the pristine file — appended data changes nothing.
    let pristine_pixels = HeifContainer::parse(pristine)
        .unwrap()
        .decode_primary_rgba8(&mut De265Decoder)
        .unwrap();
    let motion_pixels = container.decode_primary_rgba8(&mut De265Decoder).unwrap();
    assert_eq!(
        pristine_pixels.as_samples(),
        motion_pixels.as_samples(),
        "motion-photo still must decode identically to the pristine still"
    );
    // And identically to libheif on both the pristine and the appended file.
    let (_, _, oracle_pristine) = libheif_oracle::decode_primary_rgba(pristine).unwrap();
    let (_, _, oracle_motion) = libheif_oracle::decode_primary_rgba(&file).unwrap();
    assert_eq!(
        oracle_pristine, oracle_motion,
        "libheif also ignores appended data"
    );

    eprintln!(
        "t5 motion-photo: mpvd={}B appended={}B segments={}",
        mpvd_payload.len(),
        appended.len(),
        segs.len()
    );
}

#[test]
fn t5b_trailer_bytes_surfaced() {
    // The complementary shape: trailing non-box garbage (no second ftyp) is retained as a Trailer.
    // The leading bytes are a box "size" of 0xFFFFFFF0 — far larger than the remaining bytes — so the
    // top-level walk hits a truncated/malformed box immediately and closes the tail out as a Trailer
    // (rather than accidentally parsing a well-formed box header).
    let pristine = full_fixture();
    let garbage = b"\xff\xff\xff\xf0 samsung-SEF-like proprietary trailer \x00\x11\x22";
    let mut file = pristine.to_vec();
    file.extend_from_slice(garbage);

    let container = HeifContainer::parse(&file).expect("parse file with trailing garbage");
    let trailer = container.trailer().expect("trailer surfaced");
    assert_eq!(trailer, garbage.as_slice(), "trailer must be byte-exact");
    assert!(container.appended_stream().is_none());

    // Pixels unchanged from pristine.
    let motion = container.decode_primary_rgba8(&mut De265Decoder).unwrap();
    let pristine_px = HeifContainer::parse(pristine)
        .unwrap()
        .decode_primary_rgba8(&mut De265Decoder)
        .unwrap();
    assert_eq!(motion.as_samples(), pristine_px.as_samples());
    eprintln!(
        "t5b trailer: {}B trailing bytes surfaced byte-exact",
        garbage.len()
    );
}

// ============================================================================================
//   Test 6 — hvcC round-trip sanity (config lens vs the decoded YUV)
// ============================================================================================

#[test]
fn t6_hvcc_coherent_with_oracle_yuv() {
    let heic = full_fixture();
    let container = HeifContainer::parse(heic).expect("parse");
    let primary = container.image().primary_item();

    let config = primary
        .hevc_config()
        .expect("primary carries an hvcC")
        .expect("hvcC parses");

    // Decode the same payload to get the ground-truth YUV facts.
    let payload = &primary.as_isobmff_item().payload;
    let cfg = config_nals(&config);
    let pics = picture_nals(payload, config.nal_length_size()).unwrap();
    let yuv = libheif_oracle::decode_hevc_intra(&cfg, &pics).expect("decode");

    // Chroma format from hvcC matches the decoded chroma.
    let expect_chroma = match yuv.chroma {
        OracleChroma::Mono => ChromaFormat::Monochrome,
        OracleChroma::Yuv420 => ChromaFormat::Yuv420,
        OracleChroma::Yuv422 => ChromaFormat::Yuv422,
        OracleChroma::Yuv444 => ChromaFormat::Yuv444,
    };
    assert_eq!(
        config.chroma_format(),
        expect_chroma,
        "hvcC chroma_format_idc disagrees with the decoded chroma"
    );

    // Bit depths from hvcC match the decoded bit depth.
    assert_eq!(
        config.bit_depth_luma(),
        yuv.bit_depth,
        "hvcC luma bit depth"
    );
    assert_eq!(
        config.bit_depth_chroma(),
        yuv.bit_depth,
        "hvcC chroma bit depth"
    );

    // NAL length size is one of the three legal widths.
    let ls = config.nal_length_size();
    assert!(
        matches!(ls, 1 | 2 | 4),
        "nal_length_size must be 1/2/4, got {ls}"
    );

    eprintln!(
        "t6 hvcC sanity: chroma={:?} depth={} nal_len={ls} profile_idc={}",
        config.chroma_format(),
        config.bit_depth_luma(),
        config.general_profile_idc
    );
}

// ============================================================================================
//   Test 7 — The reference decoder driven through the HevcDecoders registry (issue #273)
// ============================================================================================

/// The registry is a drop-in for a single `&mut dyn HevcDecoder`: pushing the libde265-backed
/// `De265Decoder` as the only backend must decode the fixture *identically* to passing that backend
/// directly, and an empty registry must decline with `Error::Unsupported(NO_BACKEND)` — gamut ships
/// no in-tree software HEVC decoder (issue #18), so there is no implicit fallback tail.
#[test]
fn t7_registry_drives_the_reference_backend() {
    let heic = full_fixture();
    let container = HeifContainer::parse(heic).expect("parse");
    let primary_id = container.image().primary_item().id();

    // Baseline: the backend used directly, exactly as tests 1-6 do.
    let direct = container
        .decode_item_planar(primary_id, &mut De265Decoder)
        .expect("direct planar decode");

    // Through the registry, at the same call site with the same signature.
    let mut decoders = HevcDecoders::new();
    decoders.push_backend(De265Decoder);
    assert_eq!(decoders.len(), 1);
    let via_registry = container
        .decode_item_planar(primary_id, &mut decoders)
        .expect("registry planar decode");

    assert_eq!(
        via_registry, direct,
        "registry decode differs from direct decode"
    );

    // The presentation path routes through the same seam.
    let rgba_direct = container
        .decode_primary_rgba8(&mut De265Decoder)
        .expect("direct rgba decode");
    let rgba_registry = container
        .decode_primary_rgba8(&mut decoders)
        .expect("registry rgba decode");
    assert_eq!(
        rgba_registry.as_samples(),
        rgba_direct.as_samples(),
        "registry RGBA differs from direct RGBA"
    );

    // No backends ⇒ no implicit software fallback.
    let mut empty = HevcDecoders::new();
    assert!(empty.is_empty());
    let err = container
        .decode_item_planar(primary_id, &mut empty)
        .expect_err("an empty registry cannot decode");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(err.static_message(), Some(NO_BACKEND));

    eprintln!(
        "t7 registry: {}x{} {:?} decoded identically through HevcDecoders",
        direct.width(),
        direct.height(),
        direct.chroma()
    );
}
