//! Dev-only differential oracle over [`c2pa-rs`], the C2PA reference implementation, for gamut's
//! half of the C2PA epic (issue #239): **locating, bounding, carrying and reserving** a manifest
//! store. gamut never renders a validity verdict; this crate is where the verdict comes from, and
//! it lives only under `tooling/`.
//!
//! What is here is the *plumbing* both directions need — a signing identity, a manifest
//! definition, the reserve-then-fill dance, and the one search that recovers a raw JUMBF store
//! from a composed `ContentProvenanceBox`. The claims themselves are in `tests/`.
//!
//! # The two directions
//!
//! 1. **gamut reserves → an external signer completes → c2pa-rs validates.** [`reserve_then_fill`]
//!    drives c2pa-rs's own placeholder workflow (`Builder::placeholder` →
//!    `Builder::update_hash_from_stream` → `Builder::sign_embeddable`) over bytes *gamut* wrote,
//!    so the store is bound to a file c2pa-rs never touched.
//! 2. **c2pa-rs embeds → gamut locates the identical byte range.** [`embed`] hands the asset to
//!    `Builder::save_to_stream`; a test then asks gamut for the store's range and hands the bytes
//!    at exactly that range straight back to [`read_with_external_store`], which is c2pa-rs
//!    re-validating gamut's own bounds.
//!
//! # Why there is no "parse but do not judge" mode
//!
//! See `README.md`. In short: [`ValidationState::Invalid`] is also what c2pa-rs reports when
//! verification is *disabled*, so it cannot stand in for a locator. gamut owns that step.
//!
//! [`c2pa-rs`]: https://github.com/contentauth/c2pa-rs

use std::io::Cursor;
use std::ops::Range;

use c2pa::{Builder, Context, EphemeralSigner, Reader, Settings, ValidationState};

/// The oracle's own errors, kept separate from [`c2pa::Error`] so a failure names which side of
/// the differential produced it.
#[derive(Debug)]
pub enum OracleError {
    /// c2pa-rs refused an operation. Carries its error unchanged: the reference implementation's
    /// own classification is the diagnostic, so it is never re-coded into one of ours.
    C2pa(c2pa::Error),
    /// The asset side of the differential went wrong: a gamut crate refused to produce or read
    /// what the oracle asked for, or the asset it produced does not fit what c2pa-rs signed.
    /// Raised by a caller's closure, and by [`reserve_then_fill`] when a signed store and the slot
    /// reserved for it are not the same length.
    Asset(String),
    /// A composed `ContentProvenanceBox` — or any other buffer — carried no JUMBF superbox: no
    /// [`JUMBF_SUPERBOX_TYPE`] was found in it at all.
    ///
    /// [`find_jumbf_superbox`] is the only function that raises this; [`split_composed_box`] and
    /// [`jumbf_superbox_span`] call it and propagate the refusal unchanged. It is strictly about
    /// *absence*: a superbox that is present but declares a length nothing can use is
    /// [`UnusableSuperboxLength`](Self::UnusableSuperboxLength) instead, so the two are never
    /// conflated.
    NoJumbfSuperbox,
    /// A JUMBF superbox header is present, but the length it declares cannot be used: its
    /// `LBox`/`XLBox` fields are truncated, the length is shorter than the header it is part of —
    /// `LBox` in 2..=7 against the 8-byte header, `XLBox` below 16 against the 16-byte one, or
    /// `LBox == 0` in a buffer that ends inside that 8-byte header — or the length runs past the
    /// end of the buffer it is read from. Carries which of those it was.
    ///
    /// This exists so the length is never *guessed*. A span silently derived from a length that
    /// describes no box would be an oracle handing gamut a wrong answer and calling it a reference
    /// one; see [`declared_store_len`].
    UnusableSuperboxLength(&'static str),
}

impl std::fmt::Display for OracleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::C2pa(error) => write!(f, "c2pa-rs: {error}"),
            Self::Asset(message) => write!(f, "asset: {message}"),
            Self::NoJumbfSuperbox => {
                f.write_str("composed ContentProvenanceBox carries no JUMBF superbox")
            }
            Self::UnusableSuperboxLength(what) => {
                write!(f, "JUMBF superbox declares no usable length: {what}")
            }
        }
    }
}

impl std::error::Error for OracleError {}

impl From<c2pa::Error> for OracleError {
    fn from(error: c2pa::Error) -> Self {
        Self::C2pa(error)
    }
}

/// Shorthand for a fallible oracle operation.
pub type Result<T> = std::result::Result<T, OracleError>;

/// The MIME type gamut-avif's output is handed to c2pa-rs under.
pub const AVIF_MIME: &str = "image/avif";

/// The MIME type a HEIF/HEIC asset is handed to c2pa-rs under.
pub const HEIC_MIME: &str = "image/heic";

