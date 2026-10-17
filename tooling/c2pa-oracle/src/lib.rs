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

use c2pa::{Builder, Context, EphemeralSigner, HashRange, Reader, Settings, ValidationState};

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

/// The MIME type gamut-png's output is handed to c2pa-rs under.
pub const PNG_MIME: &str = "image/png";

/// The MIME type gamut-webp's output is handed to c2pa-rs under — c2pa-rs's RIFF handler.
pub const WEBP_MIME: &str = "image/webp";

/// The MIME type gamut-tiff's output is handed to c2pa-rs under.
pub const TIFF_MIME: &str = "image/tiff";

/// The MIME type gamut-dng's output is handed to c2pa-rs under — the same TIFF handler as
/// [`TIFF_MIME`], which c2pa-rs registers for DNG too.
pub const DNG_MIME: &str = "image/dng";

/// The format [`reserve_then_fill_data_hashed`] asks c2pa-rs to *compose* its store for: a bare
/// `.c2pa` manifest store, whose composition is the identity.
///
/// Every container the data-hashed driver serves writes its own framing around the store — a PNG
/// `caBX` chunk header and CRC, a RIFF chunk header, a TIFF tag value — so what the oracle wants
/// back from c2pa-rs is the raw JUMBF and nothing else. Asking for it under the asset's own MIME
/// type would not work uniformly: c2pa-rs wraps a PNG store in a whole chunk, and its RIFF handler
/// has no composer at all, so `Builder::placeholder("image/webp")` is refused as an unsupported
/// type. The *binding* is still computed under the asset's own MIME type; only the wrapping of the
/// returned bytes uses this one.
pub const RAW_JUMBF_FORMAT: &str = "application/c2pa";

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
    ///
    /// # Panics
    ///
    /// If [`store_offset`](Self::store_offset) is past the end of [`composed`](Self::composed).
    /// [`split_composed_box`] cannot produce such a value — it derives the offset by finding a
    /// superbox type *inside* the buffer — so this is reachable only by building the struct
    /// literally, which both fields being `pub` permits.
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
    /// The byte range the caller reserved. The store was written at its start; under
    /// [`reserve_then_fill_padded`] the rest of it is zeros.
    pub slot: Range<usize>,
    /// The signed store, exactly as patched in — never including any padding after it.
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
    reserve: impl FnMut(usize) -> Result<(Vec<u8>, Range<usize>)>,
) -> Result<Filled> {
    reserve_then_fill_padded(format, 0, reserve)
}

/// [`reserve_then_fill`] for a signer that reserves **generously**: the slot is `padding` bytes
/// longer than the store `Builder::placeholder` pins, and the remainder after the signed store is
/// zero-filled.
///
/// This is the one shape in which gamut's two locators legitimately diverge, and nothing else in
/// this crate produces it — c2pa-rs's own embedder sizes the box to the store exactly. The store's
/// JUMBF `LBox` still declares the store alone, so a reader bounding by that `LBox`
/// (`gamut-heic`) reports `slot.start..slot.start + store.len()`, while a reader bounding by the
/// enclosing `ContentProvenanceBox` (`gamut-avif`) reports the whole `slot`, padding included.
///
/// `reserve` is handed `placeholder store length + padding`, and must return a slot of exactly that
/// length; `padding == 0` is [`reserve_then_fill`]. The padding is written as zeros explicitly
/// rather than inherited from whatever the encoder left in the slot, so the claim "zero-padded" is
/// this function's, not the encoder's.
///
/// The hard binding is unaffected for the same reason it is in [`reserve_then_fill`]: the BMFF
/// binding excludes the `ContentProvenanceBox` by box path, so neither the store nor the padding
/// after it is digested — only the box's size is, and it does not change.
///
/// # Errors
///
/// As [`reserve_then_fill`]; [`OracleError::Asset`] if the slot `reserve` returned is not exactly
/// `padding` bytes longer than the signed store, naming both lengths and the padding asked for.
/// That includes a `padding` large enough to overflow the requested length.
pub fn reserve_then_fill_padded(
    format: &str,
    padding: usize,
    mut reserve: impl FnMut(usize) -> Result<(Vec<u8>, Range<usize>)>,
) -> Result<Filled> {
    let mut builder = manifest_builder()?;
    let placeholder = split_composed_box(builder.placeholder(format)?)?;
    let placeholder_store_len = placeholder.store().len();

    let reserved_len = placeholder_store_len.checked_add(padding).ok_or_else(|| {
        OracleError::Asset(format!(
            "a {placeholder_store_len}-byte store plus {padding} bytes of padding overflows usize"
        ))
    })?;
    let (mut asset, slot) = reserve(reserved_len)?;

    builder.update_hash_from_stream(format, &mut Cursor::new(asset.clone()))?;
    let store = split_composed_box(builder.sign_embeddable(format)?)?
        .store()
        .to_vec();

    // `slot.len() - padding` cannot underflow-panic: a slot shorter than `padding` is caught by
    // `checked_sub` and reported through the same refusal as any other mismatch.
    if slot.len().checked_sub(padding) != Some(store.len()) {
        return Err(OracleError::Asset(format!(
            "signed store is {} bytes but the reserved slot is {} (padding asked for: {padding})",
            store.len(),
            slot.len()
        )));
    }
    let (store_bytes, padding_bytes) = asset[slot.clone()].split_at_mut(store.len());
    store_bytes.copy_from_slice(&store);
    padding_bytes.fill(0);

    Ok(Filled {
        asset,
        slot,
        store,
        placeholder_store_len,
    })
}

