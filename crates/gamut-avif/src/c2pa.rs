//! Carrying and locating a C2PA manifest store in an AVIF file (C2PA 2.4 Appendix A.5).
//!
//! C2PA (Coalition for Content Provenance and Authenticity) embeds its manifest store in a
//! BMFF-based asset — AVIF is named in §A.5.1 — as a top-level `uuid` box, the
//! `ContentProvenanceBox`, whose 16-byte extended type is [`C2PA_UUID`]. This module is the
//! crate's whole C2PA surface, and it is deliberately small: it knows the **framing** §A.5.1 puts
//! around the store, on the write side to reserve or fill a slot for one
//! ([`AvifEncoder::with_c2pa_reserved`](crate::AvifEncoder::with_c2pa_reserved) /
//! [`with_c2pa`](crate::AvifEncoder::with_c2pa)) and on the read side to report where one sits
//! ([`AvifContainer::c2pa_slot`]). It parses nothing inside the store, verifies no hash, checks no
//! signature and reaches no verdict — validation is a C2PA validator's job, downstream.
//!
//! §A.5.1.2 defines the box as a `FullBox` with `version = 0` and `flags = 0`:
//!
//! ```text
//! aligned(8) class ContentProvenanceBox extends FullBox('uuid', extended_type = C2PA_UUID,
//!                                                      version = 0, 0) {
//!   string box_purpose;   // null-terminated
//!   bit(8) data[];
//! }
//! ```
//!
//! So after the box's size/type header the bytes run: the 16-byte user type, the 1-byte version
//! and 3-byte flags, the NUL-terminated `box_purpose`, then `data`. §A.5.3 fixes what `data`
//! holds for a box that carries a manifest store: **the first 8 bytes are the absolute file offset
//! of the first auxiliary `merkle` box** (zero when the file has none), followed by the raw
//! manifest-store bytes, followed by zero or more unused padding bytes. The store slot this module
//! reserves, fills and reports is everything after those 8 bytes.
//!
//! # Placement
//!
//! §A.5.3: the box "shall appear before the first `mdat` box in the file and before any `moov` box
//! … it shall be placed after the `ftyp` box". The encoder puts it at
//! [`gamut_isobmff::TopLevelPosition::AfterFtyp`] — between `ftyp` and `meta` — which satisfies
//! all three. A file mid-update carries two boxes: the previous store re-labelled `original` where
//! it was, and an `update` store as the last box of the file; the locator reports both, in file
//! order, and never decides which is active.
//!
//! # The range is not an exclusion range
//!
//! Every byte range this module reports — [`C2paSlot::range`] on read,
//! [`AvifEncodeReport::c2pa`](crate::AvifEncodeReport::c2pa) on write — is for patching,
//! extraction and byte accounting. A BMFF asset's hard binding is `c2pa.hash.bmff.v3`, which
//! excludes content by **box path**, not by byte offset (§18.6, §A.5.6). Hashing "everything but
//! this range" is therefore not how a BMFF manifest is bound, and no type here is named
//! "exclusion" so that no caller is invited to try.

use core::ops::Range;

use gamut_core::{Error, Result};
use gamut_isobmff::{Segment, SegmentKind};

use crate::container::AvifContainer;

/// The 16-byte extended (user) type identifying a `uuid` box as a C2PA `ContentProvenanceBox`:
/// `D8FEC3D6-1B0E-483C-9297-5828877EC481` (C2PA 2.4 §A.5.1.1).
pub const C2PA_UUID: [u8; 16] = [
    0xD8, 0xFE, 0xC3, 0xD6, 0x1B, 0x0E, 0x48, 0x3C, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7E, 0xC4, 0x81,
];

/// The `FullBox` version (1 byte) and flags (3 bytes) that follow the user type: both zero
/// (§A.5.1.2, `version = 0, 0`).
const VERSION_FLAGS: [u8; 4] = [0; 4];

/// Length of the absolute file offset of the first `merkle` box that opens `data` (§A.5.3). A
/// still image this crate writes has no `merkle` box, so the encoder writes it as zero.
const MERKLE_OFFSET_LEN: usize = 8;

