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
use core::iter::Peekable;
use core::ops::Range;
use core::slice;

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
    /// present, so a malformed box or one of another extended type yields nothing rather than an
    /// error.
    ///
    /// A box that *is* a [`C2PA_UUID`] box and still yields nothing is not silent, though — it is
    /// reported, with the reason, by [`c2pa_summary`](Self::c2pa_summary), so a reader cannot take
    /// an empty iterator here for a file carrying no provenance at all.
    pub fn c2pa_manifest_stores(&self) -> impl Iterator<Item = C2paManifestStore<'a>> + '_ {
        self.top_level_uuid_boxes()
            .filter_map(|(_, _, classified)| match classified {
                TopLevelUuidBox::ContentProvenance(outcome) => outcome.ok(),
                TopLevelUuidBox::OtherExtendedType => None,
            })
    }

    /// Every top-level `uuid` box, as its whole box range, where it sits relative to the file's
    /// media data, and what it turned out to be.
    ///
    /// The one scan every public view is built from, so "a store", "a box that yielded no store"
    /// and "a `uuid` box of some other extended type" can never disagree about which boxes were
    /// looked at.
    ///
    /// The position is taken from the walk itself rather than by comparing offsets: the boundary is
    /// simply whether a media-data box has already been passed when this box is reached.
    fn top_level_uuid_boxes(
        &self,
    ) -> impl Iterator<Item = (Range<usize>, C2paBoxPosition, TopLevelUuidBox<'a>)> + '_ {
        self.segments()
            .iter()
            .scan(C2paBoxPosition::BeforeMediaData, |position, segment| {
                let entry = match segment.kind {
                    SegmentKind::Box { ty, .. } if &ty == b"mdat" => {
                        *position = C2paBoxPosition::AfterMediaData;
                        None
                    }
                    SegmentKind::Box { ty, body } if &ty == b"uuid" => {
                        // `range` spans the header and the body, so `range.end - body.len()` is the
                        // absolute offset of the body — correct for an 8-byte header and a 16-byte
                        // largesize one alike, without the container needing to report the header
                        // width.
                        segment.range.end.checked_sub(body.len()).map(|body_start| {
                            (
                                segment.range.clone(),
                                *position,
                                classify_uuid_box(body, body_start),
                            )
                        })
                    }
                    _ => None,
                };
                // `scan` stops at the first `None` it yields, so every segment yields `Some`; the
                // inner `Option` is what filters.
                Some(entry)
            })
            .flatten()
    }
}

/// What one top-level `uuid` box turned out to be.
enum TopLevelUuidBox<'a> {
    /// A C2PA `ContentProvenanceBox` — its extended type is [`C2PA_UUID`] — and what reading it
    /// yielded: the manifest store it carries, or the reason it carries none.
    ContentProvenance(Result<C2paManifestStore<'a>, C2paUnreadReason>),
    /// A `uuid` box carrying some other extended type. §A.5.1.1 makes the extended type the whole
    /// test, so this is not C2PA framing however near the miss.
    OtherExtendedType,
}

/// Classifies one top-level `uuid` box body (starting at absolute offset `body_start`).
///
/// [`TopLevelUuidBox::OtherExtendedType`] means the box is not a C2PA `ContentProvenanceBox` at all
/// — a vendor `uuid` box, which is no evidence of provenance and must not be reported as one.
fn classify_uuid_box(body: &[u8], body_start: usize) -> TopLevelUuidBox<'_> {
    // §A.5.1.1: the extended type is what makes a `uuid` box a ContentProvenanceBox.
    //
    // The short-body arm is unreachable, not a classification: `gamut_isobmff::BoxReader::next_box`
    // rejects a `uuid` box whose body cannot hold its complete 16-byte user type with a fatal
    // "truncated uuid user type", so `HeifContainer::parse` fails and no summary is produced at
    // all. The split is written to be total anyway, because this function must not depend on a
    // guarantee its own signature does not carry.
    let Some((extended_type, after_uuid)) = body.split_at_checked(C2PA_UUID.len()) else {
        return TopLevelUuidBox::OtherExtendedType;
    };
    if extended_type != &C2PA_UUID[..] {
        return TopLevelUuidBox::OtherExtendedType;
    }
    TopLevelUuidBox::ContentProvenance(read_content_provenance_box(
        after_uuid,
        body_start + C2PA_UUID.len(),
    ))
}

