//! Optional metadata embedded in a TIFF: an Exif sub-IFD plus XMP / IPTC / ICC / C2PA blocks.
//!
//! TIFF stores metadata the way it stores everything else — as IFD entries — so the seam is thin
//! by construction: [`TiffMetadata`] is a plain struct of optional payloads that
//! [`TiffEncoder::with_metadata`](crate::TiffEncoder::with_metadata) writes into IFD 0 and
//! [`TiffDecoder::metadata`](crate::TiffDecoder::metadata) reads back.
//!
//! Everything except EXIF is a **single opaque payload** in the file — XMP (700), IPTC-IIM
//! (33723), ICC (34675) and the C2PA manifest store (52545) — so this crate carries the bytes
//! verbatim in both directions and parses none of them. That is deliberate: they are the raw
//! blocks the workspace's metadata facade consumes (the same shape `gamut-png` and `gamut-webp`
//! hand over), and keeping them opaque here is what lets a caller choose its own conflict policy
//! instead of inheriting one from the container.
//!
//! EXIF is the exception, and only because TIFF makes it one: an `ExifIFD` (34665) *is* an IFD,
//! which this crate has already parsed by the time a caller sees it. Handing it back as
//! [`gamut_ifd::Ifd`] rather than as bytes saves every caller from re-parsing a directory the
//! decoder already walked. Its fields are neither validated nor completed — what the caller
//! supplies is what the file gets, and what the file holds is what the caller gets — subject to
//! the three normalisations a directory model implies, named on [`TiffMetadata::exif`], and to
//! the four shapes the encode **refuses** outright rather than write a file it could not read
//! back ([`TiffMetadata::check`]).
//!
//! # Where the blocks live, and what that costs a page-at-a-time reader
//!
//! Every block goes in **IFD 0** and only there, including for a multi-page document: they
//! describe the document, and an N-page file carrying N copies of an ICC profile is the worse
//! outcome. IFD 0 is also where a reader conventionally looks. The cost is real and worth
//! stating: a reader that decodes page 3 on its own sees no ICC profile, no XMP and no EXIF, and
//! must consult IFD 0 for them. The C2PA manifest store is the deliberate exception — §A.3.6
//! puts its entry in the **last** IFD of the main chain, so for a multi-page file that is the
//! last page rather than the first.
//!
//! # The C2PA manifest store
//!
//! One carrier has a placement rule of its own: C2PA 2.4 §A.3.6 puts the manifest store's entry
//! in the **last IFD of the main chain** and its bytes at the **end of the file**, and §18.5.5
//! makes an external signer exclude two disjoint ranges from its hard binding. All of that is
//! [`gamut_ifd::c2pa`]'s — the one place the workspace states §A.3.6, shared with `gamut-dng`
//! rather than re-derived here. This module only wires it to the encoder
//! ([`TiffEncoder::with_c2pa_reserved`](crate::TiffEncoder::with_c2pa_reserved)) and exposes the
//! read-side locator as [`c2pa_exclusions`].

use std::collections::BTreeSet;

use gamut_core::{Error, Result};
use gamut_ifd::c2pa::{self, C2paExclusions};
use gamut_ifd::{ByteOrder, Ifd, Value, Variant, read, read_header, read_ifd_at};

use crate::tags;

/// Metadata to embed in a TIFF, or read back from one: an Exif sub-IFD and/or opaque
/// XMP / IPTC-IIM / ICC / C2PA payloads.
///
/// `#[non_exhaustive]`, so a later carrier is an additive change: build one from
/// [`TiffMetadata::new`] and the `with_*` builders, or assign the public fields of a value you
/// already hold.
///
/// ```
/// use gamut_tiff::TiffMetadata;
///
/// let meta = TiffMetadata::new()
///     .with_xmp(b"<x:xmpmeta/>".to_vec())
///     .with_icc(vec![0, 0, 2, 32]);
/// assert!(!meta.is_empty());
/// assert_eq!(meta.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct TiffMetadata {
    /// The Exif private sub-IFD (`ExifIFD`, 34665), as the shared directory model.
    ///
    /// **Entries are carried unchanged; ordering is normalised.** This crate adds no mandatory
    /// Exif field — not even `ExifVersion` — and drops none, because a TIFF's `ExifIFD` is the
    /// caller's directory and completing it would silently change what a round trip returns. What
    /// it does not promise is byte-identity, because [`gamut_ifd::Ifd`] is a directory model
    /// rather than a byte range, and three normalisations are inherent to it:
    ///
    /// 1. fields are kept **sorted by ascending tag**, as TIFF 6.0 §2 requires on disk, so a
    ///    source directory written out of order comes back in order;
    /// 2. a **duplicated tag collapses** to its last occurrence;
    /// 3. a child directory's **next-IFD pointer is ignored** — a sub-IFD is a directory, not a
    ///    chain.
    ///
    /// A conforming source directory is unaffected by all three. A non-conforming one is
    /// silently repaired, which is worth knowing before using a re-encode to prove a file
    /// unmodified.
    ///
    /// The one shape the model can express and the file cannot is a tag holding **both** a field
    /// and a sub-IFD group: two entries under one tag, which TIFF 6.0 §2 does not allow. That is
    /// refused by the encode rather than normalised, because normalising it would mean choosing —
    /// silently — which of the two the caller meant; see [`TiffMetadata::check`].
    ///
    /// **Every standard pointer tag inside this directory is resolved.** All four members of
    /// [`gamut_ifd::tags::STANDARD_POINTER_TAGS`] — `SubIFDs` (330), `ExifIFD` (34665), `GPSInfo`
    /// (34853) and `InteroperabilityIFD` (40965) — come back as parsed
    /// [`sub_ifds`](gamut_ifd::Ifd::sub_ifds) groups rather than as raw offsets, so the writer
    /// gives each a fresh offset when the directory is embedded again. `InteroperabilityIFD` is
    /// the one EXIF 2.3 §4.6.3 puts here and the one a camera writes; the other three are
    /// resolved because a directory this crate *hands back* must not contain an offset into the
    /// file it was read from, whichever tag carries it.
    ///
    /// This is the Exif subtree's rule and not IFD 0's: at IFD 0 only `ExifIFD` is followed, so a
    /// `SubIFDs` or `GPSInfo` pointer sitting on the page — whose target feeds no field here —
    /// cannot fail the call. Inside this directory the same pointer is followed, and an
    /// unreadable target is an error, because this directory is the one that comes back.
    ///
    /// **Only the standard pointer tags are recognised as pointers.** A *private* tag whose value
    /// happens to be a `LONG` file offset — some vendors point at their own sub-directories this
    /// way — is indistinguishable from an ordinary integer field here, so it is carried through
    /// unchanged and re-encoded verbatim, still holding an offset into the file it was read from.
    /// Neither this crate nor [`deconstruct`](crate::deconstruct) can grade that, because neither
    /// knows the tag is a pointer. A caller rewriting a file with vendor metadata must not treat a
    /// round trip through this field as proof the result is pointer-safe.
    pub exif: Option<Ifd>,
    /// An XMP packet (UTF-8 RDF/XML), stored in the `XMP` tag (700) as `BYTE`, verbatim.
    pub xmp: Option<Vec<u8>>,
    /// A legacy IPTC-IIM dataset stream, stored in the `IPTC/NAA` tag (33723) as `BYTE`,
    /// verbatim.
    ///
    /// Kept as its own carrier rather than folded into [`xmp`](Self::xmp): IIM is a genuinely
    /// separate serialization that real TIFFs hold, and reconciling it into an XMP graph is a
    /// policy decision that belongs to the caller.
    pub iptc: Option<Vec<u8>>,
    /// An ICC profile, stored in the `ICCProfile` tag (34675) as `UNDEFINED`, verbatim.
    pub icc: Option<Vec<u8>>,
    /// A C2PA manifest store, stored in the `C2PA` tag
    /// ([`gamut_ifd::c2pa::C2PA_MANIFEST_STORE`], 52545, type `UNDEFINED`), verbatim.
    ///
    /// **Opaque, and bound to one exact file.** A manifest store is signed over the bytes
    /// *around* it (C2PA 2.4 §18.5), so the only store valid here is one an external signer
    /// computed over this encoder's own output — through
    /// [`TiffEncoder::with_c2pa_reserved`](crate::TiffEncoder::with_c2pa_reserved) and the
    /// exclusion ranges [`c2pa_exclusions`] reports. A store copied out of another file is
    /// invalid by construction. The bytes are written exactly as given: the TIFF header's
    /// `ByteOrder` does not govern them (§A.3.6).
    pub c2pa: Option<Vec<u8>>,
}