/// The shortest slot a **reservation** may ask for: the 8 bytes of a JUMBF box header — its 4-byte
/// `LBox` length and its 4-byte `TBox` type — since a manifest store is a JUMBF superbox and a
/// shorter slot could not hold even that header, let alone a store.
///
/// C2PA 2.4 §8.4.2.3 ("Hashing JUMBF Boxes") is where the width and endianness of the pair are
/// traceable within the C2PA specification itself; the general JUMBF grammar is ISO/IEC 19566-5,
/// which C2PA references but does not restate and which is not vendored here. `gamut-heic` bounds
/// its `LBox` reads by the same constant (its `JUMBF_HEADER_LEN`).
///
/// This bounds the **write** side only. See [`content_provenance_reserved`] for the refusal and
/// [`AvifContainer::c2pa_slots`] for why the read side stays permissive.
const MIN_SLOT_LEN: usize = 8;

/// The longest `ContentProvenanceBox` payload [`content_provenance_reserved`] will build: a `Vec`
/// holds at most `isize::MAX` bytes, and asking for more *panics* with "capacity overflow" rather
/// than returning. Refusing above this ceiling turns that panic into an error, so no reservation
/// length can panic the library.
///
/// This is a panic guard, not the encoder's effective limit. Long before it, the container writer
/// refuses the box: a top-level box carries a 32-bit size field, so
/// [`gamut_isobmff::write`] rejects one at or beyond 4 GiB with
/// [`Error::Unsupported`](gamut_core::Error::Unsupported). Measured on this framing, the largest
/// `len` that clears that check is `4_294_967_250` and `4_294_967_251` is refused, and a `len`
/// just under it aborts in the allocator while the writer copies the payload into the output
/// buffer. So above the writer's bound lies the writer's error, and below it — for a length the
/// machine has no memory for — lies the allocator's own limit, which no fallible API here can
/// intercept.
const MAX_PAYLOAD_LEN: usize = isize::MAX as usize;

/// The `box_purpose` of a C2PA `uuid` box that carries a manifest store (C2PA 2.4 §A.5.3).
///
/// §A.5.3 admits exactly three purposes for a box that carries a manifest store — `manifest`,
/// `original` and `update` — and describes their `data` framing:
///
/// | `box_purpose` | Meaning | `data` |
/// | --- | --- | --- |
/// | `manifest` | the ordinary manifest store | the 8-byte merkle offset, the store, padding |
/// | `original` | the previous store of a file mid-update, unchanged apart from this label | "identical apart from value of `box_purpose`" to `manifest` |
/// | `update` | a store holding update manifests only, the last box of the file | not stated — see below |
///
/// # `update`: the specification does not say, so the offset is probed for
///
/// §A.5.3 states the merkle offset for `manifest` and `original`. For `update` it constrains only
/// the store's *contents* ("shall only contain update manifests") and says nothing about the bytes
/// ahead of it. The reference implementation writes the 8-byte offset there too, so files in
/// circulation carry it, but the silence is a gap rather than a prohibition.
///
/// So `update` is **probed**, over the same two candidate offsets `gamut-heic`'s locator uses —
/// `8` first, falling back to `0`. `manifest` and `original` are not probed: the specification
/// states their framing, so a single offset is used.
///
/// **How strong the probe is here.** `gamut-heic` discriminates on JUMBF `LBox` validity;
/// a box-bounded slot ([`C2paSlot`]) has no such field to read, so the only in-band signal is
/// whether `data` is long enough for the prefix at all. The fallback therefore fires exactly when
/// an `update` box's `data` is shorter than 8 bytes — which without it would be dropped as absent,
/// the one case where the two crates disagreed about identical bytes. A *longer* prefix-less
/// `update` store is still reported 8 bytes short at its front; detecting that needs the `LBox`
/// check, which is #505's to unify. No known writer emits either shape.
///
/// A fourth purpose, `merkle`, names an *auxiliary* box holding Merkle-tree hashes (§A.5.4), not a
/// manifest store; a `merkle` box, like any unrecognised purpose, is not reported.
///
/// Non-exhaustive, with permanent append-only discriminants: a later revision may add a variant
/// without a breaking change, so match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum C2paBoxPurpose {
    /// `manifest` — the ordinary manifest store.
    Manifest = 0,
    /// `original` — the untouched previous store of a file being updated.
    Original = 1,
    /// `update` — a store containing update manifests only.
    Update = 2,
}