/// Reads what follows the [`C2PA_UUID`] extended type of a box already known to carry it, per
/// §A.5.1.2 and §A.5.3. `after_uuid_start` is that slice's absolute offset in the file.
fn read_content_provenance_box(
    after_uuid: &[u8],
    after_uuid_start: usize,
) -> Result<C2paManifestStore<'_>, C2paUnreadReason> {
    // §A.5.1.2: a FullBox with version 0 and flags 0. `RawBox::payload` strips the user type but not
    // these four bytes, so they are read here.
    let (version_flags, after_full_box) = after_uuid
        .split_at_checked(VERSION_FLAGS_LEN)
        .ok_or(C2paUnreadReason::Truncated)?;
    if version_flags != &[0u8; VERSION_FLAGS_LEN][..] {
        return Err(C2paUnreadReason::NotVersionZero);
    }

    // `string box_purpose` — null-terminated, per §A.5.1.2. Splitting on the NUL yields the string
    // and everything after it; a body with no NUL yields only a first part, and that missing second
    // part is the truncation. `splitn` always yields a first part, empty slice included.
    let mut purpose_and_data = after_full_box.splitn(2, |&b| b == 0);
    let purpose_bytes = purpose_and_data.next().unwrap_or_default();
    let data = purpose_and_data.next().ok_or(C2paUnreadReason::Truncated)?;
    let purpose = C2paBoxPurpose::from_bytes(purpose_bytes)
        .ok_or(C2paUnreadReason::NotAManifestStorePurpose)?;

    // Where the store begins inside `data`: one fixed offset for `manifest`/`original`, whose framing
    // §A.5.3 states, and two probed in order for `update`, whose framing it does not — see
    // `C2paBoxPurpose`. The first candidate whose `LBox` is a valid bound wins.
    let data_start = after_uuid_start + (after_uuid.len() - data.len());
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
/// evidence of provenance; a near-miss on the sixteen bytes is a box of another type, not a damaged
/// C2PA one. It is never given a reason — the specification has no notion of an approximate
/// extended type — but it is not passed over in silence either: it is counted in
/// [`C2paSummary::other_uuid_boxes`], because a report in which a one-byte miss and no box at all
/// read identically hides exactly the file a reader most needs to look at.
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

/// Where a top-level `uuid` box sits relative to the file's media data — a **positional fact about
/// bytes**, and never a verdict.
///
/// C2PA 2.4 §A.5.3 places the box carrying a manifest store "before the first 'mdat' box in the
/// file and before any 'moov' box in the file", and after the `ftyp`. The box is reported wherever
/// it is found and its position is never enforced, but a box sitting past that boundary is worth
/// stating for the same reason a byte range is: it is where an *appended* box lands, and a reader
/// deciding what to hand a validator cannot see it from a range alone.
///
/// The boundary this crate can observe is the first `mdat` alone. §A.5.3's other one, `moov`, is a
/// movie box, and [`HeifContainer::parse`] refuses a file carrying a top-level one before any of
/// this runs — image sequences and tracks are out of the workspace's scope — so a `moov` test here
/// would be a branch no parsed file could take.
///
/// Stating it is not judging it, and deliberately so. §A.5.3 separately requires the `update` box
/// of a mid-update file to "exist as the last box of the file", which on a file with media data
/// puts it past this boundary by the specification's own instruction. So
/// [`AfterMediaData`](Self::AfterMediaData) is neither a violation nor a validity finding: it says
/// only where the bytes are.
///
/// Non-exhaustive and with permanent discriminants: a later revision may distinguish a further
/// position without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum C2paBoxPosition {
    /// The box was reached before any `mdat` box — the window §A.5.3 places a manifest-store box
    /// in. A file with no `mdat` at all reports this too.
    BeforeMediaData = 0,
    /// An `mdat` box was already passed when this box was reached.
    AfterMediaData = 1,
}

