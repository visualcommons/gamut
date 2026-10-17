//! `gamut-png` against c2pa-rs: the C2PA 2.4 §A.3.2 `caBX` chunk, bound by `c2pa.hash.data`
//! (issue #534).
//!
//! A PNG store's chunk carries a CRC over its payload, so filling a reservation rewrites four
//! bytes *outside* the store. The exclusion `gamut-png` reports — `C2paSpan::chunk`, length field
//! through CRC — is what keeps those bytes out of the digest, and both directions below put that
//! range in front of c2pa-rs rather than taking it on trust.

use c2pa::ValidationState;
use c2pa_oracle::{
    OracleError, PNG_MIME, Result, data_hash_exclusions, embed, jumbf_superbox_span, read,
    read_with_external_store, reserve_then_fill_data_hashed,
};
use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
use gamut_png::{C2paSpan, PngEncoder, deconstruct, fill_c2pa};

/// Fixture width and height: not multiples of 8, so no filter or row arithmetic lands on a round
/// number by luck.
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

/// The fixture with no store: the asset c2pa-rs is handed to embed into.
fn plain_png() -> Vec<u8> {
    PngEncoder::new()
        .encode_to_vec(image(&source_rgb()))
        .expect("the fixture encodes")
}

/// The fixture with a `len`-byte reserved `caBX` payload, and the span `encode_with_report` gave.
fn reserve_png(len: usize) -> Result<(Vec<u8>, C2paSpan)> {
    let (png, report) = PngEncoder::new()
        .with_c2pa_reserved(len)
        .encode_with_report(image(&source_rgb()))
        .map_err(|error| OracleError::Asset(error.to_string()))?;
    let span = report
        .c2pa
        .ok_or_else(|| OracleError::Asset("encode_with_report reported no caBX span".into()))?;
    Ok((png, span))
}

/// Reserve, sign around `exclude(span)`, and fill through `fill_c2pa` — direction 1 with the
/// exclusion chosen by the caller, so the chunk span and the payload alone can be compared.
fn sign_excluding(exclude: fn(&C2paSpan) -> std::ops::Range<usize>) -> Vec<u8> {
    let filled = reserve_then_fill_data_hashed(
        PNG_MIME,
        |len| {
            let (png, span) = reserve_png(len)?;
            Ok((png, vec![exclude(&span)], span))
        },
        |mut png, span, store| {
            fill_c2pa(&mut png, &span, store)
                .map_err(|error| OracleError::Asset(error.to_string()))?;
            Ok(png)
        },
    )
    .expect("reserve, sign and fill");
    filled.asset
}

#[test]
fn a_store_signed_around_the_reported_chunk_span_validates_once_filled() {
    let png = sign_excluding(|span| span.chunk.clone());

    assert_eq!(
        read(PNG_MIME, &png).expect("the filled PNG carries a store"),
        ValidationState::Valid,
        "c2pa-rs must accept a store whose data hash excludes exactly the `caBX` span \
         `PngEncoder::encode_with_report` reported, once `fill_c2pa` has written it"
    );
}

#[test]
fn excluding_the_payload_alone_leaves_the_rewritten_crc_digested() {
    // The reason `C2paSpan::chunk` and not `C2paSpan::payload` is the exclusion: `fill_c2pa`
    // rewrites the chunk CRC, which lies outside the payload. Signing around the payload alone
    // digests the reservation's CRC, and the fill then changes it.
    let png = sign_excluding(|span| span.payload.clone());

    assert_ne!(
        read(PNG_MIME, &png).ok(),
        Some(ValidationState::Valid),
        "a data hash that digests the `caBX` CRC must break when the fill rewrites it; if this \
         validates, the payload range is a sufficient exclusion and `C2paSpan::chunk` is wider \
         than it needs to be"
    );
}

#[test]
fn a_signed_store_longer_than_the_placeholder_is_refused_before_it_is_filled() {
    // `Builder::placeholder` sizes its data hash for ten exclusions. Handing the binding more
    // than that grows the signed store past the length the host reserved, and `sign_embeddable`
    // only ever pads *up*. Writing that store would overrun the reservation, so the driver must
    // refuse it before `fill` runs.
    let mut filled = false;
    let error = reserve_then_fill_data_hashed(
        PNG_MIME,
        |len| {
            let (png, span) = reserve_png(len)?;
            // Sixteen one-byte ranges inside the reserved payload: legal, disjoint, and more
            // than the placeholder made room for.
            let exclusions =
                (0..16).map(|i| span.payload.start + 2 * i..span.payload.start + 2 * i + 1);
            Ok((png, exclusions.collect(), ()))
        },
        |png, (), _| {
            filled = true;
            Ok(png)
        },
    )
    .expect_err("a store longer than its reservation cannot be written into it");

    assert!(
        !filled && error.to_string().contains("but the placeholder pinned"),
        "the refusal must come before the fill and name both lengths; got {error}"
    );
}

#[test]
fn gamut_png_locates_the_store_c2pa_rs_embedded_at_its_own_superbox_span() {
    let asset = embed(PNG_MIME, &plain_png()).expect("c2pa-rs embeds into the gamut PNG");
    let span = deconstruct(&asset)
        .expect("gamut-png reads the PNG c2pa-rs wrote")
        .c2pa()
        .expect("gamut-png finds the `caBX` chunk c2pa-rs wrote");

    assert_eq!(
        span.payload,
        jumbf_superbox_span(&asset).expect("the store's own JUMBF header"),
        "the payload gamut-png reports must be exactly the span the store's own `LBox` declares"
    );
    assert_eq!(
        read_with_external_store(PNG_MIME, &asset[span.payload], &asset)
            .expect("c2pa-rs reads the bytes gamut-png located"),
        ValidationState::Valid,
        "the bytes at the payload gamut-png reported must be the store c2pa-rs embedded"
    );
}

#[test]
fn the_chunk_span_gamut_png_reports_is_the_exclusion_c2pa_rs_wrote() {
    let asset = embed(PNG_MIME, &plain_png()).expect("c2pa-rs embeds into the gamut PNG");
    let span = deconstruct(&asset)
        .expect("gamut-png reads the PNG c2pa-rs wrote")
        .c2pa()
        .expect("gamut-png finds the `caBX` chunk c2pa-rs wrote");

    assert_eq!(
        data_hash_exclusions(PNG_MIME, &asset).expect("c2pa-rs's own data hash"),
        vec![span.chunk],
        "c2pa-rs's own embedder must exclude exactly the whole `caBX` chunk gamut-png reports"
    );
}