impl C2paBoxPurpose {
    /// The `box_purpose` string §A.5.3 spells for this purpose, without its NUL terminator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Original => "original",
            Self::Update => "update",
        }
    }

    /// Maps the NUL-terminated `box_purpose` string's bytes to a manifest-store purpose, or `None`
    /// for `merkle` and every unrecognised value.
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        [Self::Manifest, Self::Original, Self::Update]
            .into_iter()
            .find(|purpose| purpose.as_str().as_bytes() == bytes)
    }

    /// The offsets into `data`, in probe order, at which this purpose's store slot may begin: one
    /// candidate where §A.5.3 states the framing, two where it is silent (see the
    /// [type docs](Self)). The same list `gamut-heic`'s locator probes.
    const fn store_prefix_candidates(self) -> &'static [usize] {
        match self {
            Self::Manifest | Self::Original => &[MERKLE_OFFSET_LEN],
            Self::Update => &[MERKLE_OFFSET_LEN, 0],
        }
    }
}

/// The C2PA manifest-store **slot** located in an AVIF file: the slot's bytes, its exact byte
/// range in the file, and the `box_purpose` of the `uuid` box that carries it.
///
/// # A slot, not a trimmed store — and why the name says so
///
/// [`slot_bytes`](Self::slot_bytes) is **box-bounded**: it runs from just after the 8-byte merkle
/// offset to the end of the enclosing `uuid` box. For a filled box that is the manifest store
/// **plus any unused padding** §A.5.3 permits after it; for a box
/// [`AvifEncoder::with_c2pa_reserved`](crate::AvifEncoder::with_c2pa_reserved) reserved and no
/// signer has filled, it is all zeros. Whether the slot holds a valid store is a validator's
/// question, not this locator's.
///
/// [`gamut_heic::C2paManifestStore`] reports something different under a similar name: it trims to
/// the store's own outer JUMBF `LBox`, so for one file the two crates can report different lengths
/// — a filled store without its padding there, the whole slot here. This type is therefore named
/// for the slot rather than for the store, so the two claims cannot be confused when both are
/// re-exported from the `gamut` umbrella.
///
/// The bound is not an oversight. An `LBox` trim cannot locate a *reservation*: an all-zero slot
/// has no `LBox`, so trimming would report it as absent and the reserve → report → patch flow
/// this crate exists to support would have nothing to patch. Unifying the two lenses — and
/// deciding the bound once for both crates — is issue #505; until then a consumer that wants the
/// store alone reads its length off the front of `slot_bytes`.
///
/// [`gamut_heic::C2paManifestStore`]: https://docs.rs/gamut-heic
///
/// # The range is observability, not an exclusion range
///
/// [`range`](Self::range) says where the slot sits in the file — for patching a reserved slot,
/// extraction and byte accounting. It is **not** a BMFF hard-binding exclusion range:
/// `c2pa.hash.bmff.v3` excludes content by *box path*, not by byte offset (C2PA 2.4 §18.6,
/// §A.5.6), so computing or checking a BMFF hash from this range would be wrong.
///
/// Non-exhaustive: a later revision may report more of the box's framing without a breaking
/// change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paSlot<'a> {
    /// The slot: every byte of the box's `data` after the merkle offset — the store, then any
    /// padding — or all zeros for an unfilled reservation. Opaque; nothing inside it is parsed,
    /// and it is **not** trimmed to the store's own JUMBF `LBox` (see the [type docs](Self)).
    pub slot_bytes: &'a [u8],
    /// The half-open byte range [`slot_bytes`](Self::slot_bytes) occupies within the file, so
    /// `range.len() == slot_bytes.len()`.
    pub range: Range<usize>,
    /// The `box_purpose` of the `uuid` box that carries this slot.
    pub purpose: C2paBoxPurpose,
}

