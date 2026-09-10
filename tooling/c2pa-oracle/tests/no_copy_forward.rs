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
//! Issue #428 names `C2paPolicy` as the mechanism, and its default arm — `Drop` — is what this
//! file drives end to end. Its other arm, `Reject`, refuses the same store loudly instead, and is
//! a claim about `gamut-metadata` alone: `crates/gamut-metadata/tests/roundtrip.rs` already pins
//! it against a synthetic store, with no c2pa-rs and no encoder in reach. Restating it here would
//! only re-run that assertion behind a signing chain that judges none of it.
//!
//! Forcing the forward (assigning the model's store to `EncodedMetadata::c2pa` by hand, the arm
//! `C2paPolicy` deliberately does not offer) makes the test below fail with c2pa-rs reporting
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
use gamut_metadata::{MetadataBlock, MetadataEmbedder, MetadataExtractor};

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
    assert!(
        meta.c2pa.is_some(),
        "the parent's store must be in the model handed to the embedder, or the policy is asked \
         to drop nothing and the derivative carries no store for a reason that is not the policy"
    );
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