/// The JUMBF box type that opens a manifest store's outer superbox: `jumb`.
///
/// Read here as an *observation about what c2pa-rs writes*, never as a gamut constant. The C2PA
/// specification names it only in JPEG XL clauses that attribute it to ISO/IEC 18181-2 §9.3
/// (§A.3.9, §15.12.3.2), which is why `gamut-heic`'s locator deliberately does not assert it — see
/// the deferred row in `crates/gamut-heic/STATUS.md`. An oracle observing it is exactly the
/// empirical evidence that row asks for.
pub const JUMBF_SUPERBOX_TYPE: &[u8; 4] = b"jumb";

/// The manifest definition every store this oracle signs is built from.
///
/// Deliberately minimal: one claim generator and one `c2pa.created` action. The epic's subject is
/// *carriage*, so nothing here exercises assertions, ingredients or thumbnails — a bigger manifest
/// would only make the store longer without testing another byte of gamut.
pub const MANIFEST_DEFINITION: &str = r#"{
    "claim_generator_info": [{ "name": "gamut-c2pa-oracle", "version": "0.0.0" }],
    "title": "gamut c2pa-oracle fixture",
    "assertions": [
        {
            "label": "c2pa.actions.v2",
            "data": {
                "actions": [
                    {
                        "action": "c2pa.created",
                        "digitalSourceType": "http://cv.iptc.org/newscodes/digitalsourcetype/algorithmicMedia"
                    }
                ]
            }
        }
    ]
}"#;

/// A [`Context`] carrying an ephemeral Ed25519 signing identity, with trust-list checking off.
///
/// [`EphemeralSigner`] mints a self-signed CA and an end-entity certificate in memory, carrying
/// the key usage and EKU the C2PA certificate profile requires. Nothing is committed to the tree
/// and no OpenSSL is involved: the chain is built by c2pa-rs's `rust_native_crypto` backend.
///
/// `verify.verify_trust` is turned **off** deliberately. An ephemeral certificate is on no trust
/// list, so leaving it on would make every read fail `signingCredential.untrusted` — a verdict
/// about *provenance of the key*, which is not what this oracle measures. With it off, a store
/// whose cryptographic integrity and hard binding hold reads back [`ValidationState::Valid`], and
/// anything less means gamut moved a byte it should not have. [`ValidationState::Trusted`] is
/// therefore unreachable here by construction, and no assertion asks for it.
///
/// # Errors
///
/// [`OracleError::C2pa`] if the settings are rejected or the ephemeral chain cannot be built.
pub fn signing_context() -> Result<Context> {
    let settings = Settings::new().with_value("verify.verify_trust", false)?;
    Ok(Context::new()
        .with_settings(settings)?
        .with_signer(EphemeralSigner::new("gamut-c2pa-oracle.test")?))
}

/// A [`Builder`] over [`MANIFEST_DEFINITION`], signing through [`signing_context`].
///
/// # Errors
///
/// [`OracleError::C2pa`] if the signing identity cannot be built or the definition does not parse.
pub fn manifest_builder() -> Result<Builder> {
    Ok(Builder::from_context(signing_context()?).with_definition(MANIFEST_DEFINITION)?)
}

/// Where a raw JUMBF manifest store sits inside a **composed** `ContentProvenanceBox` — the whole
/// `uuid` box c2pa-rs returns from `Builder::placeholder` and `Builder::sign_embeddable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedBox {
    /// The whole composed box: ISOBMFF header, 16-byte user type, `FullBox` version and flags,
    /// NUL-terminated `box_purpose`, merkle offset, then the store.
    pub composed: Vec<u8>,
    /// The offset of the store within [`composed`](Self::composed) — equivalently, the length of
    /// everything C2PA 2.4 §A.5.1.2 puts in front of it.
    pub store_offset: usize,
}

impl ComposedBox {
    /// The raw JUMBF manifest store: what a host embeds in its own framing, and what gamut's
    /// locators report.
    #[must_use]
    pub fn store(&self) -> &[u8] {
        &self.composed[self.store_offset..]
    }
}

/// Where the first JUMBF superbox in `buffer` begins — a composed `ContentProvenanceBox`, or a
/// whole asset file.
///
/// # Why this searches rather than parses
///
/// The oracle must not re-derive gamut's own C2PA 2.4 §A.5.1.2 walk: a second copy of the parser
/// under test would prove nothing about it. So the store is found by its *own* header instead —
/// the first [`JUMBF_SUPERBOX_TYPE`] in the buffer, whose four preceding bytes are the superbox's
/// big-endian `LBox`. Nested superboxes inside the store carry the same type, so the first
/// occurrence is the outermost one, and nothing ahead of it can match: the ISOBMFF framing between
/// them is a box header, a UUID, four zero bytes, an ASCII purpose and an eight-byte offset.
///
/// The returned offset is therefore an *independent* claim about where the store begins, which is
/// what makes "gamut reports the same range" a differential rather than a tautology.
///
/// A `jumb` in the first four bytes cannot be a superbox type — there would be no room for the
/// `LBox` in front of it — so the search **continues past** one rather than giving up on it. That
/// costs nothing on today's fixtures, where the framing ahead of the store is fixed; it matters
/// the moment this crate is pointed at a container whose store follows arbitrary bytes.
///
/// # Errors
///
/// [`OracleError::NoJumbfSuperbox`] if no `jumb` box type appears at or after offset 4.
pub fn find_jumbf_superbox(buffer: &[u8]) -> Result<usize> {
    buffer
        .windows(JUMBF_SUPERBOX_TYPE.len())
        .enumerate()
        // Offsets 0..4 have no room for an `LBox`, so skip those windows and keep looking.
        .skip(4)
        .find(|(_, window)| *window == JUMBF_SUPERBOX_TYPE)
        .map(|(type_offset, _)| type_offset - 4)
        .ok_or(OracleError::NoJumbfSuperbox)
}

