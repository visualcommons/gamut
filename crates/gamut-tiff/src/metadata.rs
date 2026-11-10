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
//! EXIF is the exception, and only because TIFF makes it one: a TIFF has no EXIF *block*. Its
//! `ExifIFD` (34665) and `GPSInfo` (34853) are IFDs, which this crate has already parsed by the
//! time a caller sees them, and the Exif *0th IFD* is the page's own IFD 0, limited to the
//! descriptive tags of [`IFD0_EXIF_TAGS`]: the rest of IFD 0 is the encoder's. So
//! [`TiffMetadata::exif`] carries them as the shared [`Exif`] model — the type `gamut-dng` and the
//! facade hold — rather than as bytes a caller would re-parse. Its fields are neither validated
//! nor completed, subject to the three normalisations a directory model implies (named on
//! [`TiffMetadata::exif`]); what of the model a TIFF cannot hold is left behind with a
//! [`MetadataNotice`]; and the four sub-IFD shapes this crate's reader could not hand back are
//! **refused** by the encode ([`TiffMetadata::check`]).
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
//! gamut-ifd's ([`gamut_ifd::append_c2pa_store`], [`gamut_ifd::c2pa_exclusions`]) — the one
//! place the workspace states §A.3.6, shared with `gamut-dng`
//! rather than re-derived here. The encoder places a store set with
//! [`TiffEncoder::with_c2pa`](crate::TiffEncoder::with_c2pa) or reserved with
//! [`TiffEncoder::with_c2pa_reserved`](crate::TiffEncoder::with_c2pa_reserved); this module
//! reports the store a file carries as [`TiffMetadata::c2pa`], gated on the shared locator
//! [`c2pa_exclusions`](crate::c2pa_exclusions).

use gamut_core::{Error, Result};
use gamut_exif::Exif;
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
    /// The page's EXIF, as the shared [`Exif`] model — the type `gamut-dng`'s `DngMetadata` and
    /// the facade's `Metadata` hold.
    ///
    /// A TIFF has no EXIF *block*: the model's directories are directories of the file itself.
    /// Its **0th IFD** is the page's own IFD 0 — EXIF 2.32 §4.6.4 Table 4 lists those tags as
    /// TIFF 6.0 attributes — limited to the descriptive tags of [`IFD0_EXIF_TAGS`]; its **Exif
    /// sub-IFD** is the `ExifIFD` (34665) directory, which holds its **Interoperability sub-IFD**
    /// as an `InteroperabilityIFD` (40965) group; and its **GPS sub-IFD** is the `GPSInfo` (34853)
    /// directory. On read the model is in the file's byte order; on write its byte order does not
    /// travel — the file's is [`TiffEncoder::with_byte_order`](crate::TiffEncoder::with_byte_order)'s.
    ///
    /// **What a TIFF cannot hold is left behind, and said so.** A thumbnail (the 1st IFD) would be
    /// a page of its own, and every other Table 4 tag (`ImageWidth`, `Compression`,
    /// `XResolution`, `StripOffsets`, `YCbCrPositioning`, `TransferFunction`, …) — and any sub-IFD
    /// group in the 0th IFD — is one the encoder writes from the pixels it lays out, or one whose
    /// meaning depends on that layout. The encoder writes neither and reports each with a
    /// [`MetadataNotice`] ([`TiffEncoder::metadata_notices`](crate::TiffEncoder::metadata_notices))
    /// rather than letting it overwrite or contradict the page's own entry. On read, IFD 0's
    /// fields under the carried tags come back as the 0th IFD and nothing else does.
    ///
    /// **Entries are carried unchanged; ordering is normalised.** This crate adds no mandatory
    /// Exif field — not even `ExifVersion` — and drops none of a sub-IFD's entries. Values are
    /// not validated: an `Orientation` of 6 is written as 6, and the stored pixels are not rotated
    /// by either side. What it does not promise is byte-identity, because a directory is a model
    /// rather than a byte range, and three normalisations are inherent to it:
    ///
    /// 1. fields are kept **sorted by ascending tag**, as TIFF 6.0 §2 requires on disk;
    /// 2. a **duplicated tag collapses** to its last occurrence;
    /// 3. a child directory's **next-IFD pointer is ignored** — a sub-IFD is a directory, not a
    ///    chain.
    ///
    /// A conforming source directory is unaffected by all three. A non-conforming one is
    /// silently repaired, which is worth knowing before using a re-encode to prove a file
    /// unmodified.
    ///
    /// **One directory each, and an array is refused.** An IFD-0 `ExifIFD` or `GPSInfo` entry
    /// naming two or more directories (EXIF 2.32 §4.6.3 A fixes each count at 1) is refused by
    /// [`TiffDecoder::metadata`](crate::TiffDecoder::metadata) with an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error rather than narrowed
    /// to its first directory — use [`deconstruct`](crate::deconstruct) to see the whole array.
    ///
    /// **Every standard pointer tag inside the Exif and GPS directories is resolved.** All four
    /// members of [`gamut_ifd::tags::STANDARD_POINTER_TAGS`] come back as parsed
    /// [`sub_ifds`](gamut_ifd::Ifd::sub_ifds) groups rather than as raw offsets, so the writer
    /// gives each a fresh offset when the directory is embedded again; at IFD 0 only `ExifIFD` and
    /// `GPSInfo` are followed. A *private* tag whose value happens to be a `LONG` file offset is
    /// indistinguishable from an ordinary integer field, so it is carried through unchanged and
    /// re-encoded verbatim, still holding an offset into the file it was read from; neither this
    /// crate nor [`deconstruct`](crate::deconstruct) can grade that.
    ///
    /// The shapes of a caller-built Exif or GPS sub-IFD this crate's reader could not hand back
    /// are refused by the encode, each with its own message — see
    /// [`TiffEncoder::with_tiff_metadata`](crate::TiffEncoder::with_tiff_metadata).
    pub exif: Option<Exif>,
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
    /// The C2PA manifest store the file carries, from the `C2PA` tag
    /// ([`gamut_ifd::C2PA_MANIFEST_STORE`], 52545, type `UNDEFINED`) of the last IFD of its main
    /// chain (C2PA 2.4 §A.3.6), verbatim; [`c2pa_exclusions`](crate::c2pa_exclusions) gives
    /// its byte ranges.
    ///
    /// **Read-side only.** A manifest store is signed over the bytes *around* it (§18.5), so a
    /// store read from one file is invalid in any other by construction:
    /// [`TiffEncoder::with_tiff_metadata`](crate::TiffEncoder::with_tiff_metadata) leaves it
    /// behind with [`MetadataNotice::C2paManifestStore`](crate::MetadataNotice::C2paManifestStore)
    /// rather than write it. A store computed for an encoder's own output goes through
    /// [`TiffEncoder::with_c2pa`](crate::TiffEncoder::with_c2pa).
    pub c2pa: Option<Vec<u8>>,
}

