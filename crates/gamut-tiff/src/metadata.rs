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
//! the three normalisations a directory model implies, named on [`TiffMetadata::exif`].
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

use gamut_core::Result;
use gamut_ifd::c2pa::{self, C2paExclusions};
use gamut_ifd::{Ifd, Value, read_tree};

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
    /// The **standard** pointer tag that occurs inside this directory — `InteroperabilityIFD`
    /// (40965) — comes back as a parsed [`sub_ifds`](gamut_ifd::Ifd::sub_ifds) group rather than a
    /// raw offset, so the writer gives it a fresh offset when the directory is embedded again.
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

    /// Whether there is nothing to embed: no payload set, and no Exif sub-IFD with fields in it.
    ///
    /// An `exif` directory with no entries counts as empty — writing it would add an `ExifIFD`
    /// pointer to a directory with nothing in it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exif_ifd().is_none()
            && self.xmp.is_none()
            && self.iptc.is_none()
            && self.icc.is_none()
            && self.c2pa.is_none()
    }

    /// The Exif sub-IFD to write, or `None` when there is no Exif content worth a directory.
    fn exif_ifd(&self) -> Option<&Ifd> {
        self.exif.as_ref().filter(|ifd| !ifd.fields().is_empty())
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

/// The pointer tags [`read_metadata`] follows, scoped to what [`TiffMetadata`] actually returns.
///
/// Two tags, and the pair is a deliberate lower bound rather than a subset of convenience.
///
/// `ExifIFD` is followed because that directory **is** a field of [`TiffMetadata`]: it is handed to
/// the caller and may be written back, so a pointer under it that stayed a raw offset would be
/// re-encoded into a file laid out differently. `InteroperabilityIFD` is the one standard pointer
/// that occurs *inside* an Exif directory (EXIF 2.3 §4.6.3), and it is near-universal in camera
/// EXIF — leaving it unresolved is exactly the dangling-pointer defect this list exists to prevent.
///
/// The other two members of [`gamut_ifd::tags::STANDARD_POINTER_TAGS`] are deliberately **not**
/// here. `SubIFDs` (330) locates thumbnails and reduced-resolution subfiles and `GPSInfo` (34853)
/// locates a GPS directory; neither feeds any field of [`TiffMetadata`], and neither is re-encoded
/// by [`TiffMetadata::apply`], which writes into a directory the encoder builds fresh. Following
/// them could therefore only *add* failure modes, and it did: a single dangling `SubIFDs` offset
/// made XMP, IPTC, ICC and C2PA all unreachable on a file whose pixels decode perfectly, and two
/// pages sharing one thumbnail directory tripped the reader's cross-chain loop guard. A pointer
/// whose target this reader throws away must not be able to fail the whole call.
///
/// One over-reach remains and is harmless: `InteroperabilityIFD` is also followed if it appears at
/// IFD 0, where it does not belong. A TIFF whose IFD 0 carries tag 40965 is already out of spec,
/// and `read_tree` takes one flat list for the whole tree.
const POINTER_TAGS: &[u16] = &[tags::EXIF_IFD, tags::INTEROPERABILITY_IFD];

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
    let file = read_tree(data, POINTER_TAGS)?;
    // §A.3.6: one store for the whole asset, in the last IFD of the main chain. `ifds` is that
    // chain, so its last element is where the entry belongs — and a single-page file makes the
    // two the same directory. A file with no IFD at all carries no metadata.
    let (Some(ifd0), Some(store_ifd)) = (file.ifds.first(), file.ifds.last()) else {
        return Ok(TiffMetadata::new());
    };
    let located = c2pa::locate(data)?.is_some();
    let c2pa = match store_ifd.get(tags::C2PA_MANIFEST_STORE) {
        Some(Value::Undefined(store)) if located => Some(store.clone()),
        _ => None,
    };
    Ok(TiffMetadata {
        exif: ifd0
            .sub_ifds()
            .iter()
            .find(|group| group.tag == tags::EXIF_IFD)
            .and_then(|group| group.ifds.first())
            .cloned(),
        xmp: bytes_value(ifd0.get(tags::XMP)),
        iptc: bytes_value(ifd0.get(tags::IPTC_NAA)),
        icc: bytes_value(ifd0.get(tags::ICC_PROFILE)),
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
        write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd0],
        })
        .expect("write")
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
