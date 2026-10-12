//! The C2PA `ContentProvenanceBox` framing (C2PA 2.4 §A.5.1–§A.5.3): the one lens `gamut-heic` and
//! `gamut-avif` read a top-level `uuid` box through, and the one builder that writes one (#505).
//!
//! C2PA carries a BMFF asset's manifest store in a top-level `uuid` box whose 16-byte extended
//! (user) type is [`C2PA_UUID`]. §A.5.1.2 defines it as a `FullBox` with `version = 0` and
//! `flags = 0`:
//!
//! ```text
//! aligned(8) class ContentProvenanceBox extends FullBox('uuid', extended_type = C2PA_UUID,
//!                                                      version = 0, 0) {
//!   string box_purpose;   // null-terminated
//!   bit(8) data[];
//! }
//! ```
//!
//! For a box carrying a manifest store, §A.5.3 fixes what `data` holds: the 8-byte absolute file
//! offset of the first auxiliary `merkle` box (zero when the file has none), the manifest store,
//! then "zero or more unused padding bytes".
//!
//! This module knows that framing and nothing inside it: it parses no JUMBF beyond the store's
//! outer `LBox` length, verifies no hash, checks no signature and reaches no verdict. Placement
//! (after `ftyp`, before the first `mdat` and any `moov`) is the writer's
//! [`TopLevelPosition::AfterFtyp`](crate::TopLevelPosition::AfterFtyp), and where a box sits is the
//! consumer's to report.
//!
//! # Two bounds, two names
//!
//! The lens reports two regions of one box, and never calls one by the other's name:
//!
//! - the **slot** ([`C2paSlot::slot_bytes`]) — every byte of `data` after the merkle offset, up to
//!   the end of the box. It is what a writer reserves and a signer patches, and the only region a
//!   reservation nobody has filled *has*: an all-zero slot carries no store length to trim to.
//! - the **store** ([`C2paSlot::store`]) — the front of the slot, trimmed to the store's own JUMBF
//!   `LBox` so §A.5.3's padding is excluded. Under `references/c2pa/README.md`'s *refuse rather
//!   than guess* rule an `LBox` below the 8-byte JUMBF header or overrunning the slot is no store
//!   at all, never an error and never a truncated one.
//!
//! A consumer reports whichever its contract names — `gamut-heic` the store, `gamut-avif` the slot
//! — and the two now agree byte for byte on the one they share, because both come from here.
//!
//! # The ranges are not exclusion ranges
//!
//! Every range here is for patching, extraction and byte accounting. A BMFF asset's hard binding
//! is `c2pa.hash.bmff.v3`, which excludes content by **box path**, not by byte offset (§18.6,
//! §A.5.6), so hashing "everything but this range" is not how a BMFF manifest is bound. Every
//! range is therefore a [`C2paByteRange`] rather than a bare `Range<usize>`, the type a hash's
//! exclusion list takes (#780).

use core::fmt;

use gamut_core::{Error, Result};

use crate::segments::{Segment, SegmentKind};
use crate::writer::TOP_LEVEL_HEADER_MAX;

/// The 16-byte extended (user) type identifying a `uuid` box as a C2PA `ContentProvenanceBox`:
/// `D8FEC3D6-1B0E-483C-9297-5828877EC481` (C2PA 2.4 §A.5.1.1).
pub const C2PA_UUID: [u8; 16] = [
    0xD8, 0xFE, 0xC3, 0xD6, 0x1B, 0x0E, 0x48, 0x3C, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7E, 0xC4, 0x81,
];

/// Where a C2PA region sits in a file: a half-open byte range `[start, end)` reported for
/// **patching, extraction and byte accounting**, and deliberately not a `Range<usize>`.
///
/// A BMFF asset's hard binding is `c2pa.hash.bmff.v3`, which excludes content by **box path**,
/// not by byte offset (C2PA 2.4 §18.6, §A.5.6). Hashing "every byte but this range" is therefore
/// not how a BMFF manifest is bound, and a bare `Range<usize>` is exactly the shape a byte-range
/// hasher's exclusion list takes. This type reads as a position and nothing else: it offers
/// [`start`](Self::start), [`end`](Self::end) and [`len`](Self::len), extracts or patches its bytes
/// in a file ([`bytes_in`](Self::bytes_in), [`bytes_in_mut`](Self::bytes_in_mut)), renders as
/// `[start, end)`, and has **no conversion back to
/// a `Range`** or to any exclusion set, so feeding it to a hash takes a caller spelling the range
/// out by hand.
///
/// Every C2PA range `gamut-isobmff` reports ([`C2paSlot::range`], [`C2paSlot::store_range`]) is
/// one, and so is every range `gamut-heic` and `gamut-avif` report, so the three crates name one
/// type (#780).
///
/// ```
/// use gamut_isobmff::C2paByteRange;
///
/// let range = C2paByteRange::new(1037, 1042).expect("start <= end");
/// assert_eq!((range.start(), range.end(), range.len()), (1037, 1042, 5));
/// assert_eq!(range.to_string(), "[1037, 1042)");
/// // An end before the start places nothing.
/// assert_eq!(C2paByteRange::new(5, 4), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct C2paByteRange {
    /// The first byte's file offset. Never above `end`.
    start: usize,
    /// The file offset one past the last byte.
    end: usize,
}

impl C2paByteRange {
    /// The half-open range `[start, end)`, or `None` when `end` is before `start`. An empty range
    /// (`start == end`) is a range: an empty slot sits somewhere.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Option<Self> {
        if end < start {
            None
        } else {
            Some(Self { start, end })
        }
    }

    /// The file offset of the range's first byte.
    #[must_use]
    pub const fn start(self) -> usize {
        self.start
    }

    /// The file offset one past the range's last byte.
    #[must_use]
    pub const fn end(self) -> usize {
        self.end
    }

    /// The number of bytes the range spans: `end - start`.
    #[must_use]
    pub const fn len(self) -> usize {
        self.end - self.start
    }

    /// Whether the range spans no bytes.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The bytes this range covers in `file` — the extraction a report's range exists for — or
    /// `None` when `file` is too short to hold them (the range came from a different file).
    #[must_use]
    pub fn bytes_in(self, file: &[u8]) -> Option<&[u8]> {
        file.get(self.start..self.end)
    }

    /// The bytes this range covers in `file`, mutably — the patch a signer makes when it writes a
    /// finished store over a reserved slot of the same length — or `None` when `file` is too short
    /// to hold them.
    #[must_use]
    pub fn bytes_in_mut(self, file: &mut [u8]) -> Option<&mut [u8]> {
        file.get_mut(self.start..self.end)
    }
}