impl TiffMetadata {
    /// Creates an empty metadata set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a copy carrying `exif` as the file's EXIF — see [`exif`](Self::exif) for which of
    /// the model's directories a TIFF holds, and where.
    #[must_use]
    pub fn with_exif(mut self, exif: Exif) -> Self {
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

    /// Whether there is nothing to embed: no payload set, and no EXIF directory a TIFF holds —
    /// carried 0th-IFD fields, Exif sub-IFD or GPS sub-IFD — with content in it.
    ///
    /// An Exif or GPS sub-IFD with no entries counts as empty — writing it would add a pointer to
    /// a directory with nothing in it — and so does a 0th IFD with no carried field. A sub-IFD
    /// group *is* content: an Exif sub-IFD whose only content is an Interoperability sub-IFD
    /// still writes one on-disk entry. What a TIFF leaves behind (a thumbnail, an uncarried 0th-IFD
    /// field) is not content here. A read-side manifest store is: the set is not empty while it
    /// holds one, though no encoder writes it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.carriers() == ExifCarriers::default()
            && self.xmp.is_none()
            && self.iptc.is_none()
            && self.icc.is_none()
            && self.c2pa.is_none()
    }

    /// The directories a TIFF writes for [`exif`](Self::exif), each `None` when it has nothing
    /// worth a directory.
    pub(crate) fn carriers(&self) -> ExifCarriers {
        self.exif.as_ref().map(exif_carriers).unwrap_or_default()
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
    /// The 0th-IFD fields are not checked here: they are written into the page's own directory
    /// rather than below it, and what of them a TIFF cannot hold is left behind with a notice
    /// ([`exif_carriers`], [`notices`](Self::notices)) rather than refused.
    ///
    /// The blocks [`apply`](Self::apply) writes into IFD 0 sit under `XMP`, `IPTC_NAA` and
    /// `ICC_PROFILE`, none of which any level treats as a pointer, and the rest of IFD 0 is the
    /// encoder's own.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the Exif
    /// or GPS sub-IFD
    /// carries a pointer-typed field under a pointer tag, hangs a group off a tag the reader does
    /// not resolve, carries a field and a group under one tag, or nests deeper than the reader
    /// walks back.
    pub(crate) fn check(&self) -> Result<()> {
        let carriers = self.carriers();
        for directory in [&carriers.exif, &carriers.gps].into_iter().flatten() {
            check_exif_subtree(directory, MAX_POINTER_DEPTH - 1)?;
        }
        Ok(())
    }

    /// Writes the XMP / IPTC / ICC blocks, the Exif 0th-IFD fields and the Exif and GPS sub-IFDs
    /// into `ifd0`.
    ///
    /// The 0th-IFD fields are set after the encoder's own and cannot replace one, because
    /// [`exif_carriers`] keeps only [`IFD0_EXIF_TAGS`], none of which the encoder writes.
    ///
    /// The C2PA store is **never** written from here: one read from another file is signed over
    /// that file's bytes ([`notices`](Self::notices) reports it left behind), and one computed for
    /// this output is the encoder's own, placed at the end of the file (C2PA 2.4 §A.3.6) once the
    /// rest of it exists ([`gamut_ifd::append_c2pa_store`]).
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
        let carriers = self.carriers();
        if let Some(fields) = carriers.ifd0 {
            for field in fields.fields() {
                ifd0.set(field.tag, field.value.clone());
            }
        }
        if let Some(exif) = carriers.exif {
            ifd0.set_sub_ifd(tags::EXIF_IFD, vec![exif]);
        }
        if let Some(gps) = carriers.gps {
            ifd0.set_sub_ifd(tags::GPS_INFO, vec![gps]);
        }
    }

