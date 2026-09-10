//! **Direction 1** of the epic's oracle: gamut reserves a manifest-store slot, an external signer
//! completes it, and c2pa-rs validates the result.
//!
//! The point of the direction is that c2pa-rs never writes a byte of the container. It is handed a
//! finished AVIF that `gamut-avif` produced, computes the `c2pa.hash.bmff.v3` hard binding over
//! those bytes, and returns a store the host patches into the range the *encoder* reported before
//! any signer existed. If gamut's reported range were wrong by a byte, or if anything after the
//! slot moved when it was filled, the binding would not verify and c2pa-rs would say so.

mod common;

use c2pa::ValidationState;
use c2pa_oracle::{AVIF_MIME, declared_store_len, read, reserve_then_fill};
use common::reserve_avif;

#[test]
fn a_reserved_but_unfilled_slot_never_validates() {
    // Reserving is not signing. A slot nobody filled is `len` zero bytes inside a box whose
    // `box_purpose` says `manifest`, so c2pa-rs finds the box and then cannot parse its contents.
    //
    // The observed verdict is a *parse* error ("unexpected end of file"), not `JumbfNotFound`:
    // c2pa-rs distinguishes "no store here" from "a store here that is not one", and a half-built
    // file is honestly the second. What matters for the reserve seam is only the negative — the
    // zeros must never come back `Valid` — so that is what is asserted, and c2pa-rs's choice of
    // error is left as c2pa-rs's business rather than pinned as a promise it never made.
    let (asset, slot) = reserve_avif(4096).expect("reserve a slot");
    assert!(
        asset[slot].iter().all(|byte| *byte == 0),
        "an unfilled slot is zeros"
    );

    assert_ne!(
        read(AVIF_MIME, &asset).ok(),
        Some(ValidationState::Valid),
        "an unfilled slot must never validate as a manifest store"
    );
}

#[test]
fn a_store_signed_over_the_reserved_file_validates_once_patched_into_the_reported_slot() {
    let filled = reserve_then_fill(AVIF_MIME, reserve_avif).expect("reserve, sign and patch");

    assert_eq!(
        read(AVIF_MIME, &filled.asset).expect("the patched asset carries a store"),
        ValidationState::Valid,
        "c2pa-rs must accept a store signed over the reserved file and written at the range \
         `AvifEncoder::encode_with_report` reported"
    );
}

#[test]
fn the_signed_store_exactly_fills_the_slot_that_was_reserved() {
    let filled = reserve_then_fill(AVIF_MIME, reserve_avif).expect("reserve, sign and patch");

    // `Builder::placeholder` pins the JUMBF length and `sign_embeddable` zero-pads back to it, so
    // the store is the size the caller was told to reserve. Nothing in the file after the slot can
    // move, which is the encoder-side criterion the epic states.
    assert_eq!(
        filled.store.len(),
        filled.placeholder_store_len,
        "the signed store must be exactly the length the placeholder asked the host to reserve"
    );
    assert_eq!(
        filled.slot.len(),
        filled.store.len(),
        "the reserved slot must be exactly the store's length"
    );
    assert_eq!(
        &filled.asset[filled.slot.clone()],
        filled.store.as_slice(),
        "the slot's bytes must be the store's bytes"
    );
}

#[test]
fn the_store_declares_its_own_length_as_the_whole_slot() {
    let filled = reserve_then_fill(AVIF_MIME, reserve_avif).expect("reserve, sign and patch");

    // The store's outer JUMBF `LBox` is what `gamut-heic`'s locator trims to. When the slot is
    // sized from the placeholder there is no padding, so the two bounds coincide — which is what
    // makes it safe for `gamut-avif` (box-bounded) and `gamut-heic` (`LBox`-bounded) to report the
    // same range for the same file.
    assert_eq!(
        declared_store_len(&filled.store).expect("the store declares its own length"),
        filled.slot.len(),
        "the store's own `LBox` must account for the whole reserved slot"
    );
}