impl TiffMetadata {
    /// Creates an empty metadata set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a copy carrying `exif` as the file's `ExifIFD` sub-IFD.
    #[must_use]
    pub fn with_exif(mut self, exif: Ifd) -> Self {
        self.exif = Some(exif);
        self
    }

    /// Returns a copy carrying `packet` as the file's XMP.
    #[must_use]
    pub fn with_xmp(mut self, packet: Vec<u8>) -> Self {
        self.xmp = Some(packet);
        self
    }

    /// Returns a copy carrying `iim` as the file's IPTC-IIM block.
    #[must_use]
    pub fn with_iptc(mut self, iim: Vec<u8>) -> Self {
        self.iptc = Some(iim);
        self
    }

    /// Returns a copy carrying `profile` as the file's embedded ICC profile.
    #[must_use]
    pub fn with_icc(mut self, profile: Vec<u8>) -> Self {
        self.icc = Some(profile);
        self
    }

    /// Returns a copy carrying `store` as the file's C2PA manifest store — see
    /// [`c2pa`](Self::c2pa) for what makes a store valid.
    #[must_use]
    pub fn with_c2pa(mut self, store: Vec<u8>) -> Self {
        self.c2pa = Some(store);
        self
    }

    /// Whether there is nothing to embed: no payload set, and no Exif sub-IFD with content in it.
    ///
    /// An `exif` directory with no entries counts as empty — writing it would add an `ExifIFD`
    /// pointer to a directory with nothing in it. A sub-IFD group *is* content: a directory whose
    /// only entry is an `InteroperabilityIFD` pointer still writes one on-disk entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exif_ifd().is_none()
            && self.xmp.is_none()
            && self.iptc.is_none()
            && self.icc.is_none()
            && self.c2pa.is_none()
    }

    /// The Exif sub-IFD to write, or `None` when there is no Exif content worth a directory.
    ///
    /// A directory is worth writing when it holds **either** a field **or** a sub-IFD group.
    /// Testing only [`fields`](Ifd::fields) dropped a directory whose sole content was a group —
    /// an `ExifIFD` holding nothing but its `InteroperabilityIFD` pointer, which
    /// [`read_metadata`] returns in exactly that shape — silently, into a file with no Exif
    /// directory at all and no error to say so.
    fn exif_ifd(&self) -> Option<&Ifd> {
        self.exif
            .as_ref()
            .filter(|ifd| !ifd.fields().is_empty() || !ifd.sub_ifds().is_empty())
    }

    /// Refuses a set this crate would write into a file its own [`read_metadata`] then rejects,
    /// or reads back as something other than what was written.
    ///
    /// The Exif sub-IFD is a caller's directory, and nothing about a directory in memory stops it
    /// nesting a hundred levels down, hanging a group off a tag no reader treats as a pointer,
    /// carrying a bare integer under a tag every reader does, or naming one tag twice. Four bounds
    /// therefore apply, and [`check_exif_subtree`] reports them as **four distinct refusals**
    /// because they are four distinct mistakes:
    ///
    /// 1. **the field.** No field under a tag in [`EXIF_SUBTREE_POINTER_TAGS`] may carry a
    ///    pointer's own on-disk type code ([`POINTER_TYPE_CODES`]). The reader decides "pointer"
    ///    from the entry it parses, not from the group a caller built, so such a field is followed
    ///    as an offset into a file it never came from — the round trip returns a parsed directory,
    ///    an error, or nothing, but never the field that was written.
    /// 2. **the tag.** A group's tag must be one the reader resolves inside the Exif subtree
    ///    ([`EXIF_SUBTREE_POINTER_TAGS`]). Under any other tag the writer emits a pointer the
    ///    reader hands back as a raw offset into the file it came from, so the directory does not
    ///    survive a round trip.
    /// 3. **the pair.** One tag may carry a field **or** a group, never both: two entries under
    ///    one tag is not a TIFF directory (TIFF 6.0 §2), and the field is what a reader drops.
    /// 4. **the depth.** The reader follows [`MAX_POINTER_DEPTH`] levels below IFD 0 and refuses
    ///    what is deeper. The Exif directory occupies the first of those levels, so its own
    ///    nesting may use the rest — one further directory, which for a decoded camera EXIF is
    ///    `InteroperabilityIFD` (EXIF 2.3 §4.6.3).
    ///
    /// Only the Exif subtree is checked, because it is the only directory a caller supplies: the
    /// blocks [`apply`](Self::apply) writes into IFD 0 sit under `XMP`, `IPTC_NAA` and
    /// `ICC_PROFILE`, none of which any level treats as a pointer, and the rest of IFD 0 is the
    /// encoder's own.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`](gamut_core::Error::InvalidInput) if the Exif sub-IFD
    /// carries a pointer-typed field under a pointer tag, hangs a group off a tag the reader does
    /// not resolve, carries a field and a group under one tag, or nests deeper than the reader
    /// walks back.
    pub(crate) fn check(&self) -> Result<()> {
        match self.exif_ifd() {
            Some(exif) => check_exif_subtree(exif, MAX_POINTER_DEPTH - 1),
            None => Ok(()),
        }
    }

    /// Writes the XMP / IPTC / ICC blocks and the Exif sub-IFD into `ifd0`.
    ///
    /// The C2PA store is deliberately **not** written here: its bytes must land at the end of
    /// the file (C2PA 2.4 §A.3.6), after the image data, which only the encoder can arrange once
    /// the rest of the file exists ([`gamut_ifd::c2pa::append_store`]).
    pub(crate) fn apply(&self, ifd0: &mut Ifd) {
        if let Some(xmp) = &self.xmp {
            ifd0.set(tags::XMP, Value::Byte(xmp.clone()));
        }
        if let Some(iptc) = &self.iptc {
            ifd0.set(tags::IPTC_NAA, Value::Byte(iptc.clone()));
        }
        if let Some(icc) = &self.icc {
            ifd0.set(tags::ICC_PROFILE, Value::Undefined(icc.clone()));
        }
        if let Some(exif) = self.exif_ifd() {
            ifd0.set_sub_ifd(tags::EXIF_IFD, vec![exif.clone()]);
        }
    }
}

/// The pointer tags [`read_metadata`] resolves **at IFD 0**, scoped to what [`TiffMetadata`]
/// actually returns.
///
/// One tag, and it is a deliberate lower bound rather than a subset of convenience. `ExifIFD` is
/// followed because that directory **is** a field of [`TiffMetadata`]: it is handed to the caller
/// and may be written back, so a pointer under it that stayed a raw offset would be re-encoded
/// into a file laid out differently.
///
/// The other three members of [`gamut_ifd::tags::STANDARD_POINTER_TAGS`] are deliberately **not**
/// here. `SubIFDs` (330) locates thumbnails and reduced-resolution subfiles, `GPSInfo` (34853)
/// locates a GPS directory, and `InteroperabilityIFD` (40965) does not belong at IFD 0 at all;
/// none feeds any field of [`TiffMetadata`] from this level, and none is re-encoded by
/// [`TiffMetadata::apply`], which writes into a directory the encoder builds fresh. Following them
/// here could therefore only *add* failure modes, and it did: a single dangling `SubIFDs` offset
/// made XMP, IPTC, ICC and C2PA all unreachable on a file whose pixels decode perfectly, and two
/// pages sharing one thumbnail directory tripped the reader's cross-chain loop guard. A pointer
/// whose target this reader throws away must not be able to fail the whole call.
///
/// The same rule scopes *where* the walk runs, not only what it follows: it is applied to
/// **IFD 0's subtree and nowhere else** ([`resolve_pointers`]). Every page after IFD 0 feeds one
/// field of [`TiffMetadata`] — the C2PA manifest store, whose entry holds the store's bytes
/// directly rather than a pointer — so a *pointer* on such a page is a thrown-away target too,
/// and one on page 1 of a two-page document used to fail the whole call. [`gamut_ifd::read_tree`]
/// cannot be scoped either way: it resolves the flat list it is given at every node of every page.
const IFD0_POINTER_TAGS: &[u16] = &[tags::EXIF_IFD];

