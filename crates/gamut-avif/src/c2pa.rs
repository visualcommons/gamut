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
//! fills and locates ([`AvifContainer::c2pa_slots`]) is everything after the merkle offset,
//! padding included; [`C2paSlot::store`] / [`C2paSlot::store_range`] trim it to the store's own
//! JUMBF `LBox`. The non-validating report ([`AvifContainer::c2pa_summary`]) is `gamut-heif`'s
//! [`C2paSummary`], shared with `gamut-heic`, so the two crates report the same bytes for one
//! store.
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
//! "exclusion" so that no caller is invited to try. Every one is a half-open `Range<u64>` of
//! absolute file offsets, the shape every gamut container reports a C2PA region in.

use gamut_heif::C2paSummary;
#[cfg(doc)]
use gamut_isobmff::C2paBoxPurpose;
use gamut_isobmff::{C2paSlot, C2paUuidBox, Segment};

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
    /// [`IsoBmffImage::parse`](gamut_isobmff::IsoBmffImage::parse) stores in
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
/// reports is the range the reader finds. `classify` is `None` for every segment that is not a
/// top-level `uuid` box.
pub(crate) fn slots<'a, 's>(
    segments: &'s [Segment<'a>],
) -> impl Iterator<Item = C2paSlot<'a>> + 's {
    segments
        .iter()
        .filter_map(|segment| match C2paUuidBox::classify(segment)? {
            C2paUuidBox::Slot(slot) => Some(slot),
            _ => None,
        })
}

impl AvifContainer<'_> {
    /// A non-validating summary of the file's top-level C2PA boxes, in file order: every
    /// manifest store (with the slot it sits in), every C2PA box that yielded none with the reason,
    /// every auxiliary `merkle` box, and a count of `uuid` boxes of other extended types — the
    /// summary `gamut-heif` computes for every HEIF-family file.
    ///
    /// The same boxes as [`c2pa_slots`](Self::c2pa_slots), reported without the bytes, and
    /// strictly the fuller view: a C2PA box that iterator drops is listed here, and a slot it
    /// yields that bounds no store is listed as unread. See [`C2paSummary`] for what it is for
    /// and [`C2PA_NOT_VALIDATED`](crate::C2PA_NOT_VALIDATED) for what is said beside it.
    #[must_use]
    pub fn c2pa_summary(&self) -> C2paSummary {
        C2paSummary::from_segments(self.segments())
    }
}

#[cfg(test)]
mod tests {
    use core::ops::Range;

    use gamut_isobmff::{C2PA_UUID, C2paBoxPurpose, C2paUnreadReason, SegmentKind};

    use super::*;

    /// `[start, end)`, which the caller states with `start <= end`.
    fn range(start: usize, end: usize) -> Range<u64> {
        start as u64..end as u64
    }

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
        assert_eq!(stores[0].range, range(65, 67));
        assert_eq!(stores[1].purpose, C2paBoxPurpose::Update);
        assert_eq!(stores[1].slot_bytes, &[8, 8, 8]);
        // 300 + 8 + 16 + 4 + 7 (`update\0`) + 8.
        assert_eq!(stores[1].range, range(343, 346));
    }

    #[test]
    fn a_container_summary_reports_the_slot_the_encoder_wrote() {
        // End to end through the real container: the summary's slot is the encoder's reported
        // range, its range the store trimmed out of it, and an unfilled reservation is no store.
        let rgb = [0x40u8; 8 * 8 * 3];
        let image = || {
            gamut_core::ImageRef::<gamut_core::Rgb8>::new(
                &rgb,
                gamut_core::Dimensions::new(8, 8).unwrap(),
            )
            .expect("buffer matches dimensions")
        };
        // A 12-byte JUMBF store followed by 4 bytes of padding, signed in place.
        let mut padded = 12u32.to_be_bytes().to_vec();
        padded.extend_from_slice(b"jumbabcd");
        padded.extend_from_slice(&[0xEE; 4]);
        let mut file = Vec::new();
        let report = crate::AvifEncoder::new()
            .with_c2pa(&padded)
            .encode_with_report(image(), &mut file)
            .expect("encode");
        let slot = report.c2pa.expect("a slot was written");
        let summary = AvifContainer::parse(&file).expect("parse").c2pa_summary();
        assert_eq!(summary.stores.len(), 1);
        assert_eq!(summary.stores[0].slot, slot);
        assert_eq!(summary.stores[0].range, slot.start..slot.start + 12);
        assert!(summary.unread.is_empty() && summary.merkle.is_empty());

        let mut file = Vec::new();
        let report = crate::AvifEncoder::new()
            .with_c2pa_reserved(64)
            .expect("a usable reservation")
            .encode_with_report(image(), &mut file)
            .expect("encode");
        let summary = AvifContainer::parse(&file).expect("parse").c2pa_summary();
        assert!(summary.stores.is_empty());
        assert_eq!(summary.unread.len(), 1);
        assert_eq!(summary.unread[0].reason, C2paUnreadReason::NoStoreBound);
        // The unread entry names the whole box, which holds the reserved slot.
        let slot = report.c2pa.expect("a slot was reserved");
        assert!(summary.unread[0].range.start < slot.start);
        assert_eq!(summary.unread[0].range.end, slot.end);
    }
}
