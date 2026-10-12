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
//!
//! That framing is not this crate's: it is read through [`gamut_isobmff::C2paUuidBox`], the one
//! lens `gamut-avif` reads through too (#505), and [`C2PA_UUID`], [`C2paBoxPurpose`] and
//! [`C2paUnreadReason`] are `gamut-isobmff`'s, re-exported. The lens locates the box-bounded
//! *slot* after the merkle offset; this crate reports the *store* at its front
//! ([`gamut_isobmff::C2paSlot::store`]), so where the two crates both name the store they agree
//! byte for byte.
#[cfg(doc)]
use gamut_isobmff::C2PA_UUID;
use gamut_isobmff::{C2paBoxPurpose, C2paByteRange, C2paSlot, C2paUnreadReason, C2paUuidBox};

use crate::container::{HeifContainer, SegmentKind};

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
/// from this range would be wrong. Nothing here validates anything. It is therefore a
/// [`C2paByteRange`], which offers its bounds and the bytes it covers but no conversion to the
/// `Range<usize>` a byte-range hasher's exclusion list takes (#780).
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
    pub range: C2paByteRange,
    /// The `box_purpose` of the `uuid` box that carried this store.
    pub purpose: C2paBoxPurpose,
    /// The half-open byte range of the whole **slot** the store sits at the front of: everything
    /// after the merkle offset to the end of the `uuid` box, so the store plus any §A.5.3 padding.
    /// It is [`gamut_isobmff::C2paSlot::range`], the range `gamut-avif`'s `c2pa_slot` reports, so
    /// both crates name both regions alike (#505). Starts where [`range`](Self::range) does.
    pub slot: C2paByteRange,
}

impl<'a> C2paManifestStore<'a> {
    /// The store at the front of a slot the shared lens located, trimmed to its own JUMBF `LBox`,
    /// or `None` when the slot holds no valid bound ([`C2paUnreadReason::NoStoreBound`]).
    fn from_slot(slot: &C2paSlot<'a>) -> Option<Self> {
        Some(Self {
            bytes: slot.store()?,
            range: slot.store_range()?,
            purpose: slot.purpose,
            slot: slot.range,
        })
    }
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
                C2paUuidBox::Slot(slot) => C2paManifestStore::from_slot(&slot),
                C2paUuidBox::Merkle | C2paUuidBox::Unread(_) | C2paUuidBox::OtherExtendedType => {
                    None
                }
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
    /// simply whether an `mdat` or a `moov` box has already been passed when this box is reached.
    fn top_level_uuid_boxes(
        &self,
    ) -> impl Iterator<Item = (C2paByteRange, C2paBoxPosition, C2paUuidBox<'a>)> + '_ {
        self.segments()
            .iter()
            .scan(C2paBoxPosition::BeforeMediaData, |position, segment| {
                let entry = match segment.kind {
                    SegmentKind::Box { ty, .. } if &ty == b"mdat" || &ty == b"moov" => {
                        *position = position.passing(ty);
                        None
                    }
                    // `classify` is `None` for every segment that is not a top-level `uuid` box.
                    // The walk never yields a segment ending before it starts, so the range is
                    // always placed.
                    _ => C2paUuidBox::classify(segment)
                        .zip(C2paByteRange::new(segment.range.start, segment.range.end))
                        .map(|(classified, range)| (range, *position, classified)),
                };
                // `scan` stops at the first `None` it yields, so every segment yields `Some`; the
                // inner `Option` is what filters.
                Some(entry)
            })
            .flatten()
    }
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

/// Where a top-level `uuid` box sits relative to the file's media data — a **positional fact about
/// bytes**, and never a verdict.
///
/// C2PA 2.4 §A.5.3 places the box carrying a manifest store "before the first 'mdat' box in the
/// file and before any 'moov' box in the file", and after the `ftyp`. The box is reported wherever
/// it is found and its position is never enforced, but a box sitting past that boundary is worth
/// stating for the same reason a byte range is: it is where an *appended* box lands, and a reader
/// deciding what to hand a validator cannot see it from a range alone.
///
/// Both of §A.5.3's boundaries are observed. [`HeifContainer::parse`] skips a top-level `moov` that
/// sits beside the `meta` still (an image sequence's track: the workspace reads the still and never
/// the track), so a parsed file can hold one, and the box is then reported past whichever boundary
/// the walk crossed **first** — [`AfterMediaData`](Self::AfterMediaData) or
/// [`AfterMovieBox`](Self::AfterMovieBox). Once either is crossed the box is outside the window, and
/// the first boundary is the one that put it there.
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
    /// The box was reached before any `mdat` or `moov` box — the window §A.5.3 places a
    /// manifest-store box in. A file with neither reports this too.
    BeforeMediaData = 0,
    /// An `mdat` box was already passed when this box was reached, and no `moov` before it.
    AfterMediaData = 1,
    /// A `moov` box was already passed when this box was reached, and no `mdat` before it.
    AfterMovieBox = 2,
}

impl C2paBoxPosition {
    /// The position after the walk passes a top-level box of type `ty`: the first `mdat` or
    /// `moov` closes the §A.5.3 window and names the position, and nothing reopens or renames it.
    const fn passing(self, ty: [u8; 4]) -> Self {
        match (self, &ty) {
            (Self::BeforeMediaData, b"mdat") => Self::AfterMediaData,
            (Self::BeforeMediaData, b"moov") => Self::AfterMovieBox,
            (crossed, _) => crossed,
        }
    }

