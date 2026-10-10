//! The provenance lens through the facade: `Metadata::provenance()` over blocks a container
//! located. Four states from two independent sources — an embedded manifest store
//! (`MetadataBlock::C2pa`) and a `dcterms:provenance` URL in the XMP packet (C2PA 2.4 §11.5,
//! §15.5.3.1) — with neither source suppressing the other, and no attempt to resolve the URL.

use gamut_metadata::xmp::{WellKnownNs, XmpMeta};
use gamut_metadata::{Metadata, MetadataBlock, ProvenanceState};

const URL: &str = "https://example.com/manifests/photo.c2pa";

/// An XMP packet whose only property is `dcterms:provenance` as element text.
fn xmp_with_provenance_text(url: &str) -> Vec<u8> {
    let mut xmp = XmpMeta::new();
    xmp.set_text(WellKnownNs::DcTerms.uri(), "provenance", url);
    xmp.to_packet()
}

/// An XMP packet with an unrelated property, so the graph is present but says nothing about
/// provenance.
fn xmp_without_provenance() -> Vec<u8> {
    let mut xmp = XmpMeta::new();
    xmp.set_text(WellKnownNs::Xmp.uri(), "CreatorTool", "gamut");
    xmp.to_packet()
}

/// Bytes shaped like a JUMBF superbox header; the facade never looks inside, so nothing here
/// needs to be a valid manifest.
fn c2pa_store() -> Vec<u8> {
    let mut store = vec![0x00, 0x00, 0x00, 0x1C, b'j', b'u', b'm', b'b'];
    store.extend_from_slice(b"c2pa\xFF\x00not a real manifest");
    store
}

#[test]
fn no_store_and_no_url_is_none() {
    let xmp = xmp_without_provenance();
    let meta = Metadata::from_blocks(&[MetadataBlock::Xmp(&xmp)]).unwrap();
    assert_eq!(meta.provenance(), ProvenanceState::None);

    // No metadata at all is None too, not a panic or a synthesized state.
    assert_eq!(
        Metadata::from_blocks(&[]).unwrap().provenance(),
        ProvenanceState::None
    );
}

#[test]
fn url_without_a_store_is_remote() {
    // The issue's motivating case: a file with no embedded manifest store but a
    // `dcterms:provenance` URL must not report None.
    let xmp = xmp_with_provenance_text(URL);
    let meta = Metadata::from_blocks(&[MetadataBlock::Xmp(&xmp)]).unwrap();
    assert_eq!(meta.provenance(), ProvenanceState::Remote(URL.to_owned()));
}

#[test]
fn store_without_a_url_is_embedded() {
    let (xmp, store) = (xmp_without_provenance(), c2pa_store());
    let meta =
        Metadata::from_blocks(&[MetadataBlock::Xmp(&xmp), MetadataBlock::C2pa(&store)]).unwrap();
    assert_eq!(meta.provenance(), ProvenanceState::Embedded);
}

#[test]
fn store_and_url_is_embedded_and_remote() {
    // §11.5 reserves the key for external manifests, yet nothing stops a file from carrying both:
    // the embedded store must not hide the URL, and the URL must not hide the store.
    let (xmp, store) = (xmp_with_provenance_text(URL), c2pa_store());
    let meta =
        Metadata::from_blocks(&[MetadataBlock::Xmp(&xmp), MetadataBlock::C2pa(&store)]).unwrap();
    assert_eq!(
        meta.provenance(),
        ProvenanceState::EmbeddedAndRemote(URL.to_owned())
    );
}