impl C2paBoxPosition {
    /// The clause a report adds to this box's line, or `None` when the box sits where §A.5.3 puts
    /// one and its position adds nothing to what the range already says.
    ///
    /// Worded as a position, not as a fault: see the [type docs](Self) for why an `update` box past
    /// the boundary is exactly what §A.5.3 asks for.
    #[must_use]
    pub const fn note(self) -> Option<&'static str> {
        match self {
            Self::AfterMediaData => Some("its box begins after the first mdat box"),
            _ => None,
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
    /// Where the box sits relative to the file's media data.
    pub position: C2paBoxPosition,
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
    /// Where the `uuid` box that carried the store sits relative to the file's media data.
    pub position: C2paBoxPosition,
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
    /// How many top-level `uuid` boxes carried an extended type other than [`C2PA_UUID`].
    ///
    /// A **count of bytes present**, not a provenance claim. §A.5.1.1 makes the extended type the
    /// whole test, so none of these boxes is C2PA framing and none of them may be reported as
    /// damaged C2PA framing — an ordinary file carries vendor `uuid` boxes.
    ///
    /// It is counted because otherwise a file whose only `uuid` box is a *single byte* off the
    /// C2PA type — what a signed file corrupted in transit looks like — would be indistinguishable
    /// from a file with no such box at all: same lists, same lines, same words. Reporting the count
    /// says what is true about the bytes and leaves the reading to a validator. No range is kept
    /// and no line is emitted per box: the boxes are not this crate's subject, and the specification
    /// has no notion of an approximate extended type.
    ///
    /// # What the count cannot tell you
    ///
    /// Which kind of box it counted. A corrupted C2PA extended type and an ordinary vendor `uuid`
    /// box are the *same* observation to this field — a box whose sixteen bytes are not
    /// [`C2PA_UUID`] — and a file carrying one of each renders exactly as a file carrying two of
    /// either. Nothing here narrows that: telling them apart would need a notion of an approximate
    /// extended type, which §A.5.1.1 does not have. So the count separates a file with such a box
    /// from a file with none, and nothing finer; a non-zero count on ordinary camera output is the
    /// expected reading, not a signal.
    pub other_uuid_boxes: usize,
}

impl C2paSummary {
    /// Whether the file carries a manifest store this reader could **read**.
    ///
    /// # A host that reports this predicate on its own reintroduces the defect
    ///
    /// `false` here is not "no provenance", and it is not even "no C2PA box". A file whose only
    /// `ContentProvenanceBox` has a non-zero `FullBox` version — among other things, what a signed
    /// file corrupted in transit looks like — plainly carries C2PA framing and still answers
    /// `false`, because no *store* was read from it. Printing "C2PA: none" from this alone is
    /// exactly the inference [`unread`](Self::unread) exists to prevent, and it is why the report
    /// is rendered by [`report_lines`](Self::report_lines) rather than assembled per host: those
    /// lines keep all three outcomes apart and this boolean cannot.
    ///
    /// Read [`unread`](Self::unread) and [`other_uuid_boxes`](Self::other_uuid_boxes) beside it, or
    /// render the report instead of this. Whether a two-valued accessor should answer a
    /// three-valued question at all is issue #597.
    #[must_use]
    pub fn is_present(&self) -> bool {
        !self.stores.is_empty()
    }

    /// The whole human-readable report, uncapped: [`summary_lines`](Self::summary_lines) followed
    /// by every [`detail_lines`](Self::detail_lines) entry.
    ///
    /// A host printing to a terminal should cap the detail lines instead — the number of them is
    /// chosen by the input, since §A.5.3 permits any number of these boxes — and
    /// [`detail_line_count`](Self::detail_line_count) is the true total to report beside a capped
    /// list. This method exists for a host that wants everything.
    ///
    /// No store's bytes can appear here — neither [`C2paStoreSummary`] nor [`C2paUnreadBox`] holds
    /// them.
    #[must_use]
    pub fn report_lines(&self) -> Vec<String> {
        let mut lines = self.summary_lines();
        lines.extend(self.detail_lines());
        lines
    }