impl fmt::Display for C2paByteRange {
    /// The half-open interval notation every C2PA report line uses: `[start, end)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}, {})", self.start, self.end)
    }
}

/// The `FullBox` version (1 byte) and flags (3 bytes) that follow the user type: both zero
/// (§A.5.1.2, `version = 0, 0`).
const VERSION_FLAGS: [u8; 4] = [0; 4];

/// Length of the absolute file offset of the first `merkle` box that opens `data` (§A.5.3).
const MERKLE_OFFSET_LEN: usize = 8;

/// The `box_purpose` of the auxiliary box carrying Merkle-tree hashes (C2PA 2.4 §A.5.4.1.4): C2PA
/// framing, but not a manifest store.
const MERKLE_PURPOSE: &[u8] = b"merkle";

/// A JUMBF box header: its 4-byte big-endian `LBox` length and its 4-byte `TBox` type.
///
/// The general JUMBF grammar is ISO/IEC 19566-5, which C2PA references but does not restate and
/// which is not vendored. Within C2PA 2.4 the pair's width and endianness are traceable only
/// incidentally, through §8.4.2.3's description of the `c2sh` salt box; that `LBox` counts its own
/// header is the inference `references/c2pa/README.md` records. It is both the smallest `LBox` a
/// store may declare and the shortest slot a reservation may ask for.
const JUMBF_HEADER_LEN: usize = 8;

/// The longest `ContentProvenanceBox` payload [`content_provenance_reserved`] will build: a `Vec`
/// holds at most `isize::MAX` bytes, and asking for more *panics* with "capacity overflow" rather
/// than returning. Refusing above this ceiling turns that panic into an error.
///
/// On 64-bit targets this is a panic guard, not the effective limit: [`MAX_BOX_PAYLOAD_LEN`]
/// refuses long before it, and the largest `len` that clears it on the `manifest` framing is
/// `4_294_967_250`. On 32-bit targets (`wasm32`) `isize::MAX` is just under 2 GiB, so this ceiling
/// *is* the effective limit and every framed box it admits is below 4 GiB.
const MAX_PAYLOAD_LEN: usize = isize::MAX as usize;

/// The longest `ContentProvenanceBox` payload (everything after the user type) whose complete box
/// still fits a top-level box's 32-bit size field — the bound [`write`](crate::write) enforces on
/// every top-level box, checked here so a reservation is refused before its zeros are allocated.
const MAX_BOX_PAYLOAD_LEN: u64 = u32::MAX as u64 - TOP_LEVEL_HEADER_MAX as u64;

/// The `box_purpose` of a C2PA `uuid` box that carries a manifest store (C2PA 2.4 §A.5.3).
///
/// §A.5.3 admits exactly three purposes for a box that carries a manifest store:
///
/// | `box_purpose` | Meaning (§A.5.3) | Start of `data` |
/// | --- | --- | --- |
/// | `manifest` | the ordinary manifest store | the 8-byte merkle offset, then the store, then padding — stated |
/// | `original` | the unchanged store of a file mid-update; a sibling `update` box is present | as `manifest`: "the original and manifest boxes are identical apart from value of box_purpose" |
/// | `update` | a store holding update manifests only, the last box of the file | **not stated** — probed for, see below |
///
/// # `update`: the specification does not say, so the offset is probed for
///
/// §A.5.3's only sentence about an `update` box constrains the store's *contents* ("shall only
/// contain update manifests"), not the bytes ahead of it. The reference implementation, `c2pa-rs`,
/// writes the zero-filled 8-byte offset ahead of an `update` store exactly as for the other two and
/// skips it on read, so the mid-update files in circulation carry it; but the silence is a gap,
/// not a prohibition.
///
/// So `update` is **probed** by the store's own JUMBF `LBox`: the slot begins after the 8-byte
/// offset unless the `LBox` there is no valid bound *and* the one at the very start of `data` is —
/// then it begins at `data`'s start, and [`C2paSlot::merkle_offset`] is `None`. When neither offset
/// bounds a store, the slot keeps the reference implementation's layout (after the offset); a
/// `data` shorter than the offset itself can only be prefix-less, so it is the slot whole.
/// `manifest` and `original` are **not** probed: §A.5.3 states their framing, so one offset is used
/// and a `data` too short for it is [`C2paUnreadReason::Truncated`].
///
/// ## How strong the probe is
///
/// `LBox` validity is content-dependent, because a JUMBF superbox's interior is itself
/// length-prefixed: reading an `LBox` 8 bytes into a store that does *not* open with a merkle offset
/// lands past `LBox` and `TBox` on the first interior box's own length — small, plausible and
/// in-bounds — and can be accepted, bounding the reported store to a fragment of itself. So a
/// spec-conformant `manifest`/`original` store and a `c2pa-rs`-written `update` store are located
/// exactly; an out-of-spec `manifest`/`original` store written without the stated offset is
/// mis-bounded rather than rejected; and a prefix-less `update` store is the one *in-spec* layout
/// exposed to the hazard, which no known writer emits. The fallback is still strictly better than a
/// fixed 8-byte skip: it moves the slot only where the fixed offset bounds no store and offset 0
/// does, so it can rescue a file the fixed offset would have mis-read and never spoils one it would
/// have read right.
///
/// What would make it exact is a `TBox` check (a store's header is `LBox` then `jumb`; a wrong
/// offset lands on an interior box of another type). §A.3.9 and §15.12.3.2 name the superbox
/// `jumb`, but only as JPEG XL clauses citing ISO/IEC 18181-2, so asserting it narrows what is
/// reported on an aside about another container — a deliberate deferral, tracked in this crate's
/// `STATUS.md`, not an absence of source.
///
/// A fourth purpose, `merkle`, names an *auxiliary* box of Merkle-tree hashes (§A.5.4.1.4), whose
/// `data` is CBOR rather than a manifest store: [`C2paUuidBox::Merkle`], never a slot.
///
/// Non-exhaustive, with permanent append-only discriminants: match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum C2paBoxPurpose {
    /// `manifest` — the ordinary manifest store.
    Manifest = 0,
    /// `original` — the untouched previous store of a file being updated; framed as `manifest`.
    Original = 1,
    /// `update` — a store containing update manifests only, whose framing §A.5.3 does not state;
    /// see the [type docs](Self).
    Update = 2,
}

