//! **Direction 2** of the epic's oracle: c2pa-rs embeds a manifest store and chooses where it
//! goes; gamut must locate the **identical byte range**.
//!
//! "Identical", not "overlapping", is the whole claim. `gamut-heic`'s locator bounds a store by its
//! JUMBF `LBox` alone, which is content-dependent: reading an `LBox` from a wrong offset can land
//! on an interior box's own length — small, plausible and in bounds — and silently trim the store
//! to a fragment rather than fail. `crates/gamut-heic/STATUS.md` records that as a deferred hazard
//! and names this crate as the fixture that settles it. A range compared against a store c2pa-rs
//! actually wrote is what catches it.
//!
//! The span the two locators are compared against is derived independently, from the store's own
//! JUMBF header (`c2pa_oracle::jumbf_superbox_span`), never from gamut's parse of the ISOBMFF
//! framing — otherwise the comparison would be a tautology.
//!
//! # Why `gamut-heic` is measured on an AVIF
//!
//! C2PA 2.4 Appendix A defines **one** placement for every BMFF-based asset — a top-level `uuid`
//! box with user type `D8FEC3D6-…` — and names HEIF and AVIF together; c2pa-rs likewise serves
//! `image/avif` and `image/heic` from the same BMFF handler with the same writer. `HeifContainer`
//! is a container lens over ISOBMFF/MIAF, and an AVIF is a MIAF file, so pointing it at one
//! exercises exactly the locator under test. The alternative — hand-building a HEIF around real
//! HEVC — would test the fixture, not the locator.

mod common;

use c2pa::ValidationState;
use c2pa_oracle::{
    AVIF_MIME, HEIC_MIME, JUMBF_SUPERBOX_TYPE, embed, jumbf_superbox_span, read,
    read_with_external_store,
};
use common::plain_avif;
use gamut_avif::AvifContainer;
use gamut_heic::HeifContainer;

/// The fixture both locators are pointed at: a gamut-encoded AVIF that c2pa-rs then signed and
/// embedded a store into, itself choosing the placement.
fn signed_avif() -> Vec<u8> {
    embed(AVIF_MIME, &plain_avif()).expect("c2pa-rs embeds a store into the gamut-encoded AVIF")
}

#[test]
fn gamut_avif_bounds_the_store_c2pa_rs_embedded_by_the_box_that_carries_it() {
    let asset = signed_avif();
    let expected = jumbf_superbox_span(&asset).expect("the signed asset carries a JUMBF superbox");

    let container = AvifContainer::parse(&asset).expect("the signed asset parses");
    let slot = container.c2pa_slot().expect("gamut-avif locates the slot");

    // `gamut-avif` bounds the slot by the *box*, so it reports the store and anything the writer
    // left after it (`C2paSlot::slot_bytes`: "the store, then any padding"). The claim this test
    // holds gamut to is therefore containment, not equality: the slot begins exactly where the
    // store begins and holds every byte of it. Whether c2pa-rs leaves padding at all is c2pa-rs's
    // business, and it is pinned on its own below — so a future padding c2pa-rs fails *that* test
    // rather than being misread here as gamut mis-locating.
    assert_eq!(
        slot.range.start, expected.start,
        "gamut-avif's reported range must begin at the store c2pa-rs embedded, not before or \
         after it"
    );
    assert!(
        slot.range.end >= expected.end,
        "gamut-avif's reported range must hold the whole store, not a fragment of it: reported \
         {:?}, store at {expected:?}",
        slot.range
    );
    assert_eq!(
        &slot.slot_bytes[..expected.len()],
        &asset[expected],
        "the bytes gamut-avif hands back must be the bytes at that range"
    );
}

