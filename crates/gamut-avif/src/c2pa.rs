//! Carrying and locating a C2PA manifest store in an AVIF file (C2PA 2.4 Appendix A.5).
//!
//! C2PA (Coalition for Content Provenance and Authenticity) embeds its manifest store in a
//! BMFF-based asset — AVIF is named in §A.5.1 — as a top-level `uuid` box, the
//! `ContentProvenanceBox`, whose 16-byte extended type is [`C2PA_UUID`](crate::C2PA_UUID). This
//! module is the
//! crate's whole C2PA surface, and it is deliberately small: on the write side the encoder
//! reserves or fills a slot for one
//! ([`AvifEncoder::with_c2pa_reserved`](crate::AvifEncoder::with_c2pa_reserved) /
//! [`with_c2pa`](crate::AvifEncoder::with_c2pa)), and on the read side this module reports where
//! one sits ([`AvifContainer::c2pa_slot`]). It parses nothing inside the store, verifies no hash,
//! checks no signature and reaches no verdict — validation is a C2PA validator's job, downstream.
//!
//! The **framing** §A.5.1.2 and §A.5.3 put around the store — the user type, the `FullBox`
//! version and flags, the NUL-terminated `box_purpose`, and the 8-byte merkle offset ahead of the
//! slot — is not this crate's: it is [`gamut_isobmff`]'s, read through
//! [`gamut_isobmff::C2paUuidBox`] and written by [`gamut_isobmff::content_provenance_payload`], the
//! one lens `gamut-heic` reads through too (#505). [`C2PA_UUID`](crate::C2PA_UUID),
//! [`C2paBoxPurpose`], [`C2paSlot`]
//! and [`C2paUnreadReason`] are that crate's types, re-exported. The slot this crate reserves,
//! fills and reports is everything after the merkle offset, padding included;
//! [`C2paSlot::store`] trims it to the store's own JUMBF `LBox`, which is what `gamut-heic`
//! reports.
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

use gamut_isobmff::{
    C2paBoxPurpose, C2paSlot, C2paUnreadReason, C2paUuidBox, Segment, SegmentKind,
};

use crate::container::AvifContainer;

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
    /// Where the slot begins is [`C2paBoxPurpose`]'s: after the stated merkle offset for
    /// `manifest`/`original`, and for `update` wherever the store's JUMBF `LBox` probe puts it —
    /// the same placement `gamut-heic` trims its store from, so the two crates agree on every
    /// byte they both report.
    ///
    /// A top-level `uuid` box whose user type is not [`C2PA_UUID`](crate::C2PA_UUID), whose
    /// `FullBox` version or
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
    /// reports them. That builder applies no minimum by decision (issue #577); its docs say why.
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
    top_level_uuid_boxes(segments).filter_map(|(_, _, classified)| match classified {
        C2paUuidBox::Slot(slot) => Some(slot),
        C2paUuidBox::Merkle | C2paUuidBox::Unread(_) | C2paUuidBox::OtherExtendedType => None,
    })
}

