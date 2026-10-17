//! Optional metadata embedded in a TIFF: Exif (0th-IFD fields, Exif and GPS sub-IFDs) plus
//! XMP / IPTC / ICC / C2PA blocks.
//!
//! TIFF stores metadata the way it stores everything else — as IFD entries — so the seam is thin
//! by construction: [`TiffMetadata`] is a plain struct of optional payloads that
//! [`TiffEncoder::with_tiff_metadata`](crate::TiffEncoder::with_tiff_metadata) writes into IFD 0 and
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
//! back ([`TiffMetadata::check`]). The `GPSInfo` sub-IFD is carried the same way
//! ([`TiffMetadata::gps`]). The Exif *0th IFD* has no directory of its own in a TIFF — its tags
//! are TIFF 6.0 tags of the page's own IFD 0 — so [`TiffMetadata::ifd0`] carries those fields
//! directly, limited to the descriptive tags of [`IFD0_EXIF_TAGS`]: the rest of IFD 0 is the
//! encoder's.
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

use gamut_core::{Error, Result};
use gamut_ifd::c2pa::{self, C2paExclusions, C2paSummary};
use gamut_ifd::{
    ByteOrder, Ifd, SubtreeScope, Value, read, read_header, read_ifd_at, resolve_subtree,
};

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
    /// Exif field — not even `ExifVersion` — and drops none of the directory's entries, because a
    /// TIFF's `ExifIFD` is the caller's directory and completing it would silently change what a
    /// round trip returns. What
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
    /// **One directory, and an array of them is refused.** The field holds a single directory,
    /// and an `ExifIFD` entry names one (EXIF 2.32 §4.6.3 A fixes its `count` at 1). A source
    /// file whose `ExifIFD` entry instead carries an **array** of two or more offsets is refused
    /// by [`TiffDecoder::metadata`](crate::TiffDecoder::metadata) with
    /// [`Error::InvalidInput`](gamut_core::Error::InvalidInput) rather than narrowed to its first
    /// directory, which would drop the rest without a report — use
    /// [`deconstruct`](crate::deconstruct) to see the whole array.
    ///
    /// The one shape the model can express and the file cannot is a tag holding **both** a field
    /// and a sub-IFD group: two entries under one tag, which TIFF 6.0 §2 does not allow. That is
    /// refused by the encode rather than normalised, because normalising it would mean choosing —
    /// silently — which of the two the caller meant; see `TiffMetadata::check`.
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
    /// This is the Exif subtree's rule and not IFD 0's: at IFD 0 only `ExifIFD` and `GPSInfo` are
    /// followed, so a `SubIFDs` pointer sitting on the page — whose target feeds no field here —
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
    /// The Exif **0th-IFD** fields a TIFF holds in the page's own IFD 0 — `Make`, `Model`,
    /// `Orientation`, `DateTime`, `Copyright`, … — as a directory of plain fields.
    ///
    /// A TIFF has no Exif 0th IFD of its own: EXIF 2.32 §4.6.4 Table 4 lists those tags as TIFF
    /// 6.0 attributes, and in a TIFF they *are* IFD 0's. Only the tags in [`IFD0_EXIF_TAGS`] are
    /// carried — Table 4's tags that describe the image rather than the way its samples are laid
    /// out. Every other tag of Table 4 (`ImageWidth`, `Compression`, `XResolution`,
    /// `StripOffsets`, `YCbCrPositioning`, `TransferFunction`, …) is one the encoder writes from the
    /// pixels it lays out, or one whose meaning depends on that layout, so a field under any tag
    /// outside the list — or a sub-IFD group — is **refused** by the encode with
    /// [`Error::InvalidInput`](gamut_core::Error::InvalidInput) rather than allowed to overwrite
    /// or contradict the page's own entry. On read, IFD 0's fields under those tags come back here and nothing else does; `None`
    /// when the page carries none of them.
    ///
    /// Values are carried unchanged and not validated: an `Orientation` of 6 is written as 6, and
    /// the stored pixels are not rotated by either side.
    pub ifd0: Option<Ifd>,
    /// The GPS private sub-IFD (`GPSInfo`, 34853, EXIF 2.32 §4.6.3 A), pointed to from IFD 0.
    ///
    /// Carried as [`exif`](Self::exif) is, under the same three normalisations and the same four
    /// refusals at the encode (see
    /// [`TiffEncoder::with_tiff_metadata`](crate::TiffEncoder::with_tiff_metadata)); an IFD-0 `GPSInfo` entry naming two or more directories
    /// is refused on read, as an `ExifIFD` array is. Its pointer is followed on read, so an
    /// **unreadable** `GPSInfo` target fails [`TiffDecoder::metadata`](crate::TiffDecoder::metadata)
    /// — the same trade `ExifIFD` makes, for the same reason: reporting `gps: None` for a directory
    /// the file declares would be silent loss.
    pub gps: Option<Ifd>,
    /// An XMP packet (UTF-8 RDF/XML), stored in the `XMP` tag (700) as `BYTE`, verbatim.
    pub xmp: Option<Vec<u8>>,
    /// A legacy IPTC-IIM dataset stream, stored in the `IPTC/NAA` tag (33723) as `BYTE`,
    /// verbatim. On read the tag is also accepted typed `UNDEFINED` or `LONG` — Adobe software
    /// writes it as `LONG` — and a `LONG` value yields its on-disk bytes, unswapped.
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
    /// The byte order of the file the Exif carriers — [`exif`](Self::exif), [`ifd0`](Self::ifd0)
    /// and [`gps`](Self::gps) — were read from: `Some` exactly when
    /// [`TiffDecoder::metadata`](crate::TiffDecoder::metadata) returned any of the three, and
    /// `None` on a value built by hand.
    ///
    /// **Read-side provenance only.** An `ExifIFD` has no byte order of its own — it is written in
    /// the file's, which [`TiffEncoder::with_byte_order`](crate::TiffEncoder::with_byte_order)
    /// chooses — so the encoder ignores this field. It exists so the typed EXIF model the
    /// `metadata` feature builds from the three states the order the directories were actually
    /// stored in.
    pub exif_byte_order: Option<ByteOrder>,
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

    /// Returns a copy carrying `fields` as the Exif 0th-IFD fields of the file's IFD 0 — see
    /// [`ifd0`](Self::ifd0) for which tags it may hold.
    #[must_use]
    pub fn with_ifd0(mut self, fields: Ifd) -> Self {
        self.ifd0 = Some(fields);
        self
    }

    /// Returns a copy carrying `gps` as the file's `GPSInfo` sub-IFD.
    #[must_use]
    pub fn with_gps(mut self, gps: Ifd) -> Self {
        self.gps = Some(gps);
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

    /// Whether there is nothing to embed: no payload set, and no Exif directory — sub-IFD, 0th-IFD
    /// fields or GPS sub-IFD — with content in it.
    ///
    /// An `exif` or `gps` directory with no entries counts as empty — writing it would add a
    /// pointer to a directory with nothing in it — and so does an `ifd0` with none. A sub-IFD group
    /// *is* content: a directory whose only entry is an `InteroperabilityIFD` pointer still writes
    /// one on-disk entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exif_ifd().is_none()
            && self.ifd0_fields().is_none()
            && self.gps_ifd().is_none()
            && self.xmp.is_none()
            && self.iptc.is_none()
            && self.icc.is_none()
            && self.c2pa.is_none()
    }

    /// The Exif sub-IFD to write, or `None` when there is no Exif content worth a directory.
    fn exif_ifd(&self) -> Option<&Ifd> {
        with_content(self.exif.as_ref())
    }

    /// The 0th-IFD fields to write into IFD 0, or `None` when there are none.
    fn ifd0_fields(&self) -> Option<&Ifd> {
        with_content(self.ifd0.as_ref())
    }

    /// The GPS sub-IFD to write, or `None` when there is no GPS content worth a directory.
    fn gps_ifd(&self) -> Option<&Ifd> {
        with_content(self.gps.as_ref())
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
    /// The GPS sub-IFD is a caller's directory too, read back by the same walk at the same level,
    /// so the same four bounds apply to it unchanged.
    ///
    /// The 0th-IFD fields are checked by a rule of their own ([`ifd0_refusal`]), because they are
    /// written into the page's own directory rather than below it: only the tags in
    /// [`IFD0_EXIF_TAGS`] are accepted, and no group.
    ///
    /// The blocks [`apply`](Self::apply) writes into IFD 0 sit under `XMP`, `IPTC_NAA` and
    /// `ICC_PROFILE`, none of which any level treats as a pointer, and the rest of IFD 0 is the
    /// encoder's own.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`](gamut_core::Error::InvalidInput) if the Exif or GPS sub-IFD
    /// carries a pointer-typed field under a pointer tag, hangs a group off a tag the reader does
    /// not resolve, carries a field and a group under one tag, or nests deeper than the reader
    /// walks back; or if the 0th-IFD fields hold a tag outside [`IFD0_EXIF_TAGS`] or a group.
    pub(crate) fn check(&self) -> Result<()> {
        if let Some(refusal) = self.ifd0_fields().and_then(ifd0_refusal) {
            return Err(Error::invalid_input(env!("CARGO_PKG_NAME"), refusal));
        }
        for directory in [self.exif_ifd(), self.gps_ifd()].into_iter().flatten() {
            check_exif_subtree(directory, MAX_POINTER_DEPTH - 1)?;
        }
        Ok(())
    }

    /// Writes the XMP / IPTC / ICC blocks, the Exif 0th-IFD fields and the Exif and GPS sub-IFDs
    /// into `ifd0`.
    ///
    /// The 0th-IFD fields are set after the encoder's own and cannot replace one, because
    /// [`check`](Self::check) admits only [`IFD0_EXIF_TAGS`], none of which the encoder writes.
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
        if let Some(fields) = self.ifd0_fields() {
            for field in fields.fields() {
                ifd0.set(field.tag, field.value.clone());
            }
        }
        if let Some(exif) = self.exif_ifd() {
            ifd0.set_sub_ifd(tags::EXIF_IFD, vec![exif.clone()]);
        }
        if let Some(gps) = self.gps_ifd() {
            ifd0.set_sub_ifd(tags::GPS_INFO, vec![gps.clone()]);
        }
    }
}

