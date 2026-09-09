//! A derivative must not carry its parent's manifest store, and c2pa-rs must report it as
//! **unsigned** rather than **invalid**.
//!
//! The distinction is the epic's, and it is not cosmetic. Re-encoding invalidates the hard binding,
//! so a store copied forward would still be *found* — and would then fail validation, presenting a
//! derivative that is merely a new rendition as a tampered file. The correct outcome is that there
//! is no store to find at all; a derivative that wants provenance needs a *new* manifest naming the
//! parent as an ingredient, which is a claim generator's job and not a container library's.
//!
//! `gamut-avif` reaches that outcome structurally: a `ContentProvenanceBox` appears only when
//! `with_c2pa_reserved` or `with_c2pa` asked for one, and no encoder input can introduce one.
//! These tests pin the observable half of that — what c2pa-rs says about the two files.

mod common;

use c2pa::ValidationState;
use c2pa_oracle::{AVIF_MIME, embed, is_jumbf_not_found, read};
use common::plain_avif;

#[test]
fn a_gamut_re_encode_of_a_signed_parent_reads_back_as_unsigned_not_as_invalid() {
    let parent = embed(AVIF_MIME, &plain_avif()).expect("c2pa-rs signs the parent");
    assert_eq!(
        read(AVIF_MIME, &parent).expect("the parent carries a store"),
        ValidationState::Valid,
        "the parent must be valid, or the derivative's outcome proves nothing"
    );

    // The derivative: the same pixels encoded again, with no C2PA knob set — which is every
    // re-encode this crate can perform.
    let derivative = plain_avif();

    let error =
        read(AVIF_MIME, &derivative).expect_err("a derivative must carry no manifest store at all");
    assert!(
        is_jumbf_not_found(&error),
        "c2pa-rs must report the derivative as unsigned (`JumbfNotFound`), not as a file whose \
         store fails to validate; got {error}"
    );
}
