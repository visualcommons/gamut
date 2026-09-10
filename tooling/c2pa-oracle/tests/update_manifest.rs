//! The one in-spec BMFF layout `gamut-heic`'s and `gamut-avif`'s locators cannot discriminate: a
//! manifest store written **without** the 8-byte merkle offset under `box_purpose = update`.
//!
//! # What the deferred row asks
//!
//! C2PA 2.4 §A.5.3 states the framing for `manifest` and `original` — the 8-byte merkle offset,
//! then the store — and for `update` says nothing at all about the bytes ahead of the store; its
//! only sentence about that purpose constrains the store's *contents*. gamut therefore **probes**
//! offset 8 first and falls back to 0, and `crates/gamut-heic/STATUS.md` records the offset-less
//! `update` layout as a shape that would be mis-bounded rather than rejected, noting "no known
//! writer emits either shape".
//!
//! "No known writer" is a claim about the world, and this file is where it stops being an
//! assumption. The finding, recorded here and in `README.md`:
//!
//! > **c2pa-rs, driven through its public API to emit a `box_purpose = update` box, writes the
//! > 8-byte merkle offset in front of the store exactly as it does for `manifest` and `original`.**
//!
//! So the reference implementation does not emit the ambiguous layout, the probe's offset-8 arm is
//! the one that fires on real files, and its offset-0 fallback is dead weight against every store
//! in circulation rather than a source of mis-bounding. That is the empirical evidence the
//! deferred row asked for; it does not make the `LBox` bound self-checking, which remains #505's.
//!
//! Driving it needs `BuilderIntent::Update` (c2pa-rs exposes no way to set the purpose string
//! directly) over a file that already carries a store, which is why every test here signs twice.

mod common;

use std::io::Cursor;

use c2pa::{Builder, BuilderIntent, ValidationState};
use c2pa_oracle::{
    AVIF_MIME, OracleError, Result, declared_store_len, embed, read, signing_context,
};
use common::plain_avif;
use gamut_avif::{AvifContainer, C2paBoxPurpose};

/// A file mid-update: a gamut AVIF signed once, then signed again with an update intent, so
/// c2pa-rs relabels the first store `original` and appends an `update` box.
fn mid_update_avif() -> Result<Vec<u8>> {
    let parent = embed(AVIF_MIME, &plain_avif())?;
    let mut builder = Builder::from_context(signing_context()?);
    builder.set_intent(BuilderIntent::Update);
    let mut source = Cursor::new(parent);
    let mut dest = Cursor::new(Vec::new());
    builder.save_to_stream(AVIF_MIME, &mut source, &mut dest)?;
    Ok(dest.into_inner())
}

/// The `update` store in a mid-update file, as gamut reports it.
fn update_slot(asset: &[u8]) -> Result<std::ops::Range<usize>> {
    let container =
        AvifContainer::parse(asset).map_err(|error| OracleError::Asset(error.to_string()))?;
    container
        .c2pa_manifest_stores()
        .find(|slot| slot.purpose == C2paBoxPurpose::Update)
        .map(|slot| slot.range)
        .ok_or_else(|| OracleError::Asset("no `update` store in the mid-update file".into()))
}

#[test]
fn c2pa_rs_writes_the_merkle_offset_in_front_of_an_update_store_too() {
    let asset = mid_update_avif().expect("c2pa-rs produces a mid-update file");
    let update = update_slot(&asset).expect("gamut locates the `update` store");

    // The eight bytes immediately before the store are the merkle offset §A.5.3 states for the
    // other two purposes, written as zero because a still image carries no `merkle` box. Ahead of
    // them is the NUL that terminates the `box_purpose` string.
    assert_eq!(
        &asset[update.start - 8..update.start],
        &[0u8; 8],
        "the reference implementation writes an 8-byte merkle offset in front of an `update` \
         store, so the offset-less layout gamut cannot discriminate is not one it emits"
    );
    assert_eq!(
        asset[update.start - 9],
        0,
        "and immediately before it, the NUL terminating `box_purpose`"
    );
    assert_eq!(
        &asset[update.start - 15..update.start - 9],
        b"update",
        "the purpose really is `update`"
    );
}

#[test]
fn the_update_store_is_bounded_by_its_own_lbox_at_the_probed_offset() {
    let asset = mid_update_avif().expect("c2pa-rs produces a mid-update file");
    let update = update_slot(&asset).expect("gamut locates the `update` store");

    // gamut's probe accepted offset 8 for this purpose. If it had fallen through to offset 0 it
    // would have read the merkle offset's leading bytes as an `LBox`, so the store it reported
    // would not account for its own range.
    assert_eq!(
        declared_store_len(&asset[update.clone()]).expect("the store declares its own length"),
        update.len(),
        "the located `update` store must declare exactly the range gamut reported"
    );
}

#[test]
fn the_earlier_store_is_relabelled_original_when_an_update_box_is_added() {
    let asset = mid_update_avif().expect("c2pa-rs produces a mid-update file");
    let container = AvifContainer::parse(&asset).expect("the mid-update file parses");

    // §A.5.3: once a file carries an `update` box, the store it had before is re-labelled
    // `original`. Both are reported, in file order, and neither is judged — which is why
    // `AvifContainer::c2pa` promises only "the first one".
    let purposes: Vec<_> = container
        .c2pa_manifest_stores()
        .map(|slot| slot.purpose)
        .collect();
    assert_eq!(
        purposes,
        vec![C2paBoxPurpose::Original, C2paBoxPurpose::Update],
        "a mid-update file carries an `original` store followed by an `update` store"
    );
}

#[test]
fn c2pa_rs_still_validates_the_mid_update_file_gamut_read() {
    let asset = mid_update_avif().expect("c2pa-rs produces a mid-update file");

    assert_eq!(
        read(AVIF_MIME, &asset).expect("the mid-update file carries a store"),
        ValidationState::Valid,
        "the two-store layout gamut reports above is one the reference implementation accepts"
    );
}
