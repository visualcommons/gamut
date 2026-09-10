//! Locating the C2PA manifest store carried by a HEIF file ([`HeifContainer::c2pa`]).
//!
//! C2PA (Coalition for Content Provenance and Authenticity, spec version 2.4) carries its manifest
//! store in an ISOBMFF file inside a top-level `uuid` box — the `ContentProvenanceBox` of §A.5.1 —
//! whose 16-byte extended (user) type is [`C2PA_UUID`]. This module is a **locator only**: it finds
//! that box, strips the framing the specification puts around the store, and reports the store as
//! opaque bytes plus its exact byte range. It parses nothing inside the store beyond the outer JUMBF
//! length field that bounds it, verifies no hash, checks no signature, and reaches no verdict about
//! the file's provenance — validation belongs to a C2PA validator downstream.
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
//! So the bytes after the box header run: the 16-byte user type, then the `FullBox` version and
//! flags, then the null-terminated `box_purpose`, then `data`. What sits at the front of `data`
//! depends on the purpose ([`C2paBoxPurpose`]), and what bounds the store inside it is the store's
//! own JUMBF `LBox` ([`C2paManifestStore`]).
use core::ops::Range;

use crate::container::{HeifContainer, SegmentKind};

/// The 16-byte extended (user) type identifying a `uuid` box as a C2PA `ContentProvenanceBox`.
///
/// `D8FEC3D6-1B0E-483C-9297-5828877EC481`, fixed by C2PA 2.4 §A.5.1.1.
pub const C2PA_UUID: [u8; 16] = [
    0xD8, 0xFE, 0xC3, 0xD6, 0x1B, 0x0E, 0x48, 0x3C, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7E, 0xC4, 0x81,
];

/// Length of the `FullBox` version (1 byte) + flags (3 bytes) that follow the `uuid` user type.
const VERSION_FLAGS_LEN: usize = 4;

/// Length of the absolute file offset of the first `merkle` box. §A.5.3 places it at the front of
/// `data` for the `manifest` and `original` purposes; for `update` the specification is silent and
/// its presence is probed for (see [`C2paBoxPurpose`]).
const MERKLE_OFFSET_LEN: usize = 8;

/// Minimum length of a JUMBF box: its 4-byte `LBox` plus its 4-byte `TBox`. See
/// [`C2paManifestStore`] for how far this shape is traceable to a vendored source.
const JUMBF_HEADER_LEN: usize = 8;