impl C2paBoxPurpose {
    /// The `box_purpose` string §A.5.3 spells for this purpose, without its NUL terminator — the
    /// exact bytes the lens maps back to this variant.
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

    /// How many bytes of `data` precede the slot: the stated 8-byte merkle offset for `manifest`
    /// and `original`, and for `update` the result of the [`LBox` probe](Self) — 0 only where the
    /// offset-8 `LBox` bounds no store and the offset-0 one does, or where `data` is shorter than the
    /// offset itself.
    fn slot_prefix(self, data: &[u8]) -> usize {
        match self {
            Self::Manifest | Self::Original => MERKLE_OFFSET_LEN,
            Self::Update => match data.get(MERKLE_OFFSET_LEN..) {
                Some(after) if lbox_bounded(after).is_some() || lbox_bounded(data).is_none() => {
                    MERKLE_OFFSET_LEN
                }
                _ => 0,
            },
        }
    }
}

/// The store a JUMBF `LBox` at the front of `slot` bounds, or `None` when it is no valid bound:
/// fewer than four bytes to read it from, a length below the 8-byte header it must itself cover, or
/// one overrunning `slot` — the *refuse rather than guess* rule of `references/c2pa/README.md`,
/// which rejects every extended-length sentinel by construction.
fn lbox_bounded(slot: &[u8]) -> Option<&[u8]> {
    let lbox = usize::try_from(u32::from_be_bytes(*slot.first_chunk::<4>()?)).ok()?;
    if lbox < JUMBF_HEADER_LEN {
        return None;
    }
    // The same `get` refuses an `LBox` that overruns the slot.
    slot.get(..lbox)
}

/// Why a top-level C2PA `ContentProvenanceBox` yielded no manifest store.
///
/// The box *is* a C2PA box — its extended type is [`C2PA_UUID`] — so the file carries provenance
/// framing and only its contents could not be read; a consumer reporting it as "no manifest store"
/// would let a reader infer *no provenance* from bytes it merely could not read. A `uuid` box of
/// any other extended type is [`C2paUuidBox::OtherExtendedType`], never one of these: §A.5.1.1
/// makes the extended type the whole test.
///
/// Non-exhaustive and with permanent discriminants: a later revision may distinguish a further
/// reason without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum C2paUnreadReason {
    /// The box ends before the framing C2PA 2.4 §A.5.1.2 and §A.5.3 require — the `FullBox`
    /// version and flags, the NUL that terminates `box_purpose`, or (for `manifest` and
    /// `original`) the 8-byte merkle offset.
    Truncated = 0,
    /// The `FullBox` version or flags are not zero, which §A.5.1.2 fixes them at.
    NotVersionZero = 1,
    /// The `box_purpose` is none C2PA 2.4 defines — neither a manifest store's (§A.5.3) nor the
    /// auxiliary `merkle` box's (§A.5.4.1.4).
    UnknownPurpose = 2,
    /// The framing is complete but no valid JUMBF `LBox` bounds a store where it puts one
    /// ([`C2paSlot::store`] is `None`). Never produced by [`C2paUuidBox::classify`], which reports
    /// the slot whatever it holds; a consumer that reports the *store* gives this reason for a slot
    /// with none.
    NoStoreBound = 3,
}

impl C2paUnreadReason {
    /// A one-clause explanation, for the line a report renders beside the box.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Truncated => "the box ends before the framing C2PA 2.4 §A.5.1.2/§A.5.3 requires",
            Self::NotVersionZero => {
                "its FullBox version or flags are not zero, which C2PA 2.4 §A.5.1.2 fixes them at"
            }
            Self::UnknownPurpose => "its box_purpose is none C2PA 2.4 defines (§A.5.3, §A.5.4.1.4)",
            Self::NoStoreBound => {
                "no valid JUMBF store length sits where its box_purpose puts the store"
            }
        }
    }
}

/// The manifest-store **slot** of one C2PA `ContentProvenanceBox`: the slot's bytes, its exact byte
/// range in the file, the box's `box_purpose`, and the merkle offset ahead of it.
///
/// The slot is **box-bounded**: it runs from just after the merkle offset to the end of the `uuid`
/// box, so for a filled box it is the store **plus any padding** §A.5.3 permits, and for a
/// reservation nobody has filled it is all zeros. [`store`](Self::store) is the store alone,
/// trimmed to its own JUMBF `LBox` so the padding is excluded.
///
/// # Two bounds, two names
///
/// The slot is what a writer reserves and a signer patches, and the only region an unfilled
/// reservation *has*: an all-zero slot carries no store length to trim to, so a store-only lens
/// would report a reservation as absent. The store is what a reader hands a validator. Both come
/// from this one lens, so `gamut-heic` (which reports the store) and `gamut-avif` (which reports
/// the slot) agree byte for byte wherever they name the same region (#505).
///
/// # The range is not an exclusion range
///
/// [`range`](Self::range) is for patching, extraction and byte accounting. A BMFF asset's hard
/// binding is `c2pa.hash.bmff.v3`, which excludes content by **box path**, not by byte offset
/// (C2PA 2.4 §18.6, §A.5.6), so hashing "everything but this range" is not how a BMFF manifest is
/// bound.
///
/// Non-exhaustive: a later revision may report more of the box's framing without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct C2paSlot<'a> {
    /// The slot: every byte of the box's `data` after the merkle offset — the store, then any
    /// padding — or all zeros for an unfilled reservation. Opaque; not trimmed to the store's own
    /// JUMBF `LBox` (that is [`store`](Self::store)).
    pub slot_bytes: &'a [u8],
    /// The half-open byte range [`slot_bytes`](Self::slot_bytes) occupies within the file, so
    /// `range.len() == slot_bytes.len()`.
    pub range: C2paByteRange,
    /// The `box_purpose` of the `uuid` box that carries this slot.
    pub purpose: C2paBoxPurpose,
    /// The big-endian absolute file offset of the first `merkle` box that opens `data` (§A.5.3;
    /// zero when the file has none), or `None` for an `update` box the
    /// [probe](C2paBoxPurpose) found without one. Read, never followed.
    pub merkle_offset: Option<u64>,
}