/// `directory`, unless it has nothing to write.
///
/// A directory is worth writing when it holds **either** a field **or** a sub-IFD group.
/// Testing only [`fields`](Ifd::fields) dropped a directory whose sole content was a group — an
/// `ExifIFD` holding nothing but its `InteroperabilityIFD` pointer, which [`read_metadata`]
/// returns in exactly that shape — silently, into a file with no Exif directory at all and no
/// error to say so. For the 0th-IFD fields a group is not writable, and keeping one here is what
/// lets [`TiffMetadata::check`] refuse it rather than drop it.
fn with_content(directory: Option<&Ifd>) -> Option<&Ifd> {
    directory.filter(|ifd| !ifd.fields().is_empty() || !ifd.sub_ifds().is_empty())
}

/// The Exif 0th-IFD tags [`TiffMetadata::ifd0`] carries into a TIFF's IFD 0, ascending.
///
/// EXIF 2.32 §4.6.4 Table 4 lists the TIFF 6.0 tags an Exif 0th IFD uses. These are the ten of
/// them that describe the image rather than the layout of its samples: `ImageDescription` (270),
/// `Make` (271), `Model` (272), `Orientation` (274), `Software` (305), `DateTime` (306),
/// `Artist` (315), `WhitePoint` (318), `PrimaryChromaticities` (319) and `Copyright` (33432).
///
/// The rest of Table 4 stays the encoder's, each for a stated reason: its group A structure tags
/// (`ImageWidth` … `ResolutionUnit`) and group B offset tags (`StripOffsets`,
/// `JPEGInterchangeFormat`, …) are written by the encoder from the pixels it lays out — the
/// resolution tags included, which it writes as 72 dpi; `YCbCrSubSampling`, `YCbCrPositioning`,
/// `YCbCrCoefficients` and `ReferenceBlackWhite` describe a YCbCr coding (or sample headroom)
/// this encoder never writes; and `TransferFunction`'s count is fixed by `BitsPerSample`
/// (TIFF 6.0 §20), which the encoder chooses, so a curve read from one file is not valid in
/// another.
pub const IFD0_EXIF_TAGS: &[u16] = &[
    tags::IMAGE_DESCRIPTION,
    tags::MAKE,
    tags::MODEL,
    tags::ORIENTATION,
    tags::SOFTWARE,
    tags::DATE_TIME,
    tags::ARTIST,
    tags::WHITE_POINT,
    tags::PRIMARY_CHROMATICITIES,
    tags::COPYRIGHT,
];

/// The refusal earned by a 0th-IFD field under a tag outside [`IFD0_EXIF_TAGS`].
pub(crate) const IFD0_FIELD_REFUSAL: &str = "TIFF: the Exif 0th-IFD fields may only use the tags \
     IFD0_EXIF_TAGS lists (ImageDescription, Make, Model, Orientation, Software, DateTime, Artist, \
     WhitePoint, PrimaryChromaticities, Copyright); the rest of IFD 0 is the encoder's";

/// The refusal earned by a sub-IFD group among the 0th-IFD fields: IFD 0's own pointers are the
/// encoder's (`ExifIFD` and `GPSInfo` are [`TiffMetadata::exif`] and [`TiffMetadata::gps`]).
pub(crate) const IFD0_GROUP_REFUSAL: &str = "TIFF: the Exif 0th-IFD fields may not hold a sub-IFD \
     group; the Exif and GPS sub-IFDs are TiffMetadata's own exif and gps carriers";

/// Why the 0th-IFD fields `fields` cannot be written into IFD 0, or `None` when they can: a group
/// ([`IFD0_GROUP_REFUSAL`]), or a field under a tag outside [`IFD0_EXIF_TAGS`]
/// ([`IFD0_FIELD_REFUSAL`]).
///
/// The message rather than an [`Error`], because the two callers raise it as different kinds: an
/// encode handed a [`TiffMetadata`] directly calls it invalid input, while the `metadata`
/// feature's facade path calls an Exif model holding such a field unsupported by this container.
pub(crate) fn ifd0_refusal(fields: &Ifd) -> Option<&'static str> {
    if !fields.sub_ifds().is_empty() {
        return Some(IFD0_GROUP_REFUSAL);
    }
    fields
        .fields()
        .iter()
        .any(|field| !IFD0_EXIF_TAGS.contains(&field.tag))
        .then_some(IFD0_FIELD_REFUSAL)
}

