//! `gamut-webp` (over `gamut-riff`) against c2pa-rs: the C2PA 2.4 §A.3.7 `C2PA` chunk, bound by
//! `c2pa.hash.data` (issue #534).
//!
//! WebP has no in-place fill: `WebpEncoder::with_c2pa_reserved` documents step 3 as a second
//! encode with a store of the same length, which reproduces the file with only the reserved bytes
//! replaced. Direction 1 below therefore puts that reproducibility in front of c2pa-rs too — a
//! re-encode that moved any byte outside the reported range would break the binding.

use c2pa::ValidationState;
use c2pa_oracle::{
    OracleError, Result, WEBP_MIME, data_hash_exclusions, embed, jumbf_superbox_span, read,
    read_with_external_store, reserve_then_fill_data_hashed,
};
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_webp::{WebpEncoder, c2pa_span};

/// Fixture width and height: not multiples of 4 or 16, so no block grid lines up by luck.
const W: u32 = 34;
const H: u32 = 18;

/// A structured source: every channel varies along both axes.
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

fn image(rgb: &[u8]) -> ImageRef<'_, Rgb8> {
    ImageRef::new(rgb, Dimensions::new(W, H).expect("non-zero")).expect("buffer matches")
}

fn asset_error(error: gamut_core::Error) -> OracleError {
    OracleError::Asset(error.to_string())
}

/// The fixture with no store: the asset c2pa-rs is handed to embed into.
fn plain_webp() -> Vec<u8> {
    WebpEncoder::lossless()
        .encode_to_vec(image(&source_rgb()))
        .expect("the fixture encodes")
}

/// The fixture with a `len`-byte reservation, and the `C2PA` chunk span `encode_with_report` gave.
fn reserve_webp(len: usize) -> Result<(Vec<u8>, Vec<std::ops::Range<usize>>, ())> {
    let (webp, report) = WebpEncoder::lossless()
        .with_c2pa_reserved(len)
        .map_err(asset_error)?
        .encode_with_report(image(&source_rgb()))
        .map_err(asset_error)?;
    let span = report
        .c2pa
        .ok_or_else(|| OracleError::Asset("encode_with_report reported no C2PA span".into()))?;
    Ok((webp, vec![span], ()))
}

/// The documented fill: encode again with the signed store in place of the reservation.
fn refill_webp(_reserved: Vec<u8>, (): (), store: &[u8]) -> Result<Vec<u8>> {
    WebpEncoder::lossless()
        .with_c2pa(store)
        .encode_to_vec(image(&source_rgb()))
        .map_err(asset_error)
}

#[test]
fn a_store_signed_around_the_reported_chunk_span_validates_once_re_encoded_in() {
    let filled = reserve_then_fill_data_hashed(WEBP_MIME, reserve_webp, refill_webp)
        .expect("reserve, sign and re-encode");

    assert_eq!(
        read(WEBP_MIME, &filled.asset).expect("the re-encoded WebP carries a store"),
        ValidationState::Valid,
        "c2pa-rs must accept a store whose data hash excludes exactly the `C2PA` chunk span \
         `WebpEncoder::encode_with_report` reported, once a same-length re-encode carries it"
    );
}

#[test]
fn gamut_webp_locates_the_store_c2pa_rs_embedded_at_its_own_superbox_span() {
    let asset = embed(WEBP_MIME, &plain_webp()).expect("c2pa-rs embeds into the gamut WebP");
    let chunk = c2pa_span(&asset)
        .expect("gamut-webp reads the WebP c2pa-rs wrote")
        .expect("gamut-webp finds the `C2PA` chunk c2pa-rs wrote");
    // The span covers the chunk header; the store is its payload, eight bytes in.
    let payload = chunk.start + 8..chunk.end;

    assert_eq!(
        payload,
        jumbf_superbox_span(&asset).expect("the store's own JUMBF header"),
        "the payload of the chunk gamut-webp reports must be exactly the span the store's own \
         `LBox` declares"
    );
    assert_eq!(
        read_with_external_store(WEBP_MIME, &asset[payload], &asset)
            .expect("c2pa-rs reads the bytes gamut-webp located"),
        ValidationState::Valid,
        "the bytes at the payload gamut-webp reported must be the store c2pa-rs embedded"
    );
}

#[test]
fn c2pa_rs_excludes_the_whole_chunk_header_included_as_gamut_riff_reports_it() {
    // `gamut_riff::c2pa_span` reports the chunk *with* its eight-byte header, and its docs record
    // that C2PA 2.4 §18.5.1 forbids an exclusion to overlap a header as written — the payload,
    // `start + 8..end`, being the §18.5.1 reading — with the choice left open. This pins what the
    // reference implementation's own embedder does: it is evidence for that decision, not the
    // decision.
    let asset = embed(WEBP_MIME, &plain_webp()).expect("c2pa-rs embeds into the gamut WebP");
    let chunk = c2pa_span(&asset)
        .expect("gamut-webp reads the WebP c2pa-rs wrote")
        .expect("gamut-webp finds the `C2PA` chunk c2pa-rs wrote");

    assert_eq!(
        data_hash_exclusions(WEBP_MIME, &asset).expect("c2pa-rs's own data hash"),
        vec![chunk],
        "c2pa-rs's own embedder must exclude the header-inclusive `C2PA` chunk span gamut-riff \
         reports"
    );
}
