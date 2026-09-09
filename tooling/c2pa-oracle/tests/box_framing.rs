//! The `ContentProvenanceBox` `gamut-avif` writes around a store must be the box the reference
//! implementation writes around the same store — byte for byte.
//!
//! `AvifEncoder`'s own suite pins the framing against a fixture built from the C2PA 2.4 §A.5.1.2
//! field list, which catches a transcription slip but not a *reading* of the clause that differs
//! from everyone else's. This is the differential that does: c2pa-rs's `Builder::composed_manifest`
//! wraps raw store bytes in the same box for the same format, and the two renderings are compared
//! whole — header, user type, `FullBox` version and flags, `box_purpose`, merkle offset and store.
//!
//! It is also what licenses the rest of this crate to hand gamut and c2pa-rs the same files: if the
//! two framings agreed only in the fields a locator happens to read, "the identical byte range"
//! would be a weaker statement than it looks.

mod common;

use c2pa::Builder;
use c2pa_oracle::{AVIF_MIME, reserve_then_fill};
use common::reserve_avif;

#[test]
fn the_box_gamut_avif_writes_is_byte_identical_to_the_one_c2pa_rs_composes() {
    let filled = reserve_then_fill(AVIF_MIME, reserve_avif).expect("reserve, sign and patch");
    let composed = Builder::composed_manifest(&filled.store, AVIF_MIME)
        .expect("c2pa-rs composes a box around the same store");

    // The store sits at the end of the composed box, so the box starts that far ahead of the slot.
    let framing = composed.len() - filled.store.len();
    let start = filled
        .slot
        .start
        .checked_sub(framing)
        .expect("the box begins inside the file");

    assert_eq!(
        &filled.asset[start..start + composed.len()],
        composed.as_slice(),
        "gamut-avif's ContentProvenanceBox must be byte-identical to c2pa-rs's for the same store"
    );
}