/// The exact byte span the first JUMBF superbox in `buffer` occupies: from its `LBox` through the
/// number of bytes that `LBox` declares.
///
/// This is the span a container-agnostic reader would call "the manifest store", derived from the
/// store's own header and nothing else. A gamut locator's reported range is compared against it.
///
/// # Errors
///
/// [`OracleError::NoJumbfSuperbox`] if no superbox is found at all;
/// [`OracleError::UnusableSuperboxLength`] if the superbox is there but its declared length cannot
/// be read (see [`declared_store_len`]) or runs past the end of `buffer`. A superbox that is
/// present but unbounded is never reported as one that is absent: `buffer` demonstrably carries a
/// store, and it is the *length* that is unusable — which is the case that variant exists to name.
pub fn jumbf_superbox_span(buffer: &[u8]) -> Result<Range<usize>> {
    let start = find_jumbf_superbox(buffer)?;
    let len = declared_store_len(&buffer[start..])?;
    let end = start
        .checked_add(len)
        .filter(|end| *end <= buffer.len())
        .ok_or(OracleError::UnusableSuperboxLength(
            "the declared length runs past the end of the buffer",
        ))?;
    Ok(start..end)
}

/// Splits a composed `ContentProvenanceBox` into its framing and the JUMBF store inside it.
///
/// # Errors
///
/// [`OracleError::NoJumbfSuperbox`] if the box carries no JUMBF superbox — see
/// [`find_jumbf_superbox`].
pub fn split_composed_box(composed: Vec<u8>) -> Result<ComposedBox> {
    let store_offset = find_jumbf_superbox(&composed)?;
    Ok(ComposedBox {
        composed,
        store_offset,
    })
}

/// The total length in bytes the JUMBF superbox at the start of `store` declares for itself.
///
/// This is the bound `gamut-heic`'s locator trims to, so a test can state the length it expects
/// without borrowing gamut's reading of it.
///
/// # The two reserved `LBox` values
///
/// A JUMBF box is a JPEG-family *standard box* — `LBox` (4 bytes, big-endian), `TBox` (4 bytes),
/// then optionally `XLBox`. C2PA 2.4 §8.4.2.3 is the only place the vendored specification writes
/// any of that down, and it writes down only part: defining the C2PA salt, it calls it "a standard
/// box consisting of: a box length (LBox, as a 4-byte big-endian unsigned integer); a box type
/// (TBox, 4-byte big-endian unsigned integer …)". It never mentions `XLBox`, and it states no
/// reserved `LBox` value. The full grammar — including both reserved values — is ISO 19566-5:2023,
/// which is paywalled and **not vendored in this repository**; its procurement is
/// [issue #441](https://github.com/visualcommons/gamut/issues/441). So the two arms below are the
/// convention as it is universally implemented, read against `c2pa-rs`'s behaviour, not a clause
/// this crate can cite:
///
/// * **`LBox == 0`** — the box runs to the end of the file. `store` begins at the superbox's own
///   first byte, so that end is the end of `store`, and the declared length is `store.len()`.
/// * **`LBox == 1`** — the real length is the 8-byte big-endian `XLBox` that follows `TBox`, i.e.
///   `store[8..16]`, and it counts the whole box including that 16-byte header.
///
/// Taking either literally would return 0 or 1 as a length: a wrong span, produced silently, on
/// the side of the differential whose answers are treated as the reference.
///
/// # Lengths shorter than the header they sit in
///
/// A declared length counts the header, so it can never be less than one. **Every** arm is refused
/// on that one rule, the way `gamut_isobmff`'s box reader refuses `size < header_size`:
///
/// * `LBox` in 2..=7, against the 8-byte header;
/// * `XLBox` below 16, against the 16-byte one;
/// * `LBox == 0` in a buffer of fewer than 8 bytes — the to-end-of-buffer length is still a length
///   counting the 8-byte header, so a 4-to-7-byte buffer declares a box shorter than its own
///   header just as literally as an `LBox` of 7 does.
///
/// Accepting any of them would hand back a span that ends at or before the store's own first body
/// byte — an empty or four-byte "store" — which is exactly the guessed answer this function exists
/// to refuse.
///
/// No store this crate has seen uses either reserved value — c2pa-rs writes a plain 32-bit `LBox`
/// — which is exactly why the handling is here rather than assumed away.
///
/// # Errors
///
/// [`OracleError::UnusableSuperboxLength`] when the `LBox`/`XLBox` fields are truncated, or when
/// the declared length is shorter than the header it counts.
pub fn declared_store_len(store: &[u8]) -> Result<usize> {
    // Both widths reach `usize` losslessly, so neither conversion below is fallible and neither
    // needs a runtime arm. This is dev-only host tooling — nothing cross-compiles it — so the
    // assumption is pinned here at compile time, where an error message about "this platform's
    // usize" would only be defending something that cannot happen.
    const _: () = assert!(usize::BITS >= u64::BITS);

    let field: [u8; 4] = store
        .get(..4)
        .and_then(|field| field.try_into().ok())
        .ok_or(OracleError::UnusableSuperboxLength(
            "the LBox field is truncated",
        ))?;

    match u32::from_be_bytes(field) {
        0 => match store.len() {
            0..=7 => Err(OracleError::UnusableSuperboxLength(
                "LBox is 0 but the buffer ends inside the LBox+TBox header it would count",
            )),
            to_end_of_buffer => Ok(to_end_of_buffer),
        },
        1 => match u64::from_be_bytes(
            store
                .get(8..16)
                .and_then(|field| <[u8; 8]>::try_from(field).ok())
                .ok_or(OracleError::UnusableSuperboxLength(
                    "LBox is 1 but the XLBox field that carries the length is truncated",
                ))?,
        ) {
            0..=15 => Err(OracleError::UnusableSuperboxLength(
                "XLBox is below 16, shorter than the LBox+TBox+XLBox header it is part of",
            )),
            xlbox => Ok(xlbox as usize),
        },
        2..=7 => Err(OracleError::UnusableSuperboxLength(
            "LBox is between 2 and 7, shorter than the LBox+TBox header it is part of",
        )),
        lbox => Ok(lbox as usize),
    }
}