impl<'a> C2paSlot<'a> {
    /// The manifest store at the front of the slot, trimmed to its own JUMBF `LBox` so §A.5.3's
    /// trailing padding is excluded — or `None` when the slot holds no valid bound: an unfilled
    /// reservation, a slot shorter than the 4-byte length, an `LBox` below the 8-byte JUMBF header,
    /// or one overrunning the slot (*refuse rather than guess*, `references/c2pa/README.md`).
    ///
    /// The store begins where the slot does, so its file range is
    /// `[range.start(), range.start() + store.len())` — what [`store_range`](Self::store_range)
    /// reports.
    #[must_use]
    pub fn store(&self) -> Option<&'a [u8]> {
        lbox_bounded(self.slot_bytes)
    }

    /// The half-open byte range [`store`](Self::store) occupies within the file, or `None` exactly
    /// when that is `None`. The one place a consumer turns the trimmed store into a file range, so every
    /// crate reporting "the store" reports the same bytes (#505).
    #[must_use]
    pub fn store_range(&self) -> Option<C2paByteRange> {
        let len = self.store()?.len();
        // The store is a prefix of the slot, so its end never passes the slot's.
        Some(C2paByteRange {
            start: self.range.start,
            end: self.range.start + len,
        })
    }
}

/// What one top-level box of the primary stream is, read as a possible C2PA `ContentProvenanceBox`.
///
/// Exhaustive on purpose: a consumer must place every `uuid` box in exactly one of these, so a new
/// kind is a breaking change that makes each consumer decide where it goes rather than one a
/// wildcard arm would silently drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum C2paUuidBox<'a> {
    /// A manifest-store box (`box_purpose` `manifest`, `original` or `update`, §A.5.3) and its slot.
    Slot(C2paSlot<'a>),
    /// An auxiliary Merkle-tree box (`box_purpose` `merkle`, §A.5.4.1.4), whose `data` is not read.
    Merkle,
    /// A box carrying the C2PA extended type that yielded neither, and why.
    Unread(C2paUnreadReason),
    /// A `uuid` box carrying some other extended type — or a body too short to hold one — which is
    /// not C2PA framing however near the miss (§A.5.1.1).
    OtherExtendedType,
}

impl<'a> C2paUuidBox<'a> {
    /// Reads `segment` as a C2PA `ContentProvenanceBox`, or `None` when it is not a top-level
    /// `uuid` box at all — any other box type, a non-box segment, or a hand-built segment whose
    /// range is shorter than its body.
    ///
    /// The body is walked in the §A.5.1.2 order — user type, version/flags, `box_purpose` to its
    /// NUL, then `data` — and the slot is placed by [`C2paBoxPurpose`]'s framing. Malformed framing
    /// is [`C2paUuidBox::Unread`], never an error: this is a lens over bytes that happen to be
    /// present.
    #[must_use]
    pub fn classify(segment: &Segment<'a>) -> Option<Self> {
        let SegmentKind::Box { ty, body } = segment.kind else {
            return None;
        };
        if ty != *b"uuid" {
            return None;
        }
        // `range` spans the header and the body, so the body ends where the segment does,
        // whatever the header width (8 bytes, or 16 with `largesize`). A range too short to hold
        // the body places nothing.
        let body_end = segment.range.end;
        body_end.checked_sub(body.len())?;
        // A body too short to hold the user type carries nothing that identifies it as C2PA
        // framing. (`walk_segments` refuses such a box before a segment exists, so this is
        // reachable only from a hand-built segment.)
        let Some((user_type, after_uuid)) = body.split_at_checked(C2PA_UUID.len()) else {
            return Some(Self::OtherExtendedType);
        };
        if *user_type != C2PA_UUID {
            return Some(Self::OtherExtendedType);
        }
        let (purpose, merkle_offset, slot_bytes) = match read_after_user_type(after_uuid) {
            Ok(Some(found)) => found,
            Ok(None) => return Some(Self::Merkle),
            Err(reason) => return Some(Self::Unread(reason)),
        };
        // The slot is the body's tail, and the body fits below `body_end`, so this cannot wrap.
        Some(Self::Slot(C2paSlot {
            slot_bytes,
            range: C2paByteRange {
                start: body_end - slot_bytes.len(),
                end: body_end,
            },
            purpose,
            merkle_offset,
        }))
    }
}

