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
    /// A gamut crate refused to produce or read the asset the oracle asked for. Raised only by a
    /// caller's closure, never by this crate.
    Asset(String),
    /// A composed `ContentProvenanceBox` carried no JUMBF superbox: no [`JUMBF_SUPERBOX_TYPE`] was
    /// found in it at all. Only [`split_composed_box`] raises this.
    NoJumbfSuperbox,
}

impl std::fmt::Display for OracleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::C2pa(error) => write!(f, "c2pa-rs: {error}"),
            Self::Asset(message) => write!(f, "asset: {message}"),
            Self::NoJumbfSuperbox => {
                f.write_str("composed ContentProvenanceBox carries no JUMBF superbox")
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
/// # Errors
///
/// [`OracleError::NoJumbfSuperbox`] if no `jumb` box type appears at or after offset 4.
pub fn find_jumbf_superbox(buffer: &[u8]) -> Result<usize> {
    buffer
        .windows(JUMBF_SUPERBOX_TYPE.len())
        .position(|window| window == JUMBF_SUPERBOX_TYPE)
        .filter(|type_offset| *type_offset >= 4)
        .map(|type_offset| type_offset - 4)
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
/// the end of `buffer`.
pub fn jumbf_superbox_span(buffer: &[u8]) -> Result<Range<usize>> {
    let start = find_jumbf_superbox(buffer)?;
    let len = declared_store_len(&buffer[start..]).ok_or(OracleError::NoJumbfSuperbox)?;
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

/// The outer JUMBF `LBox` a store declares for itself: its length in bytes, big-endian, read from
/// the store's own first four bytes. `None` when `store` is shorter than that field.
///
/// This is the bound `gamut-heic`'s locator trims to, so a test can state the length it expects
/// without borrowing gamut's reading of it.
#[must_use]
pub fn declared_store_len(store: &[u8]) -> Option<usize> {
    let field: [u8; 4] = store.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(field) as usize)
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