/// What [`reserve_then_fill`] produced.
#[derive(Debug, Clone)]
pub struct Filled {
    /// The asset with the signed store patched into its reserved slot.
    pub asset: Vec<u8>,
    /// The byte range the caller reserved, and where the store was written.
    pub slot: Range<usize>,
    /// The signed store, exactly as patched in.
    pub store: Vec<u8>,
    /// The store length `Builder::placeholder` asked the caller to reserve, before signing.
    ///
    /// Equal to `store.len()`: `sign_embeddable` zero-pads the signed JUMBF back to the length it
    /// pinned when the placeholder was made, so the patch cannot move a byte. A test asserts the
    /// equality rather than trusting it.
    pub placeholder_store_len: usize,
}

/// Direction 1: completes a store for an asset **whose bytes a gamut encoder produced**, without
/// c2pa-rs writing a single byte of the container.
///
/// This is c2pa-rs's own placeholder workflow, which is reserve-then-fill by another name:
///
/// 1. `Builder::placeholder` sizes the composed box and pins the JUMBF length internally;
/// 2. `reserve` is handed that store length, reserves a slot of exactly that size, and returns the
///    finished asset with the byte range the slot occupies — the shape
///    `gamut_avif::AvifEncoder::encode_with_report` already has;
/// 3. `Builder::update_hash_from_stream` computes the `c2pa.hash.bmff.v3` binding over the
///    finished asset;
/// 4. `Builder::sign_embeddable` signs and zero-pads back to the pinned length, so the store the
///    caller patches in is exactly as long as the slot and nothing after it moves.
///
/// The hard binding is computed over the asset *as reserved* — an all-zero slot. That is sound
/// because a BMFF asset's binding excludes the `ContentProvenanceBox` by box path (C2PA 2.4 §18.6,
/// §A.5.6), so the slot's contents are outside the digest; only its *size* matters, and the size
/// does not change.
///
/// # Errors
///
/// [`OracleError::C2pa`] if any c2pa-rs step fails, [`OracleError::NoJumbfSuperbox`] if a composed
/// box carries no store, and whatever `reserve` returns.
pub fn reserve_then_fill(
    format: &str,
    mut reserve: impl FnMut(usize) -> Result<(Vec<u8>, Range<usize>)>,
) -> Result<Filled> {
    let mut builder = manifest_builder()?;
    let placeholder = split_composed_box(builder.placeholder(format)?)?;
    let placeholder_store_len = placeholder.store().len();

    let (mut asset, slot) = reserve(placeholder_store_len)?;

    builder.update_hash_from_stream(format, &mut Cursor::new(asset.clone()))?;
    let store = split_composed_box(builder.sign_embeddable(format)?)?
        .store()
        .to_vec();

    if store.len() != slot.len() {
        return Err(OracleError::Asset(format!(
            "signed store is {} bytes but the reserved slot is {}",
            store.len(),
            slot.len()
        )));
    }
    asset[slot.clone()].copy_from_slice(&store);

    Ok(Filled {
        asset,
        slot,
        store,
        placeholder_store_len,
    })
}

