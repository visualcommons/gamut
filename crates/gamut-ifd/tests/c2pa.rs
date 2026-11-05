//! The C2PA disclaimer's wording, pinned as a literal.
//!
//! Every headline test interpolates `C2PA_NOT_VALIDATED` into its expectation, so a reworded
//! constant passes all of them; this one asserts the sentence itself. It pins only this crate's
//! copy: `gamut-avif`, `gamut-heic`, `gamut-png` and `gamut-riff` each pin their own copy in their
//! own `tests/c2pa.rs`, so a drift in another crate's copy fails there, not here.

#[test]
fn c2pa_not_validated_is_the_pinned_sentence() {
    assert_eq!(
        gamut_ifd::C2PA_NOT_VALIDATED,
        "gamut locates a C2PA manifest store and never validates it: it checks no signature, no \
         hash binding and no trust list. Validate with c2pa-rs."
    );
}