/// The purpose, merkle offset and slot that follow the [`C2PA_UUID`] user type, `None` for a
/// `merkle` box, or why the framing could not be read.
type AfterUserType<'a> =
    core::result::Result<Option<(C2paBoxPurpose, Option<u64>, &'a [u8])>, C2paUnreadReason>;

/// Reads what follows the [`C2PA_UUID`] user type of a box already known to carry it. The slot is
/// the tail of `after_uuid`, so its file position is the box's end less its length.
fn read_after_user_type(after_uuid: &[u8]) -> AfterUserType<'_> {
    let (version_flags, rest) = after_uuid
        .split_at_checked(VERSION_FLAGS.len())
        .ok_or(C2paUnreadReason::Truncated)?;
    if *version_flags != VERSION_FLAGS {
        return Err(C2paUnreadReason::NotVersionZero);
    }
    let terminator = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or(C2paUnreadReason::Truncated)?;
    let (purpose_bytes, nul_and_data) = rest.split_at(terminator);
    if purpose_bytes == MERKLE_PURPOSE {
        return Ok(None);
    }
    let purpose =
        C2paBoxPurpose::from_bytes(purpose_bytes).ok_or(C2paUnreadReason::UnknownPurpose)?;
    let data = nul_and_data.get(1..).unwrap_or_default();
    let (prefix, slot) = data
        .split_at_checked(purpose.slot_prefix(data))
        .ok_or(C2paUnreadReason::Truncated)?;
    // An empty prefix is the probed prefix-less `update` layout; the stated one is exactly 8 bytes.
    let merkle_offset = prefix
        .first_chunk::<MERKLE_OFFSET_LEN>()
        .map(|b| u64::from_be_bytes(*b));
    Ok(Some((purpose, merkle_offset, slot)))
}

/// How many bytes of §A.5.1.2 framing precede the slot for `purpose`: the 4-byte zero `FullBox`
/// version and flags, the NUL-terminated `box_purpose`, and the 8-byte merkle offset.
const fn framing_len(purpose: C2paBoxPurpose) -> usize {
    VERSION_FLAGS.len() + purpose.as_str().len() + 1 + MERKLE_OFFSET_LEN
}

/// The framing a `ContentProvenanceBox` payload opens with — the zero `FullBox` version and flags,
/// the NUL-terminated `box_purpose`, and the 8-byte merkle offset written as zero (a still image
/// carries no `merkle` box) — in a buffer pre-sized to `capacity`, the payload's finished length.
fn content_provenance_framing(purpose: C2paBoxPurpose, capacity: usize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(capacity);
    payload.extend_from_slice(&VERSION_FLAGS);
    payload.extend_from_slice(purpose.as_str().as_bytes());
    payload.push(0);
    payload.extend_from_slice(&[0; MERKLE_OFFSET_LEN]);
    payload
}

/// The payload of a `ContentProvenanceBox` after its user type — what
/// [`TopLevelBox::uuid`](crate::TopLevelBox::uuid) takes with [`C2PA_UUID`]: the §A.5.1.2 framing
/// with a zero merkle offset, then `slot` verbatim. [`C2paUuidBox::classify`] reads it back as
/// exactly `slot`.
///
/// Infallible where [`content_provenance_reserved`] is not: `slot` is a materialised slice, so its
/// length is one the machine already holds, and no minimum applies to it. A box this long that the
/// 32-bit size field cannot hold is refused by [`write`](crate::write).
#[must_use]
pub fn content_provenance_payload(purpose: C2paBoxPurpose, slot: &[u8]) -> Vec<u8> {
    let mut payload = content_provenance_framing(purpose, framing_len(purpose));
    payload.reserve_exact(slot.len());
    payload.extend_from_slice(slot);
    payload
}

/// The same payload with a `len`-byte **reserved** slot of zeros, for an external signer to fill
/// once the file exists. The zeros are written once, into the payload buffer itself.
///
/// # Errors
///
/// Every refusal is decided from the integers before anything is allocated:
///
/// - [`Error::InvalidInput`] if `len` is below 8 bytes — a slot too small for a JUMBF box header can
///   never hold a manifest store — or if the framed payload would exceed `isize::MAX`, the most a
///   `Vec` can hold. Unchecked, the first emits a slot no signer can use; the second either wraps
///   the framing addition (a well-formed file whose C2PA box no lens can find) or panics in `Vec`.
/// - [`Error::Unsupported`] if the complete box would reach 4 GiB — the bound
///   [`write`](crate::write) applies to every top-level box. `Unsupported` rather than
///   `InvalidInput` (issue #576) because a box that long is well-formed ISOBMFF through a 64-bit
///   `largesize`; only this crate's writer, which emits 32-bit sizes, cannot produce it.
pub fn content_provenance_reserved(purpose: C2paBoxPurpose, len: usize) -> Result<Vec<u8>> {
    let total = reserved_payload_len(purpose, len)?;
    let mut payload = content_provenance_framing(purpose, total);
    payload.resize(total, 0);
    Ok(payload)
}