/// The `box_purpose` of a C2PA `uuid` box that carries a manifest store (C2PA 2.4 §A.5.3).
///
/// §A.5.3 admits exactly three purposes for a box that carries a manifest store. What sits at the
/// front of the box's `data` field, ahead of the store itself, differs between them:
///
/// | `box_purpose` | Meaning (§A.5.3) | Start of `data` |
/// | --- | --- | --- |
/// | `manifest` | the ordinary manifest store | the 8-byte absolute file offset of the first `merkle` box (zero if the file has none), then the store, then zero or more padding bytes — stated by §A.5.3 |
/// | `original` | the unchanged store of a file that is mid-update; a sibling `update` box is present | as `manifest`: §A.5.3 places the offset "inside the 'uuid' box of type manifest **or original**", and states that "the original and manifest boxes are identical apart from value of box_purpose" |
/// | `update` | a store holding update manifests only | **not stated by the specification** — probed for, see below |
///
/// # `update`: the specification does not say, so the offset is probed for
///
/// §A.5.3 never describes an `update` box's framing. Its only sentence about that purpose constrains
/// the store's *contents* ("shall only contain update manifests"), not the bytes around it, and the
/// "manifest or original" phrasing above is explained by manifest and original being declared
/// identical to each other rather than by any contrast with `update`. The silence is a gap in the
/// specification, not a prohibition.
///
/// The reference implementation fills that gap in one direction. `c2pa-rs`
/// (`sdk/src/asset_handlers/bmff_io.rs`, whose supported types include `heic`, `heif` and `avif`)
/// writes the 8-byte offset ahead of an `update` store exactly as it does for `manifest` and
/// `original` — zero-filled, an update box having no `merkle` box to point at — and its reader skips
/// those 8 bytes for all three purposes. Mid-update files in circulation therefore carry the offset.
///
/// Rather than pick one reading and mis-locate the store under the other, this crate **probes**: for
/// `update` it looks for the store at offset 8 first and falls back to offset 0 when the `LBox` read
/// there is not a valid bound (zero, below the 8-byte JUMBF header, or overrunning the box). The
/// first candidate yielding a valid bound wins; if neither does, nothing is reported. `manifest` and
/// `original` are *not* probed: the specification states their framing, so a single offset is used.
///
/// ## How strong the probe is, exactly
///
/// Every location decision here — the probed `update` offsets and the single stated offset alike —
/// is settled by `LBox` validity alone, and that discriminator is **content-dependent**, because a
/// JUMBF superbox's interior is itself length-prefixed.
///
/// The mis-bounding shape is therefore the same in every case, and is stated once. A manifest
/// store is `LBox` (bytes 0..4), `TBox` (4..8), then its interior, so reading an `LBox` 8 bytes
/// into a store that does *not* begin with a merkle offset lands past both header fields, on the
/// first interior box's own length — small, plausible and in-bounds, so it can read as a valid
/// bound and be accepted, trimming the reported store to a fragment of itself. A wrong offset does
/// **not** always fail.
///
/// What differs between the purposes is only how a file gets into that state:
///
/// - A **spec-conformant** `manifest` or `original` store is located exactly: §A.5.3 states that
///   its `data` opens with the merkle offset, and that offset is used, not probed. A store written
///   *without* it is out of spec, and is then mis-bounded by the same mechanism rather than
///   rejected — the single offset is not self-checking either.
/// - An `update` store written with the `c2pa-rs` merkle offset is located exactly: offset 8 lands
///   on the store's own `LBox`, and it is tried first.
/// - An `update` store written **without** the offset is the one in-spec layout exposed to the
///   hazard, §A.5.3 having stated no framing for that purpose. No known writer emits it: `c2pa-rs`
///   is the only implementation and it always writes the offset.
///
/// The fallback is still strictly better than unconditionally skipping 8 bytes: it runs only when
/// offset 8 yields no valid bound, so it can rescue a file the fixed offset would have missed and
/// can never spoil one the fixed offset would have got right.
///
/// ## What would make this exact
///
/// A `TBox` check. A store's own header is `LBox` then `jumb`, whereas a wrong offset lands on an
/// interior box carrying its own type — so in every mis-bounding above, the four bytes after the
/// accepted length are not `jumb`. Comparing them would reject the wrong candidate outright.
///
/// That constant *is* traceable to the vendored specification, contrary to what a first reading
/// suggests: §A.3.9 requires a JPEG XL file to carry the store in a "JUMBF (`jumb`) superbox", and
/// §15.12.3.2 calls it "a top level JUMBF box (JUMB)". Both sentences are JPEG XL clauses, and both
/// attribute the box to ISO/IEC 18181-2 clause 9.3 rather than defining it, which is why this crate
/// does not yet assert it: adding the check narrows what is reported, on a trace the specification
/// makes in passing about a different container. That is a deliberate deferral, not an absence of
/// source.
///
/// What ISO/IEC 19566-5 genuinely withholds is a *different* constant — the JUMBF Description Box
/// layout needed to read the manifest store's JUMBF type UUID, which §11.1.4.2 does give as
/// `63327061-0011-0010-8000-00AA00389B71`. Confirming the store by its type UUID, the check the
/// specification itself describes, stays blocked on that document. Either route, or a
/// `c2pa-rs`-generated oracle fixture settling the layout empirically, is tracked as a deferred row
/// in the crate's `STATUS.md`.
///
/// # `merkle`
///
/// A fourth purpose, `merkle`, names an *auxiliary* box holding Merkle-tree hashes; §A.5.3 does not
/// list it among the purposes of a manifest-store box, so a `merkle` box is **not** reported by
/// [`HeifContainer::c2pa`] or [`HeifContainer::c2pa_manifest_stores`], and neither is any other
/// unrecognised `box_purpose` value.
///
/// Non-exhaustive and with permanent discriminants: a later revision may add a variant without a
/// breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum C2paBoxPurpose {
    /// `manifest` — the ordinary manifest store. Its `data` opens with the 8-byte absolute file
    /// offset of the first auxiliary `merkle` box (zero when the file has none).
    Manifest = 0,
    /// `original` — the untouched store of a file being updated; a sibling `update` box is present.
    /// Its `data` is framed exactly as `manifest`'s, merkle offset included.
    Original = 1,
    /// `update` — a store containing update manifests only. The specification does not state
    /// whether the 8-byte merkle offset precedes its store, so both layouts are probed for; see the
    /// [type docs](Self).
    Update = 2,
}

impl C2paBoxPurpose {
    /// Maps the null-terminated `box_purpose` string's bytes to a manifest-store purpose, or `None`
    /// for `merkle` and every unrecognised value.
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        match bytes {
            b"manifest" => Some(Self::Manifest),
            b"original" => Some(Self::Original),
            b"update" => Some(Self::Update),
            _ => None,
        }
    }

    /// The offsets into `data`, in probe order, at which this purpose's manifest store may begin.
    ///
    /// One candidate where §A.5.3 states the framing; two where it is silent (see the
    /// [type docs](Self)).
    const fn store_prefix_candidates(self) -> &'static [usize] {
        match self {
            Self::Manifest | Self::Original => &[MERKLE_OFFSET_LEN],
            Self::Update => &[MERKLE_OFFSET_LEN, 0],
        }
    }
}