/// What [`reserve_then_fill_data_hashed`] produced.
#[derive(Debug, Clone)]
pub struct DataHashFilled {
    /// The asset with the signed store written into its reservation by the caller's `fill`.
    pub asset: Vec<u8>,
    /// The byte ranges the `c2pa.hash.data` binding was computed around — exactly the ones the
    /// gamut crate reported for the reservation, in the order it reported them.
    pub exclusions: Vec<Range<usize>>,
    /// The signed store, raw JUMBF, exactly as handed to `fill`.
    pub store: Vec<u8>,
    /// The store length `Builder::placeholder` asked the caller to reserve, before signing. Equal
    /// to `store.len()`, or the driver refuses before `fill` runs.
    pub placeholder_store_len: usize,
}

/// Direction 1 for a container bound by **`c2pa.hash.data`** — PNG, RIFF/WebP, TIFF and DNG —
/// rather than by `c2pa.hash.bmff.v3`.
///
/// The BMFF driver, [`reserve_then_fill`], can let c2pa-rs work out the exclusion itself, because
/// a BMFF binding excludes the `ContentProvenanceBox` *by box path*. A data hash excludes *byte
/// ranges*, and nothing but the container knows which bytes those are. So here the ranges are the
/// gamut crate's own report, handed to `Builder::set_data_hash_exclusions` unchanged, and the
/// exclusion set is therefore itself under test: a range that misses a byte the fill rewrites —
/// a PNG chunk CRC, a TIFF `count` field — leaves that byte digested, and the filled asset stops
/// validating.
///
/// 1. `Builder::placeholder` under [`RAW_JUMBF_FORMAT`] pins the store length;
/// 2. `reserve` is handed that length and returns the finished asset, the exclusion ranges the
///    gamut encoder reported for its reservation, and whatever `fill` will need to find the
///    reservation again (`T`: the PNG `C2paSpan`, the TIFF store range, nothing for WebP);
/// 3. those ranges become the binding's exclusions, and `Builder::update_hash_from_stream`
///    digests the asset *as reserved* under `format`;
/// 4. `Builder::sign_embeddable` signs and zero-pads back to the pinned length;
/// 5. `fill` writes the store into the reservation by the container's own route — `fill_c2pa`
///    for PNG, a same-length re-encode for WebP, an in-place copy for TIFF and DNG.
///
/// # Errors
///
/// [`OracleError::C2pa`] if any c2pa-rs step fails, [`OracleError::NoJumbfSuperbox`] if a composed
/// store carries no superbox, [`OracleError::Asset`] if the signed store is not the length the
/// placeholder pinned — checked before `fill` is called, so no caller ever writes a mis-sized
/// store — and whatever `reserve` or `fill` return.
pub fn reserve_then_fill_data_hashed<T>(
    format: &str,
    reserve: impl FnOnce(usize) -> Result<(Vec<u8>, Vec<Range<usize>>, T)>,
    fill: impl FnOnce(Vec<u8>, T, &[u8]) -> Result<Vec<u8>>,
) -> Result<DataHashFilled> {
    let mut builder = manifest_builder()?;
    let placeholder_store_len = split_composed_box(builder.placeholder(RAW_JUMBF_FORMAT)?)?
        .store()
        .len();

    let (asset, exclusions, reservation) = reserve(placeholder_store_len)?;
    builder.set_data_hash_exclusions(
        exclusions
            .iter()
            .map(|range| HashRange::new(range.start as u64, range.len() as u64))
            .collect(),
    )?;
    builder.update_hash_from_stream(format, &mut Cursor::new(asset.clone()))?;
    let store = split_composed_box(builder.sign_embeddable(RAW_JUMBF_FORMAT)?)?
        .store()
        .to_vec();

    if store.len() != placeholder_store_len {
        return Err(OracleError::Asset(format!(
            "signed store is {} bytes but the placeholder pinned {placeholder_store_len}",
            store.len()
        )));
    }
    let asset = fill(asset, reservation, &store)?;

    Ok(DataHashFilled {
        asset,
        exclusions,
        store,
        placeholder_store_len,
    })
}