/// The pointer tags [`read_metadata`] resolves **inside the Exif subtree** — every standard one.
///
/// The scoping rule that keeps three tags out of [`IFD0_POINTER_TAGS`] puts all four in here, and
/// it is the same rule, not an exception to it: a pointer is followed exactly when its target
/// belongs to a directory [`TiffMetadata`] hands back. At IFD 0 a `SubIFDs` or `GPSInfo` target is
/// thrown away, so following it can only add failure modes. Under `ExifIFD` the enclosing
/// directory *is* returned, so a pointer left unresolved there is handed to the caller as a raw
/// absolute offset into the source file, and re-encoding it writes that offset into a file laid
/// out differently — the dangling pointer this crate's own [`deconstruct`](crate::deconstruct)
/// grades `Severity::Error`. `InteroperabilityIFD` is the one EXIF 2.3 §4.6.3 puts here, but a
/// `GPSInfo` or `SubIFDs` group under `ExifIFD` re-encodes just as badly, and the reader cannot
/// tell a caller's hand-built directory from a camera's.
///
/// The cost is stated rather than hidden: an unreadable target under *any* of these four fails
/// the whole [`read_metadata`] call, where at IFD 0 it would be ignored. That is the same trade
/// `ExifIFD` itself already makes — reporting `exif: None` for a directory the file declares
/// would be silent loss — extended to the pointers that directory contains.
///
/// What is still **not** resolved is a *private* tag whose value happens to be an offset; see
/// [`TiffMetadata::exif`] for why that is undecidable here and what it costs a caller.
const EXIF_SUBTREE_POINTER_TAGS: &[u16] = gamut_ifd::tags::STANDARD_POINTER_TAGS;

/// The pointer tags [`resolve_pointers`] follows at `depth`: [`IFD0_POINTER_TAGS`] at the page
/// itself, [`EXIF_SUBTREE_POINTER_TAGS`] at every level below it.
///
/// A per-node list rather than [`gamut_ifd::read_tree`]'s one flat list, because the two levels
/// answer opposite questions: at IFD 0 a followed pointer can only add a failure mode, and below
/// it an *un*followed pointer becomes a stale offset in a directory the caller is handed. Depth 0
/// is the only level whose directory is a page, and the walk never leaves IFD 0's subtree, so
/// "not depth 0" is exactly "inside the Exif subtree".
fn pointer_tags(depth: usize) -> &'static [u16] {
    if depth == 0 {
        IFD0_POINTER_TAGS
    } else {
        EXIF_SUBTREE_POINTER_TAGS
    }
}

/// An upper bound on the sub-IFD nesting [`resolve_pointers`] follows, bounding a hostile pointer
/// graph: a directory a hundred levels down is still a directory, and a file of a few kilobytes
/// holds enough of them to exhaust the stack.
///
/// It is **two**, not [`gamut_ifd::read_tree`]'s sixteen, because the walk starts at a page and
/// the deepest tree the tags it follows can legitimately reach is IFD 0 → `ExifIFD` → one
/// directory the Exif spec puts inside it, `InteroperabilityIFD` (EXIF 2.3 §4.6.3). Nothing
/// conformant nests a further directory below that, so a third level is already out of spec — a
/// generic reader needs sixteen because it is handed arbitrary tags, and this one is not.
const MAX_POINTER_DEPTH: usize = 2;

/// The on-disk field-type codes that make a directory entry a sub-IFD pointer: `LONG` (4),
/// the typed `IFD` (13) of TIFF Technical Note 1, and BigTIFF's `LONG8` (16) / `IFD8` (18).
///
/// The same set [`pointer_offsets`] accepts, stated as **codes** rather than as [`Value`]
/// variants, and that difference is the whole of this constant's reason to exist.
/// `pointer_offsets` answers about a value the *reader* parsed, where the variant and the on-disk
/// code agree by construction. [`check_exif_subtree`] answers about a value a *caller* built,
/// which nothing has written yet — and there the two can disagree: [`gamut_ifd::UnknownValue`]
/// carries an arbitrary type code beside its value word (its constructor validates only the
/// word's width), [`gamut_ifd::write`] emits that code verbatim, and the reader classifies the
/// entry by it. A `Value::Unknown` built at 4, 13, 16 or 18 is therefore a plain field to a
/// variant-shaped predicate and a **pointer** to the reader: it encoded cleanly and then failed
/// this crate's own [`read_metadata`] with `read out of bounds` or `value offset out of bounds`.
/// The discriminator that survives the write/read boundary is the code, so the writer asks about
/// the code.
///
/// The membership is pinned against `pointer_offsets` over the whole code space by
/// `the_pointer_type_codes_are_exactly_the_codes_the_resolver_follows`. The sibling half — a
/// `gamut-ifd` constructor that accepts a *known* code into `Unknown` at all — is issue #608 and
/// is not fixable from this crate.
const POINTER_TYPE_CODES: &[u16] = &[4, 13, 16, 18];

/// Whether the reader would follow `value` as a sub-IFD pointer once it has been written out and
/// read back: its on-disk type code ([`Value::type_code`], total over every variant including
/// `Unknown`) is one of [`POINTER_TYPE_CODES`].
fn is_pointer_typed(value: &Value) -> bool {
    POINTER_TYPE_CODES.contains(&value.type_code())
}

/// The refusal earned by a field under a tag in [`EXIF_SUBTREE_POINTER_TAGS`] whose on-disk type
/// code is in [`POINTER_TYPE_CODES`] — [`check_exif_subtree`]'s first clause.
///
/// A named constant rather than a literal in place because it **enumerates the tag set in prose**,
/// as this crate's public documentation does, while the set itself is a sibling crate's constant.
/// Naming it lets the enumeration be pinned in one assertion instead of drifting silently.
const POINTER_FIELD_REFUSAL: &str = "TIFF: an Exif sub-IFD may not carry a plain field under a \
     standard pointer tag (SubIFDs, ExifIFD, GPSInfo, InteroperabilityIFD) with a pointer's own \
     type (LONG, IFD, LONG8 or IFD8), which the reader would follow as a file offset";

/// The refusal earned by a sub-IFD group under a tag *outside* [`EXIF_SUBTREE_POINTER_TAGS`] —
/// [`check_exif_subtree`]'s second clause. Named for the same reason as
/// [`POINTER_FIELD_REFUSAL`].
const FOREIGN_GROUP_REFUSAL: &str = "TIFF: an Exif sub-IFD may only nest a group under a standard \
     pointer tag (SubIFDs, ExifIFD, GPSInfo, InteroperabilityIFD) and this one uses another, \
     which would read back as a raw file offset";

/// The refusal earned by a tag carrying **both** a plain field and a sub-IFD group —
/// [`check_exif_subtree`]'s third clause.
///
/// [`gamut_ifd::Ifd`] holds fields and groups in two lists, so one tag can appear in both; the
/// writer then emits **two entries under that tag**, which TIFF 6.0 §2 does not allow, and every
/// reader keeps exactly one of them. Which one differs: this crate's model collapses a duplicated
/// tag to its **last** occurrence, while libtiff marks every occurrence after the **first** to be
/// ignored (`tif_dirread.c`, "Mark duplicates of any tag to be ignored") and warns that the
/// directory is not sorted in ascending order. So the entry that survives depends on the reader,
/// and the field the caller set is dropped by at least one of them.
const FIELD_BESIDE_GROUP_REFUSAL: &str = "TIFF: an Exif sub-IFD may not carry both a plain field \
     and a sub-IFD group under one tag, which would be written as two entries under that tag \
     (TIFF 6.0 §2 allows one) and read back as only one of them";