/// A C2PA manifest store located in a HEIF file: the store's opaque bytes, its exact byte range in
/// the file, and the `box_purpose` of the `uuid` box that carried it.
///
/// # What bounds the store
///
/// Not the enclosing box length: C2PA 2.4 §A.5.3 permits "zero or more unused padding bytes" after
/// the store. The store is a JUMBF superbox, and a JUMBF box opens with a 4-byte big-endian length
/// (`LBox`) covering the whole box; that length is what bounds the store and what
/// [`bytes`](Self::bytes) is trimmed to.
///
/// The general JUMBF box grammar belongs to ISO/IEC 19566-5, which C2PA 2.4 references but does not
/// restate and which is not vendored here. Within the C2PA specification the `LBox` width and
/// endianness are traceable only *incidentally*: §8.4.2.3, titled "Hashing JUMBF Boxes", describes
/// "a box length (LBox, as a 4-byte big-endian unsigned integer); a box type (TBox, 4-byte big-endian
/// unsigned integer, with a value of `c2sh` (for C2PA salt hash))" while defining that salt box in
/// particular. It therefore evidences the *shape* of a JUMBF header, not the manifest-store
/// superbox's own type code. Only the width and endianness are relied on here. No box type code is
/// read or compared — a deliberate deferral rather than an absence of source, since §A.3.9 does
/// name the superbox `jumb`; see [`C2paBoxPurpose`] for why that check is not yet asserted.
///
/// An `LBox` smaller than the 8-byte header it must itself cover, or one overrunning the enclosing
/// `uuid` box, means the bytes are not a manifest store: nothing is reported for that box, and it is
/// never turned into an error.
///
/// # The range is observability, not an exclusion range
///
/// [`range`](Self::range) is where the store sits in the file — for byte accounting, extraction, and
/// reporting. It is **not** a BMFF hard-binding exclusion range: `c2pa.hash.bmff.v3` excludes content
/// by *box path*, not by byte offset (C2PA 2.4 §18.6, §A.5.6), so computing or checking a BMFF hash
/// from this range would be wrong. Nothing here validates anything.
///
/// Non-exhaustive: a later revision may report more of the box's framing without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paManifestStore<'a> {
    /// The manifest store's bytes, exactly: the box header, the 16-byte user type, the `FullBox`
    /// version and flags, the `box_purpose` string, the merkle offset (when the purpose carries one)
    /// and any trailing padding are all excluded. Opaque — a JUMBF superbox this crate does not
    /// parse beyond its outer `LBox`.
    pub bytes: &'a [u8],
    /// The half-open byte range [`bytes`](Self::bytes) occupies within the input file, so
    /// `range.len() == bytes.len()`.
    pub range: Range<usize>,
    /// The `box_purpose` of the `uuid` box that carried this store.
    pub purpose: C2paBoxPurpose,
}

impl<'a> HeifContainer<'a> {
    /// The first C2PA manifest store in the file, in file order, or `None` if the file carries none.
    ///
    /// A file that is mid-update legitimately carries two stores — an `original` box and an `update`
    /// box (C2PA 2.4 §A.5.3) — and deciding which of them is *active* is a validator's judgement,
    /// not a container reader's. This accessor therefore promises only "the first one"; use
    /// [`c2pa_manifest_stores`](Self::c2pa_manifest_stores) to see them all and their purposes.
    ///
    /// See [`C2paManifestStore`] for exactly what is stripped, what bounds the store, and why the
    /// reported range must not be treated as a BMFF exclusion range.
    #[must_use]
    pub fn c2pa(&self) -> Option<C2paManifestStore<'a>> {
        self.c2pa_manifest_stores().next()
    }

    /// Every C2PA manifest store among the **top-level boxes of the primary stream**, in file order.
    ///
    /// Only *top-level* `uuid` boxes are considered, which is where C2PA 2.4 §A.5.3 puts the box
    /// ("before the first 'mdat' box … after the 'ftyp' box"); a `uuid` box nested inside `meta` is
    /// not a manifest store and is surfaced — as it always was — through
    /// [`unknown_meta_boxes`](Self::unknown_meta_boxes) instead. The actual position of the box is
    /// reported as found and never enforced: a store placed outside the window §A.5.3 mandates is
    /// still reported, with its true range.
    ///
    /// # What is not scanned
    ///
    /// The scan walks [`segments`](Self::segments), which stops emitting [`SegmentKind::Box`] at a
    /// second top-level `ftyp` — from there the rest of the file is one
    /// [`SegmentKind::AppendedStream`] — or at a malformed trailing box, which becomes a
    /// [`SegmentKind::Trailer`]. Bytes inside those two regions are never examined. One real case is
    /// affected: §A.5.3 requires an `update` box to be the last box of the file, so on a motion-photo
    /// HEIC that appends a second whole file, an `update` box sitting after the appended stream is
    /// not found. Reaching into an appended vendor stream is a container-level decision this lens
    /// does not take on its own.
    ///
    /// A top-level `uuid` box whose user type is not [`C2PA_UUID`], whose `FullBox` version or flags
    /// are non-zero, whose `box_purpose` is not one of [`C2paBoxPurpose`]'s, or whose contents are
    /// truncated or self-inconsistent yields no store: this is a lens over bytes that happen to be
    /// present, so a malformed or foreign box yields nothing rather than an error.
    ///
    /// A box that *is* a [`C2PA_UUID`] box and still yields nothing is not silent, though — it is
    /// reported, with the reason, by [`c2pa_summary`](Self::c2pa_summary), so a reader cannot take
    /// an empty iterator here for a file carrying no provenance at all.
    pub fn c2pa_manifest_stores(&self) -> impl Iterator<Item = C2paManifestStore<'a>> + '_ {
        self.content_provenance_boxes()
            .filter_map(|(_, outcome)| outcome.ok())
    }

    /// Every top-level `ContentProvenanceBox`, as its whole box range paired with what reading it
    /// yielded: the manifest store it carries, or the reason it carries none.
    ///
    /// The one scan both public views are built from, so "a store" and "a box that yielded no
    /// store" can never disagree about which boxes were looked at.
    fn content_provenance_boxes(
        &self,
    ) -> impl Iterator<
        Item = (
            Range<usize>,
            Result<C2paManifestStore<'a>, C2paUnreadReason>,
        ),
    > + '_ {
        self.segments()
            .iter()
            .filter_map(|segment| match segment.kind {
                SegmentKind::Box { ty, body } if &ty == b"uuid" => {
                    // `range` spans the header and the body, so `range.end - body.len()` is the absolute
                    // offset of the body — correct for an 8-byte header and a 16-byte largesize one
                    // alike, without the container needing to report the header width.
                    let body_start = segment.range.end.checked_sub(body.len())?;
                    let outcome = parse_content_provenance_box(body, body_start)?;
                    Some((segment.range.clone(), outcome))
                }
                _ => None,
            })
    }
}