impl<'a> AvifContainer<'a> {
    /// The first C2PA manifest-store **slot** in the file, in file order, or `None` if the file
    /// carries none.
    ///
    /// A file that is mid-update legitimately carries two — an `original` box and an `update` box
    /// (C2PA 2.4 §A.5.3) — and deciding which of them is *active* is a validator's judgement, not a
    /// container reader's. This accessor therefore promises only "the first one"; use
    /// [`c2pa_slots`](Self::c2pa_slots) to see them all with their purposes.
    ///
    /// See [`C2paSlot`] for exactly what is stripped, what bounds the slot, and why the
    /// reported range must not be treated as a BMFF exclusion range.
    #[must_use]
    pub fn c2pa_slot(&self) -> Option<C2paSlot<'a>> {
        self.c2pa_slots().next()
    }

    /// Every C2PA manifest-store **slot** among the **top-level boxes of the primary stream**, in
    /// file order.
    ///
    /// Each item is a [`C2paSlot`]: the box-bounded region after the merkle offset, which is a
    /// manifest store only if somebody wrote one there. This accessor reports where a store would
    /// sit, never that one is valid — nothing inside the slot is parsed.
    ///
    /// Only *top-level* `uuid` boxes are considered — where §A.5.3 puts the box, and the set
    /// [`gamut_isobmff::read`] stores in
    /// [`IsoBmffImage::top_level_boxes`](gamut_isobmff::IsoBmffImage::top_level_boxes). A `uuid`
    /// box nested inside `meta` is not a manifest store and is surfaced, as it always was, through
    /// [`unknown_meta_boxes`](Self::unknown_meta_boxes). Where the box actually sits is reported as
    /// found and never enforced: a store outside the window §A.5.3 mandates is still reported,
    /// with its true range. Boxes inside an appended motion-photo stream or a trailer are not
    /// examined (the segment walk stops emitting boxes there).
    ///
    /// A top-level `uuid` box whose user type is not [`C2PA_UUID`], whose `FullBox` version or
    /// flags are non-zero, whose `box_purpose` is not one of [`C2paBoxPurpose`]'s, or whose body is
    /// too short to hold the framing is skipped silently: this is a lens over bytes that happen to
    /// be present, so a malformed or foreign box yields nothing rather than an error.
    ///
    /// # The read side demands nothing of a slot's length
    ///
    /// [`AvifEncoder::with_c2pa_reserved`](crate::AvifEncoder::with_c2pa_reserved) **refuses** to
    /// reserve a slot shorter than a JUMBF box header, because a *reservation* is a bare integer
    /// and a slot that cannot hold a store is a caller's mistake made before any bytes exist. This
    /// locator makes no such demand: a well-framed box with a zero-length or otherwise degenerate
    /// slot is reported with its true (possibly empty) range, because the bytes are *there* and
    /// somebody put them there.
    ///
    /// That is not a "strict writer, permissive reader" asymmetry, and it would be wrong to
    /// describe it as one: the minimum bounds the reservation path alone.
    /// [`with_c2pa`](crate::AvifEncoder::with_c2pa) carries a caller-supplied slice verbatim, so
    /// this crate does write files whose slot is shorter than a JUMBF box header, and this locator
    /// reports them. Whether that builder should apply the minimum too is issue #577.
    pub fn c2pa_slots(&self) -> impl Iterator<Item = C2paSlot<'a>> + '_ {
        slots(self.segments())
    }
}

/// The manifest-store slots among `segments`' top-level boxes, in order — the one locator behind
/// [`AvifContainer::c2pa_slots`] and the encoder's
/// [`encode_with_report`](crate::AvifEncoder::encode_with_report), so the range the encoder
/// reports is the range the reader finds.
pub(crate) fn slots<'a, 's>(
    segments: &'s [Segment<'a>],
) -> impl Iterator<Item = C2paSlot<'a>> + 's {
    segments.iter().filter_map(|segment| match segment.kind {
        SegmentKind::Box { ty, body } if &ty == b"uuid" => {
            // `range` spans the header and the body, so `range.end - body.len()` is the absolute
            // offset of the body whatever the header width (8-byte, or 16 with `largesize`).
            let body_start = segment.range.end.checked_sub(body.len())?;
            parse_content_provenance_box(body, body_start)
        }
        _ => None,
    })
}

/// Parses one top-level `uuid` box body (starting at absolute offset `body_start`) into the
/// manifest-store slot it carries, or `None` if it does not carry one.
///
/// The body is walked in the §A.5.1.2 order: user type, version/flags, `box_purpose` to its NUL,
/// then `data`, whose first 8 bytes are the merkle offset (§A.5.3) and whose remainder is the
/// slot. Where the slot begins is one stated offset for `manifest`/`original` and two probed in
/// order for `update` — see [`C2paBoxPurpose`].
fn parse_content_provenance_box(body: &[u8], body_start: usize) -> Option<C2paSlot<'_>> {
    let (user_type, rest) = body.split_at_checked(C2PA_UUID.len())?;
    if user_type != C2PA_UUID {
        return None;
    }
    let (version_flags, rest) = rest.split_at_checked(VERSION_FLAGS.len())?;
    if version_flags != VERSION_FLAGS {
        return None;
    }
    let terminator = rest.iter().position(|&b| b == 0)?;
    let purpose = C2paBoxPurpose::from_bytes(&rest[..terminator])?;
    let data = &rest[terminator + 1..];
    for &prefix in purpose.store_prefix_candidates() {
        if let Some(slot) = data.get(prefix..) {
            let start = body_start + (body.len() - slot.len());
            return Some(C2paSlot {
                slot_bytes: slot,
                range: start..start + slot.len(),
                purpose,
            });
        }
    }
    None
}