/// The writer's side of what the reader delivers: refuses an Exif subtree this crate could not
/// hand back unchanged, `depth` further levels being all that is left below `ifd`.
///
/// **This inspects what [`resolve_pointers`] inspects, and that symmetry is the whole design.**
/// The reader decides "pointer" from a directory's *fields* — `ifd.get(tag)` under a tag in
/// [`EXIF_SUBTREE_POINTER_TAGS`] whose entry carries one of [`POINTER_TYPE_CODES`] — while a
/// caller builds one from [`sub_ifds`](Ifd::sub_ifds) *groups*. Checking only the groups left the
/// writer blind to the very shape the reader misreads: a pointer tag carried as a plain `LONG`,
/// which encoded cleanly and then failed this crate's own [`read_metadata`] with `read out of
/// bounds` or `sub-IFD pointer loop` depending on the integer. So both are checked, at every
/// level.
///
/// The symmetry is stated across the **write/read boundary**, not on a parsed value, because that
/// boundary is where it kept breaking: a caller's [`Value`] and the entry the reader will parse
/// agree on nothing but the on-disk type code, so the code is what both sides ask about — see
/// [`POINTER_TYPE_CODES`].
///
/// Four refusals, deliberately distinct, because they are four different mistakes and a caller
/// reading the message has to know which one it made:
///
/// * a **field** under a tag *in* [`EXIF_SUBTREE_POINTER_TAGS`] whose type is a pointer's own —
///   the reader follows it as a file offset into a file it did not come from, so what came back
///   is a parsed directory, an error, or nothing, but never the field that was written. Only the
///   pointer *type codes* are refused: the reader leaves every other type in place, so a `SHORT`
///   under `SubIFDs` is left alone here too;
/// * a **group** under a tag *outside* [`EXIF_SUBTREE_POINTER_TAGS`] — the reader leaves that
///   pointer as a raw absolute offset, so what came back would not be what was written;
/// * a **field beside a group** under one tag — the writer emits two entries under it, which
///   TIFF 6.0 §2 does not allow, and the field is the one a reader drops
///   ([`FIELD_BESIDE_GROUP_REFUSAL`]);
/// * a *child directory* nested past `depth` — the reader refuses to walk that far
///   ([`MAX_POINTER_DEPTH`]). A group with no children reaches no further level, so it is the
///   children and not the group that the bound counts.
///
/// The order is field, then group tag, then field-beside-group, then depth, and it is the order of
/// how little the rest of the tree matters to each: a pointer-typed field is unreturnable whatever
/// else the directory holds, a group under an unfollowed tag is unreturnable whatever its depth, a
/// duplicated tag is unreturnable whatever is below it, and only the depth clause needs the tree
/// walked. All four stop at the first offender and the depth bound stops at the bound rather than
/// measuring the whole tree, so a directory a caller nested a hundred levels deep costs a hundred
/// levels of neither recursion nor time.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`](gamut_core::Error::InvalidInput) for any of the four refusals.
fn check_exif_subtree(ifd: &Ifd, depth: usize) -> Result<()> {
    for &tag in EXIF_SUBTREE_POINTER_TAGS {
        if ifd.get(tag).is_some_and(is_pointer_typed) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                POINTER_FIELD_REFUSAL,
            ));
        }
    }
    for group in ifd.sub_ifds() {
        if !EXIF_SUBTREE_POINTER_TAGS.contains(&group.tag) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                FOREIGN_GROUP_REFUSAL,
            ));
        }
        if ifd.get(group.tag).is_some() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                FIELD_BESIDE_GROUP_REFUSAL,
            ));
        }
        for child in &group.ifds {
            let Some(left) = depth.checked_sub(1) else {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: an Exif sub-IFD may nest one further directory \
                     (ExifIFD -> InteroperabilityIFD, EXIF 2.3 §4.6.3) and this one nests deeper",
                ));
            };
            check_exif_subtree(child, left)?;
        }
    }
    Ok(())
}

/// The file offsets a sub-IFD pointer value carries: a `LONG` array (TIFF 6.0 §2), the typed
/// `IFD` (13) form of TIFF Technical Note 1, or BigTIFF's 64-bit `LONG8`/`IFD8` forms. Any other
/// type is not a pointer, and its field is left in place — the rule
/// [`gamut_ifd::read_tree`] applies, restated here so the two walks cannot disagree about what a
/// pointer is.
///
/// The source guards its 64-bit arm with `#[cfg(feature = "bigtiff")]` and this restatement does
/// not, because it cannot: `bigtiff` is `gamut-ifd`'s feature, enabled unconditionally by this
/// crate's dependency on it and not re-exported, so the same attribute here would name a feature
/// `gamut-tiff` does not have. `unexpected_cfgs` rejects it under the workspace's `-D warnings`,
/// and were it accepted the arm would vanish and every BigTIFF's `ExifIFD` would read back as a
/// plain integer. The arm is therefore always live here, which is what a codec that always writes
/// and reads BigTIFF needs.
fn pointer_offsets(value: &Value) -> Option<Vec<u64>> {
    match value {
        Value::Long(v) | Value::Ifd(v) => Some(v.iter().map(|&x| u64::from(x)).collect()),
        Value::Long8(v) | Value::Ifd8(v) => Some(v.clone()),
        _ => None,
    }
}

/// Resolves [`pointer_tags`]`(depth)` over `ifd` and, recursively, over the children it reaches,
/// replacing each pointer field with a [`sub_ifds`](Ifd::sub_ifds) group — what
/// [`gamut_ifd::read_tree`] does for a whole file, applied to **one** directory's subtree with a
/// **per-level** tag list.
///
/// The scoping is the whole reason this exists, and it has two axes. `read_tree` resolves the flat
/// list it is handed at every node of every page, so a pointer on a page [`read_metadata`] discards
/// can fail a call whose answer that page never contributed to — hence one page. And it cannot
/// vary the list by level, so a list wide enough to keep the Exif directory pointer-free would
/// make an unrelated `SubIFDs` offset on the page itself able to fail the call — hence
/// [`pointer_tags`]. Following a pointer by hand is [`gamut_ifd::read_ifd_at`]'s documented
/// purpose. `visited` spans the walk and `depth` bounds it, so a cycle or two pointers claiming one
/// directory fail here rather than loop — the guards are restated because they guard *this* walk.
///
/// `visited` is a **set**, not a list, and that is a hardening decision rather than a style one:
/// nothing bounds how many offsets one pointer array holds, so a linear membership scan makes the
/// walk quadratic in a number a hostile file chooses. Measured in release on hand-built files
/// whose single `ExifIFD` array names N distinct empty directories, a scanned list took 0.24 s at
/// 0.6 MB and 4.2 s at 2.3 MB — clean quadratic growth, every call returning `Ok`. A set answers
/// the same files in 5 ms and 27 ms. What still has no bound is the *breadth* of one pointer
/// array; that is issue #579.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`](gamut_core::Error::InvalidInput) if a pointer target is
/// unreadable, if two pointers name one directory, or if the tree nests deeper than
/// [`MAX_POINTER_DEPTH`].
fn resolve_pointers(
    data: &[u8],
    order: ByteOrder,
    variant: Variant,
    ifd: &mut Ifd,
    visited: &mut BTreeSet<u64>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_POINTER_DEPTH {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: sub-IFD tree too deep",
        ));
    }
    for &tag in pointer_tags(depth) {
        let Some(offsets) = ifd.get(tag).and_then(pointer_offsets) else {
            continue;
        };
        let mut children = Vec::with_capacity(offsets.len());
        for offset in offsets {
            if !visited.insert(offset) {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: sub-IFD pointer loop",
                ));
            }
            let mut child = read_ifd_at(data, offset, order, variant)?;
            resolve_pointers(data, order, variant, &mut child, visited, depth + 1)?;
            children.push(child);
        }
        ifd.remove(tag);
        ifd.set_sub_ifd(tag, children);
    }
    Ok(())
}

/// A raw `BYTE`/`UNDEFINED` payload, copied out of a directory entry.
fn bytes_value(value: Option<&Value>) -> Option<Vec<u8>> {
    value.and_then(Value::as_bytes).map(<[u8]>::to_vec)
}