    /// What an encoder handed this set leaves behind, in a fixed order — empty when everything
    /// it holds is written.
    pub(crate) fn notices(&self) -> Vec<MetadataNotice> {
        let mut notices = Vec::new();
        if self.c2pa.is_some() {
            notices.push(MetadataNotice::C2paManifestStore);
        }
        if let Some(exif) = &self.exif {
            notices.extend(exif_notices(exif));
        }
        notices
    }
}

/// Something [`TiffEncoder`](crate::TiffEncoder) could not carry from the
/// [`TiffMetadata`] it was handed, and so left behind.
///
/// Preservation exists to stop metadata disappearing quietly, so what a carry cannot take is
/// named rather than passed over: read them back with
/// [`TiffEncoder::metadata_notices`](crate::TiffEncoder::metadata_notices) and tell the user, as
/// `gamut convert` does. This is deliberately **not** an error channel — `gamut-png`'s notices
/// are the same shape — because each is something a caller has to *know*, not something that
/// should fail a conversion whose pixels are fine. What the encoder could write but its own reader
/// could not read back is still an error, raised at the encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MetadataNotice {
    /// The C2PA manifest store ([`TiffMetadata::c2pa`]), left behind. A store is signed over the
    /// exact bytes of the file it was made for (C2PA 2.4 §18.5), so carried into a re-encode it
    /// is invalid by construction and a validator reports a *tampered* file rather than an
    /// unsigned one. Re-sign the output and set the store with
    /// [`TiffEncoder::with_c2pa`](crate::TiffEncoder::with_c2pa).
    C2paManifestStore,
    /// The EXIF thumbnail (the model's 1st IFD), left behind: in a TIFF it would be a page of its
    /// own, which a metadata carrier does not add.
    ExifThumbnail,
    /// An EXIF 0th-IFD field under `tag`, left behind because a TIFF's IFD 0 holds that tag as the
    /// encoder's own: it is not in [`IFD0_EXIF_TAGS`], so it describes the way the samples are
    /// laid out (`XResolution`, `YCbCrPositioning`, …), which the encoder writes from the pixels.
    ExifImageField {
        /// The field's tag.
        tag: u16,
    },
    /// A sub-IFD group under `tag` in the EXIF 0th IFD, left behind: IFD 0's own pointers are the
    /// encoder's, and the model's Exif and GPS sub-IFDs are carried as directories of their own.
    ExifImageGroup {
        /// The group's tag.
        tag: u16,
    },
}

impl MetadataNotice {
    /// One line naming the payload and why it was left behind, fit to show a user. The
    /// [`Display`](core::fmt::Display) form adds the tag a field or group notice carries.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::C2paManifestStore => {
                "C2PA manifest store: signed over the source bytes, so a copy would be invalid \
                 (C2PA 2.4 §18.5) — re-sign the output"
            }
            Self::ExifThumbnail => "EXIF thumbnail: a TIFF would hold it as a page of its own",
            Self::ExifImageField { .. } => {
                "EXIF 0th-IFD field: a TIFF's IFD 0 holds this tag as the encoder's own"
            }
            Self::ExifImageGroup { .. } => {
                "EXIF 0th-IFD sub-IFD group: a TIFF's IFD 0 pointers are the encoder's own"
            }
        }
    }
}

impl core::fmt::Display for MetadataNotice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ExifImageField { tag } | Self::ExifImageGroup { tag } => {
                write!(f, "{} (tag {tag})", self.reason())
            }
            _ => f.write_str(self.reason()),
        }
    }
}