/// Every top-level `uuid` box among `segments`, as its whole box range, where it sits relative to
/// the file's media data, and what it turned out to be.
///
/// The one scan both [`slots`] and [`summary`] are built from, so "a slot", "a C2PA box that
/// yielded no slot" and "a `uuid` box of some other extended type" can never disagree about which
/// boxes were looked at. The position is taken from the walk itself: whether an `mdat` box has
/// already been passed when this box is reached.
fn top_level_uuid_boxes<'a, 's>(
    segments: &'s [Segment<'a>],
) -> impl Iterator<Item = (Range<usize>, C2paBoxPosition, C2paUuidBox<'a>)> + 's {
    let mut position = C2paBoxPosition::BeforeMediaData;
    segments
        .iter()
        .filter_map(move |segment| match segment.kind {
            SegmentKind::Box { ty, .. } if &ty == b"mdat" => {
                position = C2paBoxPosition::AfterMediaData;
                None
            }
            // `classify` is `None` for every segment that is not a top-level `uuid` box.
            _ => C2paUuidBox::classify(segment)
                .map(|classified| (segment.range.clone(), position, classified)),
        })
}

/// What a report of a located manifest-store slot has to say beside it, because locating one is
/// not validating it.
///
/// C2PA 2.4 §15.12 puts validation on a validator; this crate holds no COSE, X.509, RFC 3161 or
/// trust-list code and never reports a validity verdict. To anyone who has seen a Content
/// Credentials badge, "C2PA: present" printed beside EXIF and ICC reads as *verified*, so
/// [`C2paSummary::summary_lines`] carries this text in the line that reports the slots, never as a
/// footnote. The same sentence `gamut-heic` carries, so every container reports alike.
pub const C2PA_NOT_VALIDATED: &str = "gamut locates a C2PA manifest store and never validates it: \
     it checks no signature, no hash binding and no trust list. Validate with c2pa-rs.";

/// Where a top-level `uuid` box sits relative to the file's media data — a **positional fact about
/// bytes**, never a verdict.
///
/// C2PA 2.4 §A.5.3 places a manifest-store box before the first `mdat`, yet requires the `update`
/// box of a mid-update file to be the last box of the file — past that boundary by the
/// specification's own instruction. So [`AfterMediaData`](Self::AfterMediaData) says only where the
/// bytes are. (`moov`, §A.5.3's other boundary, never reaches a parsed AVIF: image sequences are out
/// of scope and refused.)
///
/// Non-exhaustive and with permanent discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum C2paBoxPosition {
    /// The box was reached before any `mdat` box. A file with no `mdat` at all reports this too.
    BeforeMediaData = 0,
    /// An `mdat` box was already passed when this box was reached.
    AfterMediaData = 1,
}

impl C2paBoxPosition {
    /// The clause a report adds to this box's line, or `None` when the box sits where §A.5.3 puts
    /// one.
    #[must_use]
    pub const fn note(self) -> Option<&'static str> {
        match self {
            Self::AfterMediaData => Some("its box begins after the first mdat box"),
            Self::BeforeMediaData => None,
        }
    }
}

/// One located manifest-store slot, reported **without its bytes**: where it sits, how big it is,
/// and what the `uuid` box that carried it said it was for.
///
/// The bytes are absent by construction: they are opaque to this crate, and a byte range is what a
/// caller hands to `c2pa-rs` or to `dd`. Use [`C2paSlot::slot_bytes`] when the bytes are wanted.
///
/// Non-exhaustive: a later revision may report more of the box without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paStoreSummary {
    /// The half-open byte range of the slot — the same range [`C2paSlot::range`] reports: the
    /// store and any padding after it, box-bounded, and *not* a BMFF exclusion range.
    pub range: Range<usize>,
    /// The `box_purpose` of the `uuid` box that carried the slot.
    pub purpose: C2paBoxPurpose,
    /// Where the `uuid` box that carried the slot sits relative to the file's media data.
    pub position: C2paBoxPosition,
}

impl C2paStoreSummary {
    /// The slot's size in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        self.range.len()
    }
}

/// A top-level C2PA `ContentProvenanceBox` that yielded no slot: where the whole box sits, and why
/// nothing was read from it. Reported without its bytes.
///
/// Non-exhaustive: a later revision may report more of the box without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paUnreadBox {
    /// The half-open byte range of the whole `uuid` box, header included.
    pub range: Range<usize>,
    /// Why no slot was read from it.
    pub reason: C2paUnreadReason,
    /// Where the box sits relative to the file's media data.
    pub position: C2paBoxPosition,
}

/// A top-level auxiliary `merkle` box (C2PA 2.4 §A.5.4.1.4): C2PA framing carrying Merkle-tree
/// hashes, not a manifest store. Reported without its bytes.
///
/// Non-exhaustive: a later revision may report more of the box without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paMerkleBox {
    /// The half-open byte range of the whole `uuid` box.
    pub range: Range<usize>,
    /// Where the box sits relative to the file's media data.
    pub position: C2paBoxPosition,
}