/// Classifies one top-level `uuid` box body (starting at absolute offset `body_start`).
///
/// `None` means the box is not a C2PA `ContentProvenanceBox` at all — a foreign vendor `uuid` box,
/// which is no evidence of provenance and must not be reported as one. `Some` means it is one, and
/// carries either the manifest store it holds or the reason it holds none.
fn parse_content_provenance_box(
    body: &[u8],
    body_start: usize,
) -> Option<Result<C2paManifestStore<'_>, C2paUnreadReason>> {
    // §A.5.1.1: the extended type is what makes a `uuid` box a ContentProvenanceBox. A body too
    // short to hold sixteen bytes carries no extended type to match, so it is a foreign box rather
    // than a truncated C2PA one — nothing in it says C2PA.
    if body.get(..C2PA_UUID.len())? != &C2PA_UUID[..] {
        return None;
    }
    Some(read_content_provenance_box(body, body_start))
}

/// Reads a box already known to carry the [`C2PA_UUID`] extended type, per §A.5.1.2 and §A.5.3.
fn read_content_provenance_box(
    body: &[u8],
    body_start: usize,
) -> Result<C2paManifestStore<'_>, C2paUnreadReason> {
    // §A.5.1.2: a FullBox with version 0 and flags 0. `RawBox::payload` strips the user type but not
    // these four bytes, so they are read here.
    let after_uuid = body
        .get(C2PA_UUID.len()..)
        .ok_or(C2paUnreadReason::Truncated)?;
    if after_uuid
        .get(..VERSION_FLAGS_LEN)
        .ok_or(C2paUnreadReason::Truncated)?
        != &[0u8; VERSION_FLAGS_LEN][..]
    {
        return Err(C2paUnreadReason::NotVersionZero);
    }
    let after_full_box = after_uuid
        .get(VERSION_FLAGS_LEN..)
        .ok_or(C2paUnreadReason::Truncated)?;

    // `string box_purpose` — null-terminated, per §A.5.1.2.
    let terminator = after_full_box
        .iter()
        .position(|&b| b == 0)
        .ok_or(C2paUnreadReason::Truncated)?;
    let purpose = C2paBoxPurpose::from_bytes(
        after_full_box
            .get(..terminator)
            .ok_or(C2paUnreadReason::Truncated)?,
    )
    .ok_or(C2paUnreadReason::NotAManifestStorePurpose)?;
    let data = after_full_box
        .get(terminator + 1..)
        .ok_or(C2paUnreadReason::Truncated)?;

    // Where the store begins inside `data`: one fixed offset for `manifest`/`original`, whose framing
    // §A.5.3 states, and two probed in order for `update`, whose framing it does not — see
    // `C2paBoxPurpose`. The first candidate whose `LBox` is a valid bound wins.
    let data_start = body_start + (body.len() - data.len());
    for &prefix in purpose.store_prefix_candidates() {
        if let Some(bytes) = locate_store(data, prefix) {
            let start = data_start + prefix;
            return Ok(C2paManifestStore {
                bytes,
                range: start..start + bytes.len(),
                purpose,
            });
        }
    }
    Err(C2paUnreadReason::NoStoreBound)
}