/// Reads the metadata a TIFF carries: IFD 0's blocks and Exif sub-IFD, plus the C2PA manifest
/// store from the last IFD of the main chain (C2PA 2.4 §A.3.6).
///
/// Whether a tag-52545 entry *is* a manifest store is [`gamut_ifd::c2pa::locate`]'s decision, not
/// a second opinion held here: this asks the locator first and reports the store only when it
/// agrees. That matters because `locate` reports absence for an entry a caller could not use —
/// one of the wrong type, one too short to be a JUMBF box, a duplicated one — and reporting such
/// an entry as a store would hand the caller a [`TiffMetadata`] that
/// [`TiffEncoder`](crate::TiffEncoder) then refuses to encode. `gamut-dng` gates its own decode
/// on the same locator for the same reason.
///
/// Reader and locator agreeing is not the same as every readable store being writable, and this
/// does not promise the latter — it cannot, because a reader does not know which container the
/// caller will write. One case exists: `locate` accepts a store of exactly
/// [`MIN_STORE_LEN`](gamut_ifd::c2pa::MIN_STORE_LEN) (8) bytes, while writing **BigTIFF** needs 9,
/// since 8 bytes would pack into the entry's own value word instead of being placed out of line
/// at the end of the file. So an 8-byte store read from any file cannot be written back to a
/// BigTIFF. The affected input is degenerate — 8 bytes is a JUMBF box header with no content, so
/// there is no manifest in it — but the refusal is real and comes from
/// [`TiffEncoder::with_c2pa_reserved`](crate::TiffEncoder::with_c2pa_reserved)'s placement rule,
/// not from disagreement here.
pub(crate) fn read_metadata(data: &[u8]) -> Result<TiffMetadata> {
    let (order, variant, _) = read_header(data)?;
    // The chain alone: `read` follows no pointer at all. A file with no IFD carries no metadata.
    let mut ifds = read(data)?.ifds;
    let Some(ifd0) = ifds.first_mut() else {
        return Ok(TiffMetadata::new());
    };
    // IFD 0's subtree and no other page's, with a per-level tag list — see [`pointer_tags`].
    resolve_pointers(data, order, variant, ifd0, &mut BTreeSet::new(), 0)?;
    let exif = ifd0
        .sub_ifds()
        .iter()
        .find(|group| group.tag == tags::EXIF_IFD)
        .and_then(|group| group.ifds.first())
        .cloned();
    let xmp = bytes_value(ifd0.get(tags::XMP));
    let iptc = bytes_value(ifd0.get(tags::IPTC_NAA));
    let icc = bytes_value(ifd0.get(tags::ICC_PROFILE));
    // §A.3.6: one store for the whole asset, in the last IFD of the main chain. `ifds` is that
    // chain, so its last element is where the entry belongs — and a single-page file makes the
    // two the same directory. The entry carries the store's bytes rather than an offset, so the
    // last IFD is reached without following a pointer, on a page whose pointers were never
    // resolved.
    let located = c2pa::locate(data)?.is_some();
    let c2pa = match ifds
        .last()
        .and_then(|ifd| ifd.get(tags::C2PA_MANIFEST_STORE))
    {
        Some(Value::Undefined(store)) if located => Some(store.clone()),
        _ => None,
    };
    Ok(TiffMetadata {
        exif,
        xmp,
        iptc,
        icc,
        c2pa,
    })
}

/// The byte ranges an external signer excludes from a `c2pa.hash.data` hard binding over `file`
/// (C2PA 2.4 §18.5.5): the manifest store's own bytes, and the `count` field of its IFD entry.
///
/// Returns `Ok(None)` when `file` carries no manifest store — including when a tag-52545 entry
/// sits somewhere other than the last IFD of the main chain, which §A.3.6 does not allow. The
/// two ranges are always disjoint, and both are offsets from the first byte of `file`.
///
/// This is the read side of [`TiffEncoder::with_c2pa_reserved`](crate::TiffEncoder::with_c2pa_reserved):
/// it reports where a store *is*, whether this crate wrote the file or not, so a caller that
/// encoded through a path without a report (a palette or multi-page image) recovers the ranges
/// from the bytes.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`](gamut_core::Error::InvalidInput) if the container is
/// unreadable (bad header, looping or runaway IFD chain, no IFD) or the store's declared extent
/// lies outside `file`.
pub fn c2pa_exclusions(file: &[u8]) -> Result<Option<C2paExclusions>> {
    c2pa::locate(file)
}

#[cfg(test)]
mod tests {
    use gamut_ifd::{ByteOrder, TiffFile, Variant, write};

    use super::*;