    /// The clause a report adds to this box's line, or `None` when the box sits where §A.5.3 puts
    /// one and its position adds nothing to what the range already says.
    ///
    /// Worded as a position, not as a fault: see the [type docs](Self) for why an `update` box past
    /// the boundary is exactly what §A.5.3 asks for.
    #[must_use]
    pub const fn note(self) -> Option<&'static str> {
        match self {
            Self::AfterMediaData => Some("its box begins after the first mdat box"),
            Self::AfterMovieBox => Some("its box begins after a moov box"),
            Self::BeforeMediaData => None,
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
    pub range: C2paByteRange,
    /// Why no manifest store was read from it.
    pub reason: C2paUnreadReason,
    /// Where the box sits relative to the file's media data.
    pub position: C2paBoxPosition,
}

/// A top-level auxiliary `merkle` box (C2PA 2.4 §A.5.4.1.4): C2PA framing that carries Merkle-tree
/// hashes for validating media piecewise, not a manifest store. Reported **without its bytes**, by
/// where the whole box sits, because it is neither a store this locator found nor a box it failed
/// to read.
///
/// Non-exhaustive: a later revision may report more of the box without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paMerkleBox {
    /// The half-open byte range of the whole `uuid` box.
    pub range: C2paByteRange,
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
    pub range: C2paByteRange,
    /// The `box_purpose` of the `uuid` box that carried the store.
    pub purpose: C2paBoxPurpose,
    /// Where the `uuid` box that carried the store sits relative to the file's media data.
    pub position: C2paBoxPosition,
    /// The half-open byte range of the slot the store sits at the front of — the store plus any
    /// §A.5.3 padding, as [`C2paManifestStore::slot`] reports it. `gamut-avif`'s summary carries
    /// the same two ranges under the same two names (#505).
    pub slot: C2paByteRange,
}

impl C2paStoreSummary {
    /// The store's size in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        self.range.len()
    }
}

