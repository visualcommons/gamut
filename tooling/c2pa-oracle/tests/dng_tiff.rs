//! `gamut-tiff` and `gamut-dng` against c2pa-rs: the C2PA 2.4 §A.3.6 tag-52545 store, bound by
//! `c2pa.hash.data` around the **two** disjoint ranges §18.5.5 names (issue #534).
//!
//! Both crates report their exclusions through the one `gamut_ifd::c2pa::C2paExclusions` — the
//! store's bytes and the `count` field of its IFD entry — so both are driven by the same tests
//! here, once per container. §18.7.3.3 removed box hash for TIFF, so `c2pa.hash.data` is the only
//! binding either has, and the second range is exactly what the oracle should check rather than
//! assume: direction 1 signs around both ranges gamut reports, and direction 2 compares them
//! against the two c2pa-rs itself wrote.

use std::ops::Range;

use c2pa::ValidationState;
use c2pa_oracle::{
    DNG_MIME, OracleError, Result, TIFF_MIME, data_hash_exclusions, embed, jumbf_superbox_span,
    read, read_with_external_store, reserve_then_fill_data_hashed,
};
use gamut_core::{Dimensions, ImageRef, Rgb8};
use gamut_dng::{
    C2paExclusions, CalibrationIlluminant, CameraProfile, DngEncoder, RawImage, cfa_color,
};
use gamut_tiff::TiffEncoder;

/// Fixture width and height: not multiples of 8 or 16, so no strip or tile boundary lines up by
/// luck.
const W: u32 = 34;
const H: u32 = 18;

/// An encoded fixture, and the exclusions the encoder reported for its reservation if it made one.
type Encoded = (Vec<u8>, Option<C2paExclusions>);

/// One of the two TIFF-based containers, as the tests below need it.
struct Container {
    /// The MIME type c2pa-rs is handed the asset under.
    mime: &'static str,
    /// The fixture, with a reservation of the given length or with none.
    encode: fn(Option<usize>) -> Result<Encoded>,
    /// The gamut crate's read-side locator.
    locate: fn(&[u8]) -> Option<C2paExclusions>,
}

const CONTAINERS: [Container; 2] = [
    Container {
        mime: TIFF_MIME,
        encode: encode_tiff,
        locate: locate_tiff,
    },
    Container {
        mime: DNG_MIME,
        encode: encode_dng,
        locate: locate_dng,
    },
];

fn asset_error(error: gamut_core::Error) -> OracleError {
    OracleError::Asset(error.to_string())
}

/// A `gamut_ifd::Range` as the half-open range the oracle compares in.
fn range(range: gamut_tiff::Range) -> Range<usize> {
    let start = usize::try_from(range.start).expect("a fixture offset fits usize");
    start..start + usize::try_from(range.len).expect("a fixture length fits usize")
}

/// The two exclusions, store first — the order `C2paExclusions` names them in.
fn ranges(exclusions: C2paExclusions) -> Vec<Range<usize>> {
    vec![range(exclusions.store()), range(exclusions.count_field())]
}

/// A structured 8-bit RGB source: every channel varies along both axes.
fn source_rgb() -> Vec<u8> {
    let mut rgb = vec![0u8; (W * H * 3) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            rgb[i] = ((x * 7 + y * 3) & 0xff) as u8;
            rgb[i + 1] = ((x * x + y) & 0xff) as u8;
            rgb[i + 2] = ((x ^ (y * 5)) & 0xff) as u8;
        }
    }
    rgb
}

fn encode_tiff(reserve: Option<usize>) -> Result<Encoded> {
    let rgb = source_rgb();
    let image = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(W, H).map_err(asset_error)?)
        .map_err(asset_error)?;
    let encoder = match reserve {
        Some(len) => TiffEncoder::new().with_c2pa_reserved(len),
        None => TiffEncoder::new(),
    };
    let mut tiff = Vec::new();
    let report = encoder
        .encode_with_report(image, &mut tiff)
        .map_err(asset_error)?;
    Ok((tiff, report.c2pa))
}

fn locate_tiff(file: &[u8]) -> Option<C2paExclusions> {
    gamut_tiff::c2pa_exclusions(file).expect("gamut-tiff reads the TIFF")
}