    /// A directory holding one recognisable field.
    fn exif_ifd() -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(33434, Value::Rational(vec![(1, 250)])); // ExposureTime
        ifd
    }

    /// A minimal one-page file carrying `ifd0`, so `read_metadata` has a chain to walk.
    fn file_with(ifd0: Ifd) -> Vec<u8> {
        file_with_variant(ifd0, Variant::Classic)
    }

    /// [`file_with`] in a named container variant, for the axes on which classic and BigTIFF
    /// differ: the width of a pointer's value word, and therefore its field type.
    fn file_with_variant(ifd0: Ifd, variant: Variant) -> Vec<u8> {
        write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant,
            ifds: vec![ifd0],
        })
        .expect("write")
    }

    /// One value of every on-disk field type, keyed by the type's code.
    ///
    /// Exhaustive over [`gamut_ifd::FieldType`] by construction: the compiler rejects this match
    /// the day a field type is added upstream, so no caller of it can silently sweep a set short
    /// by one — which is exactly the defect that produced the pointer-field regression.
    fn canonical_value(ty: gamut_ifd::FieldType) -> Value {
        use gamut_ifd::FieldType as T;
        match ty {
            T::Byte => Value::Byte(vec![8]),
            T::Ascii => Value::Ascii("ab".into()),
            T::Short => Value::Short(vec![8]),
            T::Long => Value::Long(vec![8]),
            T::Rational => Value::Rational(vec![(1, 2)]),
            T::SByte => Value::SByte(vec![8]),
            T::Undefined => Value::Undefined(vec![8]),
            T::SShort => Value::SShort(vec![8]),
            T::SLong => Value::SLong(vec![8]),
            T::SRational => Value::SRational(vec![(1, 2)]),
            T::Float => Value::Float(vec![1.0]),
            T::Double => Value::Double(vec![1.0]),
            T::Ifd => Value::Ifd(vec![8]),
            T::Utf8 => Value::Utf8("ab".into()),
            T::Long8 => Value::Long8(vec![8]),
            T::SLong8 => Value::SLong8(vec![8]),
            T::Ifd8 => Value::Ifd8(vec![8]),
        }
    }

    /// Every on-disk field-type code a directory entry can carry, derived by sweeping the whole
    /// `u16` code space rather than listed by hand.
    fn every_type_code() -> Vec<u16> {
        (0..=u16::MAX)
            .filter(|&code| gamut_ifd::FieldType::from_code(code).is_some())
            .collect()
    }

    /// One `Value` per **well-formed** directory entry representable in a file of `variant`: the
    /// natural variant of every recognised type code, the [`Value::Unknown`] form carrying that
    /// same code with an opaque value word, and the `Unknown` form at a few codes no field type
    /// claims.
    ///
    /// Derived from [`every_type_code`], so a type added upstream enters this sweep without an
    /// edit here; the `Unknown` arm exists because it is the one shape whose in-memory variant and
    /// on-disk code disagree, which is the whole subject of the sweep.
    ///
    /// **Well-formed** excludes one thing, and only for the `Unknown` arm: a single value of a
    /// recognised type wider than the variant's value word would be written *out of line*, and the
    /// word an `UnknownValue` carries is then a raw file offset a test cannot know. Such an entry
    /// is unreadable whatever tag it sits under — `Rational` under `Classic` fails
    /// `read_metadata` with `value offset out of bounds` — so it is a malformed entry rather than a
    /// misclassified pointer, and this crate cannot refuse it without refusing every vendor entry
    /// of an unrecognised type read out of a real file. That gap is issue #608, on the constructor
    /// that admits it. All four pointer codes are still swept: 4 and 13 fit both variants' words,
    /// 16 and 18 fit BigTIFF's.
    fn every_representable_value(variant: Variant) -> Vec<(String, Value)> {
        let word = vec![8u8; variant.offset_size()];
        let unknown = |code: u16| {
            Value::Unknown(
                gamut_ifd::UnknownValue::new(code, 1, &word, ByteOrder::LittleEndian, variant)
                    .expect("an unknown-type entry of the file's own width"),
            )
        };
        let mut values = Vec::new();
        for code in every_type_code() {
            let ty = gamut_ifd::FieldType::from_code(code).expect("a swept code");
            values.push((format!("{ty:?}({code})"), canonical_value(ty)));
            if ty.size() <= variant.offset_size() {
                values.push((format!("Unknown({code})"), unknown(code)));
            }
        }
        // Controls: codes no field type claims, so the entry is unsizable and stays `Unknown` on
        // the way back — the word is never followed.
        let unclaimed = (0..=u16::MAX).filter(|&c| gamut_ifd::FieldType::from_code(c).is_none());
        for code in unclaimed.take(3) {
            values.push((format!("Unknown({code}, unclaimed)"), unknown(code)));
        }
        values
    }

    #[test]
    fn empty_metadata_writes_nothing() {
        let mut ifd = Ifd::new();
        let meta = TiffMetadata::new();
        assert!(meta.is_empty());
        meta.apply(&mut ifd);
        assert!(ifd.fields().is_empty());
        assert!(ifd.sub_ifds().is_empty());
    }

    #[test]
    fn an_exif_directory_with_no_fields_is_empty() {
        // An empty directory must not become an `ExifIFD` pointer to nothing.
        let meta = TiffMetadata::new().with_exif(Ifd::new());
        assert!(meta.is_empty());
        let mut ifd = Ifd::new();
        meta.apply(&mut ifd);
        assert!(ifd.sub_ifds().is_empty());
    }

    #[test]
    fn each_carrier_alone_makes_the_set_non_empty() {
        // One assertion per carrier, so a builder that assigned the wrong field, or an
        // `is_empty` that stopped consulting one, fails here rather than in a whole-file test.
        let singles = [
            TiffMetadata::new().with_exif(exif_ifd()),
            TiffMetadata::new().with_xmp(vec![1]),
            TiffMetadata::new().with_iptc(vec![1]),
            TiffMetadata::new().with_icc(vec![1]),
            TiffMetadata::new().with_c2pa(vec![1]),
        ];
        for (i, meta) in singles.iter().enumerate() {
            assert!(!meta.is_empty(), "carrier {i} alone must be non-empty");
        }
    }

    #[test]
    fn the_builders_set_the_field_they_name() {
        let meta = TiffMetadata::new()
            .with_exif(exif_ifd())
            .with_xmp(b"<x:xmpmeta/>".to_vec())
            .with_iptc(vec![0x1c, 0x02, 0x05])
            .with_icc(vec![7; 4])
            .with_c2pa(b"\0\0\0\x14jumbc2pa".to_vec());
        assert_eq!(meta.exif, Some(exif_ifd()));
        assert_eq!(meta.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        assert_eq!(meta.iptc.as_deref(), Some(&[0x1c, 0x02, 0x05][..]));
        assert_eq!(meta.icc.as_deref(), Some(&[7, 7, 7, 7][..]));
        assert_eq!(meta.c2pa.as_deref(), Some(&b"\0\0\0\x14jumbc2pa"[..]));
    }

    #[test]
    fn apply_writes_each_block_under_its_own_tag_and_type() {
        // The tag *and* the field type are the on-disk contract: XMP and IPTC are BYTE, ICC is
        // UNDEFINED. Distinct payloads, so a block written under the wrong tag is visible.
        let meta = TiffMetadata::new()
            .with_xmp(b"<x:xmpmeta/>".to_vec())
            .with_iptc(vec![0x1c, 0x02, 0x05])
            .with_icc(vec![7; 4])
            .with_exif(exif_ifd());
        let mut ifd = Ifd::new();
        meta.apply(&mut ifd);
        assert_eq!(
            ifd.get(tags::XMP),
            Some(&Value::Byte(b"<x:xmpmeta/>".to_vec()))
        );
        assert_eq!(
            ifd.get(tags::IPTC_NAA),
            Some(&Value::Byte(vec![0x1c, 0x02, 0x05]))
        );
        assert_eq!(
            ifd.get(tags::ICC_PROFILE),
            Some(&Value::Undefined(vec![7; 4]))
        );
        let group = &ifd.sub_ifds()[0];
        assert_eq!(group.tag, tags::EXIF_IFD);
        assert_eq!(group.ifds, vec![exif_ifd()]);
    }

    #[test]
    fn apply_never_writes_the_c2pa_store() {
        // The store is the encoder's to place at the end of the file (§A.3.6), so `apply` must
        // leave the directory without it even when one is configured.
        let mut ifd = Ifd::new();
        TiffMetadata::new().with_c2pa(vec![0; 16]).apply(&mut ifd);
        assert!(ifd.get(tags::C2PA_MANIFEST_STORE).is_none());
        assert!(ifd.fields().is_empty());
    }

    #[test]
    fn a_bigtiff_exif_pointer_is_followed_through_its_64_bit_form() {
        // BigTIFF writes a sub-IFD pointer as `LONG8`, not `LONG`. A resolver that knew only the
        // 32-bit forms would leave the field in place as a plain integer and report `exif: None` —
        // silent loss on every BigTIFF, the one file shape where the type differs.
        let mut ifd0 = Ifd::new();
        ifd0.set_sub_ifd(tags::EXIF_IFD, vec![exif_ifd()]);
        let bytes = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Big,
            ifds: vec![ifd0],
        })
        .expect("write");
        assert!(matches!(
            read(&bytes).expect("read").ifds[0].get(tags::EXIF_IFD),
            Some(Value::Long8(_))
        ));
        assert_eq!(read_metadata(&bytes).expect("read").exif, Some(exif_ifd()));
    }

    #[test]
    fn an_exif_directory_whose_only_content_is_a_group_is_still_written() {
        // `exif_ifd` filtered on `fields()` alone, so a directory holding nothing but its
        // `InteroperabilityIFD` group — the exact shape `read_metadata` returns for an Exif
        // directory with one pointer and no scalar fields — was dropped: it encoded to a file with
        // no Exif directory at all and read back as absent, with no error to say so. A group is one
        // on-disk entry, so a directory holding one is not an empty directory.
        let mut interop = Ifd::new();
        interop.set(1, Value::Ascii("R98".into())); // InteroperabilityIndex
        let mut only_a_group = Ifd::new();
        only_a_group.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![interop.clone()]);

        let meta = TiffMetadata::new().with_exif(only_a_group);
        assert!(!meta.is_empty(), "a group is content");
        let mut ifd0 = Ifd::new();
        meta.apply(&mut ifd0);
        let written = ifd0
            .sub_ifds()
            .iter()
            .find(|group| group.tag == tags::EXIF_IFD)
            .and_then(|group| group.ifds.first())
            .expect("the Exif directory must be written");
        assert_eq!(
            written
                .sub_ifds()
                .iter()
                .find(|group| group.tag == tags::INTEROPERABILITY_IFD)
                .map(|group| group.ifds.as_slice()),
            Some(&[interop][..]),
            "carrying the group that was its only content"
        );
    }

    #[test]
    fn the_writer_refuses_an_exif_group_under_a_tag_the_reader_does_not_resolve() {
        // The reader resolves the four standard pointer tags inside the Exif subtree and nothing
        // else, so a group hung off any other tag comes back as the raw offset the writer put
        // there — the directory does not survive its own round trip. The *message* is the claim:
        // this tree is one level deep, well inside the depth bound, so a refusal naming the
        // nesting clause would be the wrong check answering. Depth is
        // `the_writer_refuses_the_exif_nesting_the_reader_refuses` below.
        let mut child = Ifd::new();
        child.set(1, Value::Byte(vec![9]));
        let mut vendor = exif_ifd();
        vendor.set_sub_ifd(50000, vec![child]); // a private tag, not a standard pointer
        let err = TiffMetadata::new()
            .with_exif(vendor)
            .check()
            .expect_err("a group the reader hands back as an offset must not be written");
        assert!(err.to_string().contains("standard pointer tag"), "{err}");
    }

    #[test]
    fn the_writer_refuses_an_exif_pointer_tag_carried_as_a_pointer_typed_field() {
        // `check` inspected only `sub_ifds()` while `resolve_pointers` inspects `get(tag)`, so the
        // one shape the reader misreads was the one shape the writer never looked at: a standard
        // pointer tag carried as a plain `LONG`. All four encoded cleanly and then failed this
        // crate's own `read_metadata` — `read out of bounds` for a large integer, `sub-IFD pointer
        // loop` for a small one — which is exactly what `check` documents it refuses. Every
        // pointer type is swept, because which one a caller's directory happens to use is not
        // something the writer can know. The *message* is the claim: this tree has no group and no
        // nesting at all, so a refusal naming either of the other two clauses would be the wrong
        // check answering.
        let pointer_typed = [
            Value::Long(vec![8]),
            Value::Ifd(vec![8]),
            Value::Long8(vec![8]),
            Value::Ifd8(vec![8]),
        ];
        for tag in EXIF_SUBTREE_POINTER_TAGS {
            for value in &pointer_typed {
                let mut exif = exif_ifd();
                exif.set(*tag, value.clone());
                let Err(err) = TiffMetadata::new().with_exif(exif).check() else {
                    panic!("tag {tag}, {value:?}: a field the reader follows as an offset");
                };
                assert!(
                    err.to_string().contains("may not carry a plain field"),
                    "tag {tag}, {value:?}: {err}"
                );
            }
        }
    }

    #[test]
    fn the_pointer_type_codes_are_exactly_the_codes_the_resolver_follows() {
        // `POINTER_TYPE_CODES` restates, as on-disk codes, the set `pointer_offsets` restates as
        // `Value` variants — and the writer now asks the code while the reader still asks the
        // variant, so the two must not drift. Derived by sweeping the whole `u16` code space
        // through `FieldType::from_code` and asking `pointer_offsets` about a value of each type,
        // never by repeating the four numbers: a hand list short by one is the defect this
        // constant exists to close.
        let derived: Vec<u16> = every_type_code()
            .into_iter()
            .filter(|&code| {
                let ty = gamut_ifd::FieldType::from_code(code).expect("a swept code");
                pointer_offsets(&canonical_value(ty)).is_some()
            })
            .collect();
        assert_eq!(
            derived, POINTER_TYPE_CODES,
            "the codes the reader follows must be exactly the codes the writer refuses"
        );
    }

    #[test]
    fn every_value_the_writer_accepts_under_a_pointer_tag_reads_back_as_a_field() {
        // The regression this crate kept reopening, stated at the boundary it kept breaking at.
        // `check` was shaped by the `Value` variant while the reader classifies by the on-disk
        // type code, and `Value::Unknown` is the one shape where those disagree: built at code 4,
        // 13, 16 or 18 it is a plain field to a variant-shaped predicate and a pointer to the
        // reader, so it encoded cleanly and then failed `read_metadata` — or, in BigTIFF at the
        // top level, came back as a *group* where a field was written.
        //
        // The sweep is derived, not listed: every representable entry type (natural and `Unknown`
        // at every code) x the four pointer tags x classic and BigTIFF x on the Exif directory
        // itself and one level below it. Only accepted sets are exercised; that the refused ones
        // are exactly the pointer-coded ones is
        // `the_pointer_type_codes_are_exactly_the_codes_the_resolver_follows`.
        for variant in [Variant::Classic, Variant::Big] {
            for (name, value) in every_representable_value(variant) {
                for &tag in EXIF_SUBTREE_POINTER_TAGS {
                    for nested in [false, true] {
                        let mut carrier = exif_ifd();
                        carrier.set(tag, value.clone());
                        let mut exif = exif_ifd();
                        if nested {
                            exif.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![carrier]);
                        } else {
                            exif = carrier;
                        }
                        let meta = TiffMetadata::new().with_exif(exif.clone());
                        if meta.check().is_err() {
                            continue;
                        }
                        let where_ = format!("{name} under {tag}, {variant:?}, nested={nested}");
                        let mut ifd0 = Ifd::new();
                        meta.apply(&mut ifd0);
                        let bytes = file_with_variant(ifd0, variant);
                        let back = read_metadata(&bytes)
                            .unwrap_or_else(|e| panic!("{where_}: accepted but unreadable: {e}"))
                            .exif
                            .unwrap_or_else(|| panic!("{where_}: accepted but no Exif directory"));
                        let carrier = if nested {
                            back.sub_ifds()
                                .iter()
                                .find(|group| group.tag == tags::INTEROPERABILITY_IFD)
                                .and_then(|group| group.ifds.first())
                                .cloned()
                                .unwrap_or_else(|| panic!("{where_}: the nested directory"))
                        } else {
                            back
                        };
                        assert!(
                            carrier.get(tag).is_some(),
                            "{where_}: must come back a field, not be followed as an offset"
                        );
                        assert!(
                            carrier.sub_ifds().iter().all(|group| group.tag != tag),
                            "{where_}: a field must not come back a group"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_writer_refuses_a_field_and_a_group_under_one_tag() {
        // `Ifd` keeps fields and groups in two lists, so one tag can sit in both; the writer then
        // emits two entries under it — not a TIFF directory (TIFF 6.0 §2) — and the field is what
        // a reader drops. It encoded cleanly, read back without the field, and this crate's own
        // `deconstruct` graded the file clean, so nothing in the round trip could see it. The
        // *message* is the claim: the tag is a standard pointer tag and the value is a `SHORT`, so
        // neither of the other clauses applies to this tree.
        for &tag in EXIF_SUBTREE_POINTER_TAGS {
            let mut child = Ifd::new();
            child.set(1, Value::Ascii("R98".into()));
            let mut exif = exif_ifd();
            exif.set(tag, Value::Short(vec![7]));
            exif.set_sub_ifd(tag, vec![child]);
            let Err(err) = TiffMetadata::new().with_exif(exif).check() else {
                panic!("tag {tag}: two entries under one tag is not a directory");
            };
            assert!(
                err.to_string().contains("both a plain field"),
                "tag {tag}: {err}"
            );
        }
    }

    #[test]
    fn a_value_no_reader_would_follow_survives_under_a_pointer_tag() {
        // The refusal above is shaped by the value's *type*, not by its tag, and that is the whole
        // difference between bounding the writer by what the reader misreads and banning four tags
        // outright. `pointer_offsets` accepts only LONG/IFD/LONG8/IFD8, so a `SHORT` under
        // `SubIFDs` is left in place by the reader — and must therefore be written and handed back
        // unchanged rather than refused.
        for tag in EXIF_SUBTREE_POINTER_TAGS {
            let mut exif = exif_ifd();
            exif.set(*tag, Value::Short(vec![8]));
            let meta = TiffMetadata::new().with_exif(exif);
            meta.check()
                .unwrap_or_else(|e| panic!("tag {tag}: a SHORT is not a pointer: {e}"));

            let mut ifd0 = Ifd::new();
            meta.apply(&mut ifd0);
            let back = read_metadata(&file_with(ifd0))
                .expect("read")
                .exif
                .unwrap_or_else(|| panic!("tag {tag}: an Exif directory"));
            assert_eq!(
                back.get(*tag),
                Some(&Value::Short(vec![8])),
                "tag {tag}: must come back the field that was written"
            );
        }
    }

    #[test]
    fn the_exif_subtree_pointer_tags_are_the_four_this_crate_documents() {
        // The set is a sibling crate's constant, `gamut_ifd::tags::STANDARD_POINTER_TAGS`, while
        // this crate's public contract enumerates its four members one by one — in
        // `TiffMetadata::exif`, `TiffEncoder::with_metadata`, `TiffDecoder::metadata`, README.md,
        // STATUS.md, and both refusal messages. A fifth member added upstream would widen every
        // one of those silently, and a public contract must not widen without someone deciding it.
        // So the enumeration is pinned once, here, against the literal numbers the prose gives and
        // the names the messages give.
        assert_eq!(
            EXIF_SUBTREE_POINTER_TAGS,
            [330, 34665, 34853, 40965],
            "the four pointer tags this crate's documentation and messages enumerate"
        );
        for message in [POINTER_FIELD_REFUSAL, FOREIGN_GROUP_REFUSAL] {
            for name in ["SubIFDs", "ExifIFD", "GPSInfo", "InteroperabilityIFD"] {
                assert!(message.contains(name), "{name} unnamed by: {message}");
            }
        }
    }

    #[test]
    fn a_standard_pointer_group_inside_the_exif_directory_is_resolved() {
        // `POINTER_TAGS` once listed `ExifIFD` and `InteroperabilityIFD` only, so a `SubIFDs` or
        // `GPSInfo` group *inside* the Exif directory came back as a raw absolute offset into the
        // source file. Which tag the reader resolves at which level is this crate's decision, so
        // it is asserted here on the directory model; that the re-encode of such a directory is a
        // clean file is `every_standard_pointer_inside_the_exif_directory_survives_a_round_trip`
        // (tests/metadata.rs).
        for tag in gamut_ifd::tags::STANDARD_POINTER_TAGS {
            let mut child = Ifd::new();
            child.set(1, Value::Byte(vec![2, 3, 0, 0]));
            let mut exif = exif_ifd();
            exif.set_sub_ifd(*tag, vec![child.clone()]);
            let mut ifd0 = Ifd::new();
            ifd0.set_sub_ifd(tags::EXIF_IFD, vec![exif]);

            let back = read_metadata(&file_with(ifd0))
                .expect("read")
                .exif
                .unwrap_or_else(|| panic!("tag {tag}: an Exif directory"));
            assert_eq!(back.get(*tag), None, "tag {tag}: left as a raw offset");
            assert_eq!(
                back.sub_ifds()
                    .iter()
                    .find(|group| group.tag == *tag)
                    .map(|group| group.ifds.as_slice()),
                Some(&[child][..]),
                "tag {tag}: must come back parsed"
            );
        }
    }

    #[test]
    fn a_standard_pointer_at_ifd_0_that_feeds_no_field_is_left_alone() {
        // The other half of the per-level rule: at IFD 0 only `ExifIFD` is followed, so a
        // `SubIFDs` or `GPSInfo` field on the page stays the plain integer field it was read as
        // rather than becoming a group. What that buys — a dangling one of them not hiding the
        // blocks — is `a_broken_pointer_the_metadata_does_not_use_does_not_hide_the_blocks`
        // (tests/metadata.rs); this pins the resolution itself, on a pointer that is perfectly
        // readable, so the two claims cannot be confused.
        //
        // `resolve_pointers` is driven directly, at the depth `read_metadata` calls it with,
        // because IFD 0 is the one directory the seam never hands back: asking `read_metadata`
        // instead can only observe `exif`, which stays `None` whether the pointer was resolved or
        // not, so the regression this names — `pointer_tags` returning the full set at every
        // level — would pass unnoticed. Here it does not: resolution turns the field into a group.
        for tag in [tags::SUB_IFDS, tags::GPS_INFO] {
            let mut source = Ifd::new();
            source.set_sub_ifd(tag, vec![exif_ifd()]);
            source.set(tags::XMP, Value::Byte(b"x".to_vec()));
            let bytes = file_with(source);
            let mut ifd0 = read(&bytes).expect("read").ifds.swap_remove(0);
            let offset = ifd0
                .get_u32(tag)
                .unwrap_or_else(|| panic!("tag {tag}: a written pointer"));
            assert!(offset > 0, "tag {tag}: the pointer must name a directory");

            resolve_pointers(
                &bytes,
                ByteOrder::LittleEndian,
                Variant::Classic,
                &mut ifd0,
                &mut BTreeSet::new(),
                0,
            )
            .unwrap_or_else(|e| panic!("tag {tag}: resolving IFD 0: {e}"));
            assert_eq!(
                ifd0.get_u32(tag),
                Some(offset),
                "tag {tag}: must stay the integer field it was read as"
            );
            assert!(
                ifd0.sub_ifds().is_empty(),
                "tag {tag}: must not become a group at IFD 0"
            );
        }
    }

    #[test]
    fn the_writer_refuses_the_exif_nesting_the_reader_refuses() {
        // `metadata()` walks two levels below IFD 0 and refuses a third, so a set the encoder
        // accepted at three levels became a well-formed file this crate could not read back —
        // the encoder emitting what its own reader rejects. The bound is the whole claim, so
        // both sides of it are asserted: the `ExifIFD` → `InteroperabilityIFD` pair (EXIF 2.3
        // §4.6.3) is accepted, and one directory below it is not.
        let mut interop = Ifd::new();
        interop.set(1, Value::Ascii("R98".into())); // InteroperabilityIndex
        let mut pair = exif_ifd();
        pair.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![interop]);
        TiffMetadata::new()
            .with_exif(pair.clone())
            .check()
            .expect("the Exif -> Interop pair is what a decoded camera EXIF is");

        let mut deeper = exif_ifd();
        deeper.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![pair]);
        let err = TiffMetadata::new()
            .with_exif(deeper)
            .check()
            .expect_err("a directory below the pair is one this crate could not read back");
        assert!(err.to_string().contains("nests deeper"), "{err}");
    }

    #[test]
    fn a_directory_below_the_exif_interop_pair_is_too_deep() {
        // The bound is two because the pair can legitimately reach two levels and no more, so
        // both sides of it are asserted: `a_decoded_exif_sub_ifd_re_encodes_into_a_fully_classified_file`
        // (tests/metadata.rs) reads an Exif → Interop tree back, and a third level — an Interop
        // directory inside an Interop directory, which no conformant file writes — is refused
        // here rather than walked.
        let mut third = Ifd::new();
        third.set(1, Value::Ascii("R98".into())); // InteroperabilityIndex
        let mut interop = Ifd::new();
        interop.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![third]);
        let mut exif = exif_ifd();
        exif.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![interop]);
        let mut ifd0 = Ifd::new();
        ifd0.set_sub_ifd(tags::EXIF_IFD, vec![exif]);
        let err = read_metadata(&file_with(ifd0)).expect_err("three levels is out of spec");
        assert!(err.to_string().contains("sub-IFD tree too deep"), "{err}");
    }

    #[test]
    fn read_metadata_returns_each_payload_verbatim() {
        let mut ifd0 = Ifd::new();
        TiffMetadata::new()
            .with_xmp(b"<x:xmpmeta/>".to_vec())
            .with_iptc(vec![0x1c, 0x02, 0x05])
            .with_icc(vec![7; 4])
            .with_exif(exif_ifd())
            .apply(&mut ifd0);
        ifd0.set(tags::C2PA_MANIFEST_STORE, Value::Undefined(vec![0x10; 12]));
        let read = read_metadata(&file_with(ifd0)).expect("read");
        assert_eq!(read.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        assert_eq!(read.iptc.as_deref(), Some(&[0x1c, 0x02, 0x05][..]));
        assert_eq!(read.icc.as_deref(), Some(&[7, 7, 7, 7][..]));
        assert_eq!(read.exif, Some(exif_ifd()));
        assert_eq!(read.c2pa.as_deref(), Some(&[0x10; 12][..]));
    }

    #[test]
    fn the_reader_and_the_locator_agree_on_what_a_store_is() {
        // The two surfaces must never disagree: a `TiffMetadata` reporting a store that
        // `c2pa_exclusions` cannot find is one the encoder would refuse, turning a decode→encode
        // round trip into a hard error. Each case below is an entry `locate` reports absent for a
        // *different* reason — wrong type (§A.3.6 fixes it at 7), and too short to be a JUMBF box
        // (below `MIN_STORE_LEN`) — so a fix that only handled one of them still fails here.
        for value in [
            Value::Byte(vec![0x10; 12]),
            Value::Undefined(vec![9; c2pa::MIN_STORE_LEN - 1]),
        ] {
            let mut ifd0 = Ifd::new();
            ifd0.set(tags::C2PA_MANIFEST_STORE, value.clone());
            let bytes = file_with(ifd0);
            assert_eq!(read_metadata(&bytes).expect("read").c2pa, None, "{value:?}");
            assert_eq!(c2pa_exclusions(&bytes).expect("locate"), None, "{value:?}");
        }
    }

    #[test]
    fn a_store_the_locator_accepts_is_returned_verbatim() {
        // The other side of the agreement: the reader must not be so strict that it drops a store
        // the locator does find, which would make the two disagree in the opposite direction.
        let store = b"\0\0\0\x16jumb\x01\x02\x03".to_vec();
        let mut ifd0 = Ifd::new();
        ifd0.set(tags::C2PA_MANIFEST_STORE, Value::Undefined(store.clone()));
        let bytes = file_with(ifd0);
        assert_eq!(read_metadata(&bytes).expect("read").c2pa, Some(store));
        assert!(c2pa_exclusions(&bytes).expect("locate").is_some());
    }

    #[test]
    fn a_file_with_no_metadata_reads_back_empty() {
        let mut ifd0 = Ifd::new();
        ifd0.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
        let read = read_metadata(&file_with(ifd0)).expect("read");
        assert!(read.is_empty());
        assert_eq!(read, TiffMetadata::new());
    }

    #[test]
    fn read_metadata_takes_the_store_from_the_last_ifd_of_the_chain() {
        // §A.3.6 puts the one store in the last main-chain IFD; a tag-52545 entry in an earlier
        // page is not it. Distinct payloads pin which directory was consulted.
        let mut first = Ifd::new();
        first.set(tags::C2PA_MANIFEST_STORE, Value::Undefined(vec![0xAA; 12]));
        first.set(tags::XMP, Value::Byte(b"first".to_vec()));
        let mut last = Ifd::new();
        last.set(tags::C2PA_MANIFEST_STORE, Value::Undefined(vec![0xBB; 12]));
        last.set(tags::XMP, Value::Byte(b"last".to_vec()));
        let bytes = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![first, last],
        })
        .expect("write");
        let read = read_metadata(&bytes).expect("read");
        assert_eq!(read.c2pa.as_deref(), Some(&[0xBB; 12][..]));
        // The other blocks stay IFD 0's, so the two directories are not confused for each other.
        assert_eq!(read.xmp.as_deref(), Some(&b"first"[..]));
    }
}