/// A non-validating report of the C2PA framing an AVIF file carries in its top-level boxes:
/// every manifest-store slot with its range, size and `box_purpose`, every C2PA box that yielded no
/// slot with the reason, every auxiliary `merkle` box, and a count of `uuid` boxes of other types.
///
/// Built by [`AvifContainer::c2pa_summary`]. The summary carries the facts and renders the lines,
/// so every host prints the same words and none can drop [`C2PA_NOT_VALIDATED`] while rewording
/// them. The shape and the wording are `gamut-heic`'s, repeated for this container (issue #530).
///
/// Non-exhaustive: a later revision may report more without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct C2paSummary {
    /// Every located slot, in file order. A file mid-update legitimately carries two — `original`
    /// and `update` (§A.5.3) — and both are listed: which is *active* is a validator's judgement.
    pub stores: Vec<C2paStoreSummary>,
    /// Every top-level C2PA box that yielded no slot, in file order, with the reason. Non-empty
    /// beside an empty [`stores`](Self::stores) is the case a report must not collapse into "none
    /// found".
    pub unread: Vec<C2paUnreadBox>,
    /// Every top-level auxiliary `merkle` box, in file order.
    pub merkle: Vec<C2paMerkleBox>,
    /// How many top-level `uuid` boxes carried an extended type other than
    /// [`C2PA_UUID`](crate::C2PA_UUID) — a count
    /// of bytes present, not a provenance claim. It is what keeps a file whose only `uuid` box is
    /// one byte off the C2PA type from reading identically to a file with none; it cannot tell a
    /// corrupted C2PA type from an ordinary vendor box, and §A.5.1.1 gives no way to.
    pub other_uuid_boxes: usize,
}

impl C2paSummary {
    /// The whole human-readable report, uncapped: [`summary_lines`](Self::summary_lines) followed
    /// by every [`detail_lines`](Self::detail_lines) entry. A terminal host should cap the detail
    /// lines instead, reporting [`detail_line_count`](Self::detail_line_count) beside them.
    #[must_use]
    pub fn report_lines(&self) -> Vec<String> {
        let mut lines = self.summary_lines();
        lines.extend(self.detail_lines());
        lines
    }

    /// The report's head — **at most three lines, whatever the file holds**: the headline, which
    /// names every non-empty slot category and carries [`C2PA_NOT_VALIDATED`] inline; the count of
    /// auxiliary `merkle` boxes when there are any; and the count of `uuid` boxes of another
    /// extended type when there are any. Not indented.
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

    /// One line per top-level C2PA box — slot, unread box or `merkle` box — **in true file order**,
    /// so a host that caps the list hides the file's last boxes rather than one kind of box. Each
    /// line is indented two spaces relative to the headline and built on demand.
    pub fn detail_lines(&self) -> impl Iterator<Item = String> + '_ {
        let mut entries: Vec<DetailEntry<'_>> = self
            .stores
            .iter()
            .map(DetailEntry::Store)
            .chain(self.unread.iter().map(DetailEntry::Unread))
            .chain(self.merkle.iter().map(DetailEntry::Merkle))
            .collect();
        // Different top-level boxes occupy disjoint ranges and a slot lies inside its box, so the
        // lower start is the earlier box.
        entries.sort_by_key(DetailEntry::start);
        entries.into_iter().map(|entry| entry.line())
    }

    /// How many lines [`detail_lines`](Self::detail_lines) yields, without building any.
    #[must_use]
    pub fn detail_line_count(&self) -> usize {
        self.stores.len() + self.unread.len() + self.merkle.len()
    }

    /// The report's first line: what the scan found, with [`C2PA_NOT_VALIDATED`] inline. Four
    /// outcomes, because a file carrying C2PA boxes none of which yielded a slot is neither
    /// "located" nor "none found".
    fn headline(&self) -> String {
        let located = located_clause(self.stores.len());
        let present = unread_clause(self.unread.len());
        match (located, present) {
            (None, None) => format!(
                "C2PA: no manifest store found in the top-level boxes of the primary stream — \
                 {C2PA_NOT_VALIDATED}"
            ),
            (None, Some(present)) => format!(
                "C2PA: no manifest store could be read, but {present} — that is NOT absence of \
                 provenance — {C2PA_NOT_VALIDATED}"
            ),
            (Some(located), None) => format!("C2PA: {located} — {C2PA_NOT_VALIDATED}"),
            (Some(located), Some(present)) => format!(
                "C2PA: {located}, and {present} from which no store could be read — a box gamut \
                 could not read through is NOT absence of provenance — {C2PA_NOT_VALIDATED}"
            ),
        }
    }
}