/// Direction 2: lets c2pa-rs embed a store into `asset` itself, choosing the placement.
///
/// # Errors
///
/// [`OracleError::C2pa`] if the manifest cannot be built, signed or embedded.
pub fn embed(format: &str, asset: &[u8]) -> Result<Vec<u8>> {
    let mut builder = manifest_builder()?;
    let mut source = Cursor::new(asset.to_vec());
    let mut dest = Cursor::new(Vec::new());
    builder.save_to_stream(format, &mut source, &mut dest)?;
    Ok(dest.into_inner())
}

/// c2pa-rs's verdict on an asset that carries its own store.
///
/// # Errors
///
/// [`OracleError::C2pa`] — notably a stringified [`c2pa::Error::JumbfNotFound`] when the asset
/// carries no store at all. That is a *different* outcome from a store that fails to validate, and
/// the no-copy-forward test turns on the difference; use [`is_jumbf_not_found`] to tell them
/// apart.
pub fn read(format: &str, asset: &[u8]) -> Result<ValidationState> {
    let context = signing_context()?;
    Ok(Reader::from_context(context)
        .with_stream(format, Cursor::new(asset.to_vec()))?
        .validation_state())
}

/// c2pa-rs's verdict on `asset` when the store is supplied **out of band** — the bytes a gamut
/// locator reported, handed back as if they were a sidecar.
///
/// This is the sharpest form of direction 2. The hard binding digests the asset, and the store
/// carries its own JUMBF length, so a span that starts one byte early or late does not parse and a
/// span cut short fails its own length check: only the range c2pa-rs actually embedded validates
/// here.
///
/// # Errors
///
/// [`OracleError::C2pa`] if the store does not parse, or does not bind to the asset.
pub fn read_with_external_store(
    format: &str,
    store: &[u8],
    asset: &[u8],
) -> Result<ValidationState> {
    let context = signing_context()?;
    Ok(Reader::from_context(context)
        .with_manifest_data_and_stream(store, format, Cursor::new(asset.to_vec()))?
        .validation_state())
}

/// Whether an error is c2pa-rs reporting that the asset carries **no manifest at all**, as opposed
/// to carrying one that fails to validate.
///
/// The distinction is the whole point of the no-copy-forward claim: a derivative must read back as
/// *unsigned*, not as *invalid*. A file whose store was copied forward across a re-encode would
/// still be found and would then fail its hard binding, which is a different error entirely.
#[must_use]
pub fn is_jumbf_not_found(error: &OracleError) -> bool {
    matches!(error, OracleError::C2pa(c2pa::Error::JumbfNotFound))
}

#[cfg(test)]
mod tests {
    //! The JUMBF header reading this crate does *not* borrow from gamut, on the inputs c2pa-rs
    //! never produces. Everything c2pa-rs does produce is pinned in `tests/` against c2pa-rs
    //! itself; these are the arms an oracle has to get right before it is pointed at a container
    //! whose store follows arbitrary bytes.
    //!
    //! # Every arm, enumerated once
    //!
    //! Three rounds of review each found one more untested arm here, because each round looked at
    //! the arm the last one had missed rather than at the set. So the set is written down. Every
    //! branch that can refuse an input across the three header-reading functions is listed below
    //! with the test that pins each of its two directions — the input it refuses, and the nearest
    //! input it must *not* refuse. Adding a branch means adding a row, and a row with one side
    //! blank is the finding, not a matter of taste.
    //!
    //! | Function | Branch | Refuses | Accepts |
    //! |---|---|---|---|
    //! | [`find_jumbf_superbox`] | `.skip(4)`: a type offset below 4 has no `LBox` in front of it | [`a_jumb_three_bytes_in_is_too_early_to_carry_an_lbox`] | [`a_superbox_whose_lbox_opens_the_buffer_is_found_at_offset_zero`] |
    //! | [`find_jumbf_superbox`] | `.ok_or`: no `jumb` in the buffer at all | [`a_buffer_carrying_no_jumb_at_all_is_an_absent_superbox`] | [`the_search_continues_past_a_jumb_too_early_to_carry_an_lbox`] |
    //! | [`declared_store_len`] | `get(..4)`: the `LBox` field is truncated | [`a_buffer_too_short_for_an_lbox_field_is_refused_as_a_truncated_field`] | [`an_lbox_of_zero_in_a_buffer_shorter_than_the_header_is_refused`] |
    //! | [`declared_store_len`] | `LBox == 0` in a buffer below the 8-byte header | [`an_lbox_of_zero_in_a_buffer_shorter_than_the_header_is_refused`] | [`an_lbox_of_zero_in_a_buffer_of_exactly_the_header_size_is_a_length`] |
    //! | [`declared_store_len`] | `LBox == 1`: `get(8..16)`, the `XLBox` field is truncated | [`an_lbox_of_one_without_room_for_an_xlbox_is_refused`] | [`an_lbox_of_one_in_a_buffer_of_exactly_the_sixteen_byte_header_is_a_length`] |
    //! | [`declared_store_len`] | `XLBox` below the 16-byte header it counts | [`an_xlbox_below_the_sixteen_byte_header_is_refused_rather_than_resolved`] | [`an_xlbox_of_exactly_the_header_size_is_a_length`] |
    //! | [`declared_store_len`] | `LBox` in 2..=7, below the 8-byte header it counts | [`an_lbox_between_two_and_seven_is_refused_rather_than_resolved`] | [`an_lbox_of_exactly_the_header_size_is_a_length`] |
    //! | [`jumbf_superbox_span`] | `checked_add`: offset plus declared length leaves `usize` | [`a_declared_length_that_overflows_the_buffer_offset_is_an_unusable_length`] | [`a_span_ending_exactly_at_the_end_of_the_buffer_is_a_length`] |
    //! | [`jumbf_superbox_span`] | `end <= buffer.len()`: the length runs past the buffer | [`a_declared_length_running_past_the_buffer_is_an_unusable_length_not_an_absent_superbox`] | [`a_span_ending_exactly_at_the_end_of_the_buffer_is_a_length`] |
    //!
    //! Two rows share an "accepts" column deliberately: the same input is the nearest non-refused
    //! one for both, and splitting it would only give the second row a fixture that differs in a
    //! byte neither branch reads.
    //!
    //! Two arms of [`declared_store_len`] refuse nothing and so appear in no row — the reserved
    //! `LBox` values, which resolve to a length rather than rejecting it. They are pinned by
    //! [`an_lbox_of_zero_declares_the_rest_of_the_buffer`] and
    //! [`an_lbox_of_one_takes_its_length_from_the_xlbox_field`], which are about *not* taking a
    //! reserved value literally rather than about a boundary.
    //!
    //! The refusal outside this layer — `reserve_then_fill` rejecting a slot that is not the
    //! signed store's length — needs c2pa-rs and a gamut encoder, so it is pinned where those are
    //! in reach: `tests/reserve_then_fill.rs`.
    //!
    //! Every refusal is asserted by the **message** it carries, never by `is_err()`. Several
    //! branches refuse the same input for different reasons — deleting the truncated-`LBox` arm,
    //! for instance, sends a 3-byte buffer into the `LBox == 0` arm, which refuses it too — so
    //! only the message distinguishes the arm that fired from the one that caught the fall.