    /// The report's head: the headline, and — when the file carries any — the count of top-level
    /// `uuid` boxes whose extended type is not C2PA's.
    ///
    /// **At most two lines, whatever the file holds**, which is what makes this the part a host
    /// prints unconditionally. The headline states every non-empty category and carries
    /// [`C2PA_NOT_VALIDATED`] inline; the second line is a byte count, worded so it cannot be read
    /// as a provenance claim (see [`other_uuid_boxes`](Self::other_uuid_boxes)).
    ///
    /// Neither line is indented, because neither is a list entry: they are the head a host prints
    /// above whatever it shows of [`detail_lines`](Self::detail_lines), which *is* indented.
    #[must_use]
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![self.headline()];
        if self.other_uuid_boxes > 0 {
            lines.push(format!(
                "top-level uuid boxes of another extended type: {count} (not the C2PA one; a uuid \
                 box is not provenance framing)",
                count = self.other_uuid_boxes,
            ));
        }
        lines
    }

    /// One line per top-level C2PA box — a located store or a box that yielded none — **in true
    /// file order**, the two kinds interleaved exactly as the file carries them.
    ///
    /// # Why the order is part of the contract
    ///
    /// A host that caps this list keeps a *prefix* of it. Grouped by kind, one budget can silence
    /// a whole category: twenty legal stores ahead of one unreadable box render no unread line at
    /// all, and which category gets silenced would be decided by this rendering rather than by the
    /// file. In file order the cut is category-blind — it hides the last boxes, whatever they are —
    /// and [`summary_lines`](Self::summary_lines) names every non-empty category regardless, so no
    /// cap can hide that a category exists.
    ///
    /// Each store line names its `box_purpose`, its size and its half-open byte range, and repeats
    /// "located, not validated" so a line read on its own still cannot be mistaken for a verdict;
    /// each unread-box line names the box's range and [`C2paUnreadReason::describe`]. Either kind
    /// carries [`C2paBoxPosition::note`] when the box sits past the file's media data. Lines are
    /// indented two spaces relative to the headline; a caller prefixes its own indent to every one.
    ///
    /// Lazy, so a host that caps the list at N builds N lines rather than one per box in a file
    /// that chose how many to carry. [`detail_line_count`](Self::detail_line_count) is how many
    /// there are in total.
    pub fn detail_lines(&self) -> impl Iterator<Item = String> + '_ {
        DetailLines {
            stores: self.stores.iter().peekable(),
            unread: self.unread.iter().peekable(),
        }
    }

    /// How many lines [`detail_lines`](Self::detail_lines) yields — one per store plus one per
    /// unread box — without building any of them.
    #[must_use]
    pub fn detail_line_count(&self) -> usize {
        self.stores.len() + self.unread.len()
    }

    /// The report's first line: what the scan found, with [`C2PA_NOT_VALIDATED`] inline.
    ///
    /// **Every non-empty category is named, not just the first.** A file carrying stores *and*
    /// boxes no store could be read from says both, in this one line. That matters because
    /// [`detail_lines`](Self::detail_lines) is what a host caps, and a category can fall wholly
    /// past the cut; the headline is the line a reader cannot lose, so it is where the existence
    /// of a category has to be stated.
    ///
    /// Four outcomes, not two. A file carrying C2PA boxes none of which yielded a store is neither
    /// "located" nor "none found": saying "none found" for it would let a reader infer absence of
    /// provenance from bytes gamut merely could not read.
    fn headline(&self) -> String {
        let stores = self.stores.len();
        let unread = self.unread.len();
        match (stores, unread) {
            (0, 0) => format!(
                "C2PA: no manifest store found in the top-level boxes of the primary stream — \
                 {C2PA_NOT_VALIDATED}"
            ),
            (0, _) => format!(
                "C2PA: no manifest store could be read, but {present} — that is NOT absence of \
                 provenance — {C2PA_NOT_VALIDATED}",
                present = unread_clause(unread),
            ),
            (_, 0) => format!(
                "C2PA: {located} — {C2PA_NOT_VALIDATED}",
                located = located_clause(stores),
            ),
            _ => format!(
                "C2PA: {located}, and {present} from which no store could be read — a box gamut \
                 could not read through is NOT absence of provenance — {C2PA_NOT_VALIDATED}",
                located = located_clause(stores),
                present = unread_clause(unread),
            ),
        }
    }
}