/// Reads the JUMBF `LBox` sitting `prefix` bytes into `data` and returns the store it bounds, or
/// `None` if there is no valid bound there.
///
/// A JUMBF box opens with a 4-byte big-endian length covering the whole box; that length, not the
/// enclosing `uuid` box, bounds the store, since §A.5.3 allows unused padding bytes after it (see
/// [`C2paManifestStore`] for how far that framing is traceable). A length below the 8-byte header it
/// must itself cover, or one overrunning the bytes actually present, is not a valid bound.
fn locate_store(data: &[u8], prefix: usize) -> Option<&[u8]> {
    let store_and_padding = data.get(prefix..)?;
    let lbox_bytes: [u8; 4] = store_and_padding.get(..4)?.try_into().ok()?;
    let lbox = u32::from_be_bytes(lbox_bytes) as usize;
    if lbox < JUMBF_HEADER_LEN {
        return None;
    }
    // The same `get` rejects an `LBox` that overruns the box.
    store_and_padding.get(..lbox)
}

/// What a report of a located manifest store has to say beside it, because locating one is not
/// validating it.
///
/// C2PA 2.4 §15.12 puts validation on a validator: the signature layer (§13) and the
/// validation-side rules are outside this crate entirely, and `references/c2pa/README.md` records
/// the boundary — gamut locates, bounds and carries a manifest store, holds no COSE, X.509,
/// RFC 3161 or trust-list code, and never reports a validity verdict.
///
/// The sentence exists because the *absence* of a verdict does not read as one. To anyone who has
/// seen a Content Credentials badge, "C2PA: present" printed beside EXIF and ICC reads as
/// *verified*, so a report that lets a reader infer a verdict is a defect rather than a wording
/// preference. [`C2paSummary::report_lines`] therefore carries this text in the line that reports
/// the stores, never as a footnote a reader can skip.
pub const C2PA_NOT_VALIDATED: &str = "gamut locates a C2PA manifest store and never validates it: \
     it checks no signature, no hash binding and no trust list. Validate with c2pa-rs.";

impl C2paBoxPurpose {
    /// The `box_purpose` string this purpose is written as in the file (C2PA 2.4 §A.5.3).
    ///
    /// The exact inverse of the parse: [`from_bytes`](Self::from_bytes) maps these bytes back to
    /// this variant.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Original => "original",
            Self::Update => "update",
        }
    }
}

/// Why a top-level C2PA `ContentProvenanceBox` yielded no manifest store.
///
/// The box is a C2PA box — its extended type is [`C2PA_UUID`] — so the file *does* carry
/// provenance framing; only the store inside it could not be read. That distinction is the reason
/// this type exists: reporting such a box as "no manifest store found" would let a reader infer
/// *no provenance* from a box gamut merely could not read, which is the mirror of the verdict
/// [`C2PA_NOT_VALIDATED`] exists to prevent.
///
/// A `uuid` box whose extended type is **not** [`C2PA_UUID`] is not one of these. §A.5.1.1 makes
/// the extended type the whole test, and an ordinary file carries vendor `uuid` boxes that are no
/// evidence of provenance; a near-miss on the sixteen bytes is a foreign box, not a damaged C2PA
/// one, and is reported as absence.
///
/// Non-exhaustive and with permanent discriminants: a later revision may distinguish a further
/// reason without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum C2paUnreadReason {
    /// The box ends before the framing §A.5.1.2 requires — the `FullBox` version and flags, or the
    /// NUL that terminates `box_purpose`.
    Truncated = 0,
    /// The `FullBox` version or flags are not zero, which §A.5.1.2 fixes them at.
    NotVersionZero = 1,
    /// The `box_purpose` is not one §A.5.3 gives a manifest store: the auxiliary `merkle` box, or a
    /// value this revision does not know.
    NotAManifestStorePurpose = 2,
    /// No valid JUMBF `LBox` bounds a store where this `box_purpose`'s framing puts one — the
    /// length there is zero, below the 8-byte header it must cover, or overruns the box.
    NoStoreBound = 3,
}

impl C2paUnreadReason {
    /// A one-clause explanation, for the line [`C2paSummary::report_lines`] renders.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Truncated => "the box ends before the framing C2PA 2.4 §A.5.1.2 requires",
            Self::NotVersionZero => {
                "its FullBox version or flags are not zero, which C2PA 2.4 §A.5.1.2 fixes them at"
            }
            Self::NotAManifestStorePurpose => {
                "its box_purpose is not one C2PA 2.4 §A.5.3 gives a manifest store"
            }
            Self::NoStoreBound => {
                "no valid JUMBF store length sits where its box_purpose puts the store"
            }
        }
    }
}