/// The byte ranges the `c2pa.hash.data` assertion of `asset`'s active manifest excludes, in the
/// order the assertion lists them — what c2pa-rs itself chose to exclude when it embedded the
/// store, read back through its own `Reader::detailed_json`.
///
/// This is the only route c2pa-rs offers: its `Manifest` deliberately drops hard-binding
/// assertions when it reads a store (`src/manifest.rs:604-608`), so the exclusions are reachable
/// only through the detailed report, which carries the claim's whole assertion store.
///
/// # Errors
///
/// [`OracleError::C2pa`] if c2pa-rs cannot read the asset; [`OracleError::Asset`] if the detailed
/// report names no active manifest, carries no `c2pa.hash.data` assertion with an `exclusions`
/// array, or lists an exclusion whose `start`/`length` is not an unsigned integer.
pub fn data_hash_exclusions(format: &str, asset: &[u8]) -> Result<Vec<Range<usize>>> {
    let reader = Reader::from_context(signing_context()?)
        .with_stream(format, Cursor::new(asset.to_vec()))?;
    let report: serde_json::Value = serde_json::from_str(&reader.detailed_json())
        .map_err(|error| OracleError::Asset(format!("detailed report is not JSON: {error}")))?;

    let active = report["active_manifest"]
        .as_str()
        .ok_or_else(|| OracleError::Asset("the detailed report names no active manifest".into()))?;
    // A JSON pointer escapes only `~` and `/`, which a `urn:c2pa:` label does not contain.
    let pointer = format!("/manifests/{active}/assertion_store/c2pa.hash.data/exclusions");
    let exclusions = report
        .pointer(&pointer)
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            OracleError::Asset("the active manifest carries no c2pa.hash.data exclusions".into())
        })?;

    exclusions
        .iter()
        .map(|exclusion| {
            let field = |name: &str| {
                exclusion[name]
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| {
                        OracleError::Asset(format!("an exclusion's `{name}` is not a usize"))
                    })
            };
            let start = field("start")?;
            Ok(start..start + field("length")?)
        })
        .collect()
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
    //! # Every branch, enumerated once
    //!
    //! Three rounds of review each found one more untested arm here, because each round looked at
    //! the arm the last one had missed rather than at the set. So the set is written down — as a
    //! **named list**, not as a rule to derive membership from. The table covers every
    //! discriminating branch, every point where the code chooses between two answers, in exactly
    //! these four functions: [`find_jumbf_superbox`], [`declared_store_len`],
    //! [`jumbf_superbox_span`] and [`is_jumbf_not_found`]. Each row names the test pinning each
    //! of its two directions. Adding a branch to one of those four means adding a row, and a row
    //! with one side blank is the finding, not a matter of taste.
    //!
    //! The scope was narrower once: only branches that could *refuse* an input. That boundary
    //! excluded exactly the predicates whose whole job is telling two cases apart —
    //! [`is_jumbf_not_found`] sat outside it and had one direction unpinned, which is the same
    //! blank-side shape the table exists to make visible. Widening it to discrimination is what
    //! made the boundary undrawable in prose, though: [`is_jumbf_not_found`] discriminates
    //! between two answers about an *error*, not about a buffer, so no wording about buffers
    //! covers the row the widening was made for. Hence the list. It claims to be complete over
    //! four named functions and nothing wider.
    //!
    //! Three kinds of discriminating branch in this file sit outside it, named here so their
    //! absence is a decision and not an oversight:
    //!
    //! * The data-hashed driver and its reader (issue #534):
    //!   `reserve_then_fill_data_hashed`'s refusal of a signed store that is not the
    //!   placeholder's length, and [`data_hash_exclusions`]'s walk of c2pa-rs's detailed report.
    //!   Both are reached only through c2pa-rs signing or reading a real asset, so — like the
    //!   padded driver's below — what keeps them out is reach: the refusal is pinned in
    //!   `tests/png.rs`, and the walk's accepting side in every exclusion comparison of the
    //!   per-container test files, `tests/png.rs` first. Its refusing sides — no
    //!   active manifest, no `c2pa.hash.data`, a non-integer bound — are not pinned: every asset
    //!   those files hand it carries a data hash c2pa-rs wrote itself.
    //! * [`reserve_then_fill_padded`]'s two refusals — the `checked_add` of store length and
    //!   padding, and the `slot.len() - padding != store.len()` guard (which [`reserve_then_fill`]
    //!   reaches with `padding == 0`). They discriminate exactly as a row does; what separates
    //!   them is *reach*, not subject — pinning them needs c2pa-rs and a gamut encoder in scope,
    //!   so their tests live in `tests/reserve_then_fill.rs`. A row could
    //!   not point there anyway: [`every_test_the_enumeration_names_exists_in_this_file`] resolves
    //!   every name a row links against *this* file's definitions.
    //! * `Display for OracleError`'s match over the variants. It chooses the *wording* for an
    //!   outcome some other branch already decided, so it discriminates nothing about an input.
    //!   Every refusal test below asserts on the message it renders, so it is exercised
    //!   throughout without a row of its own.
    //!
    //! *Taken* is the input that reaches the branch; *not taken* is the nearest input that does
    //! not. For a branch that refuses, those are the input it refuses and the nearest input it
    //! must **not** refuse.
    //!
    //! | Function | Branch | Taken | Not taken |
    //! |---|---|---|---|
    //! | [`find_jumbf_superbox`] | `.skip(4)`: a type offset below 4 has no `LBox` in front of it | [`a_jumb_three_bytes_in_is_too_early_to_carry_an_lbox`] | [`a_superbox_whose_lbox_opens_the_buffer_is_found_at_offset_zero`] |
    //! | [`find_jumbf_superbox`] | `.ok_or`: no `jumb` in the buffer at all | [`a_buffer_carrying_no_jumb_at_all_is_an_absent_superbox`] | [`the_search_continues_past_a_jumb_too_early_to_carry_an_lbox`] |
    //! | [`declared_store_len`] | `get(..4)`: the `LBox` field is truncated | [`a_buffer_too_short_for_an_lbox_field_is_refused_as_a_truncated_field`] | [`an_lbox_of_zero_in_a_buffer_shorter_than_the_header_is_refused`] |
    //! | [`declared_store_len`] | `LBox == 0`: the reserved to-end-of-buffer value, not a literal zero | [`an_lbox_of_zero_declares_the_rest_of_the_buffer`] | [`an_lbox_of_exactly_the_header_size_is_a_length`] |
    //! | [`declared_store_len`] | `LBox == 0` in a buffer below the 8-byte header | [`an_lbox_of_zero_in_a_buffer_shorter_than_the_header_is_refused`] | [`an_lbox_of_zero_in_a_buffer_of_exactly_the_header_size_is_a_length`] |
    //! | [`declared_store_len`] | `LBox == 1`: the reserved defer-to-`XLBox` value, not a literal one | [`an_lbox_of_one_takes_its_length_from_the_xlbox_field`] | [`an_lbox_of_exactly_the_header_size_is_a_length`] |
    //! | [`declared_store_len`] | `LBox == 1`: `get(8..16)`, the `XLBox` field is truncated | [`an_lbox_of_one_without_room_for_an_xlbox_is_refused`] | [`an_lbox_of_one_in_a_buffer_of_exactly_the_sixteen_byte_header_is_a_length`] |
    //! | [`declared_store_len`] | `XLBox` below the 16-byte header it counts | [`an_xlbox_below_the_sixteen_byte_header_is_refused_rather_than_resolved`] | [`an_xlbox_of_exactly_the_header_size_is_a_length`] |
    //! | [`declared_store_len`] | `LBox` in 2..=7, below the 8-byte header it counts | [`an_lbox_between_two_and_seven_is_refused_rather_than_resolved`] | [`an_lbox_of_exactly_the_header_size_is_a_length`] |
    //! | [`jumbf_superbox_span`] | `checked_add`: offset plus declared length leaves `usize` | [`a_declared_length_that_overflows_the_buffer_offset_is_an_unusable_length`] | [`a_span_ending_exactly_at_the_end_of_the_buffer_is_a_span`] |
    //! | [`jumbf_superbox_span`] | `end <= buffer.len()`: the length runs past the buffer | [`a_declared_length_running_past_the_buffer_is_an_unusable_length_not_an_absent_superbox`] | [`a_span_ending_exactly_at_the_end_of_the_buffer_is_a_span`] |
    //! | [`is_jumbf_not_found`] | `C2pa(JumbfNotFound)`: the asset carries no manifest at all | [`c2pa_rss_jumbf_not_found_is_an_absent_manifest`] | [`a_c2pa_error_other_than_jumbf_not_found_is_not_an_absent_manifest`] |
    //!
    //! The table is machine-checked. [`the_enumeration_is_a_table_with_no_blank_cell`] and
    //! [`every_test_the_enumeration_names_exists_in_this_file`] parse it out of this very doc
    //! comment and fail on a blank cell or on a name nothing defines, so renaming a pinned test
    //! cannot leave a stale row behind a green suite; rustdoc would not catch it, because it never
    //! compiles a `cfg(test)` module and never resolves these links.
    //! [`every_test_file_the_enumeration_names_exists`] does the same for the `tests/` files the
    //! prose points at.
    //!
    //! Those three read **rows → tests**. The fourth reads the other way, **code → rows**:
    //! [`every_discriminating_branch_in_the_named_functions_has_a_row`] cuts each function a row
    //! names out of this file's source, counts its discriminating branches with
    //! [`discriminating_branches`], and fails, naming the function, when the count differs from
    //! the rows the table gives it. So adding a branch to one of the four without a row goes red,
    //! and so does deleting one while its row stays. The counter is syntactic, not a parser: its
    //! vocabulary — `match` arms, `matches!`, `if`, and the `ok_or`/`filter`/`skip` forms these
    //! functions chain — is stated on [`discriminating_branches`], and a construct outside it
    //! counts as nothing, so a branch written in a new form needs its token added there too.
    //!
    //! Two things about the table read as gaps and are not. Two rows share a "not taken" column —
    //! [`a_span_ending_exactly_at_the_end_of_the_buffer_is_a_span`] is the nearest non-refused
    //! input for both of [`jumbf_superbox_span`]'s refusals, and splitting it would only give the
    //! second row a fixture differing in a byte neither branch reads; so do the three
    //! [`declared_store_len`] rows whose neighbouring arm is the ordinary 32-bit `LBox`, which
    //! [`an_lbox_of_exactly_the_header_size_is_a_length`] is. And one test appears in both columns
    //! — [`an_lbox_of_zero_in_a_buffer_shorter_than_the_header_is_refused`] is the *taken* side of
    //! the `LBox == 0` header-minimum branch and the *not taken* side of the truncation branch
    //! above it, because a 4-to-7-byte buffer is one whose `LBox` field was read successfully and
    //! whose resulting length is then too short. Adjacent branches on the same input share a
    //! boundary; that is what makes it a boundary.
    //!
    //! Five of this module's tests are in no row, and the count is derived rather than
    //! remembered: `grep -c '^    #\[test\]$' src/lib.rs` gives 25 tests, and
    //! `grep -c '^    //! | \[' src/lib.rs` gives the table's 12 rows, which name 20 distinct
    //! tests between their two columns. The five are
    //! [`a_span_is_still_found_when_a_decoy_jumb_precedes_the_superbox`], which pins the
    //! *composition* of the search and the length reading rather than a branch of either, and the
    //! four checks whose subject is this doc comment rather than a buffer —
    //! [`the_enumeration_is_a_table_with_no_blank_cell`],
    //! [`every_test_the_enumeration_names_exists_in_this_file`],
    //! [`every_test_file_the_enumeration_names_exists`] and
    //! [`every_discriminating_branch_in_the_named_functions_has_a_row`].
    //!
    //! Nothing checks that count, and the reason is worth stating: the four checks hold rows
    //! against tests and branches against rows, but none runs tests → rows. A test that is here
    //! and in no row — or this sentence going stale — is invisible to all of them, because in
    //! that direction they ask nothing. That asymmetry is why the number above carries the
    //! command that re-derives it.
    //!
    //! [`is_jumbf_not_found`]'s taken side is also observed end to end, on an error c2pa-rs itself
    //! raised, in `tests/no_copy_forward.rs`. The row points at the inline test rather than that
    //! one because the inline test fails for exactly one reason — the predicate misread an error —
    //! while the differential fails for anything wrong anywhere in a re-encode.
    //!
    //! Every one of these tests asserts **which** refusal fired — the message where the variant
    //! carries one, the variant itself where it does not — never merely that an error occurred.
    //! Several branches refuse the same input for different reasons: deleting the
    //! truncated-`LBox` arm sends a 3-byte buffer into the `LBox == 0` arm, which refuses it too,
    //! so only the message distinguishes the arm that fired from the one that caught the fall.

    use super::{
        OracleError, declared_store_len, find_jumbf_superbox, is_jumbf_not_found,
        jumbf_superbox_span,
    };

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
    fn a_span_ending_exactly_at_the_end_of_the_buffer_is_a_span() {
        // The accepted side of both of `jumbf_superbox_span`'s refusals: the sum stays inside
        // `usize` and the end lands on the last byte of the buffer. A bound that refused one byte
        // too early would refuse this store, which is the shape every real store has.
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
        // 33 bytes declared in a 32-byte buffer: the *nearest* length the bound must refuse, one
        // byte past the end. A far-past length — 4096, say — is refused by a bound off by any
        // amount, so it says nothing about where the bound sits; this one is refused only by a
        // bound that stops exactly at `buffer.len()`.
        let mut buffer = vec![0u8; 32];
        buffer[..4].copy_from_slice(&33u32.to_be_bytes());
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

    #[test]
    fn c2pa_rss_jumbf_not_found_is_an_absent_manifest() {
        assert!(
            is_jumbf_not_found(&OracleError::C2pa(c2pa::Error::JumbfNotFound)),
            "`JumbfNotFound` is c2pa-rs saying the asset carries no manifest at all, which is the \
             one verdict this predicate exists to recognise"
        );
    }

    #[test]
    fn a_c2pa_error_other_than_jumbf_not_found_is_not_an_absent_manifest() {
        // `JumbfBoxNotFound` is the nearest neighbour there is: same crate, same phrasing, and it
        // is raised only once a store *has* been found and something inside it is missing. Reading
        // it as an absent manifest would turn "the derivative carries a broken copy of its
        // parent's store" into "the derivative carries nothing", which is precisely the confusion
        // the no-copy-forward claim turns on.
        let error = OracleError::C2pa(c2pa::Error::JumbfBoxNotFound);

        assert!(
            !is_jumbf_not_found(&error),
            "an error raised about a manifest that is present is not the absence of one; got \
             {error} reported as an absent manifest"
        );
    }

    /// This file's own source, so the enumeration in the module doc above can be checked against
    /// the definitions below it.
    const SOURCE: &str = include_str!("lib.rs");

    /// The lines of this module's doc comment, `//!` and surrounding space stripped.
    fn module_doc_lines() -> impl Iterator<Item = &'static str> {
        SOURCE
            .lines()
            .skip_while(|line| !line.starts_with("mod tests {"))
            .filter_map(|line| line.trim_start().strip_prefix("//!"))
            .map(str::trim)
    }

    /// The enumeration's rows, header and separator included, each split into its cells.
    fn enumeration_rows() -> Vec<Vec<&'static str>> {
        module_doc_lines()
            .filter(|line| line.starts_with('|'))
            .map(|line| line.trim_matches('|').split('|').map(str::trim).collect())
            .collect()
    }

    /// The `` [`name`] `` intra-doc links in `text`, in order.
    fn intra_doc_links(text: &str) -> Vec<&str> {
        text.match_indices("[`")
            .filter_map(|(at, _)| {
                let rest = &text[at + 2..];
                rest.find("`]").map(|end| &rest[..end])
            })
            .collect()
    }

    #[test]
    fn the_enumeration_is_a_table_with_no_blank_cell() {
        const HEADER: [&str; 4] = ["Function", "Branch", "Taken", "Not taken"];

        let rows = enumeration_rows();
        assert!(
            rows.len() > 2,
            "the module doc must carry the enumeration table: header, separator and at least one \
             branch"
        );
        assert_eq!(
            rows[0], HEADER,
            "the columns this test reads by position must be the ones the table declares"
        );

        for row in &rows[2..] {
            assert_eq!(
                row.len(),
                HEADER.len(),
                "every row names a function, a branch and both of its directions: {row:?}"
            );
            for (column, cell) in HEADER.iter().zip(row) {
                assert!(
                    !cell.is_empty(),
                    "a row with a blank `{column}` is the finding, not a formatting slip: {row:?}"
                );
            }
            for column in [0usize, 2, 3] {
                assert!(
                    !intra_doc_links(row[column]).is_empty(),
                    "`{}` must name its function or test as an intra-doc link, so this file can \
                     check it resolves: {row:?}",
                    HEADER[column]
                );
            }
        }
    }

    #[test]
    fn every_test_the_enumeration_names_exists_in_this_file() {
        let mut checked = 0usize;

        for line in module_doc_lines() {
            for name in intra_doc_links(line) {
                checked += 1;
                assert!(
                    SOURCE.contains(&format!("fn {name}(")),
                    "the enumeration names `{name}`, and this file defines no function by that \
                     name; a renamed test has to be renamed in its row too"
                );
            }
        }

        assert!(
            checked > 0,
            "the enumeration names its functions with intra-doc links; finding none means this \
             test read the wrong lines and was checking nothing"
        );
    }

    #[test]
    fn every_test_file_the_enumeration_names_exists() {
        let mut checked = 0usize;

        for line in module_doc_lines() {
            for span in line.split('`').skip(1).step_by(2) {
                if !span.starts_with("tests/") || !span.ends_with(".rs") {
                    continue;
                }
                checked += 1;
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(span);
                assert!(
                    path.exists(),
                    "the enumeration points at `{span}` for a claim it does not pin itself, and \
                     there is no such file"
                );
            }
        }

        assert!(
            checked > 0,
            "the enumeration points at `tests/` files for the two claims outside this layer; \
             finding none means this test read the wrong lines and was checking nothing"
        );
    }

    /// `line` with its string literals emptied and its `//` comment dropped, so neither prose nor a
    /// refusal message can contribute a token to [`discriminating_branches`].
    fn code_of(line: &str) -> String {
        let mut code = String::with_capacity(line.len());
        let mut in_string = false;
        let mut escaped = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if in_string {
                match (escaped, c) {
                    (false, '\\') => escaped = true,
                    (false, '"') => {
                        in_string = false;
                        code.push('"');
                    }
                    _ => escaped = false,
                }
            } else if c == '"' {
                in_string = true;
                code.push('"');
            } else if c == '/' && chars.peek() == Some(&'/') {
                break;
            } else {
                code.push(c);
            }
        }
        code
    }

    /// The body of the top-level `pub fn name` in this file — from the `{` that opens it to the
    /// `}` that closes it — with string literals and comments stripped by [`code_of`].
    fn function_body(name: &str) -> String {
        let signature = format!("pub fn {name}(");
        let code: String = SOURCE
            .lines()
            .skip_while(|line| !line.starts_with(&signature))
            .map(|line| code_of(line) + "\n")
            .collect();
        assert!(
            !code.is_empty(),
            "the enumeration names `{name}`, and this file defines no `pub fn` by that name"
        );

        let open = code
            .find('{')
            .unwrap_or_else(|| panic!("`{name}` has no body"));
        let mut depth = 0usize;
        for (at, c) in code.char_indices().skip(open) {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return code[open..=at].to_owned();
                    }
                }
                _ => {}
            }
        }
        panic!("`{name}`'s body never closes")
    }

    /// How many discriminating branches `body` holds, by the syntactic reading the enumeration's
    /// rows take of the four functions it names:
    ///
    /// * a `match` with *n* arms is *n − 1* branches — every arm but the one the input falls to
    ///   when it matches none of the others — so it counts each `=>` and subtracts one per `match`;
    /// * `matches!` is one two-way branch;
    /// * `if` is one, whether it opens an `if`, an `else if` or an `if let`;
    /// * an `Option` chain contributes one branch for its terminal `ok_or`/`ok_or_else` — the
    ///   chain's first way to come up empty — and one more for each `filter` or `skip` narrowing
    ///   it, each of which is a further way to come up empty. Hence `jumbf_superbox_span`'s
    ///   `checked_add(..).filter(..).ok_or(..)` is two branches, its two rows, and
    ///   `find_jumbf_superbox`'s `skip(..).find(..).ok_or(..)` is two, its two rows.
    ///
    /// Deliberately *not* counted: `?`, which propagates a branch some other function took (that
    /// function's rows hold it); `checked_add`, `get` and `find`, the base sources an `ok_or`
    /// already stands for; and `and_then(|field| field.try_into().ok())`, which converts a slice
    /// the `get` before it has already sized, so it cannot come up empty on its own. A form outside
    /// this vocabulary — `let … else`, `&&`/`||`, `unwrap_or` — is not used by these functions;
    /// one that starts to be needs its token added here, or the count runs short of the rows.
    fn discriminating_branches(body: &str) -> usize {
        let words: Vec<&str> = body
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '!'))
            .filter(|word| !word.is_empty())
            .collect();
        let count = |word: &str| words.iter().filter(|w| **w == word).count();

        let arms = body.matches("=>").count();
        let matches = count("match");
        assert!(
            arms >= matches,
            "every `match` has at least one arm, so {arms} `=>` cannot serve {matches} matches"
        );

        arms - matches
            + count("matches!")
            + count("if")
            + count("ok_or")
            + count("ok_or_else")
            + count("filter")
            + count("skip")
    }

    #[test]
    fn every_discriminating_branch_in_the_named_functions_has_a_row() {
        let mut rows_per_function: Vec<(&str, usize)> = Vec::new();
        for row in enumeration_rows().iter().skip(2) {
            let function = intra_doc_links(row[0])
                .first()
                .copied()
                .unwrap_or_else(|| panic!("a row names no function: {row:?}"));
            match rows_per_function
                .iter_mut()
                .find(|(name, _)| *name == function)
            {
                Some((_, rows)) => *rows += 1,
                None => rows_per_function.push((function, 1)),
            }
        }
        assert_eq!(
            rows_per_function.len(),
            4,
            "the enumeration covers exactly four named functions: {rows_per_function:?}"
        );

        for (function, rows) in rows_per_function {
            let branches = discriminating_branches(&function_body(function));
            assert_eq!(
                branches, rows,
                "`{function}` holds {branches} discriminating branches and the enumeration gives \
                 it {rows} rows; a branch added without a row (or a row left behind a deleted \
                 branch) is the finding"
            );
        }
    }
}