/// The length of the payload [`content_provenance_reserved`] builds for a `len`-byte slot, or why
/// it cannot be built — decided from the integers alone, so a refused reservation never allocates.
fn reserved_payload_len(purpose: C2paBoxPurpose, len: usize) -> Result<usize> {
    if len < JUMBF_HEADER_LEN {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: a reserved C2PA slot is at least 8 bytes, the size of a JUMBF box header",
        ));
    }
    let total = framing_len(purpose)
        .checked_add(len)
        .filter(|&total| total <= MAX_PAYLOAD_LEN)
        .ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: the reserved C2PA slot exceeds the largest ContentProvenanceBox that \
                 fits in memory",
            )
        })?;
    if u64::try_from(total).map_or(true, |total| total > MAX_BOX_PAYLOAD_LEN) {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: the reserved C2PA slot puts the ContentProvenanceBox at or beyond 4 GiB",
        ));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PURPOSES: [C2paBoxPurpose; 3] = [
        C2paBoxPurpose::Manifest,
        C2paBoxPurpose::Original,
        C2paBoxPurpose::Update,
    ];

    /// `[start, end)`, which the caller states with `start <= end`.
    fn range(start: usize, end: usize) -> C2paByteRange {
        C2paByteRange::new(start, end).expect("start <= end")
    }

    #[test]
    fn a_byte_range_refuses_only_an_end_before_its_start() {
        assert_eq!(C2paByteRange::new(5, 4), None);
        assert_eq!(
            C2paByteRange::new(4, 4),
            Some(C2paByteRange { start: 4, end: 4 })
        );
        assert_eq!(
            C2paByteRange::new(4, 9),
            Some(C2paByteRange { start: 4, end: 9 })
        );
    }

    #[test]
    fn a_byte_range_reports_its_bounds_and_length() {
        // A non-zero start, so a length read as the end offset is visible.
        let r = range(1037, 1042);
        assert_eq!((r.start(), r.end(), r.len()), (1037, 1042, 5));
        assert!(!r.is_empty());
        assert!(range(537, 537).is_empty());
    }

    #[test]
    fn a_byte_range_extracts_exactly_its_bytes_or_nothing() {
        let file = [0, 1, 2, 3, 4, 5];
        assert_eq!(range(2, 5).bytes_in(&file), Some(&file[2..5]));
        assert_eq!(range(6, 6).bytes_in(&file), Some(&[] as &[u8]));
        assert_eq!(range(4, 7).bytes_in(&file), None);
    }

    #[test]
    fn a_byte_range_patches_exactly_its_bytes_or_nothing() {
        let mut file = [0u8; 6];
        range(2, 5)
            .bytes_in_mut(&mut file)
            .expect("inside the file")
            .copy_from_slice(&[7, 8, 9]);
        assert_eq!(file, [0, 0, 7, 8, 9, 0]);
        assert_eq!(range(4, 7).bytes_in_mut(&mut file), None);
    }

    #[test]
    fn a_byte_range_renders_as_a_half_open_interval() {
        assert_eq!(range(61, 90).to_string(), "[61, 90)");
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

    /// `data` for a manifest-store box: a merkle offset then the slot.
    fn data(merkle_offset: u64, slot: &[u8]) -> Vec<u8> {
        let mut d = merkle_offset.to_be_bytes().to_vec();
        d.extend_from_slice(slot);
        d
    }

    /// A JUMBF-shaped store: a valid `LBox` covering `interior` plus the 8-byte header, a `jumb`
    /// `TBox`, then `interior`.
    fn store(interior: &[u8]) -> Vec<u8> {
        let mut s = u32::try_from(8 + interior.len())
            .expect("small")
            .to_be_bytes()
            .to_vec();
        s.extend_from_slice(b"jumb");
        s.extend_from_slice(interior);
        s
    }

    /// `body` as the top-level box segment starting at `start`, behind an 8-byte header.
    fn classify_at(start: usize, ty: [u8; 4], body: &[u8]) -> Option<C2paUuidBox<'_>> {
        C2paUuidBox::classify(&Segment {
            range: start..start + 8 + body.len(),
            kind: SegmentKind::Box { ty, body },
        })
    }

    /// The slot `body` carries as a `uuid` box at `start`, or `None`.
    fn slot_at(start: usize, body: &[u8]) -> Option<C2paSlot<'_>> {
        match classify_at(start, *b"uuid", body) {
            Some(C2paUuidBox::Slot(slot)) => Some(slot),
            _ => None,
        }
    }

    /// Why `body` yielded nothing, as a `uuid` box at 0.
    fn reason(body: &[u8]) -> Option<C2paUnreadReason> {
        match classify_at(0, *b"uuid", body) {
            Some(C2paUuidBox::Unread(reason)) => Some(reason),
            _ => None,
        }
    }

    #[test]
    fn content_provenance_payload_is_the_a_5_1_2_layout_exactly() {
        // Version 0 / flags 0, `manifest` with its NUL, eight zero bytes for the merkle offset
        // (§A.5.3: zero when the file has no merkle box), then the slot verbatim.
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
        assert_eq!(
            content_provenance_payload(C2paBoxPurpose::Manifest, &[]).len(),
            4 + 9 + 8
        );
    }

    #[test]
    fn a_reserved_slot_is_the_payload_a_zero_slot_would_give() {
        for len in [JUMBF_HEADER_LEN, JUMBF_HEADER_LEN + 1, 96] {
            for purpose in PURPOSES {
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
        for len in 0..JUMBF_HEADER_LEN {
            let err = content_provenance_reserved(C2paBoxPurpose::Manifest, len)
                .expect_err("shorter than a JUMBF box header");
            assert_eq!(err.kind(), gamut_core::ErrorKind::InvalidInput, "{len}");
            assert!(
                format!("{err}").contains("at least 8 bytes"),
                "{len}: {err}"
            );
        }
        // Literals on both sides, so moving the constant away from the 8 its message names fails
        // here (the mutation gate does not mutate constants).
        assert!(content_provenance_reserved(C2paBoxPurpose::Manifest, 7).is_err());
        assert!(content_provenance_reserved(C2paBoxPurpose::Manifest, 8).is_ok());
    }

    #[test]
    fn a_reservation_that_cannot_be_framed_is_refused_rather_than_truncated_or_panicked() {
        for purpose in PURPOSES {
            let over_ceiling = MAX_PAYLOAD_LEN - framing_len(purpose) + 1;
            for len in [over_ceiling, usize::MAX - framing_len(purpose), usize::MAX] {
                let err = content_provenance_reserved(purpose, len)
                    .expect_err("no framed payload this size can be built");
                assert_eq!(err.kind(), gamut_core::ErrorKind::InvalidInput);
                assert!(
                    format!("{err}").contains("exceeds the largest"),
                    "{purpose:?} at {len}: {err}"
                );
            }
        }
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn a_reservation_whose_box_reaches_4_gib_is_refused_before_it_is_allocated() {
        // Every length that could pass a loosened bound goes through `reserved_payload_len`, which
        // allocates nothing, so a mutant fails fast instead of zero-filling 4 GiB.
        assert!(reserved_payload_len(C2paBoxPurpose::Manifest, 4_294_967_250).is_ok());
        let err = reserved_payload_len(C2paBoxPurpose::Manifest, 4_294_967_251)
            .expect_err("the first refused length on the manifest framing");
        assert_eq!(err.kind(), gamut_core::ErrorKind::Unsupported, "{err}");
        for purpose in PURPOSES {
            let largest =
                usize::try_from(MAX_BOX_PAYLOAD_LEN).expect("64-bit") - framing_len(purpose);
            assert_eq!(
                reserved_payload_len(purpose, largest).expect("the box fits exactly"),
                largest + framing_len(purpose),
                "{purpose:?}"
            );
            let err = reserved_payload_len(purpose, largest + 1)
                .expect_err("one byte past the 32-bit box size");
            assert_eq!(
                err.kind(),
                gamut_core::ErrorKind::Unsupported,
                "{purpose:?}"
            );
            assert!(format!("{err}").contains("4 GiB"), "{purpose:?}: {err}");
        }
        let err = content_provenance_reserved(C2paBoxPurpose::Manifest, 4_294_967_251)
            .expect_err("refused before the zeros are allocated");
        assert_eq!(err.kind(), gamut_core::ErrorKind::Unsupported, "{err}");
    }

    #[test]
    fn the_writer_payload_reads_back_as_exactly_the_slot_it_was_given() {
        for purpose in PURPOSES {
            let slot = store(b"payload");
            let mut b = C2PA_UUID.to_vec();
            b.extend_from_slice(&content_provenance_payload(purpose, &slot));
            let found = slot_at(40, &b).expect("parses");
            assert_eq!(found.slot_bytes, &slot[..], "{purpose:?}");
            assert_eq!(found.purpose, purpose);
            assert_eq!(found.merkle_offset, Some(0));
            assert_eq!(found.range, range(48 + b.len() - slot.len(), 48 + b.len()));
        }
    }

    #[test]
    fn the_slot_follows_the_merkle_offset_at_its_file_offset() {
        // A non-zero box offset and merkle offset: the 8 offset bytes are read into
        // `merkle_offset`, not into the slot, and the slot is placed in the file exactly.
        let slot = [0x11, 0x22, 0x33, 0x44, 0x55];
        let b = body(C2PA_UUID, [0; 4], b"manifest", &data(0x0102_0304, &slot));
        let found = slot_at(992, &b).expect("a manifest box");
        assert_eq!(found.slot_bytes, &slot);
        assert_eq!(found.purpose, C2paBoxPurpose::Manifest);
        assert_eq!(found.merkle_offset, Some(0x0102_0304));
        // 992 + 8 (header) + 16 (user type) + 4 (version/flags) + 9 (`manifest\0`) + 8 (offset).
        assert_eq!(found.range, range(1037, 1042));
    }

    #[test]
    fn each_a_5_3_purpose_maps_and_nothing_else_is_a_slot() {
        let b = |purpose: &[u8]| body(C2PA_UUID, [0; 4], purpose, &data(0, &[1]));
        let purpose = |purpose: &[u8]| slot_at(0, &b(purpose)).map(|s| s.purpose);
        assert_eq!(purpose(b"manifest"), Some(C2paBoxPurpose::Manifest));
        assert_eq!(purpose(b"original"), Some(C2paBoxPurpose::Original));
        assert_eq!(purpose(b"update"), Some(C2paBoxPurpose::Update));
        assert_eq!(
            classify_at(0, *b"uuid", &b(b"merkle")),
            Some(C2paUuidBox::Merkle)
        );
        for unknown in [b"Manifest".as_slice(), b"manifes", b"manifesto", b""] {
            assert_eq!(
                reason(&b(unknown)),
                Some(C2paUnreadReason::UnknownPurpose),
                "{unknown:?}"
            );
        }
        for purpose in PURPOSES {
            assert_eq!(
                C2paBoxPurpose::from_bytes(purpose.as_str().as_bytes()),
                Some(purpose)
            );
        }
    }

    #[test]
    fn only_a_uuid_box_carrying_the_c2pa_user_type_is_c2pa_framing() {
        let b = body(C2PA_UUID, [0; 4], b"manifest", &data(0, &[1, 2, 3]));
        // §A.5.1.1 fixes the box type to `uuid`: a `free` box with the same body is not read.
        assert_eq!(classify_at(0, *b"free", &b), None);
        let gap = Segment {
            range: 0..b.len(),
            kind: SegmentKind::Trailer(&b),
        };
        assert_eq!(C2paUuidBox::classify(&gap), None);
        // A flipped user-type bit, or a body too short to hold one, is another extended type.
        let mut other = C2PA_UUID;
        other[15] ^= 0x01;
        assert_eq!(
            classify_at(
                0,
                *b"uuid",
                &body(other, [0; 4], b"manifest", &data(0, &[1]))
            ),
            Some(C2paUuidBox::OtherExtendedType)
        );
        assert_eq!(
            classify_at(0, *b"uuid", &C2PA_UUID[..15]),
            Some(C2paUuidBox::OtherExtendedType)
        );
        // A hand-built segment whose range cannot hold its body places nothing.
        let inconsistent = Segment {
            range: 0..3,
            kind: SegmentKind::Box {
                ty: *b"uuid",
                body: &b,
            },
        };
        assert_eq!(C2paUuidBox::classify(&inconsistent), None);
    }

    #[test]
    fn a_non_zero_full_box_header_is_not_version_zero() {
        let d = data(0, &[1, 2, 3]);
        for version_flags in [[1, 0, 0, 0], [0, 0, 0, 1], [0, 1, 0, 0]] {
            assert_eq!(
                reason(&body(C2PA_UUID, version_flags, b"manifest", &d)),
                Some(C2paUnreadReason::NotVersionZero),
                "{version_flags:?}"
            );
        }
    }

    #[test]
    fn every_cut_before_the_framing_is_complete_is_truncated() {
        // Cut inside the version/flags, inside `box_purpose` before its NUL, or inside the stated
        // merkle offset: all truncated. Cut exactly after the offset: a complete, empty slot.
        let full = body(C2PA_UUID, [0; 4], b"manifest", &data(0, &[]));
        assert_eq!(full.len(), 16 + 4 + 9 + 8);
        for cut in C2PA_UUID.len()..full.len() {
            assert_eq!(
                reason(&full[..cut]),
                Some(C2paUnreadReason::Truncated),
                "cut at {cut}"
            );
        }
        let empty = slot_at(492, &full).expect("empty slot");
        assert_eq!(empty.slot_bytes, &[] as &[u8]);
        assert_eq!(empty.range, range(537, 537));
    }

    #[test]
    fn update_follows_the_merkle_offset_when_a_store_is_bounded_there() {
        // The `c2pa-rs` layout: a zero merkle offset, whose `LBox` reading (0) bounds nothing, then
        // the store. The offset-8 bound wins.
        let s = store(b"update-store");
        let b = body(C2PA_UUID, [0; 4], b"update", &data(0, &s));
        let found = slot_at(0, &b).expect("an update slot");
        assert_eq!(found.slot_bytes, &s[..]);
        assert_eq!(found.merkle_offset, Some(0));
        assert_eq!(found.store(), Some(&s[..]));
        // Both offsets bound a store: the stated layout still wins.
        let both = data(u64::from_be_bytes(*b"\0\0\0\x08\0\0\0\0"), &s);
        let both = body(C2PA_UUID, [0; 4], b"update", &both);
        let found = slot_at(0, &both).expect("update slot");
        assert_eq!(found.slot_bytes, &s[..]);
    }

    #[test]
    fn update_falls_back_to_the_start_of_data_only_where_that_alone_bounds_a_store() {
        // Prefix-less: the store's own `LBox` is at offset 0, and offset 8 lands inside its
        // interior on four bytes that bound nothing.
        let s = store(&[0xFF; 12]);
        let b = body(C2PA_UUID, [0; 4], b"update", &s);
        let found = slot_at(0, &b).expect("a prefix-less update slot");
        assert_eq!(found.slot_bytes, &s[..]);
        assert_eq!(found.merkle_offset, None);
        // 8 + 16 + 4 + 7 (`update\0`) + 0.
        assert_eq!(found.range.start(), 35);
        assert_eq!(found.store(), Some(&s[..]));
        // Neither offset bounds a store: the reference layout stands, and there is no store.
        let neither = body(
            C2PA_UUID,
            [0; 4],
            b"update",
            &[0, 0, 0, 0, 0xAB, 0xAB, 0xAB, 0xAB, 0xAB],
        );
        let found = slot_at(0, &neither).expect("an update slot");
        assert_eq!(found.slot_bytes, &[0xAB]);
        assert_eq!(found.merkle_offset, Some(0xABAB_ABAB));
        assert_eq!(found.store(), None);
        // `data` shorter than the offset can only be prefix-less.
        let short = body(C2PA_UUID, [0; 4], b"update", &[1, 2, 3]);
        let short = slot_at(0, &short).expect("short");
        assert_eq!(short.slot_bytes, &[1, 2, 3]);
        assert_eq!(short.merkle_offset, None);
    }

    #[test]
    fn the_stated_purposes_are_not_probed() {
        // The same prefix-less store that `update` falls back for is, for `manifest` and
        // `original`, read past the stated offset; and data short of it is truncated.
        let s = store(&[0xFF; 12]);
        for purpose in [b"manifest".as_slice(), b"original"] {
            let b = body(C2PA_UUID, [0; 4], purpose, &s);
            let found = slot_at(0, &b).expect("a slot");
            assert_eq!(found.slot_bytes, &s[8..], "{purpose:?}");
            assert_eq!(
                reason(&body(C2PA_UUID, [0; 4], purpose, &[1, 2, 3])),
                Some(C2paUnreadReason::Truncated)
            );
        }
    }

    #[test]
    fn the_store_is_the_slot_trimmed_to_its_lbox() {
        // The store of a `manifest` box whose slot is `bytes`, owned so the box can be dropped.
        let slot = |bytes: &[u8]| {
            let b = body(C2PA_UUID, [0; 4], b"manifest", &data(0, bytes));
            slot_at(0, &b).expect("slot").store().map(<[u8]>::to_vec)
        };
        // Padding after the store is excluded.
        let mut padded = store(b"abc");
        let s = padded.clone();
        padded.extend_from_slice(&[0xEE; 5]);
        assert_eq!(slot(&padded), Some(s.clone()));
        // An `LBox` filling the slot exactly, and one of exactly the 8-byte header, are bounds.
        assert_eq!(slot(&s), Some(s.clone()));
        assert_eq!(slot(&store(&[])), Some(store(&[])));
        // Refuse rather than guess: a zero, sub-header or overrunning `LBox`, a slot too short to
        // hold one, or an unfilled reservation is no store.
        for bad in [
            &[0u8, 0, 0, 7, 1, 2, 3, 4, 5][..],
            &[0, 0, 0, 0, 1, 2, 3, 4],
            &[0, 0, 0, 9, 1, 2, 3, 4],
            &[0xFF, 0xFF, 0xFF, 0xFF, 1, 2, 3, 4],
            &[0, 0, 0],
            &[0; 64],
        ] {
            assert_eq!(slot(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_store_range_starts_at_the_slot_and_spans_only_the_store() {
        // A padded store 1000 bytes into the file: the slot runs to the end of the box, the store
        // range stops at the `LBox`. 1000 + 8 + 16 + 4 + 9 (`manifest\0`) + 8.
        let s = store(b"abc");
        let mut padded = s.clone();
        padded.extend_from_slice(&[0xEE; 5]);
        let b = body(C2PA_UUID, [0; 4], b"manifest", &data(0, &padded));
        let found = slot_at(1000, &b).expect("slot");
        assert_eq!(found.range, range(1045, 1045 + padded.len()));
        assert_eq!(found.store_range(), Some(range(1045, 1045 + s.len())));
        // No store, no store range — whatever the slot's length.
        let b = body(C2PA_UUID, [0; 4], b"manifest", &data(0, &[0; 64]));
        assert_eq!(slot_at(1000, &b).expect("slot").store_range(), None);
    }

    #[test]
    fn every_unread_reason_describes_itself_distinctly() {
        let reasons = [
            C2paUnreadReason::Truncated,
            C2paUnreadReason::NotVersionZero,
            C2paUnreadReason::UnknownPurpose,
            C2paUnreadReason::NoStoreBound,
        ];
        for (i, a) in reasons.iter().enumerate() {
            assert!(!a.describe().is_empty());
            for b in &reasons[i + 1..] {
                assert_ne!(a.describe(), b.describe());
            }
        }
    }
}