/// A top-level C2PA `ContentProvenanceBox` that yielded no manifest store: where the whole box
/// sits, and why nothing was read from it.
///
/// Reported **without its bytes**, exactly as [`C2paStoreSummary`] is and for the same reason.
///
/// Non-exhaustive: a later revision may report more of the box without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paUnreadBox {
    /// The half-open byte range of the whole `uuid` box — header, extended type and all, since
    /// there is no store inside it to bound more tightly.
    pub range: Range<usize>,
    /// Why no manifest store was read from it.
    pub reason: C2paUnreadReason,
}

/// One located manifest store, reported **without its bytes**: where it sits, how big it is, and
/// what the `uuid` box that carried it said it was for.
///
/// The bytes are absent by construction rather than merely unused. They are opaque to this crate,
/// routinely tens or hundreds of kilobytes once a manifest embeds a thumbnail, and rendering them
/// invites exactly the "gamut understands manifests" reading [`C2PA_NOT_VALIDATED`] exists to
/// prevent; a byte range is what a caller hands to `c2pa-rs` or to `dd`. Use
/// [`C2paManifestStore::bytes`] when the bytes themselves are wanted.
///
/// Non-exhaustive: a later revision may report more of the box without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paStoreSummary {
    /// The half-open byte range the store occupies in the file — the same range
    /// [`C2paManifestStore::range`] reports, and just as much *not* a BMFF exclusion range.
    pub range: Range<usize>,
    /// The `box_purpose` of the `uuid` box that carried the store.
    pub purpose: C2paBoxPurpose,
}

impl C2paStoreSummary {
    /// The store's size in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        self.range.len()
    }
}

/// A non-validating report of the C2PA manifest stores a HEIF file carries — presence, and for
/// each store its byte range, its size and its `box_purpose`.
///
/// Built by [`HeifContainer::c2pa_summary`]. It is what a reporting tool prints: the summary
/// carries the reportable facts and [`report_lines`](Self::report_lines) renders them, so every
/// host renders the same words and none can drop the non-validation disclaimer while rewording
/// them. The wording is then pinned once, here, rather than once per host.
///
/// Non-exhaustive: a later revision may report more without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct C2paSummary {
    /// Every located store, in file order. A file mid-update legitimately carries two — an
    /// `original` box and an `update` box (C2PA 2.4 §A.5.3) — and both are listed with their
    /// purposes: which of them is *active* is a validator's judgement, so collapsing them to a
    /// count would hide the one fact that tells them apart.
    pub stores: Vec<C2paStoreSummary>,
    /// Every top-level C2PA box that yielded no store, in file order, with the reason.
    ///
    /// A non-empty list beside empty [`stores`](Self::stores) is the case a report must not
    /// collapse into "none found": the file carries C2PA framing this reader could not read
    /// through, which is not the same fact as carrying none. See [`C2paUnreadReason`].
    pub unread: Vec<C2paUnreadBox>,
}

impl C2paSummary {
    /// Whether the file carries a manifest store at all.
    ///
    /// A store, specifically — `false` here does **not** mean the file carries no provenance, only
    /// that none was read. Check [`unread`](Self::unread) too.
    #[must_use]
    pub fn is_present(&self) -> bool {
        !self.stores.is_empty()
    }

    /// The human-readable report: one headline, then one line per store, then one line per C2PA
    /// box that yielded no store.
    ///
    /// The headline states what was found and carries [`C2PA_NOT_VALIDATED`] inline. Each store
    /// line names its `box_purpose`, its size and its half-open byte range, and repeats "located,
    /// not validated" so a line read on its own still cannot be mistaken for a verdict; each
    /// unread-box line names the box's range and [`C2paUnreadReason::describe`]. Detail lines are
    /// indented two spaces relative to the headline; a caller prefixes its own indent to every
    /// line.
    ///
    /// No store's bytes can appear here — neither [`C2paStoreSummary`] nor [`C2paUnreadBox`] holds
    /// them.
    #[must_use]
    pub fn report_lines(&self) -> Vec<String> {
        let mut lines = vec![self.headline()];
        lines.extend(self.stores.iter().map(|store| {
            format!(
                "  box_purpose \"{purpose}\": {size} bytes at [{start}, {end}) — located, not validated",
                purpose = store.purpose.as_str(),
                size = store.size(),
                start = store.range.start,
                end = store.range.end,
            )
        }));
        lines.extend(self.unread.iter().map(|unread| {
            format!(
                "  unread C2PA box at [{start}, {end}): {reason}",
                start = unread.range.start,
                end = unread.range.end,
                reason = unread.reason.describe(),
            )
        }));
        lines
    }