    use super::{OracleError, declared_store_len, find_jumbf_superbox, jumbf_superbox_span};

    /// A buffer with a decoy `jumb` at offset **3** — the last offset too early to be a superbox
    /// type, because an `LBox` needs the four bytes in front of it — and a genuine `LBox` + `jumb`
    /// superbox at offset 12.
    ///
    /// The decoy sits *at* the boundary on purpose. A search that considers one window too early
    /// lands on exactly this one, where the offset arithmetic underflows; a decoy placed further
    /// back would leave that off-by-one unobserved, which is what an earlier version of this
    /// fixture did.
    fn decoy_jumb_at_the_boundary_then_a_real_superbox() -> Vec<u8> {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&[0xAA; 3]); // offsets 0..3
        buffer.extend_from_slice(b"jumb"); // offset 3: the last offset with no room for an `LBox`
        buffer.extend_from_slice(&[0xAA; 5]); // filler, to offset 12
        buffer.extend_from_slice(&24u32.to_be_bytes()); // offset 12: the real `LBox`
        buffer.extend_from_slice(b"jumb"); // offset 16: the real `TBox`
        buffer.extend_from_slice(&[0x11; 16]); // the store's body, to `LBox`'s 24 bytes
        buffer
    }

    /// A buffer whose **only** `jumb` sits at offset 3 — one byte too early to be a superbox
    /// type. Nothing here is a store, and the search must say so rather than report an offset it
    /// had to compute by subtracting four from three.
    fn only_a_decoy_jumb_at_the_boundary() -> Vec<u8> {
        let mut buffer = vec![0xAAu8; 3];
        buffer.extend_from_slice(b"jumb"); // offset 3
        buffer.extend_from_slice(&[0xAA; 25]);
        buffer
    }

    /// A minimal superbox that opens the buffer: `LBox` at offset 0, so its `TBox` is at offset 4
    /// — the *first* offset a superbox type can occupy.
    fn superbox_at_the_start_of_the_buffer() -> Vec<u8> {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&24u32.to_be_bytes());
        buffer.extend_from_slice(b"jumb");
        buffer.extend_from_slice(&[0x11; 16]);
        buffer
    }

    /// A `store`-shaped buffer of `len` bytes whose `LBox` is `lbox` and whose `TBox` is `jumb`.
    fn store_with_lbox(lbox: u32, len: usize) -> Vec<u8> {
        let mut store = vec![0u8; len];
        store[..4].copy_from_slice(&lbox.to_be_bytes());
        if len >= 8 {
            store[4..8].copy_from_slice(b"jumb");
        }
        store
    }

    /// A `store`-shaped buffer of `len` bytes declaring `LBox == 1` and the given `XLBox`.
    fn store_with_xlbox(xlbox: u64, len: usize) -> Vec<u8> {
        let mut store = store_with_lbox(1, len);
        store[8..16].copy_from_slice(&xlbox.to_be_bytes());
        store
    }

    #[test]
    fn the_search_continues_past_a_jumb_too_early_to_carry_an_lbox() {
        assert_eq!(
            find_jumbf_superbox(&decoy_jumb_at_the_boundary_then_a_real_superbox())
                .expect("the real superbox"),
            12,
            "a `jumb` with no room for an `LBox` in front of it must be skipped and the search \
             continued, not treated as the end of it"
        );
    }

    #[test]
    fn a_jumb_three_bytes_in_is_too_early_to_carry_an_lbox() {
        // Offset 3 is the last offset a superbox type cannot occupy: its `LBox` would begin one
        // byte before the buffer. A search that considers one window too early lands on exactly
        // this one and computes `3 - 4`, so this input is where an off-by-one is visible at all.
        let buffer = only_a_decoy_jumb_at_the_boundary();

        let error = find_jumbf_superbox(&buffer)
            .expect_err("offset 3 leaves no room for the `LBox` in front of a superbox type");
        assert!(
            matches!(error, OracleError::NoJumbfSuperbox),
            "a `jumb` too early to carry an `LBox` is not a superbox, so a buffer holding only \
             that one holds no store; got {error}"
        );
    }

    #[test]
    fn a_superbox_whose_lbox_opens_the_buffer_is_found_at_offset_zero() {
        // The other side of the same boundary: offset 4 is the *first* offset a superbox type can
        // occupy, and a search that skips one window too many walks straight past this store.
        assert_eq!(
            find_jumbf_superbox(&superbox_at_the_start_of_the_buffer())
                .expect("a superbox whose `LBox` opens the buffer"),
            0,
            "a store at the very start of the buffer has its `TBox` at offset 4, which is the \
             first offset with room for an `LBox`, so it must be found"
        );
    }

    #[test]
    fn a_buffer_carrying_no_jumb_at_all_is_an_absent_superbox() {
        let buffer = vec![0xAAu8; 64];

        let error = find_jumbf_superbox(&buffer).expect_err("there is no superbox to find");
        assert!(
            matches!(error, OracleError::NoJumbfSuperbox),
            "a buffer with no `jumb` in it carries no store, and the refusal must say so rather \
             than name a length or hand back an offset; got {error}"
        );
    }

    #[test]
    fn a_span_is_still_found_when_a_decoy_jumb_precedes_the_superbox() {
        assert_eq!(
            jumbf_superbox_span(&decoy_jumb_at_the_boundary_then_a_real_superbox())
                .expect("the real superbox"),
            12..36,
        );
    }

    #[test]
    fn a_span_ending_exactly_at_the_end_of_the_buffer_is_a_length() {
        // The accepted side of both of `jumbf_superbox_span`'s refusals: the sum stays inside
        // `usize` and the end lands on the last byte. A bound that refused one byte too early
        // would refuse this store, which is the shape every real store has.
        let buffer = superbox_at_the_start_of_the_buffer();
        assert_eq!(buffer.len(), 24, "the declared length is the whole buffer");

        assert_eq!(
            jumbf_superbox_span(&buffer).expect("a store that ends where the buffer does"),
            0..24,
        );
    }

    #[test]
    fn an_lbox_of_zero_declares_the_rest_of_the_buffer() {
        assert_eq!(
            declared_store_len(&store_with_lbox(0, 76))
                .expect("LBox 0 is a length, not a literal zero"),
            76,
            "ISO box syntax reads `LBox = 0` as \"to the end of the file\"; taken literally it \
             would make the store zero bytes long"
        );
    }

    #[test]
    fn an_lbox_of_one_takes_its_length_from_the_xlbox_field() {
        assert_eq!(
            declared_store_len(&store_with_xlbox(40, 40)).expect("LBox 1 defers to XLBox"),
            40,
            "ISO box syntax reads `LBox = 1` as \"the 8-byte XLBox after TBox holds the length\"; \
             taken literally it would make the store one byte long"
        );
    }

    #[test]
    fn a_buffer_too_short_for_an_lbox_field_is_refused_as_a_truncated_field() {
        for len in 0usize..4 {
            let store = vec![0u8; len];

            let error = declared_store_len(&store).expect_err("there is no LBox to read");
            assert!(
                error.to_string().contains("the LBox field is truncated"),
                "a buffer with no room for the `LBox` has no declared length at all, and the \
                 refusal must name the truncated field rather than fall through to an arm reading \
                 a length that was never there; a {len}-byte buffer gave {error}"
            );
        }
    }

    #[test]
    fn an_lbox_of_one_without_room_for_an_xlbox_is_refused() {
        for len in [8usize, 12, 15] {
            let store = store_with_lbox(1, len);

            let error = declared_store_len(&store).expect_err("there is no XLBox to read");
            assert!(
                error
                    .to_string()
                    .contains("XLBox field that carries the length is truncated"),
                "the refusal must name the truncated XLBox rather than any other unusable length; \
                 a {len}-byte buffer gave {error}"
            );
        }
    }

    #[test]
    fn an_lbox_of_one_in_a_buffer_of_exactly_the_sixteen_byte_header_is_a_length() {
        assert_eq!(
            declared_store_len(&store_with_xlbox(16, 16))
                .expect("16 bytes is the LBox+TBox+XLBox header itself"),
            16,
            "the XLBox field ends at byte 16, so a 16-byte buffer holds all of it; refusing this \
             one would refuse the smallest legal extended box"
        );
    }

    #[test]
    fn an_xlbox_below_the_sixteen_byte_header_is_refused_rather_than_resolved() {
        for xlbox in [0u64, 1, 8, 15] {
            let error = declared_store_len(&store_with_xlbox(xlbox, 40))
                .expect_err("an XLBox below 16 is shorter than the header it counts");
            assert!(
                error
                    .to_string()
                    .contains("shorter than the LBox+TBox+XLBox header"),
                "the refusal must name the undersized XLBox rather than any other unusable \
                 length; XLBox {xlbox} gave {error}"
            );
        }
    }

    #[test]
    fn an_xlbox_of_exactly_the_header_size_is_a_length() {
        assert_eq!(
            declared_store_len(&store_with_xlbox(16, 40))
                .expect("16 is the header itself, the smallest legal box"),
            16,
            "the refusal must stop exactly at the header size, not swallow the first legal length"
        );
    }

    #[test]
    fn an_lbox_between_two_and_seven_is_refused_rather_than_resolved() {
        for lbox in [2u32, 3, 4, 5, 6, 7] {
            let error = declared_store_len(&store_with_lbox(lbox, 32))
                .expect_err("an LBox below 8 is shorter than the header it counts");
            assert!(
                error
                    .to_string()
                    .contains("shorter than the LBox+TBox header"),
                "the refusal must name the undersized LBox rather than any other unusable length; \
                 LBox {lbox} gave {error}"
            );
        }
    }

    #[test]
    fn an_lbox_of_exactly_the_header_size_is_a_length() {
        assert_eq!(
            declared_store_len(&store_with_lbox(8, 32))
                .expect("8 is the header itself, the smallest legal box"),
            8,
            "the refusal must stop exactly at the header size, not swallow the first legal length"
        );
    }

    #[test]
    fn an_lbox_of_zero_in_a_buffer_shorter_than_the_header_is_refused() {
        for len in [4usize, 5, 6, 7] {
            let store = vec![0u8; len];

            let error = declared_store_len(&store)
                .expect_err("the bytes to the end of the buffer do not reach the header itself");
            assert!(
                error
                    .to_string()
                    .contains("the buffer ends inside the LBox+TBox header"),
                "the to-end-of-buffer length obeys the same header minimum as the other arms, and \
                 the refusal must name it — which also says the `LBox` field itself was read, \
                 since four bytes is enough for it; a {len}-byte buffer gave {error}"
            );
        }
    }

    #[test]
    fn an_lbox_of_zero_in_a_buffer_of_exactly_the_header_size_is_a_length() {
        assert_eq!(
            declared_store_len(&store_with_lbox(0, 8))
                .expect("8 bytes is the header itself, the smallest legal box"),
            8,
            "the refusal must stop exactly at the header size, not swallow the first legal length"
        );
    }

    #[test]
    fn a_declared_length_running_past_the_buffer_is_an_unusable_length_not_an_absent_superbox() {
        let mut buffer = vec![0u8; 32];
        buffer[..4].copy_from_slice(&4096u32.to_be_bytes());
        buffer[4..8].copy_from_slice(b"jumb");

        let error = jumbf_superbox_span(&buffer)
            .expect_err("a length that runs off the end of the buffer bounds nothing");
        assert!(
            matches!(&error, OracleError::UnusableSuperboxLength(what)
                if what.contains("runs past the end of the buffer")),
            "the buffer plainly carries a superbox, so the refusal must name its unusable length \
             rather than report the store as absent; got {error}"
        );
    }

    #[test]
    fn a_declared_length_that_overflows_the_buffer_offset_is_an_unusable_length() {
        // The `XLBox` arm is the only one that can return a length near `usize::MAX`, and the
        // superbox starts 12 bytes in, so the sum leaves `usize` entirely. Adding without the
        // overflow check would wrap to an offset *inside* the buffer and hand back a backwards
        // range — a span that passes the bound check by arithmetic accident.
        let mut buffer = vec![0xAAu8; 12];
        buffer.extend_from_slice(&1u32.to_be_bytes());
        buffer.extend_from_slice(b"jumb");
        buffer.extend_from_slice(&u64::MAX.to_be_bytes());
        buffer.resize(64, 0);

        let error = jumbf_superbox_span(&buffer)
            .expect_err("a length that cannot be added to the offset bounds nothing");
        assert!(
            matches!(&error, OracleError::UnusableSuperboxLength(what)
                if what.contains("runs past the end of the buffer")),
            "a declared length that overflows the offset it is added to runs past every end there \
             is, and must be refused as the unusable length it is; got {error}"
        );
    }
}