#[test]
fn c2pa_rs_leaves_no_padding_between_the_store_and_the_end_of_its_box() {
    let asset = signed_avif();
    let expected = jumbf_superbox_span(&asset).expect("the signed asset carries a JUMBF superbox");

    let container = AvifContainer::parse(&asset).expect("the signed asset parses");
    let slot = container.c2pa_slot().expect("gamut-avif locates the slot");

    // An observation about the reference implementation, recorded in `README.md` beside the
    // `update`-purpose finding and asserted here for the same reason: it is what makes the
    // box-bounded bound (`gamut-avif`) and the `LBox`-bounded bound (`gamut-heic`) report the
    // *same* range for the same file. Nothing in C2PA 2.4 §A.5.1.2 forbids a writer from sizing
    // the box larger than the store, so this is evidence, not a rule — and when it stops holding,
    // this is the test that says so.
    assert_eq!(
        slot.range, expected,
        "c2pa-rs sizes the ContentProvenanceBox to the store exactly; a difference here is the \
         reference implementation having started to pad, not gamut-avif mis-locating"
    );
}

#[test]
fn gamut_heic_reports_the_exact_span_c2pa_rs_embedded() {
    let asset = signed_avif();
    let expected = jumbf_superbox_span(&asset).expect("the signed asset carries a JUMBF superbox");

    let container = HeifContainer::parse(&asset).expect("the signed asset parses");
    let store = container.c2pa().expect("gamut-heic locates the store");

    assert_eq!(
        store.range, expected,
        "gamut-heic bounds the store by its `LBox`; against a store c2pa-rs really wrote, that \
         bound must land on the store exactly (crates/gamut-heic/STATUS.md's deferred row)"
    );
    assert_eq!(
        store.bytes, &asset[expected],
        "the bytes gamut-heic hands back must be the bytes at that range"
    );
}

#[test]
fn c2pa_rs_validates_the_store_read_out_of_gamut_avifs_reported_range() {
    let asset = signed_avif();
    let container = AvifContainer::parse(&asset).expect("the signed asset parses");
    let range = container
        .c2pa_slot()
        .expect("gamut-avif locates the slot")
        .range;
    let located = asset[range].to_vec();

    // The sharpest form of the claim, and it is `range` that is exercised: the bytes are cut out
    // of the asset *at the range gamut reported*, not taken from the `slot_bytes` the same call
    // hands over, so a range wrong by one byte reaches c2pa-rs as wrong bytes. A span starting a
    // byte early or late does not parse as JUMBF, and one cut short fails its own length field,
    // so only the exact range survives — and the hard binding still has to verify against the
    // asset on top of that.
    assert_eq!(
        read_with_external_store(AVIF_MIME, &located, &asset)
            .expect("the located bytes parse as a manifest store"),
        ValidationState::Valid,
        "c2pa-rs must accept the store gamut-avif extracted, bound to the same asset"
    );
}

#[test]
fn every_store_c2pa_rs_writes_opens_with_the_jumb_superbox_type() {
    let asset = signed_avif();
    let span = jumbf_superbox_span(&asset).expect("the signed asset carries a JUMBF superbox");

    // The empirical half of `crates/gamut-heic/STATUS.md`'s deferred row. gamut deliberately does
    // not assert `TBox == "jumb"`, because the C2PA specification names the constant only in JPEG
    // XL clauses attributing it to ISO/IEC 18181-2. The oracle can say what the reference
    // implementation does: a store's first eight bytes are its `LBox` and that type.
    assert_eq!(
        &asset[span.start + 4..span.start + 8],
        JUMBF_SUPERBOX_TYPE,
        "the reference implementation's manifest store opens LBox + `jumb`, which is the check \
         that would close gamut-heic's content-dependent `LBox` bound"
    );
}

#[test]
fn the_same_bytes_validate_when_offered_to_c2pa_rs_as_heic() {
    let asset = signed_avif();

    // The placement is one placement for all BMFF-based assets: c2pa-rs reads the same file under
    // either MIME type. This is what licenses measuring `gamut-heic`'s locator on this fixture.
    assert_eq!(
        read(HEIC_MIME, &asset).expect("the signed asset carries a store"),
        read(AVIF_MIME, &asset).expect("the signed asset carries a store"),
    );
}