/// A 12-bit RGGB mosaic with a spatially varying pattern, and a one-matrix D65 profile — the
/// smallest raw `DngEncoder` writes.
fn encode_dng(reserve: Option<usize>) -> Result<Encoded> {
    const BITS: u16 = 12;
    let max = (1u32 << BITS) - 1;
    let samples = (0..W * H)
        .map(|i| ((((i % W) * 53) ^ ((i / W) * 97)) % max) as u16)
        .collect();
    let pattern = vec![
        cfa_color::RED,
        cfa_color::GREEN,
        cfa_color::GREEN,
        cfa_color::BLUE,
    ];
    let raw = RawImage::new_cfa(
        Dimensions::new(W, H).map_err(asset_error)?,
        BITS,
        (2, 2),
        pattern,
        samples,
    )
    .and_then(|raw| raw.with_black_level(0.0))
    .and_then(|raw| raw.with_white_level(f64::from(max)))
    .map_err(asset_error)?;
    let profile = CameraProfile::new(
        "gamut c2pa-oracle",
        [
            0.6722, -0.0635, -0.0963, -0.4287, 1.2460, 0.2028, -0.0908, 0.2162, 0.5668,
        ],
        CalibrationIlluminant::D65,
        [0.5128, 1.0, 0.7059],
    )
    .map_err(asset_error)?;

    let encoder = match reserve {
        Some(len) => DngEncoder::new().with_c2pa_reserved(len),
        None => DngEncoder::new(),
    };
    let mut dng = Vec::new();
    let report = encoder
        .encode_with_report(&raw, &profile, &mut dng)
        .map_err(asset_error)?;
    Ok((dng, report.c2pa))
}

fn locate_dng(file: &[u8]) -> Option<C2paExclusions> {
    gamut_dng::DngDecoder::new()
        .decode(file)
        .expect("gamut-dng decodes the DNG")
        .c2pa_exclusions
}

/// Direction 1: reserve through the gamut encoder, sign around the exclusions it reported, and
/// copy the store into the reserved store range — the in-place fill §A.3.6's end-of-file
/// placement exists for.
fn reserve_and_fill(container: &Container) -> Vec<u8> {
    reserve_then_fill_data_hashed(
        container.mime,
        |len| {
            let (asset, exclusions) = (container.encode)(Some(len))?;
            let exclusions = exclusions.ok_or_else(|| {
                OracleError::Asset("encode_with_report reported no C2PA exclusions".into())
            })?;
            Ok((asset, ranges(exclusions), range(exclusions.store())))
        },
        |mut asset, store_range, store| {
            asset[store_range].copy_from_slice(store);
            Ok(asset)
        },
    )
    .unwrap_or_else(|error| panic!("{}: reserve, sign and fill: {error}", container.mime))
    .asset
}

/// Direction 2: c2pa-rs embeds into the container's plain fixture.
fn embedded(container: &Container) -> Vec<u8> {
    let (plain, _) = (container.encode)(None).expect("the plain fixture encodes");
    embed(container.mime, &plain)
        .unwrap_or_else(|error| panic!("{}: c2pa-rs embeds: {error}", container.mime))
}

#[test]
fn a_store_signed_around_both_reported_ranges_validates_once_copied_in() {
    for container in &CONTAINERS {
        assert_eq!(
            read(container.mime, &reserve_and_fill(container)).ok(),
            Some(ValidationState::Valid),
            "{}: c2pa-rs must accept a store whose data hash excludes exactly the store and \
             `count`-field ranges `encode_with_report` reported, once copied into the store range",
            container.mime
        );
    }
}

#[test]
fn the_gamut_locator_finds_the_store_c2pa_rs_embedded_at_its_own_superbox_span() {
    for container in &CONTAINERS {
        let asset = embedded(container);
        let store = range(
            (container.locate)(&asset)
                .unwrap_or_else(|| panic!("{}: gamut finds c2pa-rs's store", container.mime))
                .store(),
        );

        assert_eq!(
            store,
            jumbf_superbox_span(&asset).expect("the store's own JUMBF header"),
            "{}: the store range gamut reports must be exactly the span the store's own `LBox` \
             declares",
            container.mime
        );
        assert_eq!(
            read_with_external_store(container.mime, &asset[store], &asset).ok(),
            Some(ValidationState::Valid),
            "{}: the bytes at the store range gamut reported must be the store c2pa-rs embedded",
            container.mime
        );
    }
}

#[test]
fn c2pa_rs_excludes_the_same_two_disjoint_ranges_the_gamut_locator_reports() {
    // §18.5.5's second range, observed rather than assumed: c2pa-rs's own embedder excludes the
    // `count` field of the store's IFD entry as a range of its own, and it is the very field
    // gamut names. c2pa-rs lists it first; `C2paExclusions` names the store first, so the
    // comparison is over the sorted pair.
    for container in &CONTAINERS {
        let asset = embedded(container);
        let located = (container.locate)(&asset)
            .unwrap_or_else(|| panic!("{}: gamut finds c2pa-rs's store", container.mime));

        let mut wrote = data_hash_exclusions(container.mime, &asset).expect("c2pa-rs's data hash");
        wrote.sort_by_key(|range| range.start);
        let mut reported = ranges(located);
        reported.sort_by_key(|range| range.start);

        assert_eq!(
            wrote, reported,
            "{}: c2pa-rs's own embedder must exclude exactly the store and `count`-field ranges \
             gamut locates",
            container.mime
        );
    }
}