/// How many bytes of §A.5.1.2 framing precede the slot for `purpose`: the 4-byte zero `FullBox`
/// version and flags, the NUL-terminated `box_purpose`, and the 8-byte merkle offset.
const fn framing_len(purpose: C2paBoxPurpose) -> usize {
    VERSION_FLAGS.len() + purpose.as_str().len() + 1 + MERKLE_OFFSET_LEN
}

/// The framing a `ContentProvenanceBox` payload opens with — the zero `FullBox` version and flags,
/// the NUL-terminated `box_purpose`, and the 8-byte merkle offset written as zero (a still image
/// carries no `merkle` box) — in a buffer pre-sized to `capacity`, the payload's finished length.
///
/// `capacity` is a caller-computed figure rather than a slot length this function adds on, so the
/// one addition of a slot length to a framing length in this module lives at the single caller that
/// checks it, [`content_provenance_reserved`].
fn content_provenance_framing(purpose: C2paBoxPurpose, capacity: usize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(capacity);
    payload.extend_from_slice(&VERSION_FLAGS);
    payload.extend_from_slice(purpose.as_str().as_bytes());
    payload.push(0);
    payload.extend_from_slice(&[0; MERKLE_OFFSET_LEN]);
    payload
}

/// The payload of a `ContentProvenanceBox` after its user type — what
/// [`gamut_isobmff::TopLevelBox::uuid`] takes: the §A.5.1.2 framing, then `slot` verbatim as the
/// store slot.
///
/// Infallible where [`content_provenance_reserved`] is not: `slot` is a materialised slice, so its
/// length is one the machine already holds. A *reservation* is a bare integer with no such bound,
/// which is why only that path has to refuse.
///
/// The room for `slot` is asked for with `reserve_exact` rather than folded into the framing's
/// capacity, so this path computes no total of its own: the only place the two lengths are added is
/// [`content_provenance_reserved`], where the sum is checked.
pub(crate) fn content_provenance_payload(purpose: C2paBoxPurpose, slot: &[u8]) -> Vec<u8> {
    let mut payload = content_provenance_framing(purpose, framing_len(purpose));
    payload.reserve_exact(slot.len());
    payload.extend_from_slice(slot);
    payload
}