/// The pointer tags [`read_metadata`] resolves **at IFD 0**, scoped to what [`TiffMetadata`]
/// actually returns.
///
/// Two tags, and they are a deliberate lower bound rather than a subset of convenience. `ExifIFD`
/// and `GPSInfo` are followed because those directories **are** fields of [`TiffMetadata`]
/// ([`exif`](TiffMetadata::exif) and [`gps`](TiffMetadata::gps)): they are handed to the caller
/// and may be written back, so a pointer under either that stayed a raw offset would be
/// re-encoded into a file laid out differently.
///
/// The other two members of [`gamut_ifd::tags::STANDARD_POINTER_TAGS`] are deliberately **not**
/// here. `SubIFDs` (330) locates thumbnails and reduced-resolution subfiles, and
/// `InteroperabilityIFD` (40965) does not belong at IFD 0 at all; neither feeds any field of
/// [`TiffMetadata`] from this level, and neither is re-encoded by [`TiffMetadata::apply`], which
/// writes into a directory the encoder builds fresh. Following them here could therefore only
/// *add* failure modes, and it did: a single dangling `SubIFDs` offset made XMP, IPTC, ICC and
/// C2PA all unreachable on a file whose pixels decode perfectly, and two pages sharing one
/// thumbnail directory tripped the reader's cross-chain loop guard. A pointer whose target this
/// reader throws away must not be able to fail the whole call.
///
/// `GPSInfo` was on that side of the rule until [`TiffMetadata::gps`] gave its target a field
/// (#876), and moving it is the same rule applied, with the same cost `ExifIFD` already pays: a
/// file whose IFD-0 `GPSInfo` names an unreadable directory, which used to read with the pointer
/// ignored, now fails [`read_metadata`] — what this stops accepting is exactly that dangling GPS
/// pointer, which [`deconstruct`](crate::deconstruct) still reports.
///
/// The same rule scopes *where* the walk runs, not only what it follows: it is applied to
/// **IFD 0's subtree and nowhere else** ([`gamut_ifd::resolve_subtree`] over IFD 0). Every page
/// after IFD 0 feeds one field of [`TiffMetadata`] — the C2PA manifest store, whose entry holds
/// the store's bytes directly rather than a pointer — so a *pointer* on such a page is a
/// thrown-away target too, and one on page 1 of a two-page document used to fail the whole call.
/// [`gamut_ifd::read_tree`] cannot be scoped either way: it resolves the flat list it is given at
/// every node of every page.
const IFD0_POINTER_TAGS: &[u16] = &[tags::EXIF_IFD, tags::GPS_INFO];

/// The pointer tags [`read_metadata`] resolves **inside the Exif and GPS subtrees** — every
/// standard one.
///
/// The scoping rule that keeps two tags out of [`IFD0_POINTER_TAGS`] puts all four in here, and
/// it is the same rule, not an exception to it: a pointer is followed exactly when its target
/// belongs to a directory [`TiffMetadata`] hands back. At IFD 0 a `SubIFDs` target is thrown
/// away, so following it can only add failure modes. Under `ExifIFD` the enclosing
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

/// The walk [`read_metadata`] runs over IFD 0: [`IFD0_POINTER_TAGS`] at the page itself,
/// [`EXIF_SUBTREE_POINTER_TAGS`] at every level below it, at most [`MAX_POINTER_DEPTH`] deep.
///
/// A per-level list rather than [`gamut_ifd::read_tree`]'s one flat list, because the two levels
/// answer opposite questions: at IFD 0 a followed pointer can only add a failure mode, and below
/// it an *un*followed pointer becomes a stale offset in a directory the caller is handed. The
/// root is the only level whose directory is a page, and the walk never leaves IFD 0's subtree,
/// so "below the root" is exactly "inside the Exif or GPS subtree".
fn metadata_scope() -> SubtreeScope<'static> {
    SubtreeScope::new(IFD0_POINTER_TAGS, EXIF_SUBTREE_POINTER_TAGS)
        .with_max_depth(MAX_POINTER_DEPTH)
}

/// An upper bound on the sub-IFD nesting [`metadata_scope`] lets the walk follow, bounding a
/// hostile pointer graph: a directory a hundred levels down is still a directory, and a file of a
/// few kilobytes holds enough of them to exhaust the stack.
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
/// The same set [`gamut_ifd::resolve_subtree`] follows, stated as **codes** rather than as
/// [`Value`] variants, because the reader classifies an entry by the code it parses and the code
/// is the discriminator that survives the write/read boundary ([`Value::type_code`], total over
/// every variant). A `Value::Unknown` once carried an arbitrary code beside its word, so one built
/// at 4, 13, 16 or 18 was a plain field to a variant-shaped predicate and a **pointer** to the
/// reader; [`gamut_ifd::UnknownValue::new`] now refuses every recognised code, so variant and code
/// agree for every value a caller can build, and asking the code stays correct either way.
///
/// The membership is pinned against the resolver over the whole code space by
/// `the_pointer_type_codes_are_exactly_the_codes_the_resolver_follows`.
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

/// The refusal [`read_metadata`] returns for an IFD-0 `ExifIFD` entry naming **two or more**
/// directories.
///
/// EXIF 2.32 §4.6.3 A fixes the Exif IFD Pointer's count at 1, so no conformant file reaches this.
/// [`TiffMetadata::exif`] holds one directory, and the alternative to refusing is returning the
/// first and dropping the rest with nothing to say so — a loss a re-encode then makes permanent.
/// What this stops accepting is exactly that non-conformant array, which
/// [`deconstruct`](crate::deconstruct) still walks in full. Named so the test pins the refusal
/// itself rather than any `InvalidInput` the read could earn.
const EXIF_ARRAY_REFUSAL: &str = "TIFF: the ExifIFD entry names more than one directory, and \
     EXIF 2.32 §4.6.3 allows one (count 1); returning only the first would drop the rest";

/// [`EXIF_ARRAY_REFUSAL`]'s twin for an IFD-0 `GPSInfo` entry naming two or more directories:
/// EXIF 2.32 §4.6.3 A fixes the GPS Info IFD Pointer's count at 1 as well, and
/// [`TiffMetadata::gps`] holds one directory.
const GPS_ARRAY_REFUSAL: &str = "TIFF: the GPSInfo entry names more than one directory, and \
     EXIF 2.32 §4.6.3 allows one (count 1); returning only the first would drop the rest";

/// The one directory IFD 0's `tag` group names, `None` when there is no such group, or `refusal`
/// when it names two or more (a non-conformant array [`TiffMetadata`] has no slot for).
///
/// Matched on the array's shape rather than tested against a length bound, so each of the three
/// outcomes is one arm and no operator has an equivalent twin.
fn single_directory(ifd0: &Ifd, tag: u16, refusal: &'static str) -> Result<Option<Ifd>> {
    match ifd0
        .sub_ifds()
        .iter()
        .find(|group| group.tag == tag)
        .map(|group| group.ifds.as_slice())
        .unwrap_or_default()
    {
        [] => Ok(None),
        [directory] => Ok(Some(directory.clone())),
        [_, _, ..] => Err(Error::invalid_input(env!("CARGO_PKG_NAME"), refusal)),
    }
}

/// The writer's side of what the reader delivers: refuses an Exif subtree this crate could not
/// hand back unchanged, `depth` further levels being all that is left below `ifd`.
///
/// **This inspects what the [`metadata_scope`] walk inspects, and that symmetry is the whole
/// design.**
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

/// A raw `BYTE`/`UNDEFINED` payload, copied out of a directory entry.
fn bytes_value(value: Option<&Value>) -> Option<Vec<u8>> {
    value.and_then(Value::as_bytes).map(<[u8]>::to_vec)
}

/// The IPTC-IIM stream of an `IPTC/NAA` (33723) entry: its `BYTE`/`UNDEFINED` payload, or — the
/// type Photoshop and other Adobe software write, which TIFF/EP (ISO 12234-2) also names — the
/// bytes of a `LONG` array as they lie in the file.
///
/// libtiff's reader carries the same workaround (`tif_dirread.c`, `TIFFTAG_RICHTIFFIPTC` typed
/// `TIFF_LONG`: "Adobe's software (wrongly) writes RichTIFFIPTC tag with data type LONG instead
/// of UNDEFINED"), and exiv2 decodes the tag from its raw data whatever the type. The IIM stream
/// is a byte stream that was merely labelled `LONG`, so the file's byte order never applied to
/// it: re-packing each decoded word in that order recovers the bytes on disk. (libtiff instead
/// hands back the words in *host* order, which equals the file's bytes only on a host of the
/// file's endianness.)
fn iptc_value(value: Option<&Value>, order: ByteOrder) -> Option<Vec<u8>> {
    match value? {
        Value::Long(words) => Some(words.iter().flat_map(|&w| order.pack_u32(w)).collect()),
        other => bytes_value(Some(other)),
    }
}