/// The headline's clause for `count` located slots, or `None` when there are none.
fn located_clause(count: usize) -> Option<String> {
    (count > 0).then(|| {
        format!(
            "{count} manifest store{plural} located, NOT VALIDATED",
            plural = if count == 1 { "" } else { "s" }
        )
    })
}

/// The headline's clause for `count` C2PA boxes that yielded no slot, or `None` when there are
/// none.
fn unread_clause(count: usize) -> Option<String> {
    (count > 0).then(|| {
        format!(
            "{count} C2PA {noun} present",
            noun = if count == 1 { "box is" } else { "boxes are" }
        )
    })
}

/// One entry of [`C2paSummary::detail_lines`]'s list, borrowed from the summary.
enum DetailEntry<'a> {
    /// A slot [`C2paSummary::stores`] holds.
    Store(&'a C2paStoreSummary),
    /// A C2PA box [`C2paSummary::unread`] holds.
    Unread(&'a C2paUnreadBox),
    /// An auxiliary box [`C2paSummary::merkle`] holds.
    Merkle(&'a C2paMerkleBox),
}

impl DetailEntry<'_> {
    /// Where this entry's bytes begin in the file: the key the list is ordered by.
    fn start(&self) -> usize {
        match self {
            Self::Store(store) => store.range.start,
            Self::Unread(unread) => unread.range.start,
            Self::Merkle(merkle) => merkle.range.start,
        }
    }

    /// This entry's report line.
    fn line(&self) -> String {
        match self {
            Self::Store(store) => format!(
                "  box_purpose \"{purpose}\": {size}-byte slot (store plus any padding) at \
                 [{start}, {end}) — located, not validated{note}",
                purpose = store.purpose.as_str(),
                size = store.size(),
                start = store.range.start,
                end = store.range.end,
                note = position_note(store.position),
            ),
            Self::Unread(unread) => format!(
                "  unread C2PA box at [{start}, {end}): {reason}{note}",
                start = unread.range.start,
                end = unread.range.end,
                reason = unread.reason.describe(),
                note = position_note(unread.position),
            ),
            Self::Merkle(merkle) => format!(
                "  auxiliary merkle box at [{start}, {end}): Merkle-tree hashes (C2PA 2.4 \
                 §A.5.4.1.4), not a manifest store{note}",
                start = merkle.range.start,
                end = merkle.range.end,
                note = position_note(merkle.position),
            ),
        }
    }
}

/// The trailing clause a line carries for its box's position, or nothing when there is none.
fn position_note(position: C2paBoxPosition) -> String {
    position
        .note()
        .map_or_else(String::new, |note| format!("; {note}"))
}

/// The summary of `segments`' top-level C2PA boxes — the one builder behind
/// [`AvifContainer::c2pa_summary`], over the same scan as [`slots`].
fn summary(segments: &[Segment<'_>]) -> C2paSummary {
    let mut summary = C2paSummary::default();
    for (range, position, classified) in top_level_uuid_boxes(segments) {
        match classified {
            C2paUuidBox::Slot(slot) => {
                summary.stores.push(C2paStoreSummary {
                    range: slot.range,
                    purpose: slot.purpose,
                    position,
                });
            }
            C2paUuidBox::Merkle => {
                summary.merkle.push(C2paMerkleBox { range, position });
            }
            C2paUuidBox::Unread(reason) => summary.unread.push(C2paUnreadBox {
                range,
                reason,
                position,
            }),
            C2paUuidBox::OtherExtendedType => summary.other_uuid_boxes += 1,
        }
    }
    summary
}

impl AvifContainer<'_> {
    /// A non-validating summary of the file's top-level C2PA boxes, in file order: every
    /// manifest-store slot, every C2PA box that yielded none with the reason, every auxiliary
    /// `merkle` box, and a count of `uuid` boxes of other extended types.
    ///
    /// The same scan as [`c2pa_slots`](Self::c2pa_slots), reported without the slots' bytes, and
    /// strictly the fuller view: a C2PA box that iterator drops is listed here. See
    /// [`C2paSummary`] for what it is for and [`C2PA_NOT_VALIDATED`] for what is said beside it.
    #[must_use]
    pub fn c2pa_summary(&self) -> C2paSummary {
        summary(self.segments())
    }
}

#[cfg(test)]
mod tests {
    use gamut_isobmff::C2PA_UUID;

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

    /// A top-level box segment at `start` whose header is 8 bytes.
    fn boxed<'a>(start: usize, ty: &[u8; 4], body: &'a [u8]) -> Segment<'a> {
        Segment {
            range: start..start + 8 + body.len(),
            kind: SegmentKind::Box { ty: *ty, body },
        }
    }

    #[test]
    fn the_summary_lists_every_top_level_c2pa_box_in_file_order_with_its_position() {
        let manifest = body(C2PA_UUID, [0; 4], b"original", &data(0, &[7, 7]));
        let foreign = body([0xAA; 16], [0; 4], b"manifest", &data(0, &[9]));
        let bad_version = body(C2PA_UUID, [1, 0, 0, 0], b"manifest", &data(0, &[9]));
        let merkle = body(C2PA_UUID, [0; 4], b"merkle", &[0; 4]);
        let update = body(C2PA_UUID, [0; 4], b"update", &data(0, &[8, 8, 8]));
        let segments = vec![
            boxed(0, b"ftyp", &[0; 12]),
            boxed(20, b"uuid", &manifest),
            boxed(100, b"uuid", &foreign),
            boxed(200, b"uuid", &bad_version),
            boxed(300, b"uuid", &merkle),
            boxed(400, b"mdat", &[0; 4]),
            boxed(500, b"uuid", &update),
        ];
        let summary = summary(&segments);
        assert_eq!(
            summary.stores,
            vec![
                C2paStoreSummary {
                    range: 65..67,
                    purpose: C2paBoxPurpose::Original,
                    position: C2paBoxPosition::BeforeMediaData,
                },
                C2paStoreSummary {
                    range: 543..546,
                    purpose: C2paBoxPurpose::Update,
                    position: C2paBoxPosition::AfterMediaData,
                },
            ]
        );
        assert_eq!(
            summary.unread,
            vec![C2paUnreadBox {
                range: 200..200 + 8 + bad_version.len(),
                reason: C2paUnreadReason::NotVersionZero,
                position: C2paBoxPosition::BeforeMediaData,
            }]
        );
        assert_eq!(
            summary.merkle,
            vec![C2paMerkleBox {
                range: 300..300 + 8 + merkle.len(),
                position: C2paBoxPosition::BeforeMediaData,
            }]
        );
        assert_eq!(summary.other_uuid_boxes, 1);
        // The slot view over the same scan sees exactly the two slots.
        assert_eq!(slots(&segments).count(), 2);

        // The detail lines interleave the three kinds in file order, and the count agrees.
        let details: Vec<String> = summary.detail_lines().collect();
        assert_eq!(summary.detail_line_count(), 4);
        assert_eq!(
            details,
            vec![
                "  box_purpose \"original\": 2-byte slot (store plus any padding) at [65, 67) — \
                 located, not validated"
                    .to_owned(),
                format!(
                    "  unread C2PA box at [200, {}): {}",
                    208 + bad_version.len(),
                    C2paUnreadReason::NotVersionZero.describe()
                ),
                format!(
                    "  auxiliary merkle box at [300, {}): Merkle-tree hashes (C2PA 2.4 §A.5.4.1.4), \
                     not a manifest store",
                    308 + merkle.len()
                ),
                "  box_purpose \"update\": 3-byte slot (store plus any padding) at [543, 546) — \
                 located, not validated; its box begins after the first mdat box"
                    .to_owned(),
            ]
        );
        let lines = summary.report_lines();
        assert_eq!(
            lines.len(),
            3 + 4,
            "headline, merkle count, other count, details"
        );
        assert_eq!(lines[3..], details[..]);
        assert_eq!(
            lines[0],
            format!(
                "C2PA: 2 manifest stores located, NOT VALIDATED, and 1 C2PA box is present from \
                 which no store could be read — a box gamut could not read through is NOT absence \
                 of provenance — {C2PA_NOT_VALIDATED}"
            )
        );
        assert_eq!(
            lines[1],
            "auxiliary C2PA merkle boxes: 1 (Merkle-tree hashes, C2PA 2.4 §A.5.4; not a manifest \
             store)"
        );
        assert_eq!(
            lines[2],
            "top-level uuid boxes of another extended type: 1 (not the C2PA one; a uuid box is not \
             provenance framing)"
        );
    }

    #[test]
    fn the_headline_keeps_the_four_outcomes_apart_and_carries_the_disclaimer() {
        let store = |start| C2paStoreSummary {
            range: start..start + 9,
            purpose: C2paBoxPurpose::Manifest,
            position: C2paBoxPosition::BeforeMediaData,
        };
        let unread = |start| C2paUnreadBox {
            range: start..start + 30,
            reason: C2paUnreadReason::UnknownPurpose,
            position: C2paBoxPosition::BeforeMediaData,
        };
        let headline = |stores: Vec<C2paStoreSummary>, unread: Vec<C2paUnreadBox>| {
            let summary = C2paSummary {
                stores,
                unread,
                ..C2paSummary::default()
            };
            assert_eq!(
                summary.summary_lines().len(),
                1,
                "no merkle or other-uuid line"
            );
            summary.summary_lines().remove(0)
        };
        assert_eq!(
            headline(vec![], vec![]),
            format!(
                "C2PA: no manifest store found in the top-level boxes of the primary stream — \
                 {C2PA_NOT_VALIDATED}"
            )
        );
        assert_eq!(
            headline(vec![], vec![unread(0), unread(50)]),
            format!(
                "C2PA: no manifest store could be read, but 2 C2PA boxes are present — that is NOT \
                 absence of provenance — {C2PA_NOT_VALIDATED}"
            )
        );
        assert_eq!(
            headline(vec![store(0)], vec![]),
            format!("C2PA: 1 manifest store located, NOT VALIDATED — {C2PA_NOT_VALIDATED}")
        );
        assert_eq!(
            headline(vec![store(0), store(50)], vec![unread(100), unread(200)]),
            format!(
                "C2PA: 2 manifest stores located, NOT VALIDATED, and 2 C2PA boxes are present from \
                 which no store could be read — a box gamut could not read through is NOT absence \
                 of provenance — {C2PA_NOT_VALIDATED}"
            )
        );
    }

    #[test]
    fn only_a_box_past_the_media_data_carries_a_position_note() {
        // The reasons' words are `gamut-isobmff`'s, tested there; the positions are this crate's.
        assert_eq!(C2paBoxPosition::BeforeMediaData.note(), None);
        assert_eq!(
            C2paBoxPosition::AfterMediaData.note(),
            Some("its box begins after the first mdat box")
        );
    }

    #[test]
    fn a_container_summary_reports_the_slot_the_encoder_wrote() {
        // End to end through the real container: the summary's range is the encoder's reported
        // range, and the slot's bytes appear nowhere in the report.
        let rgb = [0x40u8; 8 * 8 * 3];
        let image = gamut_core::ImageRef::<gamut_core::Rgb8>::new(
            &rgb,
            gamut_core::Dimensions {
                width: 8,
                height: 8,
            },
        )
        .expect("buffer matches dimensions");
        let (file, report) = crate::AvifEncoder::new()
            .with_c2pa_reserved(64)
            .encode_with_report(image)
            .expect("encode");
        let container = AvifContainer::parse(&file).expect("parse");
        let summary = container.c2pa_summary();
        assert_eq!(summary.stores.len(), 1);
        assert_eq!(Some(summary.stores[0].range.clone()), report.c2pa);
        assert_eq!(summary.stores[0].size(), 64);
        assert!(summary.unread.is_empty() && summary.merkle.is_empty());
    }
}