    /// The report's first line: what the scan found, with [`C2PA_NOT_VALIDATED`] inline.
    ///
    /// Three outcomes, not two. A file carrying C2PA boxes none of which yielded a store is
    /// neither "located" nor "none found": saying "none found" for it would let a reader infer
    /// absence of provenance from bytes gamut merely could not read.
    fn headline(&self) -> String {
        let count = self.stores.len();
        if count > 0 {
            return format!(
                "C2PA: {count} manifest store{plural} located, NOT VALIDATED — {C2PA_NOT_VALIDATED}",
                plural = if count == 1 { "" } else { "s" }
            );
        }
        let unread = self.unread.len();
        if unread > 0 {
            return format!(
                "C2PA: no manifest store could be read, but {unread} C2PA {noun} present — that is \
                 NOT absence of provenance — {C2PA_NOT_VALIDATED}",
                noun = if unread == 1 { "box is" } else { "boxes are" }
            );
        }
        format!(
            "C2PA: no manifest store found in the top-level boxes of the primary stream — \
             {C2PA_NOT_VALIDATED}"
        )
    }
}

impl HeifContainer<'_> {
    /// A non-validating summary of the file's C2PA boxes, in file order: every manifest store,
    /// and every C2PA box that yielded none, with the reason.
    ///
    /// The same scan as [`c2pa_manifest_stores`](Self::c2pa_manifest_stores), with the same reach
    /// and the same silence on a *foreign* `uuid` box, reported without the stores' bytes. It is
    /// strictly the fuller view: a C2PA box that yields no store is dropped by that iterator and
    /// listed in [`unread`](C2paSummary::unread) here. See [`C2paSummary`] for what it is for, and
    /// [`C2PA_NOT_VALIDATED`] for what has to be said beside it.
    #[must_use]
    pub fn c2pa_summary(&self) -> C2paSummary {
        let mut stores = Vec::new();
        let mut unread = Vec::new();
        for (range, outcome) in self.content_provenance_boxes() {
            match outcome {
                Ok(store) => stores.push(C2paStoreSummary {
                    range: store.range,
                    purpose: store.purpose,
                }),
                Err(reason) => unread.push(C2paUnreadBox { range, reason }),
            }
        }
        C2paSummary { stores, unread }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        C2PA_NOT_VALIDATED, C2paBoxPurpose, C2paStoreSummary, C2paSummary, C2paUnreadBox,
        C2paUnreadReason,
    };

    /// A summary of the stores at the given `(start, end, purpose)` triples, and nothing unread.
    fn summary(stores: &[(usize, usize, C2paBoxPurpose)]) -> C2paSummary {
        C2paSummary {
            stores: stores
                .iter()
                .map(|&(start, end, purpose)| C2paStoreSummary {
                    range: start..end,
                    purpose,
                })
                .collect(),
            unread: Vec::new(),
        }
    }

    /// A summary of no stores and the unread C2PA boxes at the given `(start, end, reason)` triples.
    fn unread_summary(boxes: &[(usize, usize, C2paUnreadReason)]) -> C2paSummary {
        C2paSummary {
            stores: Vec::new(),
            unread: boxes
                .iter()
                .map(|&(start, end, reason)| C2paUnreadBox {
                    range: start..end,
                    reason,
                })
                .collect(),
        }
    }

    #[test]
    fn a_purpose_renders_as_the_box_purpose_string_it_parses_from() {
        for purpose in [
            C2paBoxPurpose::Manifest,
            C2paBoxPurpose::Original,
            C2paBoxPurpose::Update,
        ] {
            assert_eq!(
                C2paBoxPurpose::from_bytes(purpose.as_str().as_bytes()),
                Some(purpose),
                "{} must parse back to the purpose that renders it",
                purpose.as_str()
            );
        }
    }

    #[test]
    fn a_stores_size_is_the_length_of_its_range() {
        // Not its end offset: a store never starts at zero, the box framing preceding it.
        let one = summary(&[(61, 90, C2paBoxPurpose::Manifest)]);
        assert_eq!(one.stores[0].size(), 29);
    }

    #[test]
    fn the_disclaimer_names_every_check_gamut_omits_and_what_makes_them() {
        for fragment in [
            "signature",
            "hash binding",
            "trust list",
            "c2pa-rs",
            "never validates",
        ] {
            assert!(
                C2PA_NOT_VALIDATED.contains(fragment),
                "the disclaimer must mention {fragment}: {C2PA_NOT_VALIDATED}"
            );
        }
    }

    #[test]
    fn an_absent_store_is_reported_as_absent_and_still_says_gamut_never_validates() {
        let none = C2paSummary::default();
        assert!(!none.is_present());
        assert_eq!(
            none.report_lines(),
            vec![format!(
                "C2PA: no manifest store found in the top-level boxes of the primary stream — \
                 {C2PA_NOT_VALIDATED}"
            )]
        );
    }

    #[test]
    fn one_store_is_reported_with_its_purpose_size_and_half_open_range() {
        let one = summary(&[(61, 90, C2paBoxPurpose::Manifest)]);
        assert!(one.is_present());
        assert_eq!(
            one.report_lines(),
            vec![
                format!("C2PA: 1 manifest store located, NOT VALIDATED — {C2PA_NOT_VALIDATED}"),
                "  box_purpose \"manifest\": 29 bytes at [61, 90) — located, not validated"
                    .to_owned(),
            ]
        );
    }

    #[test]
    fn a_c2pa_box_that_yielded_no_store_is_reported_instead_of_absence() {
        // The mirror of the verdict `C2PA_NOT_VALIDATED` prevents: this file plainly carries C2PA
        // framing, so the wording a file with *no* C2PA box gets would let a reader infer absence
        // of provenance from bytes gamut could not read.
        let one = unread_summary(&[(16, 106, C2paUnreadReason::NotVersionZero)]);
        assert!(!one.is_present());
        assert_eq!(
            one.report_lines(),
            vec![
                format!(
                    "C2PA: no manifest store could be read, but 1 C2PA box is present — that is \
                     NOT absence of provenance — {C2PA_NOT_VALIDATED}"
                ),
                format!(
                    "  unread C2PA box at [16, 106): {}",
                    C2paUnreadReason::NotVersionZero.describe()
                ),
            ]
        );
    }

    #[test]
    fn several_c2pa_boxes_that_yielded_no_store_are_counted_in_the_plural() {
        // Two boxes differing in range *and* reason: a headline built from the wrong count, or a
        // line built from the wrong element, is visible here.
        let two = unread_summary(&[
            (16, 40, C2paUnreadReason::Truncated),
            (40, 120, C2paUnreadReason::NoStoreBound),
        ]);
        assert_eq!(
            two.report_lines(),
            vec![
                format!(
                    "C2PA: no manifest store could be read, but 2 C2PA boxes are present — that is \
                     NOT absence of provenance — {C2PA_NOT_VALIDATED}"
                ),
                format!(
                    "  unread C2PA box at [16, 40): {}",
                    C2paUnreadReason::Truncated.describe()
                ),
                format!(
                    "  unread C2PA box at [40, 120): {}",
                    C2paUnreadReason::NoStoreBound.describe()
                ),
            ]
        );
    }

    #[test]
    fn a_located_store_headlines_the_report_even_beside_an_unread_box() {
        // Precedence: one store read is what the headline states, and the box that yielded nothing
        // is still listed under it rather than replacing the headline or being dropped.
        let mixed = C2paSummary {
            stores: vec![C2paStoreSummary {
                range: 61..90,
                purpose: C2paBoxPurpose::Manifest,
            }],
            unread: vec![C2paUnreadBox {
                range: 90..150,
                reason: C2paUnreadReason::NotAManifestStorePurpose,
            }],
        };
        assert_eq!(
            mixed.report_lines(),
            vec![
                format!("C2PA: 1 manifest store located, NOT VALIDATED — {C2PA_NOT_VALIDATED}"),
                "  box_purpose \"manifest\": 29 bytes at [61, 90) — located, not validated"
                    .to_owned(),
                format!(
                    "  unread C2PA box at [90, 150): {}",
                    C2paUnreadReason::NotAManifestStorePurpose.describe()
                ),
            ]
        );
    }

    #[test]
    fn every_unread_reason_describes_itself_distinctly() {
        // A reason a reader cannot tell from another reason is not a reason: the whole point of
        // reporting the box is naming what stopped it being read.
        let reasons = [
            C2paUnreadReason::Truncated,
            C2paUnreadReason::NotVersionZero,
            C2paUnreadReason::NotAManifestStorePurpose,
            C2paUnreadReason::NoStoreBound,
        ];
        for (i, reason) in reasons.iter().enumerate() {
            assert!(
                !reason.describe().is_empty(),
                "{reason:?} must describe itself"
            );
            for other in &reasons[i + 1..] {
                assert_ne!(
                    reason.describe(),
                    other.describe(),
                    "{reason:?} and {other:?} must read differently"
                );
            }
        }
    }

    #[test]
    fn a_mid_update_file_lists_both_stores_with_their_own_purposes() {
        // Two stores differing in size, offset *and* purpose: a line built from the wrong element,
        // or a purpose read from the wrong store, changes the output.
        let two = summary(&[
            (61, 90, C2paBoxPurpose::Original),
            (131, 172, C2paBoxPurpose::Update),
        ]);
        assert_eq!(
            two.report_lines(),
            vec![
                format!("C2PA: 2 manifest stores located, NOT VALIDATED — {C2PA_NOT_VALIDATED}"),
                "  box_purpose \"original\": 29 bytes at [61, 90) — located, not validated"
                    .to_owned(),
                "  box_purpose \"update\": 41 bytes at [131, 172) — located, not validated"
                    .to_owned(),
            ]
        );
    }
}