/// What a file's top-level boxes say about C2PA, as one answer a host can match on: the shape of
/// the question "does this file carry C2PA?", which has more than two answers.
///
/// Returned by [`C2paSummary::presence`]. Each variant is a **fact about bytes this reader
/// walked**, never a provenance verdict — not even [`StoreLocated`](Self::StoreLocated), which
/// says a store was *located* and nothing about whether it validates (see
/// [`C2PA_NOT_VALIDATED`]). The variants are decided in declaration order, so each one is the
/// strongest statement the summary supports; the lists on [`C2paSummary`] still hold every box,
/// whichever variant is returned.
///
/// The middle answers are why this is not a `bool`. A file whose only `ContentProvenanceBox` has
/// a non-zero `FullBox` version — among other things, what a signed file corrupted in transit
/// looks like — carries C2PA framing and yields no store, and a boolean "is a store present?"
/// answers it exactly as it answers a file with no C2PA box at all. Matching on this type makes
/// that case one a host has to name.
///
/// [`other_uuid_boxes`](C2paSummary::other_uuid_boxes) is deliberately **not** an input: a
/// top-level `uuid` box of another extended type is not C2PA framing (C2PA 2.4 §A.5.1.1), so a file
/// whose only such box is one byte off the C2PA type answers [`NoC2paBox`](Self::NoC2paBox), and
/// that count is the separate byte fact a report states beside it.
///
/// Non-exhaustive and with permanent discriminants: a later revision may distinguish a further
/// answer without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum C2paPresence {
    /// At least one manifest store was located ([`C2paSummary::stores`] is non-empty). Located,
    /// **not validated**; boxes that yielded no store may sit beside it in
    /// [`C2paSummary::unread`].
    StoreLocated = 0,
    /// No store was located, but at least one top-level C2PA box yielded none
    /// ([`C2paSummary::unread`] is non-empty): the file carries C2PA framing this reader could not
    /// read through. This is **not** absence of provenance.
    FramingUnread = 1,
    /// No store was located and no C2PA box failed to yield one, but the file carries auxiliary
    /// `merkle` boxes ([`C2paSummary::merkle`], C2PA 2.4 §A.5.4.1.4): C2PA framing, read correctly,
    /// that holds Merkle-tree hashes rather than a manifest store. Not absence of C2PA framing.
    AuxiliaryOnly = 2,
    /// No top-level `uuid` box carries the C2PA extended type. Says nothing about provenance
    /// carried anywhere this reader does not look, and nothing about
    /// [`other_uuid_boxes`](C2paSummary::other_uuid_boxes).
    NoC2paBox = 3,
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
    /// Every top-level auxiliary `merkle` box (C2PA 2.4 §A.5.4.1.4), in file order.
    ///
    /// Read correctly and listed as what it is: C2PA framing whose `data` is Merkle-tree hashes, so
    /// it is neither a located store nor a box that failed to yield one.
    pub merkle: Vec<C2paMerkleBox>,
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
    /// Which C2PA answer the file's top-level boxes support — a store located, C2PA framing that
    /// yielded no store, only auxiliary `merkle` boxes, or no C2PA box — as one value a host
    /// matches on. See [`C2paPresence`] for what each answer does and does not claim.
    ///
    /// A host that prints its own words for a variant still owes the reader
    /// [`C2PA_NOT_VALIDATED`]; [`report_lines`](Self::report_lines) renders the pinned wording with
    /// the disclaimer inline.
    #[must_use]
    pub fn presence(&self) -> C2paPresence {
        if !self.stores.is_empty() {
            C2paPresence::StoreLocated
        } else if !self.unread.is_empty() {
            C2paPresence::FramingUnread
        } else if !self.merkle.is_empty() {
            C2paPresence::AuxiliaryOnly
        } else {
            C2paPresence::NoC2paBox
        }
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
    /// **At most three lines, whatever the file holds**, which is what makes this the part a host
    /// prints unconditionally. The headline states every non-empty store category and carries
    /// [`C2PA_NOT_VALIDATED`] inline; a count of auxiliary [`merkle`](Self::merkle) boxes follows
    /// when there are any; the last is a byte count, worded so it cannot be read as a provenance
    /// claim (see [`other_uuid_boxes`](Self::other_uuid_boxes)).
    ///
    /// Neither line is indented, because neither is a list entry: they are the head a host prints
    /// above whatever it shows of [`detail_lines`](Self::detail_lines), which *is* indented.
    #[must_use]
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![self.headline()];
        if !self.merkle.is_empty() {
            lines.push(format!(
                "auxiliary C2PA merkle boxes: {count} (Merkle-tree hashes, C2PA 2.4 §A.5.4; not a \
                 manifest store)",
                count = self.merkle.len(),
            ));
        }
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
    /// Every *line* is built on demand, so a host that caps the list at N builds N strings rather
    /// than one per box in a file that chose how many to carry — a manifest store's line is the
    /// costly part, and §A.5.3 puts the number of boxes in the file's hands. Ordering them costs
    /// one borrowed entry per box, which is what the summary already holds.
    /// [`detail_line_count`](Self::detail_line_count) is how many there are in total.
    pub fn detail_lines(&self) -> impl Iterator<Item = String> + '_ {
        self.detail_entries().into_iter().map(|entry| entry.line())
    }

    /// The two lists merged back into the file order both were built in, as borrowed entries: the
    /// order [`detail_lines`](Self::detail_lines) renders and nothing else.
    ///
    /// Ordering by `range.start` is exact rather than approximate. Different top-level `uuid`
    /// boxes occupy disjoint byte ranges, and a store's range lies inside the box that carried it,
    /// so the lower start is always the earlier box. A tie is not reachable from a parsed file — a
    /// store begins strictly after its box's header — and the sort being stable resolves one to
    /// the store, since the stores are chained first, so the order is total for a summary
    /// assembled by hand as well.
    ///
    /// The list is built rather than merged lazily so that its length is fixed by the summary and
    /// not by this function: a host consuming it can be handed no more entries than the file has
    /// boxes, whatever the merge does.
    fn detail_entries(&self) -> Vec<DetailEntry<'_>> {
        let mut entries: Vec<DetailEntry<'_>> = self
            .stores
            .iter()
            .map(DetailEntry::Store)
            .chain(self.unread.iter().map(DetailEntry::Unread))
            .chain(self.merkle.iter().map(DetailEntry::Merkle))
            .collect();
        entries.sort_by_key(DetailEntry::start);
        entries
    }

    /// How many lines [`detail_lines`](Self::detail_lines) yields — one per store, unread box and
    /// `merkle` box — without building any of them.
    #[must_use]
    pub fn detail_line_count(&self) -> usize {
        self.stores.len() + self.unread.len() + self.merkle.len()
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

/// One entry of [`C2paSummary::detail_lines`]'s list: a located store, or a top-level C2PA box
/// that yielded none. Borrowed from the summary, so ordering the list copies no line.
enum DetailEntry<'a> {
    /// A store [`C2paSummary::stores`] holds.
    Store(&'a C2paStoreSummary),
    /// A C2PA box [`C2paSummary::unread`] holds.
    Unread(&'a C2paUnreadBox),
    /// An auxiliary box [`C2paSummary::merkle`] holds.
    Merkle(&'a C2paMerkleBox),
}

impl DetailEntry<'_> {
    /// Where this entry's box begins in the file: the key the list is ordered by.
    fn start(&self) -> usize {
        match self {
            Self::Store(store) => store.range.start(),
            Self::Unread(unread) => unread.range.start(),
            Self::Merkle(merkle) => merkle.range.start(),
        }
    }

    /// This entry's report line.
    fn line(&self) -> String {
        match self {
            Self::Store(store) => store_line(store),
            Self::Unread(unread) => unread_line(unread),
            Self::Merkle(merkle) => merkle_line(merkle),
        }
    }
}

/// The report line for one located store.
fn store_line(store: &C2paStoreSummary) -> String {
    format!(
        "  box_purpose \"{purpose}\": {size} bytes at {range} — located, not validated{note}",
        purpose = store.purpose.as_str(),
        size = store.size(),
        range = store.range,
        note = position_note(store.position),
    )
}

/// The report line for one C2PA box that yielded no store.
fn unread_line(unread: &C2paUnreadBox) -> String {
    format!(
        "  unread C2PA box at {range}: {reason}{note}",
        range = unread.range,
        reason = unread.reason.describe(),
        note = position_note(unread.position),
    )
}

/// The report line for one auxiliary `merkle` box.
fn merkle_line(merkle: &C2paMerkleBox) -> String {
    format!(
        "  auxiliary merkle box at {range}: Merkle-tree hashes (C2PA 2.4 §A.5.4.1.4), not a \
         manifest store{note}",
        range = merkle.range,
        note = position_note(merkle.position),
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
        let mut merkle = Vec::new();
        let mut other_uuid_boxes = 0;
        for (range, position, classified) in self.top_level_uuid_boxes() {
            match classified {
                C2paUuidBox::Slot(slot) => match C2paManifestStore::from_slot(&slot) {
                    Some(store) => stores.push(C2paStoreSummary {
                        range: store.range,
                        purpose: store.purpose,
                        position,
                        slot: store.slot,
                    }),
                    None => unread.push(C2paUnreadBox {
                        range,
                        reason: C2paUnreadReason::NoStoreBound,
                        position,
                    }),
                },
                C2paUuidBox::Merkle => {
                    merkle.push(C2paMerkleBox { range, position });
                }
                C2paUuidBox::Unread(reason) => unread.push(C2paUnreadBox {
                    range,
                    reason,
                    position,
                }),
                C2paUuidBox::OtherExtendedType => other_uuid_boxes += 1,
            }
        }
        C2paSummary {
            stores,
            unread,
            merkle,
            other_uuid_boxes,
        }
    }
}

#[cfg(test)]
mod tests {
    use gamut_isobmff::C2paByteRange;

    use super::{
        C2PA_NOT_VALIDATED, C2paBoxPosition, C2paBoxPurpose, C2paManifestStore, C2paMerkleBox,
        C2paPresence, C2paStoreSummary, C2paSummary, C2paUnreadBox, C2paUnreadReason, C2paUuidBox,
    };

    /// `[start, end)`, which the caller states with `start <= end`.
    fn range(start: usize, end: usize) -> C2paByteRange {
        C2paByteRange::new(start, end).expect("start <= end")
    }

    /// A summary of the stores at the given `(start, end, purpose)` triples, all in the window
    /// §A.5.3 mandates, and nothing unread.
    fn summary(stores: &[(usize, usize, C2paBoxPurpose)]) -> C2paSummary {
        C2paSummary {
            stores: stores
                .iter()
                .map(|&(start, end, purpose)| C2paStoreSummary {
                    range: range(start, end),
                    purpose,
                    position: C2paBoxPosition::BeforeMediaData,
                    slot: range(start, end),
                })
                .collect(),
            unread: Vec::new(),
            merkle: Vec::new(),
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
                    range: range(start, end),
                    reason,
                    position: C2paBoxPosition::BeforeMediaData,
                })
                .collect(),
            merkle: Vec::new(),
            other_uuid_boxes: 0,
        }
    }

    /// An auxiliary `merkle` box is its own category: counted in the head, listed in file order
    /// among the details with its own words, and never worded as a store or as an unread box.
    #[test]
    fn a_merkle_box_is_reported_as_auxiliary_in_file_order() {
        let mut with_merkle = summary(&[(100, 200, C2paBoxPurpose::Manifest)]);
        with_merkle.merkle.push(C2paMerkleBox {
            range: range(40, 90),
            position: C2paBoxPosition::AfterMediaData,
        });
        assert_eq!(with_merkle.detail_line_count(), 2);
        assert_eq!(
            with_merkle.summary_lines()[1],
            "auxiliary C2PA merkle boxes: 1 (Merkle-tree hashes, C2PA 2.4 §A.5.4; not a manifest \
             store)"
        );
        let details: Vec<String> = with_merkle.detail_lines().collect();
        assert_eq!(
            details[0],
            "  auxiliary merkle box at [40, 90): Merkle-tree hashes (C2PA 2.4 §A.5.4.1.4), not a \
             manifest store; its box begins after the first mdat box"
        );
        assert!(
            details[1].starts_with("  box_purpose \"manifest\""),
            "{details:?}"
        );
        assert!(!with_merkle.headline().contains("could be read"));
    }

    /// `presence` takes the strongest answer the lists support: a located store outranks an unread
    /// box, which outranks an auxiliary `merkle` box, which outranks nothing.
    #[test]
    fn presence_names_the_strongest_category_the_lists_hold() {
        let merkle = C2paMerkleBox {
            range: range(40, 90),
            position: C2paBoxPosition::BeforeMediaData,
        };

        let mut merkle_only = C2paSummary::default();
        merkle_only.merkle.push(merkle.clone());
        assert_eq!(merkle_only.presence(), C2paPresence::AuxiliaryOnly);

        let mut unread_and_merkle = unread_summary(&[(100, 200, C2paUnreadReason::Truncated)]);
        unread_and_merkle.merkle.push(merkle);
        assert_eq!(unread_and_merkle.presence(), C2paPresence::FramingUnread);

        let mut store_and_unread = summary(&[(61, 90, C2paBoxPurpose::Manifest)]);
        store_and_unread.unread = unread_summary(&[(100, 200, C2paUnreadReason::Truncated)]).unread;
        assert_eq!(store_and_unread.presence(), C2paPresence::StoreLocated);

        // Another extended type is not C2PA framing (§A.5.1.1), so it never lifts the answer.
        let near_miss = C2paSummary {
            other_uuid_boxes: 1,
            ..C2paSummary::default()
        };
        assert_eq!(near_miss.presence(), C2paPresence::NoC2paBox);
    }

    #[test]
    fn a_store_is_the_front_of_its_slot_up_to_its_lbox() {
        use gamut_isobmff::{C2PA_UUID, Segment, SegmentKind, content_provenance_payload};
        // A 12-byte store (`LBox` 12) followed by 5 padding bytes, in an `original` box at 100.
        let mut slot_bytes = 12u32.to_be_bytes().to_vec();
        slot_bytes.extend_from_slice(b"jumbabcd");
        slot_bytes.extend_from_slice(&[0xEE; 5]);
        let mut body = C2PA_UUID.to_vec();
        body.extend_from_slice(&content_provenance_payload(
            C2paBoxPurpose::Original,
            &slot_bytes,
        ));
        let segment = Segment {
            range: 100..108 + body.len(),
            kind: SegmentKind::Box {
                ty: *b"uuid",
                body: &body,
            },
        };
        let Some(C2paUuidBox::Slot(slot)) = C2paUuidBox::classify(&segment) else {
            panic!("an original slot");
        };
        let store = C2paManifestStore::from_slot(&slot).expect("an LBox-bounded store");
        assert_eq!(store.bytes, &slot_bytes[..12]);
        assert_eq!(
            store.range,
            range(slot.range.start(), slot.range.start() + 12)
        );
        assert_eq!(store.purpose, C2paBoxPurpose::Original);
        // The slot is the lens's, padding included: 12 + 5 bytes.
        assert_eq!(store.slot, slot.range);
        assert_eq!(store.slot.len(), 17);
        // The same slot zeroed is a reservation: no store, which the summary calls NoStoreBound.
        let mut zeroed = body.clone();
        let len = zeroed.len();
        zeroed[len - slot_bytes.len()..].fill(0);
        let segment = Segment {
            range: 100..108 + zeroed.len(),
            kind: SegmentKind::Box {
                ty: *b"uuid",
                body: &zeroed,
            },
        };
        let Some(C2paUuidBox::Slot(slot)) = C2paUuidBox::classify(&segment) else {
            panic!("an original slot");
        };
        assert_eq!(C2paManifestStore::from_slot(&slot), None);
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
        assert_eq!(none.presence(), C2paPresence::NoC2paBox);
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
        assert_eq!(one.presence(), C2paPresence::StoreLocated);
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
        assert_eq!(one.presence(), C2paPresence::FramingUnread);
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
                range: range(61, 90),
                purpose: C2paBoxPurpose::Manifest,
                position: C2paBoxPosition::BeforeMediaData,
                slot: range(61, 90),
            }],
            unread: vec![C2paUnreadBox {
                range: range(90, 150),
                reason: C2paUnreadReason::UnknownPurpose,
                position: C2paBoxPosition::BeforeMediaData,
            }],
            merkle: Vec::new(),
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
                    C2paUnreadReason::UnknownPurpose.describe()
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
                range: range(16, 40),
                reason: C2paUnreadReason::Truncated,
                position: C2paBoxPosition::BeforeMediaData,
            },
            C2paUnreadBox {
                range: range(130, 200),
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
            range: range(61, 150),
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
    fn a_box_past_a_moov_says_which_boundary_it_crossed() {
        let mut past_moov = summary(&[(61, 90, C2paBoxPurpose::Manifest)]);
        past_moov.stores[0].position = C2paBoxPosition::AfterMovieBox;
        assert_eq!(
            past_moov.report_lines()[1],
            "  box_purpose \"manifest\": 29 bytes at [61, 90) — located, not validated; its box \
             begins after a moov box"
        );
    }

    #[test]
    fn the_first_boundary_crossed_names_the_position_and_nothing_renames_it() {
        use C2paBoxPosition::{AfterMediaData, AfterMovieBox, BeforeMediaData};
        assert_eq!(BeforeMediaData.passing(*b"mdat"), AfterMediaData);
        assert_eq!(BeforeMediaData.passing(*b"moov"), AfterMovieBox);
        assert_eq!(BeforeMediaData.passing(*b"free"), BeforeMediaData);
        assert_eq!(AfterMediaData.passing(*b"moov"), AfterMediaData);
        assert_eq!(AfterMovieBox.passing(*b"mdat"), AfterMovieBox);
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
            range: range(172, 200),
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
