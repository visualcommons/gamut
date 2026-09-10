//! A derivative must not carry its parent's manifest store, and c2pa-rs must report it as
//! **unsigned** rather than **invalid**.
//!
//! The distinction is the epic's, and it is not cosmetic. Re-encoding invalidates the hard binding,
//! so a store copied forward would still be *found* — and would then fail validation, presenting a
//! derivative that is merely a new rendition as a tampered file. The correct outcome is that there
//! is no store to find at all; a derivative that wants provenance needs a *new* manifest naming the
//! parent as an ingredient, which is a claim generator's job and not a container library's.
//!
//! # The parent has to be in the causal path
//!
//! Encoding a fresh image and observing that it carries no store proves nothing: it is true of any
//! encoder that was never handed a store. So every test here runs the real pipeline — locate the
//! parent's store with `gamut-avif`, carry it into a [`Metadata`] model as the
//! [`MetadataBlock::C2pa`] carrier `gamut-metadata` defines for exactly this, then ask
//! [`MetadataEmbedder`] what to write into the derivative — and the derivative is encoded from
//! *what the embedder returned*. If [`C2paPolicy`] ever handed the store back, the derivative would
//! carry it and c2pa-rs would find one, which is the failure this file exists to see.
//!
//! Issue #428 names `C2paPolicy` as the mechanism, and it has two arms:
//!
//! * [`C2paPolicy::Drop`] (the default) — the store is not emitted, and the derivative reads back
//!   as unsigned. That is the whole round trip, end to end.
//! * [`C2paPolicy::Reject`] — the same refusal, made loud, for a caller that must be told
//!   provenance is being lost rather than discover it downstream.
//!
//! Forcing the forward (assigning the model's store to `EncodedMetadata::c2pa` by hand, the arm
//! `C2paPolicy` deliberately does not offer) makes the first test fail with c2pa-rs reporting
//! `Valid` — not `Invalid`. That is not a softening of the hazard, it is a sharpening of it: this
//! fixture re-encodes deterministically, and a BMFF hard binding excludes the `ContentProvenanceBox`
//! by box path, so a bit-identical derivative wearing its parent's store validates and presents
//! another party's claim as its own. The failure mode a `Preserve` arm would open is therefore not
//! "a file that looks tampered with" but "a file that looks signed".
//!
//! [`Metadata`]: gamut_metadata::Metadata
//! [`MetadataBlock::C2pa`]: gamut_metadata::MetadataBlock::C2pa
//! [`MetadataEmbedder`]: gamut_metadata::MetadataEmbedder
//! [`C2paPolicy`]: gamut_metadata::C2paPolicy

mod common;

use c2pa::ValidationState;
use c2pa_oracle::{AVIF_MIME, embed, is_jumbf_not_found, read};
use common::{dims, plain_avif, source_rgb};
use gamut_avif::{AvifContainer, AvifEncoder};
use gamut_core::{EncodeImage, ImageRef, Rgb8};
use gamut_metadata::{
    C2paPolicy, MetadataBlock, MetadataEmbedder, MetadataError, MetadataExtractor,
};

/// A gamut AVIF that c2pa-rs has signed, asserted `Valid` first so a later `JumbfNotFound` is
/// about the derivative rather than about a parent that never carried a store.
fn signed_parent() -> Vec<u8> {
    let parent = embed(AVIF_MIME, &plain_avif()).expect("c2pa-rs signs the parent");
    assert_eq!(
        read(AVIF_MIME, &parent).expect("the parent carries a store"),
        ValidationState::Valid,
        "the parent must be valid, or a derivative's outcome proves nothing"
    );
    parent
}

/// The parent's manifest store, located by `gamut-avif` — the bytes a container hands the facade.
fn store_of(parent: &[u8]) -> Vec<u8> {
    let container = AvifContainer::parse(parent).expect("the signed parent parses");
    let slot = container
        .c2pa()
        .expect("gamut-avif locates the parent's store");
    parent[slot.range].to_vec()
}

/// Re-encodes the fixture, writing whatever C2PA block `embedder` returned for a model carrying
/// `store` — so the derivative's contents are downstream of the policy under test.
fn derivative_through(embedder: MetadataEmbedder, store: &[u8]) -> Vec<u8> {
    let meta = MetadataExtractor::new()
        .extract(&[MetadataBlock::C2pa(store)])
        .expect("a lone C2PA block extracts");
    let blocks = embedder.embed(&meta).expect("embedding the parent's model");

    let rgb = source_rgb();
    let mut encoder = AvifEncoder::new();
    if let Some(forwarded) = blocks.c2pa.as_deref() {
        encoder = encoder.with_c2pa(forwarded);
    }
    encoder
        .encode_to_vec(ImageRef::<Rgb8>::new(&rgb, dims()).expect("buffer matches dimensions"))
        .expect("the derivative encodes")
}

#[test]
fn a_derivative_built_from_the_parents_model_reads_back_as_unsigned_not_as_invalid() {
    let parent = signed_parent();
    let derivative = derivative_through(MetadataEmbedder::new(), &store_of(&parent));

    let error =
        read(AVIF_MIME, &derivative).expect_err("a derivative must carry no manifest store at all");
    assert!(
        is_jumbf_not_found(&error),
        "c2pa-rs must report the derivative as unsigned (`JumbfNotFound`), not as a file whose \
         store fails to validate; got {error}"
    );
}

#[test]
fn the_reject_policy_refuses_the_parents_store_rather_than_losing_it_quietly() {
    let parent = signed_parent();
    let store = store_of(&parent);
    let meta = MetadataExtractor::new()
        .extract(&[MetadataBlock::C2pa(&store)])
        .expect("a lone C2PA block extracts");

    let error = MetadataEmbedder::new()
        .c2pa_policy(C2paPolicy::Reject)
        .embed(&meta)
        .expect_err("`Reject` must refuse a model carrying a store");
    assert!(
        matches!(error, MetadataError::UnembeddableC2pa { len } if len == store.len()),
        "the refusal must name the parent's store, by the length gamut-avif located; got {error}"
    );
}