/// The headline's clause for the located stores, written once so both headlines that name them
/// agree. Never called with zero.
fn located_clause(count: usize) -> String {
    format!(
        "{count} manifest store{plural} located, NOT VALIDATED",
        plural = if count == 1 { "" } else { "s" }
    )
}

/// The headline's clause for the C2PA boxes that yielded no store, written once so both headlines
/// that name them agree. Never called with zero.
fn unread_clause(count: usize) -> String {
    format!(
        "{count} C2PA {noun} present",
        noun = if count == 1 { "box is" } else { "boxes are" }
    )
}

/// [`C2paSummary::detail_lines`]'s iterator: the store list and the unread list merged back into
/// the file order they were both built in.
///
/// Merging on `range.start` is exact rather than approximate. Different top-level `uuid` boxes
/// occupy disjoint byte ranges, and a store's range lies inside the box that carried it, so the
/// lower start is always the earlier box. A tie is not reachable from a parsed file — a store
/// begins strictly after its box's header — and resolves to the store, so the iterator is total
/// for a summary assembled by hand as well.
struct DetailLines<'a> {
    stores: Peekable<slice::Iter<'a, C2paStoreSummary>>,
    unread: Peekable<slice::Iter<'a, C2paUnreadBox>>,
}

impl Iterator for DetailLines<'_> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        let store_at = self.stores.peek().map(|store| store.range.start);
        let unread_at = self.unread.peek().map(|unread| unread.range.start);
        let store_first = match (store_at, unread_at) {
            (Some(store), Some(unread)) => store <= unread,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if store_first {
            self.stores.next().map(store_line)
        } else {
            self.unread.next().map(unread_line)
        }
    }
}

/// The report line for one located store.
fn store_line(store: &C2paStoreSummary) -> String {
    format!(
        "  box_purpose \"{purpose}\": {size} bytes at [{start}, {end}) — located, not validated{note}",
        purpose = store.purpose.as_str(),
        size = store.size(),
        start = store.range.start,
        end = store.range.end,
        note = position_note(store.position),
    )
}

/// The report line for one C2PA box that yielded no store.
fn unread_line(unread: &C2paUnreadBox) -> String {
    format!(
        "  unread C2PA box at [{start}, {end}): {reason}{note}",
        start = unread.range.start,
        end = unread.range.end,
        reason = unread.reason.describe(),
        note = position_note(unread.position),
    )
}

/// The trailing clause a line carries for its box's position, or nothing when there is none to add.
fn position_note(position: C2paBoxPosition) -> String {
    position
        .note()
        .map_or_else(String::new, |note| format!("; {note}"))
}