/// The same payload with a `len`-byte **reserved** slot: the framing, then `len` zero bytes.
///
/// Written with one `resize` into the buffer the payload already owns rather than by building a
/// `vec![0; len]` and copying it in, so *the payload builder* peaks at n bytes for an n-byte slot
/// and not 2n. The encode as a whole still peaks near 2n, because `gamut_isobmff::writer` copies
/// this payload into the output buffer; the saving claimed here is this function's alone.
///
/// # Errors
///
/// [`Error::InvalidInput`] if `len` is below [`MIN_SLOT_LEN`] — a slot too small to hold a JUMBF
/// box header can never hold a manifest store — or if the framed payload would exceed
/// [`MAX_PAYLOAD_LEN`]. Both are refused rather than documented as a precondition, because both
/// unchecked outcomes were wrong in different ways: the sum wraps in the release profile every
/// downstream consumer builds with, truncating the framing and emitting a well-formed file whose
/// C2PA box no locator can find, and just below the wrap it panics inside `Vec` instead. A silent
/// wrong answer about the range a signer binds, or a panic out of an infallible builder, are both
/// outcomes this refuses to produce.
pub(crate) fn content_provenance_reserved(purpose: C2paBoxPurpose, len: usize) -> Result<Vec<u8>> {
    if len < MIN_SLOT_LEN {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: a reserved C2PA slot is at least 8 bytes, the size of a JUMBF box header",
        ));
    }
    let total = framing_len(purpose)
        .checked_add(len)
        .filter(|&total| total <= MAX_PAYLOAD_LEN)
        .ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: the reserved C2PA slot exceeds the largest ContentProvenanceBox that fits in memory",
            )
        })?;
    let mut payload = content_provenance_framing(purpose, total);
    payload.resize(total, 0);
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body per §A.5.1.2: user type, version/flags, `purpose` NUL-terminated, then `data`.
    fn body(user_type: [u8; 16], version_flags: [u8; 4], purpose: &[u8], data: &[u8]) -> Vec<u8> {
        let mut b = user_type.to_vec();
        b.extend_from_slice(&version_flags);
        b.extend_from_slice(purpose);
        b.push(0);
        b.extend_from_slice(data);
        b
    }

    /// `data` for a manifest box: a merkle offset then the slot.
    fn data(merkle_offset: u64, slot: &[u8]) -> Vec<u8> {
        let mut d = merkle_offset.to_be_bytes().to_vec();
        d.extend_from_slice(slot);
        d
    }

    #[test]
    fn content_provenance_payload_is_the_a_5_1_2_layout_exactly() {
        // Version 0 / flags 0, `manifest` with its NUL, eight zero bytes for the merkle offset
        // (§A.5.3: "shall be zero" when the file has no merkle box), then the slot verbatim.
        let slot = [0xAB, 0xCD, 0xEF];
        let mut want = vec![0, 0, 0, 0];
        want.extend_from_slice(b"manifest\0");
        want.extend_from_slice(&[0; 8]);
        want.extend_from_slice(&slot);
        assert_eq!(
            content_provenance_payload(C2paBoxPurpose::Manifest, &slot),
            want
        );
        // The purpose string is the only thing that differs between the three labels.
        assert_eq!(
            &content_provenance_payload(C2paBoxPurpose::Original, &[])[4..13],
            b"original\0"
        );
        assert_eq!(
            &content_provenance_payload(C2paBoxPurpose::Update, &[])[4..11],
            b"update\0"
        );
        // An empty slot is a complete, store-less box: header fields only.
        assert_eq!(
            content_provenance_payload(C2paBoxPurpose::Manifest, &[]).len(),
            4 + 9 + 8
        );
    }

    #[test]
    fn a_reserved_slot_is_the_payload_a_zero_slot_would_give() {
        // `content_provenance_reserved` exists only to avoid materialising the zeros twice, so it
        // must agree byte for byte with handing the same zeros to the payload builder — at the
        // shortest slot it will write and at a couple of longer ones.
        for len in [MIN_SLOT_LEN, MIN_SLOT_LEN + 1, 96] {
            for purpose in [
                C2paBoxPurpose::Manifest,
                C2paBoxPurpose::Original,
                C2paBoxPurpose::Update,
            ] {
                assert_eq!(
                    content_provenance_reserved(purpose, len).expect("a framable reservation"),
                    content_provenance_payload(purpose, &vec![0u8; len]),
                    "{purpose:?} at {len}"
                );
            }
        }
    }

    #[test]
    fn a_reservation_below_a_jumbf_box_header_is_refused() {
        // A slot shorter than the 8-byte `LBox`/`TBox` pair cannot hold even the outermost box of
        // an empty manifest store, so the writer declines rather than emitting a slot no signer
        // could use. Every length below the bound is refused and the bound itself is accepted, so
        // the comparison cannot be off by one.
        for len in 0..MIN_SLOT_LEN {
            let err = content_provenance_reserved(C2paBoxPurpose::Manifest, len)
                .expect_err("shorter than a JUMBF box header");
            assert!(
                format!("{err}").contains("at least 8 bytes"),
                "{len}: {err}"
            );
        }
        assert!(content_provenance_reserved(C2paBoxPurpose::Manifest, MIN_SLOT_LEN).is_ok());
    }

    #[test]
    fn a_reservation_that_cannot_be_framed_is_refused_rather_than_truncated_or_panicked() {
        // Two unchecked failures sit next to each other above the ceiling. Nearest `usize::MAX`,
        // the framing addition wrapped and `resize` *shrank* the framing instead of growing it,
        // producing a well-formed file whose C2PA box the locator could not find. Just below that,
        // the sum was representable but larger than a `Vec` can hold, so `with_capacity` panicked.
        // Every length from the first one over the ceiling upwards is refused, so neither survives.
        for purpose in [
            C2paBoxPurpose::Manifest,
            C2paBoxPurpose::Original,
            C2paBoxPurpose::Update,
        ] {
            let over_ceiling = MAX_PAYLOAD_LEN - framing_len(purpose) + 1;
            for len in [over_ceiling, usize::MAX - framing_len(purpose), usize::MAX] {
                let err = content_provenance_reserved(purpose, len)
                    .expect_err("no framed payload this size can be built");
                assert!(
                    format!("{err}").contains("exceeds the largest"),
                    "{purpose:?} at {len}: {err}"
                );
            }
        }
    }

    #[test]
    fn parse_reports_the_slot_after_the_merkle_offset_at_its_file_offset() {
        // A non-zero body offset and a non-zero merkle offset, so the arithmetic that places the
        // slot in the file is observable and the 8 offset bytes are seen to be skipped, not read
        // into the slot.
        let slot = [0x11, 0x22, 0x33, 0x44, 0x55];
        let b = body(C2PA_UUID, [0; 4], b"manifest", &data(0x0102_0304, &slot));
        let store = parse_content_provenance_box(&b, 1000).expect("a manifest box");
        assert_eq!(store.slot_bytes, &slot);
        assert_eq!(store.purpose, C2paBoxPurpose::Manifest);
        // body_start + 16 (user type) + 4 (version/flags) + 9 (`manifest\0`) + 8 (merkle offset).
        assert_eq!(store.range, 1037..1042);
        assert_eq!(store.range.len(), store.slot_bytes.len());
    }

    #[test]
    fn parse_maps_each_a_5_3_purpose_and_skips_the_rest() {
        let b = |purpose: &[u8]| body(C2PA_UUID, [0; 4], purpose, &data(0, &[1]));
        let purpose =
            |purpose: &[u8]| parse_content_provenance_box(&b(purpose), 0).map(|s| s.purpose);
        assert_eq!(purpose(b"manifest"), Some(C2paBoxPurpose::Manifest));
        assert_eq!(purpose(b"original"), Some(C2paBoxPurpose::Original));
        assert_eq!(purpose(b"update"), Some(C2paBoxPurpose::Update));
        // `merkle` is an auxiliary box (§A.5.4), not a store; unknown labels and a label that is
        // a prefix of a real one are foreign.
        assert_eq!(purpose(b"merkle"), None);
        assert_eq!(purpose(b"Manifest"), None);
        assert_eq!(purpose(b"manifes"), None);
        assert_eq!(purpose(b""), None);
    }

    #[test]
    fn an_update_store_is_probed_at_both_offsets_and_the_stated_purposes_are_not() {
        // §A.5.3 states the merkle offset for `manifest`/`original` and is silent for `update`, so
        // only `update` falls back to offset 0 — and the fallback is reachable exactly when `data`
        // is shorter than the 8-byte prefix, which is the case that was dropped as absent before.
        let short = [1u8, 2, 3];
        let short_body = body(C2PA_UUID, [0; 4], b"update", &short);
        let update = parse_content_provenance_box(&short_body, 0)
            .expect("a prefix-less update store is located, not dropped");
        assert_eq!(update.slot_bytes, &short);
        assert_eq!(update.purpose, C2paBoxPurpose::Update);
        // 16 (user type) + 4 (version/flags) + 7 (`update\0`) + 0 (no prefix).
        assert_eq!(update.range, 27..30);

        // The stated purposes have one candidate, so the same too-short data is not a store.
        for purpose in [b"manifest".as_slice(), b"original".as_slice()] {
            assert_eq!(
                parse_content_provenance_box(&body(C2PA_UUID, [0; 4], purpose, &short), 0),
                None,
                "{}: no fallback where §A.5.3 states the framing",
                str::from_utf8(purpose).expect("ascii"),
            );
        }

        // With room for the prefix, offset 8 wins for `update` too — the fallback never displaces
        // the stated layout, so a normal update store keeps the reference implementation's framing.
        let long_body = body(C2PA_UUID, [0; 4], b"update", &data(0, &[9, 9]));
        let update = parse_content_provenance_box(&long_body, 0).expect("a normal update store");
        assert_eq!(update.slot_bytes, &[9, 9]);
    }

    #[test]
    fn parse_requires_the_c2pa_user_type_and_a_zero_full_box_header() {
        let d = data(0, &[1, 2, 3]);
        // A different `uuid` box is somebody else's; the same bytes with a flipped user type must
        // not be read as a store.
        let mut other = C2PA_UUID;
        other[15] ^= 0x01;
        assert_eq!(
            parse_content_provenance_box(&body(other, [0; 4], b"manifest", &d), 0),
            None
        );
        // §A.5.1.2: version = 0, flags = 0. A non-zero version or any flag bit is not this box.
        for version_flags in [[1, 0, 0, 0], [0, 0, 0, 1], [0, 1, 0, 0]] {
            assert_eq!(
                parse_content_provenance_box(&body(C2PA_UUID, version_flags, b"manifest", &d), 0),
                None,
                "{version_flags:?}"
            );
        }
        // …and the well-formed control parses.
        assert!(
            parse_content_provenance_box(&body(C2PA_UUID, [0; 4], b"manifest", &d), 0).is_some()
        );
    }

    #[test]
    fn parse_yields_nothing_for_a_body_too_short_to_hold_the_framing() {
        // Each prefix of a valid body, cut before the framing is complete, is not a store: no
        // user type, no version/flags, no NUL, no merkle offset. A body cut exactly at the end of
        // the merkle offset is a complete framing around an empty slot.
        let full = body(C2PA_UUID, [0; 4], b"manifest", &data(0, &[]));
        assert_eq!(full.len(), 16 + 4 + 9 + 8);
        for cut in 0..full.len() {
            assert_eq!(
                parse_content_provenance_box(&full[..cut], 0),
                None,
                "cut at {cut}"
            );
        }
        let empty = parse_content_provenance_box(&full, 500).expect("empty slot");
        assert_eq!(empty.slot_bytes, &[] as &[u8]);
        assert_eq!(empty.range, 537..537);
    }

    #[test]
    fn slots_reads_top_level_uuid_boxes_only_and_offsets_by_the_segment() {
        // Two C2PA uuid boxes, a non-C2PA uuid box between them, and a `free` box whose body is
        // byte-for-byte a C2PA payload: the stores come back in file order, each at the offset
        // its segment gives, and nothing else is reported — §A.5.1.1 fixes the box type to
        // `uuid`, so the locator keys on the type, never on the body alone.
        let first = body(C2PA_UUID, [0; 4], b"original", &data(0, &[7, 7]));
        let foreign = body([0xAA; 16], [0; 4], b"manifest", &data(0, &[9]));
        let second = body(C2PA_UUID, [0; 4], b"update", &data(0, &[8, 8, 8]));
        let free = body(C2PA_UUID, [0; 4], b"manifest", &data(0, &[6]));
        let segments = vec![
            Segment {
                range: 0..20,
                kind: SegmentKind::Box {
                    ty: *b"ftyp",
                    body: &[0; 12],
                },
            },
            Segment {
                range: 20..20 + 8 + first.len(),
                kind: SegmentKind::Box {
                    ty: *b"uuid",
                    body: &first,
                },
            },
            Segment {
                range: 100..100 + 8 + foreign.len(),
                kind: SegmentKind::Box {
                    ty: *b"uuid",
                    body: &foreign,
                },
            },
            Segment {
                range: 200..200 + 8 + free.len(),
                kind: SegmentKind::Box {
                    ty: *b"free",
                    body: &free,
                },
            },
            Segment {
                range: 300..300 + 8 + second.len(),
                kind: SegmentKind::Box {
                    ty: *b"uuid",
                    body: &second,
                },
            },
        ];
        let stores: Vec<_> = slots(&segments).collect();
        assert_eq!(stores.len(), 2);
        assert_eq!(stores[0].purpose, C2paBoxPurpose::Original);
        assert_eq!(stores[0].slot_bytes, &[7, 7]);
        // 20 + 8 (box header) + 16 + 4 + 9 (`original\0`) + 8.
        assert_eq!(stores[0].range, 65..67);
        assert_eq!(stores[1].purpose, C2paBoxPurpose::Update);
        assert_eq!(stores[1].slot_bytes, &[8, 8, 8]);
        // 300 + 8 + 16 + 4 + 7 (`update\0`) + 8.
        assert_eq!(stores[1].range, 343..346);
    }

    #[test]
    fn the_writer_payload_reads_back_through_the_parser() {
        // The framing the encoder writes is the framing the locator strips, so a reserved slot
        // reads back as exactly the bytes handed over — the coupling `encode_with_report` relies
        // on to report the range a reader will find.
        let slot = [0u8; 32];
        let mut b = C2PA_UUID.to_vec();
        b.extend_from_slice(&content_provenance_payload(C2paBoxPurpose::Manifest, &slot));
        let store = parse_content_provenance_box(&b, 0).expect("parses");
        assert_eq!(store.slot_bytes, &slot);
        assert_eq!(store.purpose, C2paBoxPurpose::Manifest);
        assert_eq!(store.range, b.len() - slot.len()..b.len());
    }
}
