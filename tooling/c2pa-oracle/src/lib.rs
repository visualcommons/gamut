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
    /// A composed `ContentProvenanceBox` carried no JUMBF superbox: no [`JUMBF_SUPERBOX_TYPE`] was
    /// found in it at all. Only [`split_composed_box`] raises this.
    NoJumbfSuperbox,
    /// A JUMBF superbox header is present, but the length it declares cannot be read: its
    /// `LBox`/`XLBox` fields are truncated, `LBox` is one of the values ISO box syntax leaves
    /// undefined (2..=7, all shorter than the 8-byte header they sit in), or the declared length
    /// does not fit this platform's `usize`. Carries which of those it was.
    ///
    /// This exists so the length is never *guessed*. A span silently derived from an
    /// unrepresentable `LBox` would be an oracle handing gamut a wrong answer and calling it a
    /// reference one; see [`declared_store_len`].
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
/// [`OracleError::NoJumbfSuperbox`] if no superbox is found, or if the length it declares runs off
/// the end of `buffer`; [`OracleError::UnusableSuperboxLength`] if that length cannot be read at
/// all (see [`declared_store_len`]).
pub fn jumbf_superbox_span(buffer: &[u8]) -> Result<Range<usize>> {
    let start = find_jumbf_superbox(buffer)?;
    let len = declared_store_len(&buffer[start..])?;
    let end = start
        .checked_add(len)
        .filter(|end| *end <= buffer.len())
        .ok_or(OracleError::NoJumbfSuperbox)?;
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
/// then optionally `XLBox` — and C2PA 2.4 §8.4.2.3 spells that syntax out where it defines the
/// C2PA salt as "a standard box consisting of: a box length (LBox, as a 4-byte big-endian unsigned
/// integer); a box type (TBox, 4-byte big-endian unsigned integer …)". The same syntax reserves two
/// `LBox` values, and both are read here rather than taken at face value:
///
/// * **`LBox == 0`** — the box runs to the end of the file. `store` begins at the superbox's own
///   first byte, so that end is the end of `store`, and the declared length is `store.len()`.
/// * **`LBox == 1`** — the real length is the 8-byte big-endian `XLBox` that follows `TBox`, i.e.
///   `store[8..16]`, and it counts the whole box including that 16-byte header.
///
/// Taking either literally would return 0 or 1 as a length: a wrong span, produced silently, on
/// the side of the differential whose answers are treated as the reference. `LBox` values 2..=7
/// are shorter than the header they sit in and describe no box at all, so they are refused rather
/// than resolved.
///
/// No store this crate has seen uses either reserved value — c2pa-rs writes a plain 32-bit `LBox`
/// — which is exactly why the handling is here rather than assumed away.
///
/// # Errors
///
/// [`OracleError::UnusableSuperboxLength`] when the `LBox`/`XLBox` fields are truncated, when
/// `LBox` is 2..=7, or when the declared length does not fit a `usize`.
pub fn declared_store_len(store: &[u8]) -> Result<usize> {
    let field: [u8; 4] = store
        .get(..4)
        .and_then(|field| field.try_into().ok())
        .ok_or(OracleError::UnusableSuperboxLength(
            "the LBox field is truncated",
        ))?;

    match u32::from_be_bytes(field) {
        0 => Ok(store.len()),
        1 => {
            let field: [u8; 8] = store
                .get(8..16)
                .and_then(|field| field.try_into().ok())
                .ok_or(OracleError::UnusableSuperboxLength(
                    "LBox is 1 but the XLBox field that carries the length is truncated",
                ))?;
            usize::try_from(u64::from_be_bytes(field)).map_err(|_| {
                OracleError::UnusableSuperboxLength("XLBox does not fit this platform's usize")
            })
        }
        2..=7 => Err(OracleError::UnusableSuperboxLength(
            "LBox is between 2 and 7, shorter than the LBox+TBox header it is part of",
        )),
        lbox => usize::try_from(lbox).map_err(|_| {
            OracleError::UnusableSuperboxLength("LBox does not fit this platform's usize")
        }),
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

    use super::{OracleError, declared_store_len, find_jumbf_superbox, jumbf_superbox_span};

    /// A buffer with a stray `jumb` at offset 0 — too early to be a superbox type, since there is
    /// no room for an `LBox` in front of it — and a genuine `LBox` + `jumb` superbox at offset 12.
    fn stray_jumb_then_real_superbox() -> Vec<u8> {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(b"jumb"); // offset 0: too early to be a superbox type
        buffer.extend_from_slice(&[0xAA; 8]); // filler
        buffer.extend_from_slice(&24u32.to_be_bytes()); // offset 12: the real LBox
        buffer.extend_from_slice(b"jumb"); // offset 16: the real TBox
        buffer.extend_from_slice(&[0x11; 16]); // the store's body, to LBox's 24 bytes
        buffer
    }

    #[test]
    fn the_search_continues_past_a_jumb_too_early_to_carry_an_lbox() {
        assert_eq!(
            find_jumbf_superbox(&stray_jumb_then_real_superbox()).expect("the real superbox"),
            12,
            "a `jumb` in the first four bytes has no room for an `LBox` in front of it, so it must \
             be skipped and the search continued, not treated as the end of it"
        );
    }

    #[test]
    fn a_span_is_still_found_when_a_stray_jumb_precedes_the_superbox() {
        assert_eq!(
            jumbf_superbox_span(&stray_jumb_then_real_superbox()).expect("the real superbox"),
            12..36,
        );
    }

    #[test]
    fn an_lbox_of_zero_declares_the_rest_of_the_buffer() {
        let mut store = vec![0u8; 76];
        store[..4].copy_from_slice(&0u32.to_be_bytes());
        store[4..8].copy_from_slice(b"jumb");

        assert_eq!(
            declared_store_len(&store).expect("LBox 0 is a length, not a literal zero"),
            76,
            "ISO box syntax reads `LBox = 0` as \"to the end of the file\"; taken literally it \
             would make the store zero bytes long"
        );
    }

    #[test]
    fn an_lbox_of_one_takes_its_length_from_the_xlbox_field() {
        let mut store = vec![0u8; 40];
        store[..4].copy_from_slice(&1u32.to_be_bytes());
        store[4..8].copy_from_slice(b"jumb");
        store[8..16].copy_from_slice(&40u64.to_be_bytes());

        assert_eq!(
            declared_store_len(&store).expect("LBox 1 defers to XLBox"),
            40,
            "ISO box syntax reads `LBox = 1` as \"the 8-byte XLBox after TBox holds the length\"; \
             taken literally it would make the store one byte long"
        );
    }

    #[test]
    fn an_lbox_of_one_without_room_for_an_xlbox_is_refused() {
        let mut store = vec![0u8; 12];
        store[..4].copy_from_slice(&1u32.to_be_bytes());
        store[4..8].copy_from_slice(b"jumb");

        let error = declared_store_len(&store).expect_err("there is no XLBox to read");
        assert!(
            error
                .to_string()
                .contains("XLBox field that carries the length is truncated"),
            "the refusal must name the truncated XLBox rather than any other unusable length; got \
             {error}"
        );
    }

    #[test]
    fn an_lbox_between_two_and_seven_is_refused_rather_than_resolved() {
        let mut store = vec![0u8; 32];
        store[..4].copy_from_slice(&7u32.to_be_bytes());
        store[4..8].copy_from_slice(b"jumb");

        let error = declared_store_len(&store).expect_err("7 is shorter than the header itself");
        assert!(
            error
                .to_string()
                .contains("shorter than the LBox+TBox header"),
            "the refusal must name the undersized LBox rather than any other unusable length; got \
             {error}"
        );
    }

    #[test]
    fn a_declared_length_running_past_the_buffer_is_not_a_span() {
        let mut buffer = vec![0u8; 32];
        buffer[..4].copy_from_slice(&4096u32.to_be_bytes());
        buffer[4..8].copy_from_slice(b"jumb");

        assert!(
            matches!(
                jumbf_superbox_span(&buffer),
                Err(OracleError::NoJumbfSuperbox)
            ),
            "a length that runs off the end of the buffer bounds nothing"
        );
    }
}