impl HeifContainer<'_> {
    /// A non-validating summary of the file's C2PA boxes, in file order: every manifest store,
    /// and every C2PA box that yielded none, with the reason.
    ///
    /// The same scan as [`c2pa_manifest_stores`](Self::c2pa_manifest_stores), with the same reach,
    /// reported without the stores' bytes. It is
    /// strictly the fuller view: a C2PA box that yields no store is dropped by that iterator and
    /// listed in [`unread`](C2paSummary::unread) here, and a top-level `uuid` box of some other
    /// extended type — invisible to both, and to any report, before this — is counted in
    /// [`other_uuid_boxes`](C2paSummary::other_uuid_boxes). See [`C2paSummary`] for what it is for,
    /// and [`C2PA_NOT_VALIDATED`] for what has to be said beside it.
    #[must_use]
    pub fn c2pa_summary(&self) -> C2paSummary {
        let mut stores = Vec::new();
        let mut unread = Vec::new();
        let mut other_uuid_boxes = 0;
        for (range, position, classified) in self.top_level_uuid_boxes() {
            match classified {
                TopLevelUuidBox::ContentProvenance(Ok(store)) => stores.push(C2paStoreSummary {
                    range: store.range,
                    purpose: store.purpose,
                    position,
                }),
                TopLevelUuidBox::ContentProvenance(Err(reason)) => unread.push(C2paUnreadBox {
                    range,
                    reason,
                    position,
                }),
                TopLevelUuidBox::OtherExtendedType => other_uuid_boxes += 1,
            }
        }
        C2paSummary {
            stores,
            unread,
            other_uuid_boxes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        C2PA_NOT_VALIDATED, C2paBoxPosition, C2paBoxPurpose, C2paStoreSummary, C2paSummary,
        C2paUnreadBox, C2paUnreadReason,
    };

    /// A summary of the stores at the given `(start, end, purpose)` triples, all in the window
    /// §A.5.3 mandates, and nothing unread.
    fn summary(stores: &[(usize, usize, C2paBoxPurpose)]) -> C2paSummary {
        C2paSummary {
            stores: stores
                .iter()
                .map(|&(start, end, purpose)| C2paStoreSummary {
                    range: start..end,
                    purpose,
                    position: C2paBoxPosition::BeforeMediaData,
                })
                .collect(),
            unread: Vec::new(),
            other_uuid_boxes: 0,
        }
    }

    /// A summary of no stores and the unread C2PA boxes at the given `(start, end, reason)` triples,
    /// all in the window §A.5.3 mandates.
    fn unread_summary(boxes: &[(usize, usize, C2paUnreadReason)]) -> C2paSummary {
        C2paSummary {
            stores: Vec::new(),
            unread: boxes
                .iter()
                .map(|&(start, end, reason)| C2paUnreadBox {
                    range: start..end,
                    reason,
                    position: C2paBoxPosition::BeforeMediaData,
                })
                .collect(),
            other_uuid_boxes: 0,
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
    fn the_headline_states_both_categories_when_the_file_carries_both() {
        // A headline that stopped at the first non-empty category was hideable: a host caps the
        // detail lines, so a category that falls wholly past the cut would leave no trace at all
        // and the report would read as a clean bill of health. Naming both here is what no cap can
        // reach.
        let mixed = C2paSummary {
            stores: vec![C2paStoreSummary {
                range: 61..90,
                purpose: C2paBoxPurpose::Manifest,
                position: C2paBoxPosition::BeforeMediaData,
            }],
            unread: vec![C2paUnreadBox {
                range: 90..150,
                reason: C2paUnreadReason::NotAManifestStorePurpose,
                position: C2paBoxPosition::BeforeMediaData,
            }],
            other_uuid_boxes: 0,
        };
        assert_eq!(
            mixed.report_lines(),
            vec![
                format!(
                    "C2PA: 1 manifest store located, NOT VALIDATED, and 1 C2PA box is present from \
                     which no store could be read — a box gamut could not read through is NOT \
                     absence of provenance — {C2PA_NOT_VALIDATED}"
                ),
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
    fn the_detail_lines_interleave_the_two_kinds_in_file_order() {
        // Grouped by kind, a host's cap silences whichever kind renders last, and which kind that
        // is would be this rendering's choice rather than the file's. In file order the cut is
        // category-blind: it hides the last boxes, whatever they are.
        let mut mixed = summary(&[
            (61, 90, C2paBoxPurpose::Manifest),
            (231, 260, C2paBoxPurpose::Update),
        ]);
        mixed.unread = vec![
            C2paUnreadBox {
                range: 16..40,
                reason: C2paUnreadReason::Truncated,
                position: C2paBoxPosition::BeforeMediaData,
            },
            C2paUnreadBox {
                range: 130..200,
                reason: C2paUnreadReason::NoStoreBound,
                position: C2paBoxPosition::BeforeMediaData,
            },
        ];
        assert_eq!(
            mixed.detail_lines().collect::<Vec<_>>(),
            vec![
                format!(
                    "  unread C2PA box at [16, 40): {}",
                    C2paUnreadReason::Truncated.describe()
                ),
                "  box_purpose \"manifest\": 29 bytes at [61, 90) — located, not validated"
                    .to_owned(),
                format!(
                    "  unread C2PA box at [130, 200): {}",
                    C2paUnreadReason::NoStoreBound.describe()
                ),
                "  box_purpose \"update\": 29 bytes at [231, 260) — located, not validated"
                    .to_owned(),
            ]
        );
    }

    #[test]
    fn a_store_and_an_unread_box_starting_together_render_the_store_first() {
        // Unreachable from a parsed file — a store begins strictly after the header of the box
        // carrying it — but the merge must be total for a summary assembled by hand, so which side
        // wins a tie is pinned rather than left to the shape of the comparison.
        let mut tied = summary(&[(61, 90, C2paBoxPurpose::Manifest)]);
        tied.unread.push(C2paUnreadBox {
            range: 61..150,
            reason: C2paUnreadReason::Truncated,
            position: C2paBoxPosition::BeforeMediaData,
        });
        let lines: Vec<String> = tied.detail_lines().collect();
        assert_eq!(
            lines,
            vec![
                "  box_purpose \"manifest\": 29 bytes at [61, 90) — located, not validated"
                    .to_owned(),
                format!(
                    "  unread C2PA box at [61, 150): {}",
                    C2paUnreadReason::Truncated.describe()
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

    #[test]
    fn uuid_boxes_of_another_extended_type_are_reported_as_a_count_of_bytes_present() {
        // The near miss and the absent box were byte-identical reports before this line existed,
        // and a signed file corrupted in transit is exactly the first. The line states a count and
        // disclaims provenance in the same breath: it must not read as C2PA framing.
        let none = C2paSummary {
            other_uuid_boxes: 3,
            ..C2paSummary::default()
        };
        let lines = none.report_lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        // Unindented, because it is part of the head and not a detail entry, and it names what it
        // counts without an "other" whose antecedent this file does not have.
        assert_eq!(
            lines[1],
            "top-level uuid boxes of another extended type: 3 (not the C2PA one; a uuid box is \
             not provenance framing)"
        );
        // The headline is still the absence one: no store was found, and none of these is one.
        assert!(lines[0].contains("no manifest store found"), "{lines:?}");
    }

    #[test]
    fn a_file_with_no_other_uuid_boxes_gets_no_count_line() {
        // A count of zero is not a fact worth a line: every ordinary file would carry it.
        assert_eq!(C2paSummary::default().summary_lines().len(), 1);
    }

    #[test]
    fn a_store_past_the_files_media_data_says_so_on_its_own_line() {
        // The positional fact, stated and not judged: an appended box is the adversarial shape, and
        // a reader cannot see it from the byte range alone.
        let mut appended = summary(&[(61, 90, C2paBoxPurpose::Manifest)]);
        appended.stores[0].position = C2paBoxPosition::AfterMediaData;
        assert_eq!(
            appended.report_lines()[1],
            "  box_purpose \"manifest\": 29 bytes at [61, 90) — located, not validated; its box \
             begins after the first mdat box"
        );
    }

    #[test]
    fn an_unread_box_past_the_files_media_data_says_so_too() {
        let mut appended = unread_summary(&[(16, 106, C2paUnreadReason::NoStoreBound)]);
        appended.unread[0].position = C2paBoxPosition::AfterMediaData;
        assert_eq!(
            appended.report_lines()[1],
            format!(
                "  unread C2PA box at [16, 106): {}; its box begins after the first mdat box",
                C2paUnreadReason::NoStoreBound.describe()
            )
        );
    }

    #[test]
    fn a_box_in_the_mandated_window_adds_no_positional_clause() {
        // The clause is a flag, not a field: stating a position for every box would bury the one
        // that is worth reading.
        assert_eq!(C2paBoxPosition::BeforeMediaData.note(), None);
    }

    #[test]
    fn the_capped_and_uncapped_renderings_are_the_same_lines() {
        // A host caps the detail lines and prints the head unconditionally, so the two halves must
        // reassemble into exactly what `report_lines` gives, and the count must match the lines.
        let mut mixed = summary(&[
            (61, 90, C2paBoxPurpose::Original),
            (131, 172, C2paBoxPurpose::Update),
        ]);
        mixed.unread.push(C2paUnreadBox {
            range: 172..200,
            reason: C2paUnreadReason::Truncated,
            position: C2paBoxPosition::BeforeMediaData,
        });
        mixed.other_uuid_boxes = 1;

        let detail: Vec<String> = mixed.detail_lines().collect();
        assert_eq!(mixed.detail_line_count(), detail.len());
        assert_eq!(detail.len(), 3);
        let mut reassembled = mixed.summary_lines();
        reassembled.extend(detail);
        assert_eq!(mixed.report_lines(), reassembled);
    }
}
