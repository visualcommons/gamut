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
use c2pa_oracle::{
    AVIF_MIME, declared_store_len, read, reserve_then_fill, reserve_then_fill_padded,
};
use common::{PADDING, reserve_avif};

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
fn the_signed_store_is_exactly_the_length_the_placeholder_asked_the_host_to_reserve() {
    let filled = reserve_then_fill(AVIF_MIME, reserve_avif).expect("reserve, sign and patch");

    // `Builder::placeholder` pins the JUMBF length and `sign_embeddable` zero-pads back to it, so
    // the store is the size the caller was told to reserve. Nothing in the file after the slot can
    // move, which is the encoder-side criterion the epic states.
    //
    // This is the one claim here that is about c2pa-rs. `slot.len() == store.len()` and "the
    // slot's bytes are the store's bytes" were asserted alongside it and could not fail:
    // `reserve_then_fill` refuses the first before it returns — see
    // `a_slot_that_is_not_the_signed_stores_length_is_refused_rather_than_patched` below, which
    // drives that refusal instead of restating it — and produces the second with
    // `copy_from_slice`. Both said only that the oracle's own plumbing ran.
    assert_eq!(
        filled.store.len(),
        filled.placeholder_store_len,
        "the signed store must be exactly the length the placeholder asked the host to reserve"
    );
}

#[test]
fn a_slot_that_is_not_the_signed_stores_length_is_refused_rather_than_patched() {
    // The refusal `reserve_then_fill` carries, exercised rather than assumed: reserve one byte
    // more than the placeholder asked for and the signed store no longer fills the slot. Patching
    // it in anyway would leave a trailing byte of whatever the encoder wrote inside the store's
    // own `LBox` bound, and every "the range is identical" claim downstream would be measured
    // against a store the oracle had quietly mis-sized.
    //
    // A slot that is *meant* to be larger than the store is `reserve_then_fill_padded`'s job, and
    // the padded tests below sign one successfully; here no padding was asked for, so the extra
    // byte is a mis-sized slot, not padding.
    let error = reserve_then_fill(AVIF_MIME, |len| reserve_avif(len + 1))
        .expect_err("a slot one byte too long is not a slot the signed store fits");

    let message = error.to_string();
    assert!(
        message.contains("signed store is") && message.contains("the reserved slot is"),
        "the refusal must name both lengths, so a caller sees which side got it wrong rather \
         than only that something did; got {error}"
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

#[test]
fn a_store_zero_padded_into_a_slot_longer_than_it_still_validates() {
    let filled = reserve_then_fill_padded(AVIF_MIME, PADDING, reserve_avif)
        .expect("reserve a padded slot, sign and patch");

    // A signer that reserves generously, before it knows its final certificate chain, leaves
    // zeros between the end of the store and the end of the `ContentProvenanceBox`. c2pa-rs
    // reading that file `Valid` is what says the padding is in-spec carriage, not corruption — and
    // so what makes `gamut-avif` reporting it inside the slot correct rather than a mis-bound.
    assert_eq!(
        read(AVIF_MIME, &filled.asset).expect("the padded asset carries a store"),
        ValidationState::Valid,
        "c2pa-rs must accept a store followed by zero padding inside its box"
    );
}

#[test]
fn a_padded_slot_holds_the_store_its_lbox_declares_then_only_zeros() {
    // The reserve closure fills its slot with 0xAA rather than the encoder's zeros, so a padding
    // that came back zero was written by the oracle, not inherited from `with_c2pa_reserved`.
    let filled = reserve_then_fill_padded(AVIF_MIME, PADDING, |len| {
        let (mut asset, slot) = reserve_avif(len)?;
        asset[slot.clone()].fill(0xAA);
        Ok((asset, slot))
    })
    .expect("reserve a padded slot, sign and patch");

    let slot = &filled.asset[filled.slot.clone()];
    let (store, padding) = slot.split_at(filled.store.len());
    assert_eq!(
        (
            declared_store_len(store).expect("the store declares its own length"),
            padding.len(),
        ),
        (filled.store.len(), PADDING),
        "the store's own `LBox` must stop where the padding begins, and the padding must be \
         exactly what was asked for"
    );
    assert!(
        padding.iter().all(|byte| *byte == 0),
        "the bytes after the store must be zeros, whatever the slot held before"
    );
}

#[test]
fn a_padded_slot_that_is_not_store_plus_padding_long_is_refused() {
    let error = reserve_then_fill_padded(AVIF_MIME, PADDING, |len| reserve_avif(len + 1))
        .expect_err("a slot one byte longer than store plus padding is mis-sized");

    let message = error.to_string();
    assert!(
        message.contains("signed store is")
            && message.contains(&format!("padding asked for: {PADDING}")),
        "the refusal must name the padding as well as both lengths; got {error}"
    );
}

#[test]
fn a_padding_that_overflows_the_length_to_reserve_is_refused_before_reserving() {
    let mut reserved = false;
    let error = reserve_then_fill_padded(AVIF_MIME, usize::MAX, |len| {
        reserved = true;
        reserve_avif(len)
    })
    .expect_err("no store length plus usize::MAX fits in a usize");

    assert!(
        !reserved && error.to_string().contains("overflows usize"),
        "an unrepresentable length must be refused before the host is asked to reserve it; got \
         {error}"
    );
}

#[test]
fn a_slot_shorter_than_the_padding_is_refused_rather_than_underflowing() {
    // `slot.len() - padding` would panic here; the guard must turn it into the same refusal.
    let error = reserve_then_fill_padded(AVIF_MIME, PADDING, |_| reserve_avif(PADDING - 1))
        .expect_err("a slot shorter than the padding alone cannot hold a store");

    assert!(
        error.to_string().contains("the reserved slot is"),
        "a too-short slot must be refused by the length guard; got {error}"
    );
}
