//! Remote provenance against c2pa-rs (issue #846): a file with no embedded store and a
//! `dcterms:provenance` URL in its XMP is reported **remote**, at the same URL, by both
//! `gamut_metadata::Metadata::provenance` and c2pa-rs.
//!
//! C2PA 2.4 §11.5 reserves `dcterms:provenance` for an external manifest's URL, and §15.5.3.1 has a
//! validator that finds no embedded store look there. Both sides are asked about the same PNG, in
//! both directions: c2pa-rs writing the reference into a gamut-encoded PNG and gamut reading it,
//! and gamut writing the reference and c2pa-rs reading it. An embedded store is the contrast case,
//! so that neither side can pass by calling every file remote.
//!
//! c2pa-rs's side is [`remote_manifest_url`], which explains why this crate reads the URL c2pa-rs
//! *would* fetch rather than `Reader::remote_url` after a fetch: this crate is built without the
//! feature and the network a fetch needs. What a fetch would return is checked instead — the store
//! c2pa-rs signed for that URL validates against the asset carrying the reference.

mod common;

use c2pa::ValidationState;
use c2pa_oracle::{PNG_MIME, embed, embed_remote, read_with_external_store, remote_manifest_url};
use common::{dims, source_rgb};
use gamut_core::{EncodeImage, ImageRef, Rgb8};
use gamut_metadata::xmp::{WellKnownNs, XmpMeta};
use gamut_metadata::{Metadata, MetadataBlock, ProvenanceState};
use gamut_png::PngEncoder;

/// The external manifest's URL. Never fetched by either side.
const URL: &str = "https://manifests.example.com/c2pa-oracle/846.c2pa";

/// The shared fixture (`tests/common/mod.rs`) as gamut-png encodes it, with `xmp` as its XMP
/// packet when given.
fn png(xmp: Option<&str>) -> Vec<u8> {
    let rgb = source_rgb();
    let image = ImageRef::<Rgb8>::new(&rgb, dims()).expect("buffer matches dimensions");
    let encoder = match xmp {
        Some(xmp) => PngEncoder::new().with_xmp(xmp),
        None => PngEncoder::new(),
    };
    encoder.encode_to_vec(image).expect("the fixture encodes")
}

/// What gamut's metadata facade says about `asset`'s provenance, from the blocks gamut-png
/// located in it.
fn gamut_provenance(asset: &[u8]) -> ProvenanceState {
    let located = gamut_png::metadata(asset).expect("gamut-png reads the PNG");
    let blocks: Vec<MetadataBlock<'_>> = [
        located.xmp().map(MetadataBlock::Xmp),
        located.c2pa.as_deref().map(MetadataBlock::C2pa),
    ]
    .into_iter()
    .flatten()
    .collect();
    Metadata::from_blocks(&blocks)
        .expect("the facade reads the located blocks")
        .provenance()
}

#[test]
fn a_reference_c2pa_rs_wrote_into_a_gamut_png_is_remote_to_both() {
    let signed = embed_remote(PNG_MIME, &png(None), URL).expect("c2pa-rs signs a remote manifest");

    assert_eq!(
        gamut_provenance(&signed.asset),
        ProvenanceState::Remote(URL.to_owned()),
        "gamut must report the URL c2pa-rs wrote, with no embedded store"
    );
    assert_eq!(
        remote_manifest_url(PNG_MIME, &signed.asset).expect("c2pa-rs reads the asset"),
        Some(URL.to_owned()),
        "c2pa-rs must report the same URL for the asset it wrote"
    );
    assert_eq!(
        read_with_external_store(PNG_MIME, &signed.store, &signed.asset)
            .expect("the remote store parses"),
        ValidationState::Valid,
        "the store the URL stands for must bind to the asset carrying the reference"
    );
}

#[test]
fn a_reference_gamut_wrote_is_remote_to_both() {
    let mut xmp = XmpMeta::new();
    xmp.set_text(WellKnownNs::DcTerms.uri(), "provenance", URL);
    let packet = String::from_utf8(xmp.to_packet()).expect("an XMP packet is UTF-8");
    let asset = png(Some(&packet));

    assert_eq!(
        gamut_provenance(&asset),
        ProvenanceState::Remote(URL.to_owned()),
        "gamut must report the URL it wrote"
    );
    assert_eq!(
        remote_manifest_url(PNG_MIME, &asset).expect("c2pa-rs reads the asset"),
        Some(URL.to_owned()),
        "c2pa-rs must read the URL out of the XMP gamut wrote"
    );
}

#[test]
fn an_embedded_store_is_remote_to_neither() {
    let asset = embed(PNG_MIME, &png(None)).expect("c2pa-rs embeds a store");

    assert_eq!(gamut_provenance(&asset), ProvenanceState::Embedded);
    assert_eq!(
        remote_manifest_url(PNG_MIME, &asset).expect("c2pa-rs reads the embedded store"),
        None,
        "c2pa-rs must not report an embedded store as remote"
    );
}