/// The directories a TIFF writes for one EXIF model, each `None` when the model holds nothing
/// worth a directory for it.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ExifCarriers {
    /// The model's 0th-IFD fields under [`IFD0_EXIF_TAGS`], written into the page's IFD 0.
    pub(crate) ifd0: Option<Ifd>,
    /// The model's Exif sub-IFD with its Interoperability sub-IFD attached as an
    /// `InteroperabilityIFD` group, written as the `ExifIFD` directory.
    pub(crate) exif: Option<Ifd>,
    /// The model's GPS sub-IFD, written as the `GPSInfo` directory.
    pub(crate) gps: Option<Ifd>,
}

/// The directories a TIFF writes for `exif` — the inverse of [`exif_model`] over everything a TIFF
/// holds. What it does not hold is dropped here and named by [`exif_notices`].
pub(crate) fn exif_carriers(exif: &Exif) -> ExifCarriers {
    let mut fields = Ifd::new();
    for field in exif.image().fields() {
        if IFD0_EXIF_TAGS.contains(&field.tag) {
            fields.set(field.tag, field.value.clone());
        }
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
    ExifCarriers {
        ifd0: with_content(Some(&fields)).cloned(),
        exif: with_content(directory.as_ref()).cloned(),
        gps: with_content(exif.gps_ifd()).cloned(),
    }
}

/// What of `exif` a TIFF leaves behind, in a fixed order: the thumbnail, then each 0th-IFD field
/// outside [`IFD0_EXIF_TAGS`] in ascending tag order, then each 0th-IFD sub-IFD group.
fn exif_notices(exif: &Exif) -> Vec<MetadataNotice> {
    let image = exif.image();
    exif.thumbnail()
        .map(|_| MetadataNotice::ExifThumbnail)
        .into_iter()
        .chain(
            image
                .fields()
                .iter()
                .filter(|field| !IFD0_EXIF_TAGS.contains(&field.tag))
                .map(|field| MetadataNotice::ExifImageField { tag: field.tag }),
        )
        .chain(
            image
                .sub_ifds()
                .iter()
                .map(|group| MetadataNotice::ExifImageGroup { tag: group.tag }),
        )
        .collect()
}

/// The EXIF model of a TIFF's Exif directories, in `order`, or `None` when it holds none: the
/// 0th-IFD fields as the model's 0th IFD, the GPS directory as its GPS sub-IFD, and the `ExifIFD`
/// directory as its Exif sub-IFD — a single `InteroperabilityIFD` group lifted out of that into
/// the model's own Interoperability sub-IFD.
fn exif_model(carriers: ExifCarriers, order: ByteOrder) -> Option<Exif> {
    if carriers == ExifCarriers::default() {
        return None;
    }
    let mut exif = Exif::new(order);
    if let Some(fields) = carriers.ifd0 {
        *exif.image_mut() = fields;
    }
    if let Some(gps) = carriers.gps {
        exif.set_gps_ifd(gps);
    }
    if let Some(ifd) = carriers.exif {
        let mut directory = Ifd::new();
        for field in ifd.fields() {
            directory.set(field.tag, field.value.clone());
        }
        for group in ifd.sub_ifds() {
            match (group.tag, group.ifds.as_slice()) {
                (tags::INTEROPERABILITY_IFD, [interop]) => exif.set_interop_ifd(interop.clone()),
                _ => directory.set_sub_ifd(group.tag, group.ifds.clone()),
            }
        }
        exif.set_exif_ifd(directory);
    }
    Some(exif)
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

/// The Exif 0th-IFD tags [`TiffMetadata::exif`] carries into a TIFF's IFD 0, ascending.
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

/// The pointer tags [`read_metadata`] resolves **at IFD 0**, scoped to what [`TiffMetadata`]
/// actually returns.
///
/// Two tags, and they are a deliberate lower bound rather than a subset of convenience. `ExifIFD`
/// and `GPSInfo` are followed because those directories **are** fields of [`TiffMetadata`]
/// ([`exif`](TiffMetadata::exif)'s Exif and GPS sub-IFDs): they are handed to the caller
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
/// `GPSInfo` was on that side of the rule until `TiffMetadata` gave its target a field
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
/// [`gamut_ifd::IfdReader::read_tree`] cannot be scoped either way: it resolves the flat list it is given at
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
/// A per-level list rather than [`gamut_ifd::IfdReader::read_tree`]'s one flat list, because the two levels
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
/// It is **two**, not [`gamut_ifd::IfdReader::read_tree`]'s sixteen, because the walk starts at a page and
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
/// the model's GPS sub-IFD is one directory.
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
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for any of the
/// four refusals.
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
/// Whether a tag-52545 entry *is* a manifest store is [`gamut_ifd::c2pa_exclusions`]'s decision, not
/// a second opinion held here: this asks the locator first and reports the store only when it
/// agrees. That matters because `c2pa_exclusions` reports absence for an entry a caller could not use —
/// one of the wrong type, one too short to be a JUMBF box, a duplicated one — and reporting such
/// an entry as a store would hand the caller a [`TiffMetadata`] that
/// [`TiffEncoder`](crate::TiffEncoder) then refuses to encode. `gamut-dng` gates its own decode
/// on the same locator for the same reason.
///
/// Reader and locator agreeing is not the same as every readable store being writable, and this
/// does not promise the latter — it cannot, because a reader does not know which container the
/// caller will write. One case exists: `c2pa_exclusions` accepts a store of exactly
/// [`MIN_STORE_LEN`](gamut_core::c2pa::MIN_STORE_LEN) (8) bytes, while writing **BigTIFF** needs 9,
/// since 8 bytes would pack into the entry's own value word instead of being placed out of line
/// at the end of the file. So an 8-byte store read from any file cannot be written back to a
/// BigTIFF. The affected input is degenerate — 8 bytes is a JUMBF box header with no content, so
/// there is no manifest in it — but the refusal is real and comes from
/// [`TiffEncoder::with_c2pa_reserved`](crate::TiffEncoder::with_c2pa_reserved)'s placement rule,
/// not from disagreement here.
pub(crate) fn read_metadata(data: &[u8]) -> Result<TiffMetadata> {
    let gamut_ifd::Header { order, variant, .. } = read_header(data)?;
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
    let carriers = ExifCarriers {
        ifd0: with_content(Some(&fields)).cloned(),
        exif,
        gps,
    };
    let xmp = bytes_value(ifd0.get(tags::XMP));
    let iptc = iptc_value(ifd0.get(tags::IPTC_NAA), order);
    let icc = bytes_value(ifd0.get(tags::ICC_PROFILE));
    // §A.3.6: one store for the whole asset, in the last IFD of the main chain. `ifds` is that
    // chain, so its last element is where the entry belongs — and a single-page file makes the
    // two the same directory. The entry carries the store's bytes rather than an offset, so the
    // last IFD is reached without following a pointer, on a page whose pointers were never
    // resolved.
    let located = gamut_ifd::c2pa_exclusions(data)?.is_some();
    let c2pa = match ifds
        .last()
        .and_then(|ifd| ifd.get(tags::C2PA_MANIFEST_STORE))
    {
        Some(Value::Undefined(store)) if located => Some(store.clone()),
        _ => None,
    };
    Ok(TiffMetadata {
        exif: exif_model(carriers, order),
        xmp,
        iptc,
        icc,
        c2pa,
    })
}

#[cfg(test)]
mod tests {
    use gamut_ifd::{ByteOrder, TiffFile, Variant, c2pa_exclusions, c2pa_summary, write};

    use super::*;

    /// The crate's three Exif directories as the tests below were written against them, over the
    /// one model [`TiffMetadata::exif`] now holds: each setter places a directory in the model as
    /// given, and each getter is that directory as [`exif_carriers`] writes it. They route through the shipped conversions, so a test reading through them
    /// still pins what reaches the file.
    impl TiffMetadata {
        fn model(&mut self) -> &mut Exif {
            self.exif
                .get_or_insert_with(|| Exif::new(ByteOrder::LittleEndian))
        }

        fn with_exif_dir(mut self, ifd: Ifd) -> Self {
            self.model().set_exif_ifd(ifd);
            self
        }

        fn with_ifd0(mut self, fields: Ifd) -> Self {
            *self.model().image_mut() = fields;
            self
        }

        fn with_gps(mut self, gps: Ifd) -> Self {
            self.model().set_gps_ifd(gps);
            self
        }

        fn exif_dir(&self) -> Option<Ifd> {
            self.carriers().exif
        }

        fn ifd0_dir(&self) -> Option<Ifd> {
            self.carriers().ifd0
        }

        fn gps_dir(&self) -> Option<Ifd> {
            self.carriers().gps
        }
    }

    /// `meta` holding `store` as the manifest store a reader found — the field has no builder,
    /// because an encoder never writes it.
    fn with_store(mut meta: TiffMetadata, store: Vec<u8>) -> TiffMetadata {
        meta.c2pa = Some(store);
        meta
    }

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
    /// Covers every [`gamut_ifd::FieldType`]: the type is `#[non_exhaustive]`, so the compiler can
    /// no longer reject this match when a field type is added upstream, and the wildcard arm panics
    /// instead. [`every_type_code`] sweeps the whole code space through `from_code`, so the first
    /// run after such an addition fails here rather than silently sweeping a set short by one —
    /// which is exactly the defect that produced the pointer-field regression.
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
            other => panic!("no canonical value for field type {other:?}: extend this sweep"),
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
        let meta = TiffMetadata::new().with_exif_dir(Ifd::new());
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
            TiffMetadata::new().with_exif_dir(exif_ifd()),
            TiffMetadata::new().with_ifd0(page_fields()),
            TiffMetadata::new().with_gps(gps_ifd()),
            TiffMetadata::new().with_xmp(vec![1]),
            TiffMetadata::new().with_iptc(vec![1]),
            TiffMetadata::new().with_icc(vec![1]),
            with_store(TiffMetadata::new(), vec![1]),
        ];
        for (i, meta) in singles.iter().enumerate() {
            assert!(!meta.is_empty(), "carrier {i} alone must be non-empty");
        }
    }

    #[test]
    fn the_builders_set_the_field_they_name() {
        let meta = TiffMetadata::new()
            .with_exif_dir(exif_ifd())
            .with_ifd0(page_fields())
            .with_gps(gps_ifd())
            .with_xmp(b"<x:xmpmeta/>".to_vec())
            .with_iptc(vec![0x1c, 0x02, 0x05])
            .with_icc(vec![7; 4]);
        assert_eq!(meta.exif_dir(), Some(exif_ifd()));
        assert_eq!(meta.ifd0_dir(), Some(page_fields()));
        assert_eq!(meta.gps_dir(), Some(gps_ifd()));
        assert_eq!(meta.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        assert_eq!(meta.iptc.as_deref(), Some(&[0x1c, 0x02, 0x05][..]));
        assert_eq!(meta.icc.as_deref(), Some(&[7, 7, 7, 7][..]));
    }

    #[test]
    fn apply_writes_each_block_under_its_own_tag_and_type() {
        // The tag *and* the field type are the on-disk contract: XMP and IPTC are BYTE, ICC is
        // UNDEFINED. Distinct payloads, so a block written under the wrong tag is visible.
        let meta = TiffMetadata::new()
            .with_xmp(b"<x:xmpmeta/>".to_vec())
            .with_iptc(vec![0x1c, 0x02, 0x05])
            .with_icc(vec![7; 4])
            .with_exif_dir(exif_ifd());
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
        with_store(TiffMetadata::new(), vec![0; 16]).apply(&mut ifd);
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
        assert_eq!(
            read_metadata(&bytes).expect("read").exif_dir(),
            Some(exif_ifd())
        );
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

        let meta = TiffMetadata::new().with_exif_dir(only_a_group);
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
            .with_exif_dir(vendor)
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
            .exif_dir()
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
                let Err(err) = TiffMetadata::new().with_exif_dir(exif).check() else {
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
                        let meta = TiffMetadata::new().with_exif_dir(exif.clone());
                        if meta.check().is_err() {
                            continue;
                        }
                        let where_ = format!("{name} under {tag}, {variant:?}, nested={nested}");
                        let mut ifd0 = Ifd::new();
                        meta.apply(&mut ifd0);
                        let bytes = file_with_variant(ifd0, variant);
                        let back = read_metadata(&bytes)
                            .unwrap_or_else(|e| panic!("{where_}: accepted but unreadable: {e}"))
                            .exif_dir()
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
            let Err(err) = TiffMetadata::new().with_exif_dir(exif).check() else {
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
            let meta = TiffMetadata::new().with_exif_dir(exif);
            meta.check()
                .unwrap_or_else(|e| panic!("tag {tag}: a SHORT is not a pointer: {e}"));

            let mut ifd0 = Ifd::new();
            meta.apply(&mut ifd0);
            let back = read_metadata(&file_with(ifd0))
                .expect("read")
                .exif_dir()
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
                .exif_dir()
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
            .with_exif_dir(pair.clone())
            .check()
            .expect("the Exif -> Interop pair is what a decoded camera EXIF is");

        let mut deeper = exif_ifd();
        deeper.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![pair]);
        let err = TiffMetadata::new()
            .with_exif_dir(deeper)
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
            .with_exif_dir(exif_ifd())
            .apply(&mut ifd0);
        ifd0.set(tags::C2PA_MANIFEST_STORE, Value::Undefined(vec![0x10; 12]));
        let read = read_metadata(&file_with(ifd0)).expect("read");
        assert_eq!(read.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        assert_eq!(read.iptc.as_deref(), Some(&[0x1c, 0x02, 0x05][..]));
        assert_eq!(read.icc.as_deref(), Some(&[7, 7, 7, 7][..]));
        assert_eq!(read.exif_dir(), Some(exif_ifd()));
        assert_eq!(read.c2pa.as_deref(), Some(&[0x10; 12][..]));
    }

    #[test]
    fn the_reader_and_the_locator_agree_on_what_a_store_is() {
        // The two surfaces must never disagree: a `TiffMetadata` reporting a store that
        // `c2pa_exclusions` cannot find is one the encoder would refuse, turning a decode→encode
        // round trip into a hard error. Each case below is an entry `c2pa_exclusions` reports absent for a
        // *different* reason — wrong type (§A.3.6 fixes it at 7), and too short to be a JUMBF box
        // (below `MIN_STORE_LEN`) — so a fix that only handled one of them still fails here.
        for value in [
            Value::Byte(vec![0x10; 12]),
            Value::Undefined(vec![9; gamut_core::c2pa::MIN_STORE_LEN - 1]),
        ] {
            let mut ifd0 = Ifd::new();
            ifd0.set(tags::C2PA_MANIFEST_STORE, value.clone());
            let bytes = file_with(ifd0);
            assert_eq!(read_metadata(&bytes).expect("read").c2pa, None, "{value:?}");
            assert_eq!(
                c2pa_exclusions(&bytes[..]).expect("locate"),
                None,
                "{value:?}"
            );
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
        assert!(c2pa_exclusions(&bytes[..]).expect("locate").is_some());
    }

    #[test]
    fn the_summary_reports_the_located_store_and_lists_an_unreadable_one() {
        // The report is the locator's view for a host to print: the store `c2pa_exclusions` finds,
        // and — where the locator says absence — the entry that is present but yields no store.
        let mut ifd0 = Ifd::new();
        ifd0.set(tags::C2PA_MANIFEST_STORE, Value::Undefined(vec![0x10; 12]));
        let bytes = file_with(ifd0);
        let summary = c2pa_summary(&bytes[..]).expect("summarize");
        let store = summary.store.expect("a store");
        assert_eq!(
            Some(store.exclusions),
            c2pa_exclusions(&bytes[..]).expect("locate")
        );
        assert!(summary.unread.is_empty());

        let mut ifd0 = Ifd::new();
        ifd0.set(tags::C2PA_MANIFEST_STORE, Value::Byte(vec![0x10; 12]));
        let summary = c2pa_summary(&file_with(ifd0)[..]).expect("summarize");
        assert_eq!(summary.store, None);
        assert_eq!(summary.unread.len(), 1);
        assert_eq!(
            summary.unread[0].reason,
            gamut_ifd::C2paUnreadReason::NotUndefined
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
            assert_eq!(back.ifd0_dir(), Some(fields), "tag {tag}");
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
                back.exif.as_ref().map(Exif::byte_order),
                Some(ByteOrder::LittleEndian),
                "{name}"
            );
        }
        let mut xmp_only = Ifd::new();
        xmp_only.set(tags::XMP, Value::Byte(b"x".to_vec()));
        let back = read_metadata(&file_with(xmp_only)).expect("read");
        assert_eq!(
            back.exif.as_ref().map(Exif::byte_order),
            None,
            "no Exif carrier, no byte order"
        );
    }

    #[test]
    fn the_gps_directory_comes_back_parsed() {
        let mut page = Ifd::new();
        page.set_sub_ifd(tags::GPS_INFO, vec![gps_ifd()]);
        let back = read_metadata(&file_with(page)).expect("read");
        assert_eq!(back.gps_dir(), Some(gps_ifd()));
        assert_eq!(back.exif_dir(), None);
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
    fn ifd0_fields_the_page_holds_as_the_encoders_own_are_left_behind_with_a_notice() {
        // A carried field beside an uncarried one: the uncarried one is left behind and named, the
        // carried ones are still written, and nothing is refused.
        let mut layout = page_fields();
        layout.set(tags::IMAGE_WIDTH, Value::Short(vec![9]));
        let meta = TiffMetadata::new().with_ifd0(layout);
        meta.check().expect("left behind, not refused");
        assert_eq!(
            meta.notices(),
            vec![MetadataNotice::ExifImageField {
                tag: tags::IMAGE_WIDTH
            }]
        );
        let mut page = Ifd::new();
        meta.apply(&mut page);
        assert_eq!(page.fields(), page_fields().fields());

        let mut group = Ifd::new();
        group.set_sub_ifd(tags::SUB_IFDS, vec![exif_ifd()]);
        let meta = TiffMetadata::new().with_ifd0(group);
        assert_eq!(
            meta.notices(),
            vec![MetadataNotice::ExifImageGroup {
                tag: tags::SUB_IFDS
            }]
        );
        let mut page = Ifd::new();
        meta.apply(&mut page);
        assert!(
            page.sub_ifds().is_empty(),
            "IFD 0's pointers are the encoder's"
        );

        assert!(
            TiffMetadata::new()
                .with_ifd0(page_fields())
                .notices()
                .is_empty()
        );
    }

    /// A thumbnail is left behind before any field notice, and the notices come in a fixed order:
    /// the store, the thumbnail, the fields by ascending tag, then the groups.
    #[test]
    fn the_notices_name_each_thing_left_behind_in_a_fixed_order() {
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set_thumbnail(vec![0xff, 0xd8, 0xff, 0xd9]);
        let image = exif.image_mut();
        image.set(tags::X_RESOLUTION, Value::Rational(vec![(300, 1)]));
        image.set(tags::IMAGE_WIDTH, Value::Short(vec![9]));
        image.set(tags::MAKE, Value::Ascii("gamut".into()));
        image.set_sub_ifd(tags::SUB_IFDS, vec![Ifd::new()]);
        let meta = with_store(TiffMetadata::new().with_exif(exif), vec![0; 16]);
        assert_eq!(
            meta.notices(),
            vec![
                MetadataNotice::C2paManifestStore,
                MetadataNotice::ExifThumbnail,
                MetadataNotice::ExifImageField {
                    tag: tags::IMAGE_WIDTH
                },
                MetadataNotice::ExifImageField {
                    tag: tags::X_RESOLUTION
                },
                MetadataNotice::ExifImageGroup {
                    tag: tags::SUB_IFDS
                },
            ]
        );
        // The thumbnail adds no content, so a model holding only it writes nothing.
        let mut thumbnail = Exif::new(ByteOrder::LittleEndian);
        thumbnail.set_thumbnail(vec![0xff, 0xd8, 0xff, 0xd9]);
        assert!(TiffMetadata::new().with_exif(thumbnail).is_empty());
    }

    #[test]
    fn each_notice_reads_as_its_reason_and_names_its_tag() {
        assert_eq!(
            MetadataNotice::ExifThumbnail.to_string(),
            MetadataNotice::ExifThumbnail.reason()
        );
        let field = MetadataNotice::ExifImageField { tag: 282 };
        assert_eq!(field.to_string(), format!("{} (tag 282)", field.reason()));
        let group = MetadataNotice::ExifImageGroup { tag: 330 };
        assert_eq!(group.to_string(), format!("{} (tag 330)", group.reason()));
        let reasons = [
            MetadataNotice::C2paManifestStore.reason(),
            MetadataNotice::ExifThumbnail.reason(),
            field.reason(),
            group.reason(),
        ];
        for (i, a) in reasons.iter().enumerate() {
            for b in &reasons[i + 1..] {
                assert_ne!(a, b, "two notices share one reason");
            }
        }
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

    /// An `ExifIFD` as the reader returns one: an `ExposureTime` field and an
    /// `InteroperabilityIFD` group.
    fn exif_with_interop() -> (Ifd, Ifd) {
        let mut interop = Ifd::new();
        interop.set(1, Value::Ascii("R98".into()));
        let mut ifd = Ifd::new();
        ifd.set(33434, Value::Rational(vec![(1, 60)]));
        ifd.set_sub_ifd(tags::INTEROPERABILITY_IFD, vec![interop.clone()]);
        (ifd, interop)
    }

    /// The model [`exif_model`] builds from `ifd` as the only (`ExifIFD`) carrier.
    fn exif_only_model(ifd: &Ifd, order: ByteOrder) -> Exif {
        let carriers = ExifCarriers {
            exif: Some(ifd.clone()),
            ..ExifCarriers::default()
        };
        exif_model(carriers, order).unwrap()
    }

    #[test]
    fn the_exif_model_lifts_the_interoperability_group_out_of_the_directory() {
        let (ifd, interop) = exif_with_interop();
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
        let model = |carriers| exif_model(carriers, ByteOrder::LittleEndian);
        let page_only = model(ExifCarriers {
            ifd0: Some(page_fields()),
            ..ExifCarriers::default()
        })
        .unwrap();
        assert_eq!(page_only.image(), &page_fields());
        assert_eq!(page_only.exif_ifd(), None);
        assert_eq!(page_only.gps_ifd(), None);

        let gps_only = model(ExifCarriers {
            gps: Some(gps_ifd()),
            ..ExifCarriers::default()
        })
        .unwrap();
        assert_eq!(gps_only.gps_ifd(), Some(&gps_ifd()));
        assert!(gps_only.image().fields().is_empty());
        assert_eq!(gps_only.exif_ifd(), None);

        assert_eq!(model(ExifCarriers::default()), None);
    }

    #[test]
    fn exif_carriers_is_the_inverse_of_the_model() {
        let (ifd, _) = exif_with_interop();
        let carriers = ExifCarriers {
            ifd0: Some(page_fields()),
            exif: Some(ifd.clone()),
            gps: Some(gps_ifd()),
        };
        let exif = exif_model(carriers, ByteOrder::LittleEndian).unwrap();
        assert_eq!(
            exif_carriers(&exif),
            ExifCarriers {
                ifd0: Some(page_fields()),
                exif: Some(ifd),
                gps: Some(gps_ifd()),
            }
        );

        // An Interoperability sub-IFD alone still makes a directory to carry it.
        let mut interop_only = Exif::new(ByteOrder::LittleEndian);
        interop_only.set_interop_ifd(exif_with_interop().1);
        let written = exif_carriers(&interop_only).exif.unwrap();
        assert!(written.fields().is_empty());
        assert_eq!(written.sub_ifds().len(), 1);

        assert_eq!(
            exif_carriers(&Exif::new(ByteOrder::LittleEndian)),
            ExifCarriers::default()
        );
    }
}