/// Reads the metadata a TIFF carries: IFD 0's blocks, Exif 0th-IFD fields and Exif and GPS
/// sub-IFDs, plus the C2PA manifest
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
    // IFD 0's subtree and no other page's, with a per-level tag list — see [`metadata_scope`].
    // Following a pointer by hand is `read_ifd_at`'s documented purpose.
    resolve_subtree(ifd0, metadata_scope(), |offset| {
        read_ifd_at(data, offset, order, variant)
    })?;
    let exif = single_directory(ifd0, tags::EXIF_IFD, EXIF_ARRAY_REFUSAL)?;
    let gps = single_directory(ifd0, tags::GPS_INFO, GPS_ARRAY_REFUSAL)?;
    let mut fields = Ifd::new();
    for field in ifd0.fields() {
        if IFD0_EXIF_TAGS.contains(&field.tag) {
            fields.set(field.tag, field.value.clone());
        }
    }
    let ifd0_fields = with_content(Some(&fields)).cloned();
    let xmp = bytes_value(ifd0.get(tags::XMP));
    let iptc = iptc_value(ifd0.get(tags::IPTC_NAA), order);
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
    let exif_byte_order =
        (exif.is_some() || ifd0_fields.is_some() || gps.is_some()).then_some(order);
    Ok(TiffMetadata {
        exif_byte_order,
        exif,
        ifd0: ifd0_fields,
        gps,
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
/// lies outside `file` or overlaps the entry's own count field.
pub fn c2pa_exclusions(file: &[u8]) -> Result<Option<C2paExclusions>> {
    c2pa::locate(file)
}

/// A non-validating report of the C2PA manifest store `file` carries, for a host to print: the
/// store [`c2pa_exclusions`] locates, and every tag-52545 entry of the main IFD chain that yields
/// none — misplaced in an earlier directory, mistyped, duplicated, too short, or pointing outside
/// the file or over its own count field — with the reason, rather than reporting it as absence.
///
/// [`C2paSummary::report_lines`](gamut_ifd::c2pa::C2paSummary::report_lines) renders it, with the
/// non-validation disclaimer inline; the store's bytes are never part of it. See
/// [`gamut_ifd::c2pa::summarize`], which this delegates to, for the whole contract.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`](gamut_core::Error::InvalidInput) if the container is
/// unreadable (bad header, looping or runaway IFD chain, no IFD). An unreadable store is not an
/// error here; it is listed.
pub fn c2pa_summary(file: &[u8]) -> Result<C2paSummary> {
    c2pa::summarize(file)
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

    /// Exif 0th-IFD fields under two carried tags.
    fn page_fields() -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(tags::MAKE, Value::Ascii("gamut".into()));
        ifd.set(tags::ORIENTATION, Value::Short(vec![6]));
        ifd
    }

    /// A GPS directory holding one recognisable field, distinct from [`exif_ifd`]'s.
    fn gps_ifd() -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(1, Value::Ascii("N".into())); // GPSLatitudeRef
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

    /// One `Value` per directory entry representable in a file of `variant`: the natural variant
    /// of every recognised type code, and the [`Value::Unknown`] form at a few codes no field type
    /// claims.
    ///
    /// Derived from [`every_type_code`], so a type added upstream enters this sweep without an
    /// edit here. `Unknown` appears only at unclaimed codes because
    /// [`gamut_ifd::UnknownValue::new`] refuses a recognised one (#608): that is what keeps a
    /// value's in-memory variant and its on-disk code in agreement, the subject of this sweep.
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
            TiffMetadata::new().with_ifd0(page_fields()),
            TiffMetadata::new().with_gps(gps_ifd()),
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
            .with_ifd0(page_fields())
            .with_gps(gps_ifd())
            .with_xmp(b"<x:xmpmeta/>".to_vec())
            .with_iptc(vec![0x1c, 0x02, 0x05])
            .with_icc(vec![7; 4])
            .with_c2pa(b"\0\0\0\x14jumbc2pa".to_vec());
        assert_eq!(meta.exif, Some(exif_ifd()));
        assert_eq!(meta.ifd0, Some(page_fields()));
        assert_eq!(meta.gps, Some(gps_ifd()));
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
    fn read_metadata_refuses_an_exif_ifd_array_rather_than_keeping_its_first() {
        // Issue #599: `read_metadata` took the group's first directory and dropped the rest, so a
        // two-offset `ExifIFD` came back as one directory with nothing to say the second was gone.
        // The second directory differs from the first, so a reader that kept either would pass
        // `expect_err` only by refusing.
        let mut second = Ifd::new();
        second.set(33434, Value::Rational(vec![(1, 60)])); // ExposureTime
        let mut ifd0 = Ifd::new();
        ifd0.set_sub_ifd(tags::EXIF_IFD, vec![exif_ifd(), second]);
        let bytes = file_with(ifd0);
        assert_eq!(
            read(&bytes).expect("read").ifds[0].get(tags::EXIF_IFD),
            Some(&Value::Long(vec![26, 44])),
            "the fixture must carry a two-offset ExifIFD array"
        );
        let err = read_metadata(&bytes).expect_err("an ExifIFD array of two directories");
        assert_eq!(err.static_message(), Some(EXIF_ARRAY_REFUSAL), "{err}");
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
    fn the_reader_accepts_the_exif_group_tag_the_writer_refuses() {
        // The one direction in which `check` is deliberately *stricter* than the reader, stated on
        // `TiffMetadata::check` as clause 2 and nowhere held: a group under a tag outside
        // `EXIF_SUBTREE_POINTER_TAGS` is refused by the writer, while the reader takes such a file
        // without error and hands the tag back as the raw absolute offset it was written as -- the
        // child directory is lost, silently. That asymmetry is the *reason* the refusal is the
        // conservative choice, so it has to fail if it ever stops being true: if the reader began
        // resolving the tag there would be nothing left to refuse, and if it began erroring the
        // refusal would no longer be the stricter side. The refusal itself is
        // `the_writer_refuses_an_exif_group_under_a_tag_the_reader_does_not_resolve`; this is the
        // reader's half, on the same shape.
        const VENDOR: u16 = 50000;
        let mut child = Ifd::new();
        child.set(1, Value::Byte(vec![9]));
        let mut exif = exif_ifd();
        exif.set_sub_ifd(VENDOR, vec![child]);
        let mut ifd0 = Ifd::new();
        ifd0.set_sub_ifd(tags::EXIF_IFD, vec![exif]);

        let back = read_metadata(&file_with(ifd0))
            .expect("the reader takes the file the writer refuses")
            .exif
            .expect("an Exif directory");
        assert!(
            back.get_u32(VENDOR).is_some_and(|offset| offset > 0),
            "the vendor group comes back as the bare file offset the writer wrote: {back:?}"
        );
        assert!(
            back.sub_ifds().iter().all(|group| group.tag != VENDOR),
            "a tag outside the standard four is not resolved into a group: {back:?}"
        );
    }

    #[test]
    fn the_writer_refuses_an_exif_pointer_tag_carried_as_a_pointer_typed_field() {
        // `check` inspected only `sub_ifds()` while the reader's walk inspects `get(tag)`, so the
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
        // `POINTER_TYPE_CODES` restates, as on-disk codes, the set the resolver follows as
        // `Value` variants — and the writer asks the code while the reader asks the variant, so
        // the two must not drift. Derived by sweeping the whole `u16` code space through
        // `FieldType::from_code` and asking the walk `read_metadata` runs whether it follows a
        // value of each type under `ExifIFD`, never by repeating the four numbers: a hand list
        // short by one is the defect this constant exists to close.
        let derived: Vec<u16> = every_type_code()
            .into_iter()
            .filter(|&code| {
                let ty = gamut_ifd::FieldType::from_code(code).expect("a swept code");
                let mut ifd0 = Ifd::new();
                ifd0.set(tags::EXIF_IFD, canonical_value(ty));
                let mut followed = false;
                resolve_subtree(&mut ifd0, metadata_scope(), |_| {
                    followed = true;
                    Ok(Ifd::new())
                })
                .expect("an empty child directory resolves");
                followed
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
        // type code, and `Value::Unknown` was the one shape where those disagreed: built at code 4,
        // 13, 16 or 18 it was a plain field to a variant-shaped predicate and a pointer to the
        // reader, so it encoded cleanly and then failed `read_metadata` — or, in BigTIFF at the
        // top level, came back as a *group* where a field was written. `gamut-ifd` now refuses
        // that `Unknown` at construction (#608).
        //
        // The sweep is derived, not listed: every representable entry type (every recognised
        // code, and `Unknown` at unclaimed codes) x the four pointer tags x classic and BigTIFF x
        // on the Exif directory itself and one level below it. Only accepted sets are exercised;
        // that the refused ones are exactly the pointer-coded ones is
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
        //
        // `gamut-ifd`'s setters now keep a tag in one store (the later call wins, #601), so the
        // conflict is built the one way still open: attaching the group under a free tag and
        // retagging it in place through `sub_ifds_mut`.
        for &tag in EXIF_SUBTREE_POINTER_TAGS {
            let mut child = Ifd::new();
            child.set(1, Value::Ascii("R98".into()));
            let mut exif = exif_ifd();
            exif.set(tag, Value::Short(vec![7]));
            exif.set_sub_ifd(u16::MAX, vec![child]);
            exif.sub_ifds_mut()[0].tag = tag;
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
        // outright. The resolver follows only LONG/IFD/LONG8/IFD8, so a `SHORT` under
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
        // `TiffMetadata::exif`, `TiffEncoder::with_tiff_metadata`, `TiffDecoder::metadata`, README.md,
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
    fn ifd_0_resolves_the_exif_and_gps_directories_and_no_other_standard_pointer() {
        // The other half of the per-level rule, and the whole of it: at IFD 0 only `ExifIFD` and
        // `GPSInfo` are followed, so every *other* standard pointer field on the page stays the
        // plain integer field it was read as rather than becoming a group. What that buys — a dangling one of
        // them not hiding the blocks — is
        // `a_broken_pointer_the_metadata_does_not_use_does_not_hide_the_blocks`
        // (tests/metadata.rs); this pins the resolution itself, on a pointer that is perfectly
        // readable, so the two claims cannot be confused.
        //
        // Both halves are asserted here because neither holds the behaviour alone. The membership
        // assertion is what a widened `IFD0_POINTER_TAGS` fails: a `const`'s contents are not a
        // mutable expression, so the mutation gate cannot see this at all, and adding
        // `InteroperabilityIFD` to the set left all 25 of this crate's test binaries green. The
        // sweep is derived rather than repeating a list by hand — a hand list is what let that
        // widening through — so it can never assert "left alone" about a tag the constant says is
        // followed.
        //
        // It is derived from `gamut_ifd::tags::STANDARD_POINTER_TAGS`, the sibling crate's own
        // constant, and not from this crate's `EXIF_SUBTREE_POINTER_TAGS`, which is *defined* as
        // that constant. Same members today, and deliberately the wider name: the domain this
        // sweep has to cover is "every standard pointer tag that exists", so a fifth one added
        // upstream must enter it whatever this crate's alias is doing. Deriving from the alias
        // would let a later narrowing of the alias shrink the sweep silently — the same
        // hand-maintained-set failure one indirection further out.
        //
        // The walk is driven directly, with the scope `read_metadata` runs it with,
        // because IFD 0 is the one directory the seam never hands back; the public observation
        // that also sees this is `a_broken_pointer_the_metadata_does_not_use_does_not_hide_the_blocks`,
        // which fails on a dangling target rather than on a resolved one.
        assert_eq!(
            IFD0_POINTER_TAGS,
            [tags::EXIF_IFD, tags::GPS_INFO],
            "IFD 0 follows the two pointers whose targets `TiffMetadata` returns, and no other"
        );
        let unresolved = gamut_ifd::tags::STANDARD_POINTER_TAGS
            .iter()
            .copied()
            .filter(|tag| !IFD0_POINTER_TAGS.contains(tag));
        for tag in unresolved {
            let mut source = Ifd::new();
            source.set_sub_ifd(tag, vec![exif_ifd()]);
            source.set(tags::XMP, Value::Byte(b"x".to_vec()));
            let bytes = file_with(source);
            let mut ifd0 = read(&bytes).expect("read").ifds.swap_remove(0);
            let offset = ifd0
                .get_u32(tag)
                .unwrap_or_else(|| panic!("tag {tag}: a written pointer"));
            assert!(offset > 0, "tag {tag}: the pointer must name a directory");

            resolve_subtree(&mut ifd0, metadata_scope(), |offset| {
                read_ifd_at(&bytes, offset, ByteOrder::LittleEndian, Variant::Classic)
            })
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
    fn the_summary_reports_the_located_store_and_lists_an_unreadable_one() {
        // The report is the locator's view for a host to print: the store `c2pa_exclusions` finds,
        // and — where the locator says absence — the entry that is present but yields no store.
        let mut ifd0 = Ifd::new();
        ifd0.set(tags::C2PA_MANIFEST_STORE, Value::Undefined(vec![0x10; 12]));
        let bytes = file_with(ifd0);
        let summary = c2pa_summary(&bytes).expect("summarize");
        let store = summary.store.expect("a store");
        assert_eq!(
            Some(store.exclusions),
            c2pa_exclusions(&bytes).expect("locate")
        );
        assert!(summary.unread.is_empty());

        let mut ifd0 = Ifd::new();
        ifd0.set(tags::C2PA_MANIFEST_STORE, Value::Byte(vec![0x10; 12]));
        let summary = c2pa_summary(&file_with(ifd0)).expect("summarize");
        assert_eq!(summary.store, None);
        assert_eq!(summary.unread.len(), 1);
        assert_eq!(
            summary.unread[0].reason,
            gamut_ifd::c2pa::C2paUnreadReason::NotUndefined
        );
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
    fn the_ifd0_exif_tags_are_table_4s_descriptive_tags() {
        // A `const`'s contents are invisible to the mutation gate, so the list is pinned here
        // against EXIF 2.32 §4.6.4 Table 4: its group D "other tags" plus `Orientation` (group
        // A), `WhitePoint` and `PrimaryChromaticities` (group C), ascending.
        assert_eq!(
            IFD0_EXIF_TAGS,
            [270, 271, 272, 274, 305, 306, 315, 318, 319, 33432]
        );
    }

    #[test]
    fn every_ifd0_exif_tag_is_written_into_the_page_and_read_back() {
        // One tag at a time, beside a field the encoder owns: the field under the carried tag
        // comes back as the whole of `ifd0`, and the encoder's `ImageWidth` does not.
        for &tag in IFD0_EXIF_TAGS {
            let mut fields = Ifd::new();
            fields.set(tag, Value::Ascii("v".into()));
            let meta = TiffMetadata::new().with_ifd0(fields.clone());
            meta.check()
                .unwrap_or_else(|e| panic!("tag {tag}: refused: {e}"));
            let mut page = Ifd::new();
            page.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
            meta.apply(&mut page);
            assert_eq!(page.get(tag), Some(&Value::Ascii("v".into())), "tag {tag}");
            let back = read_metadata(&file_with(page)).expect("read");
            assert_eq!(back.ifd0, Some(fields), "tag {tag}");
        }
    }

    #[test]
    fn the_byte_order_is_recorded_for_each_exif_carrier_alone() {
        let mut exif_only = Ifd::new();
        exif_only.set_sub_ifd(tags::EXIF_IFD, vec![exif_ifd()]);
        let mut gps_only = Ifd::new();
        gps_only.set_sub_ifd(tags::GPS_INFO, vec![gps_ifd()]);
        for (name, page) in [
            ("exif", exif_only),
            ("ifd0", page_fields()),
            ("gps", gps_only),
        ] {
            let back = read_metadata(&file_with(page)).expect("read");
            assert_eq!(
                back.exif_byte_order,
                Some(ByteOrder::LittleEndian),
                "{name}"
            );
        }
        let mut xmp_only = Ifd::new();
        xmp_only.set(tags::XMP, Value::Byte(b"x".to_vec()));
        let back = read_metadata(&file_with(xmp_only)).expect("read");
        assert_eq!(back.exif_byte_order, None, "no Exif carrier, no byte order");
    }

    #[test]
    fn the_gps_directory_comes_back_parsed() {
        let mut page = Ifd::new();
        page.set_sub_ifd(tags::GPS_INFO, vec![gps_ifd()]);
        let back = read_metadata(&file_with(page)).expect("read");
        assert_eq!(back.gps, Some(gps_ifd()));
        assert_eq!(back.exif, None);
    }

    #[test]
    fn read_metadata_refuses_a_gps_array_rather_than_keeping_its_first() {
        let mut page = Ifd::new();
        page.set_sub_ifd(tags::GPS_INFO, vec![gps_ifd(), exif_ifd()]);
        let err = read_metadata(&file_with(page)).expect_err("a GPSInfo array of two directories");
        assert_eq!(err.static_message(), Some(GPS_ARRAY_REFUSAL), "{err}");
    }

    #[test]
    fn apply_writes_the_ifd0_fields_into_the_page_and_the_gps_group_beside_it() {
        let mut page = Ifd::new();
        TiffMetadata::new()
            .with_ifd0(page_fields())
            .with_gps(gps_ifd())
            .apply(&mut page);
        assert_eq!(page.fields(), page_fields().fields());
        let groups = page.sub_ifds();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tag, tags::GPS_INFO);
        assert_eq!(groups[0].ifds, vec![gps_ifd()]);
    }

    #[test]
    fn empty_ifd0_fields_and_gps_directory_write_nothing() {
        let meta = TiffMetadata::new()
            .with_ifd0(Ifd::new())
            .with_gps(Ifd::new());
        assert!(meta.is_empty());
        let mut page = Ifd::new();
        meta.apply(&mut page);
        assert!(page.fields().is_empty());
        assert!(page.sub_ifds().is_empty());
    }

    #[test]
    fn the_writer_refuses_ifd0_fields_the_page_holds_as_the_encoders_own() {
        // A carried field beside an uncarried one: the refusal is for *any* field outside the
        // list, not only the first.
        let mut layout = page_fields();
        layout.set(tags::IMAGE_WIDTH, Value::Short(vec![9]));
        let err = TiffMetadata::new()
            .with_ifd0(layout)
            .check()
            .expect_err("ImageWidth is the encoder's");
        assert_eq!(err.kind(), gamut_core::ErrorKind::InvalidInput);
        assert_eq!(err.static_message(), Some(IFD0_FIELD_REFUSAL));

        let mut group = Ifd::new();
        group.set_sub_ifd(tags::SUB_IFDS, vec![exif_ifd()]);
        let err = TiffMetadata::new()
            .with_ifd0(group)
            .check()
            .expect_err("IFD 0's pointers are the encoder's");
        assert_eq!(err.static_message(), Some(IFD0_GROUP_REFUSAL));

        TiffMetadata::new()
            .with_ifd0(page_fields())
            .check()
            .expect("carried tags only");
    }

    #[test]
    fn the_gps_directory_is_held_to_the_exif_directorys_bounds() {
        // The GPS sub-IFD is read back by the same walk, so a group under a tag that walk does
        // not resolve is refused there too.
        let mut gps = gps_ifd();
        gps.set_sub_ifd(700, vec![exif_ifd()]);
        let err = TiffMetadata::new()
            .with_gps(gps)
            .check()
            .expect_err("a group the reader would hand back as an offset");
        assert_eq!(err.static_message(), Some(FOREIGN_GROUP_REFUSAL));
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

    /// Adobe software types `IPTC/NAA` (33723) `LONG`, and the reader used to accept only
    /// `BYTE`/`UNDEFINED` there, so such a file's IIM stream was silently dropped. The stream is
    /// the entry's on-disk bytes in either byte order — the words are built from those bytes in
    /// the file's own order, so a reader that swapped them, or ignored the order, would differ.
    #[test]
    fn an_iptc_block_typed_long_reads_back_as_its_on_disk_bytes() {
        let iim = [0x1c, 0x02, 0x05, 0x00, 0x04, b'T', b'e', b's'];
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            let words = iim
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&c| order.u32(c))
                .collect();
            let mut ifd0 = Ifd::new();
            ifd0.set(tags::IPTC_NAA, Value::Long(words));
            let bytes = write(&TiffFile {
                order,
                variant: Variant::Classic,
                ifds: vec![ifd0],
            })
            .expect("write");
            assert!(
                bytes.windows(iim.len()).any(|w| w == iim),
                "{order:?}: the file holds the stream verbatim"
            );
            let back = read_metadata(&bytes).expect("read");
            assert_eq!(back.iptc.as_deref(), Some(&iim[..]), "{order:?}");
        }
        // A type that is neither bytes nor `LONG` is still no IIM stream.
        assert_eq!(
            iptc_value(Some(&Value::Short(vec![1])), ByteOrder::LittleEndian),
            None
        );
    }
}

/// The `metadata` feature: the facade's [`TypedMetadata`](gamut_metadata::TypedMetadata) over
/// [`TiffMetadata`], and the two conversions between a TIFF's `ExifIFD` directory and the
/// facade's [`Exif`](gamut_metadata::exif::Exif) model that it and
/// [`TiffEncoder::with_encoded_metadata`](crate::TiffEncoder::with_encoded_metadata) share.
///
/// # What of the EXIF model a TIFF holds
///
/// Everything but the thumbnail. The model's **0th-IFD fields** are [`TiffMetadata::ifd0`] —
/// a TIFF's IFD 0 *is* the image's own directory, so they are written straight into it, limited
/// to [`IFD0_EXIF_TAGS`](super::IFD0_EXIF_TAGS); its **GPS sub-IFD** is [`TiffMetadata::gps`];
/// and its **Exif sub-IFD and Interoperability sub-IFD** are [`TiffMetadata::exif`] — the reader
/// resolves `InteroperabilityIFD` as a group inside `ExifIFD`, which the model holds as its own
/// directory. The **thumbnail** (the 1st IFD) has no carrier: in a TIFF it would be a page of its
/// own. Writing a model that holds one, or a 0th-IFD field outside the carried tags, is a typed
/// [`Error::Unsupported`](gamut_core::Error::Unsupported) rather than a silent drop.
#[cfg(feature = "metadata")]
pub(crate) mod typed {
    use gamut_core::{Error, Result};
    use gamut_ifd::{ByteOrder, Ifd};
    use gamut_metadata::exif::Exif;
    use gamut_metadata::{Metadata, MetadataBlock, TypedMetadata};

    use super::{TiffMetadata, ifd0_refusal};
    use crate::tags;

    /// The refusal for an EXIF model holding a thumbnail, which this container has no slot for.
    pub(crate) const EXIF_THUMBNAIL_REFUSAL: &str = "TIFF: an EXIF thumbnail (the 1st IFD) has no \
         TiffMetadata carrier; in a TIFF it would be a page of its own";

    /// The facade model of a TIFF's Exif carriers, in `order`, or `None` when it holds none: the
    /// 0th-IFD fields as the model's 0th IFD, the GPS sub-IFD as its GPS sub-IFD, and the
    /// `ExifIFD` directory as its Exif sub-IFD — a single `InteroperabilityIFD` group lifted out
    /// of that into the model's own Interoperability sub-IFD.
    fn exif_model(meta: &TiffMetadata, order: ByteOrder) -> Option<Exif> {
        if meta.exif.is_none() && meta.ifd0.is_none() && meta.gps.is_none() {
            return None;
        }
        let mut exif = Exif::new(order);
        if let Some(fields) = &meta.ifd0 {
            *exif.image_mut() = fields.clone();
        }
        if let Some(gps) = &meta.gps {
            exif.set_gps_ifd(gps.clone());
        }
        if let Some(ifd) = &meta.exif {
            let mut directory = Ifd::new();
            for field in ifd.fields() {
                directory.set(field.tag, field.value.clone());
            }
            for group in ifd.sub_ifds() {
                match (group.tag, group.ifds.as_slice()) {
                    (tags::INTEROPERABILITY_IFD, [interop]) => {
                        exif.set_interop_ifd(interop.clone());
                    }
                    _ => directory.set_sub_ifd(group.tag, group.ifds.clone()),
                }
            }
            exif.set_exif_ifd(directory);
        }
        Some(exif)
    }

    /// The Exif carriers a TIFF writes for one EXIF model, each `None` when the model holds
    /// nothing for it.
    #[derive(Debug, PartialEq)]
    pub(crate) struct ExifCarriers {
        /// The model's 0th-IFD fields, for [`TiffMetadata::ifd0`].
        pub(crate) ifd0: Option<Ifd>,
        /// The model's Exif sub-IFD with its Interoperability sub-IFD attached as an
        /// `InteroperabilityIFD` group, for [`TiffMetadata::exif`].
        pub(crate) exif: Option<Ifd>,
        /// The model's GPS sub-IFD, for [`TiffMetadata::gps`].
        pub(crate) gps: Option<Ifd>,
    }

    /// The carriers a TIFF writes for `exif` — the inverse of `exif_model`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when the model holds a thumbnail, a 0th-IFD field under a
    /// tag outside [`IFD0_EXIF_TAGS`](super::IFD0_EXIF_TAGS), or a 0th-IFD sub-IFD group — what a
    /// `TiffMetadata` has no carrier for (see the module docs).
    pub(crate) fn exif_carriers(exif: &Exif) -> Result<ExifCarriers> {
        if exif.thumbnail().is_some() {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                EXIF_THUMBNAIL_REFUSAL,
            ));
        }
        let image = exif.image();
        if let Some(refusal) = ifd0_refusal(image) {
            return Err(Error::unsupported(env!("CARGO_PKG_NAME"), refusal));
        }
        let directory = match (exif.exif_ifd(), exif.interop_ifd()) {
            (None, None) => None,
            (directory, interop) => {
                let mut directory = directory.cloned().unwrap_or_default();
                if let Some(interop) = interop {
                    directory.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![interop.clone()]);
                }
                Some(directory)
            }
        };
        Ok(ExifCarriers {
            ifd0: (!image.fields().is_empty()).then(|| image.clone()),
            exif: directory,
            gps: exif.gps_ifd().cloned(),
        })
    }

    /// The facade's [`TypedMetadata`] over the seven carriers. Like `gamut-dng`'s, the EXIF
    /// carriers are parsed directories rather than bytes, so [`blocks`](TypedMetadata::blocks)
    /// hands over only the four byte carriers and [`metadata`](TypedMetadata::metadata) assigns
    /// the EXIF model straight onto [`Metadata::exif`].
    ///
    /// ```
    /// use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef};
    /// use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
    /// use gamut_tiff::{Metadata, TiffEncoder, TypedMetadata};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut exif = Exif::new(ByteOrder::LittleEndian);
    /// exif.set_tag(ExifTag::Make, Value::Ascii("gamut".into()));
    /// exif.set_tag(ExifTag::Orientation, Value::Short(vec![6]));
    /// exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![400]));
    /// let typed = Metadata::from_carriers(Some(exif), None, None);
    ///
    /// let pixels = vec![0u8; 64];
    /// let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8)?)?;
    /// let tiff = TiffEncoder::new()
    ///     .with_metadata(&typed)?
    ///     .encode_to_vec(image)?;
    ///
    /// let read = gamut_tiff::TiffDecoder::new().metadata(&tiff)?;
    /// assert_eq!(read.metadata()?, typed);
    /// # Ok(())
    /// # }
    /// ```
    impl TypedMetadata for TiffMetadata {
        /// The byte-carried blocks — XMP, IPTC-IIM, ICC and the C2PA store — as
        /// [`MetadataBlock`]s, each present only when the file carried it.
        /// [`exif`](TiffMetadata::exif) is deliberately absent; see the impl's docs.
        ///
        /// # Errors
        ///
        /// Never: every carrier is already held as bytes. The signature is the trait's, shared
        /// with containers that resolve payloads lazily.
        fn blocks(&self) -> gamut_metadata::Result<Vec<MetadataBlock<'_>>> {
            let mut blocks = Vec::new();
            blocks.extend(self.xmp.as_deref().map(MetadataBlock::Xmp));
            blocks.extend(self.iptc.as_deref().map(MetadataBlock::IptcIim));
            blocks.extend(self.icc.as_deref().map(MetadataBlock::Icc));
            blocks.extend(self.c2pa.as_deref().map(MetadataBlock::C2pa));
            Ok(blocks)
        }

        /// The unified model: [`Metadata::from_blocks`] over [`blocks`](TypedMetadata::blocks)
        /// (the IPTC-IIM block reconciled into the XMP graph under the default
        /// [`ConflictPolicy`](gamut_metadata::ConflictPolicy)), with the Exif carriers —
        /// [`ifd0`](TiffMetadata::ifd0), [`exif`](TiffMetadata::exif) and
        /// [`gps`](TiffMetadata::gps) — assigned onto [`Metadata::exif`] as one model whose 0th
        /// IFD, Exif sub-IFD and GPS sub-IFD they are, in
        /// [`exif_byte_order`](TiffMetadata::exif_byte_order) — or little-endian,
        /// [`TiffEncoder`](crate::TiffEncoder)'s default, on a value built by hand.
        ///
        /// # Errors
        ///
        /// The [`MetadataError`](gamut_metadata::MetadataError) naming the byte carrier whose
        /// parse failed.
        fn metadata(&self) -> gamut_metadata::Result<Metadata> {
            let mut unified = Metadata::from_blocks(&self.blocks()?)?;
            unified.exif = exif_model(
                self,
                self.exif_byte_order.unwrap_or(ByteOrder::LittleEndian),
            );
            Ok(unified)
        }
    }

    #[cfg(test)]
    mod tests {
        use gamut_ifd::Value;
        use gamut_metadata::exif::{ExifTag, Value as ExifValue};

        use super::*;

        /// An `ExifIFD` as the reader returns one: an `ExposureTime` field and an
        /// `InteroperabilityIFD` group.
        fn directory() -> (Ifd, Ifd) {
            let mut interop = Ifd::new();
            interop.set(1, Value::Ascii("R98".into()));
            let mut ifd = Ifd::new();
            ifd.set(33434, Value::Rational(vec![(1, 60)]));
            ifd.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![interop.clone()]);
            (ifd, interop)
        }

        #[test]
        fn blocks_hand_over_the_byte_carriers_only() {
            let meta = TiffMetadata::new()
                .with_exif(directory().0)
                .with_xmp(b"<x:xmpmeta/>".to_vec())
                .with_iptc(vec![0x1c, 0x02, 0x05])
                .with_icc(vec![7; 4])
                .with_c2pa(b"jumb".to_vec());
            assert_eq!(
                meta.blocks().unwrap(),
                vec![
                    MetadataBlock::Xmp(b"<x:xmpmeta/>"),
                    MetadataBlock::IptcIim(&[0x1c, 0x02, 0x05]),
                    MetadataBlock::Icc(&[7; 4]),
                    MetadataBlock::C2pa(b"jumb"),
                ]
            );
        }

        /// The model of a `TiffMetadata` carrying only `ifd` as its `ExifIFD`.
        fn exif_only_model(ifd: &Ifd, order: ByteOrder) -> Exif {
            exif_model(&TiffMetadata::new().with_exif(ifd.clone()), order).unwrap()
        }

        /// Exif 0th-IFD fields and a GPS directory, as a camera's EXIF carries them.
        fn page_and_gps() -> (Ifd, Ifd) {
            let mut page = Ifd::new();
            page.set(tags::MAKE, Value::Ascii("gamut".into()));
            page.set(tags::ORIENTATION, Value::Short(vec![6]));
            let mut gps = Ifd::new();
            gps.set(1, Value::Ascii("N".into()));
            (page, gps)
        }

        #[test]
        fn the_exif_model_lifts_the_interoperability_group_out_of_the_directory() {
            let (ifd, interop) = directory();
            let exif = exif_only_model(&ifd, ByteOrder::BigEndian);
            assert_eq!(exif.byte_order(), ByteOrder::BigEndian);
            assert_eq!(exif.interop_ifd(), Some(&interop));
            let exif_ifd = exif.exif_ifd().unwrap();
            assert_eq!(exif_ifd.get(33434), Some(&Value::Rational(vec![(1, 60)])));
            assert!(exif_ifd.sub_ifds().is_empty());
            assert!(exif.image().fields().is_empty());
        }

        #[test]
        fn a_group_that_is_not_one_interoperability_directory_stays_in_the_directory() {
            // Two Interoperability directories have no single slot in the model, and a `GPSInfo`
            // group under `ExifIFD` is not the model's GPS sub-IFD: both stay where they were.
            let mut ifd = Ifd::new();
            ifd.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![Ifd::new(), Ifd::new()]);
            ifd.set_sub_ifd(tags::GPS_INFO, vec![Ifd::new()]);
            let exif = exif_only_model(&ifd, ByteOrder::LittleEndian);
            assert_eq!(exif.interop_ifd(), None);
            assert_eq!(exif.gps_ifd(), None);
            assert_eq!(exif.exif_ifd().unwrap().sub_ifds(), ifd.sub_ifds());
        }

        #[test]
        fn the_exif_model_takes_each_carrier_into_its_own_directory() {
            let (page, gps) = page_and_gps();
            let page_only = exif_model(
                &TiffMetadata::new().with_ifd0(page.clone()),
                ByteOrder::LittleEndian,
            )
            .unwrap();
            assert_eq!(page_only.image(), &page);
            assert_eq!(page_only.exif_ifd(), None);
            assert_eq!(page_only.gps_ifd(), None);

            let gps_only = exif_model(
                &TiffMetadata::new().with_gps(gps.clone()),
                ByteOrder::LittleEndian,
            )
            .unwrap();
            assert_eq!(gps_only.gps_ifd(), Some(&gps));
            assert!(gps_only.image().fields().is_empty());
            assert_eq!(gps_only.exif_ifd(), None);

            assert_eq!(
                exif_model(&TiffMetadata::new(), ByteOrder::LittleEndian),
                None
            );
        }

        #[test]
        fn exif_carriers_is_the_inverse_of_the_model() {
            let (ifd, _) = directory();
            let (page, gps) = page_and_gps();
            let meta = TiffMetadata::new()
                .with_exif(ifd.clone())
                .with_ifd0(page.clone())
                .with_gps(gps.clone());
            let exif = exif_model(&meta, ByteOrder::LittleEndian).unwrap();
            assert_eq!(
                exif_carriers(&exif).unwrap(),
                ExifCarriers {
                    ifd0: Some(page),
                    exif: Some(ifd),
                    gps: Some(gps),
                }
            );

            // An Interoperability sub-IFD alone still makes a directory to carry it.
            let mut interop_only = Exif::new(ByteOrder::LittleEndian);
            interop_only.set_interop_ifd(directory().1);
            let written = exif_carriers(&interop_only).unwrap().exif.unwrap();
            assert!(written.fields().is_empty());
            assert_eq!(written.sub_ifds().len(), 1);

            assert_eq!(
                exif_carriers(&Exif::new(ByteOrder::LittleEndian)).unwrap(),
                ExifCarriers {
                    ifd0: None,
                    exif: None,
                    gps: None,
                }
            );
        }

        #[test]
        fn each_part_of_the_model_without_a_carrier_is_refused() {
            let mut thumbnail = Exif::new(ByteOrder::LittleEndian);
            thumbnail.set_thumbnail(vec![0xff, 0xd8, 0xff, 0xd9]);
            let mut resolution = Exif::new(ByteOrder::LittleEndian);
            resolution.set_tag(ExifTag::Make, ExifValue::Ascii("gamut".into()));
            resolution
                .image_mut()
                .set(crate::tags::X_RESOLUTION, Value::Rational(vec![(300, 1)]));
            let mut group_in_ifd0 = Exif::new(ByteOrder::LittleEndian);
            group_in_ifd0.image_mut().set_sub_ifd(330, vec![Ifd::new()]);
            for (exif, refusal) in [
                (thumbnail, EXIF_THUMBNAIL_REFUSAL),
                (resolution, super::super::IFD0_FIELD_REFUSAL),
                (group_in_ifd0, super::super::IFD0_GROUP_REFUSAL),
            ] {
                let err = exif_carriers(&exif).unwrap_err();
                assert_eq!(err.kind(), gamut_core::ErrorKind::Unsupported);
                assert_eq!(err.static_message(), Some(refusal));
            }
        }

        #[test]
        fn metadata_assigns_the_exif_model_in_the_recorded_byte_order() {
            let (ifd, _) = directory();
            let mut meta = TiffMetadata::new()
                .with_exif(ifd.clone())
                .with_c2pa(b"jumb".to_vec());
            meta.exif_byte_order = Some(ByteOrder::BigEndian);
            let unified = meta.metadata().unwrap();
            assert_eq!(
                unified.exif,
                Some(exif_only_model(&ifd, ByteOrder::BigEndian))
            );
            // The byte carriers still go through `from_blocks` beside the assigned model.
            assert_eq!(unified.c2pa.as_deref(), Some(&b"jumb"[..]));

            // Built by hand: the encoder's default order.
            meta.exif_byte_order = None;
            assert_eq!(
                meta.metadata().unwrap().exif.unwrap().byte_order(),
                ByteOrder::LittleEndian
            );
            assert_eq!(TiffMetadata::new().metadata().unwrap(), Metadata::default());
        }
    }
}
