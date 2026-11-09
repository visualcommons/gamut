//! Optional metadata embedded in a DNG: an EXIF sub-IFD plus XMP / IPTC / ICC blocks.
//!
//! DNG's `ExifIFD` (34665) *is* an EXIF sub-IFD, so it is carried as the shared [`Ifd`]
//! directory model — the same shape `gamut-tiff`'s `TiffMetadata` carries its own in, and the
//! very directory the facade's `Exif` model holds as its Exif sub-IFD. XMP (700), IPTC-IIM
//! (33723) and ICC (34675) are single opaque payloads in the file, carried verbatim as bytes.
//! With the `metadata` feature, [`DngMetadata`] implements the facade's `TypedMetadata` and
//! [`DngEncoder`](crate::DngEncoder) its `MetadataSink`; without it, nothing here depends on the
//! facade.
//!
//! The C2PA manifest store is not a `DngMetadata` carrier: it has a placement rule of its own
//! (C2PA 2.4 §A.3.6: the last main IFD, at the end of the file) and a signer's exclusion contract
//! (§18.5.5), so it is set on the encoder
//! ([`DngEncoder::with_c2pa`](crate::DngEncoder::with_c2pa)) and read beside its exclusion ranges
//! ([`DecodedDng::c2pa`](crate::DecodedDng::c2pa)).

use gamut_core::{Error, Result};
use gamut_ifd::{ByteOrder, Ifd, Value};

use crate::tags;

/// The `ExifVersion` written when the supplied EXIF sub-IFD does not carry one: EXIF 2.3.
///
/// `ExifVersion` (36864) is mandatory in an EXIF IFD, so the encoder supplies it rather than
/// emit a directory a conforming reader may reject.
const DEFAULT_EXIF_VERSION: &[u8; 4] = b"0230";

/// Metadata to embed in a DNG, or read back from one: an EXIF sub-IFD and/or opaque XMP /
/// IPTC-IIM / ICC payloads.
///
/// `#[non_exhaustive]`, so a later carrier is an additive change: build one from
/// [`DngMetadata::new`] and the `with_*` builders, or assign the public fields of a value you
/// already hold.
///
/// ```
/// use gamut_dng::{DngMetadata, Ifd, Value};
///
/// let mut exif = Ifd::new();
/// exif.set(33437, Value::Rational(vec![(28, 10)])); // FNumber
/// exif.set(34855, Value::Short(vec![400])); // PhotographicSensitivity
///
/// let meta = DngMetadata::new()
///     .with_exif(exif)
///     .with_xmp(b"<x:xmpmeta/>".to_vec());
/// assert_eq!(meta.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct DngMetadata {
    /// The Exif private sub-IFD (`ExifIFD`, 34665), as the shared directory model — the
    /// directory the facade's `Exif` model holds as its Exif sub-IFD.
    ///
    /// Only this one EXIF directory crosses into a DNG: the EXIF 0th IFD, GPS sub-IFD and
    /// thumbnail describe directories the DNG container builds itself (IFD 0, and previews as
    /// `SubIFDs` entries), so a DNG's own IFD-0 fields reach the caller through
    /// [`DecodedDng`](crate::DecodedDng) instead. Its standard pointer fields (a camera's
    /// `InteroperabilityIFD`) are sub-IFD groups, never raw offsets.
    pub exif: Option<Ifd>,
    /// The byte order of the file [`exif`](Self::exif) was read from: `Some` exactly when the
    /// decoder returned an Exif directory, and `None` on a value built by hand.
    ///
    /// **Read-side provenance only.** An `ExifIFD` has no byte order of its own — it is written in
    /// the file's, which [`DngEncoder::with_byte_order`](crate::DngEncoder::with_byte_order)
    /// chooses — so the encoder ignores this field. It exists so the typed EXIF model the
    /// `metadata` feature builds states the order the directory was actually stored in.
    pub exif_byte_order: Option<ByteOrder>,
    /// An XMP packet (UTF-8 RDF/XML), stored in the `XMP` tag (700), verbatim.
    pub xmp: Option<Vec<u8>>,
    /// A legacy IPTC-IIM dataset stream, stored in the `IPTC/NAA` tag (33723), verbatim.
    ///
    /// Kept as its own carrier rather than folded into [`xmp`](Self::xmp): IIM is a genuinely
    /// separate serialization that DNG files really do hold, and reconciling it into an XMP graph
    /// is a policy decision that belongs to the caller, not to the container.
    pub iptc: Option<Vec<u8>>,
    /// An ICC profile, stored in the `ICCProfile` tag (34675), verbatim.
    pub icc: Option<Vec<u8>>,
}

impl DngMetadata {
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

    /// The EXIF sub-IFD to write, or `None` when there is no EXIF content worth a directory.
    fn exif_ifd(&self) -> Option<&Ifd> {
        self.exif.as_ref().filter(|ifd| !ifd.fields().is_empty())
    }

    /// Whether there is nothing to embed.
    pub(crate) fn is_empty(&self) -> bool {
        self.exif_ifd().is_none() && self.xmp.is_none() && self.iptc.is_none() && self.icc.is_none()
    }

    /// Refuses two shapes of EXIF sub-IFD this crate would otherwise write into a file its own
    /// [`DngRewrite::open`](crate::DngRewrite::open) then rejects or reads back changed: a
    /// pointer-typed field under a standard pointer tag, and a tag stored twice.
    ///
    /// The Exif sub-IFD is the one directory a caller supplies, so it is the only one checked:
    /// the blocks [`apply`](Self::apply) writes into IFD 0 sit under `XMP`, `IPTC_NAA` and
    /// `ICC_PROFILE`, none of which any reader treats as a pointer, and the rest of the file is
    /// the encoder's own. [`check_exif_subtree`] states the two refusals, and what it does **not**
    /// refuse.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) errorif the Exif sub-IFD, or any directory nested in it,
    /// carries a pointer-typed field under a standard pointer tag, or stores one tag twice (a
    /// field and a sub-IFD group, or two groups).
    pub(crate) fn check(&self) -> Result<()> {
        self.exif_ifd().map_or(Ok(()), check_exif_subtree)
    }

    /// Writes the XMP / IPTC / ICC blocks into `ifd0` and returns the EXIF sub-IFD, if any.
    ///
    /// The C2PA store is deliberately **not** written here: its value must land at the end of
    /// the file (C2PA 2.4 §A.3.6), after the image data, which only the encoder can arrange once
    /// the rest of the file exists ([`gamut_ifd::append_c2pa_store`]).
    pub(crate) fn apply(&self, ifd0: &mut Ifd) -> Option<Ifd> {
        if let Some(xmp) = &self.xmp {
            ifd0.set(tags::XMP, Value::Byte(xmp.clone()));
        }
        if let Some(iptc) = &self.iptc {
            ifd0.set(tags::IPTC_NAA, Value::Byte(iptc.clone()));
        }
        if let Some(icc) = &self.icc {
            ifd0.set(tags::ICC_PROFILE, Value::Undefined(icc.clone()));
        }
        let mut exif = self.exif_ifd()?.clone();
        if exif.get(tags::EXIF_VERSION).is_none() {
            exif.set(
                tags::EXIF_VERSION,
                Value::Undefined(DEFAULT_EXIF_VERSION.to_vec()),
            );
        }
        Some(exif)
    }
}

/// The facade wiring behind the `metadata` feature: [`DngMetadata`]'s `TypedMetadata` impl, and
/// the EXIF carrier [`DngEncoder`](crate::DngEncoder)'s `MetadataSink` impl writes.
#[cfg(feature = "metadata")]
pub(crate) mod typed {
    use gamut_core::Result;
    use gamut_ifd::tags::INTEROPERABILITY_IFD;
    use gamut_ifd::{ByteOrder, Ifd};
    use gamut_metadata::exif::Exif;
    use gamut_metadata::{Metadata, MetadataBlock, TypedMetadata};

    use super::DngMetadata;

    /// The `ExifIFD` directory a DNG writes for the bare TIFF stream `tiff` (the
    /// `MetadataSink::write_exif` payload), or `None` when the stream holds no Exif sub-IFD
    /// content.
    ///
    /// The model's Interoperability sub-IFD is attached to the directory as the
    /// `InteroperabilityIFD` group the decoder hands back. Its 0th IFD, GPS sub-IFD and thumbnail
    /// are not carried: the DNG container builds those directories itself (see
    /// [`DngMetadata::exif`]).
    ///
    /// # Errors
    ///
    /// The [`ExifError`](gamut_metadata::exif::ExifError), converted, when `tiff` is not a
    /// readable EXIF TIFF stream.
    pub(crate) fn exif_directory(tiff: &[u8]) -> Result<Option<Ifd>> {
        let exif = Exif::parse(tiff)?;
        Ok(match (exif.exif_ifd(), exif.interop_ifd()) {
            (None, None) => None,
            (directory, interop) => {
                let mut directory = directory.cloned().unwrap_or_default();
                if let Some(interop) = interop {
                    directory.set_sub_ifd(INTEROPERABILITY_IFD, vec![interop.clone()]);
                }
                Some(directory)
            }
        })
    }

    /// The facade's [`TypedMetadata`] over the four carriers. The EXIF carrier is a parsed
    /// directory rather than bytes, so [`blocks`](TypedMetadata::blocks) hands over only the
    /// three byte carriers and [`metadata`](TypedMetadata::metadata) assigns the EXIF model
    /// straight onto [`Metadata::exif`] rather than re-serialising it to bytes and parsing it
    /// back.
    ///
    /// ```
    /// use gamut_dng::{DngMetadata, Ifd, TypedMetadata, Value};
    ///
    /// let mut exif = Ifd::new();
    /// exif.set(34855, Value::Short(vec![400])); // PhotographicSensitivity
    /// let meta = DngMetadata::new().with_exif(exif.clone());
    ///
    /// assert!(meta.blocks()?.is_empty()); // EXIF is a directory here, not a byte block...
    /// let model = meta.metadata()?.exif.expect("an EXIF model");
    /// assert_eq!(model.exif_ifd(), Some(&exif)); // ...and reaches the unified model as one.
    /// # Ok::<(), gamut_metadata::MetadataError>(())
    /// ```
    impl TypedMetadata for DngMetadata {
        /// The byte-carried blocks — XMP, IPTC-IIM and ICC — as [`MetadataBlock`]s.
        /// [`exif`](DngMetadata::exif) is deliberately absent; see the impl's docs.
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
            Ok(blocks)
        }

        /// The unified model: [`Metadata::parse_blocks`] over
        /// [`blocks`](TypedMetadata::blocks) (the IPTC-IIM block reconciled into the XMP graph
        /// under the default [`ConflictPolicy`](gamut_metadata::iptc::ConflictPolicy)), with
        /// [`exif`](DngMetadata::exif) assigned onto [`Metadata::exif`] as the Exif sub-IFD of a
        /// model in [`exif_byte_order`](DngMetadata::exif_byte_order) — or little-endian,
        /// [`DngEncoder`](crate::DngEncoder)'s default, on a value built by hand.
        ///
        /// # Errors
        ///
        /// The [`MetadataError`](gamut_metadata::MetadataError) naming the byte carrier whose
        /// parse failed.
        fn metadata(&self) -> gamut_metadata::Result<Metadata> {
            let mut unified = Metadata::parse_blocks(&self.blocks()?)?;
            unified.exif = self.exif.as_ref().map(|directory| {
                let mut exif =
                    Exif::new(self.exif_byte_order.unwrap_or(ByteOrder::LittleEndian));
                exif.set_exif_ifd(directory.clone());
                exif
            });
            Ok(unified)
        }
    }
}

/// The pointer tags [`DngRewrite::open`](crate::DngRewrite::open) resolves inside the Exif
/// subtree: every standard one, because it hands [`gamut_ifd::IfdReader::read_tree`] the flat
/// [`STANDARD_POINTER_TAGS`](gamut_ifd::tags::STANDARD_POINTER_TAGS) list, which that reader
/// resolves **at every node** — the Exif directory and anything nested in it included.
const EXIF_SUBTREE_POINTER_TAGS: &[u16] = gamut_ifd::tags::STANDARD_POINTER_TAGS;

/// The on-disk field-type codes that make a directory entry a sub-IFD pointer to the reader:
/// `LONG` (4), the typed `IFD` (13) of TIFF Technical Note 1, and BigTIFF's `LONG8` (16) /
/// `IFD8` (18) — the set [`gamut_ifd::IfdReader::read_tree`] follows.
///
/// Stated as **codes** rather than as [`Value`] variants, because the reader classifies an entry
/// by the code it parses, and the code is the one discriminator that survives the write/read
/// boundary ([`Value::type_code`], total over every variant). A `Value::Unknown` once carried an
/// arbitrary code beside its word, so one built at 4, 13, 16 or 18 was a plain field to a
/// variant-shaped predicate and a **pointer** to the reader; [`gamut_ifd::UnknownValue::new`]
/// now refuses every recognised code, so variant and code agree for every value a caller can
/// build, and asking the code stays correct either way.
const POINTER_TYPE_CODES: &[u16] = &[4, 13, 16, 18];

/// The refusal earned by a field under a tag in [`EXIF_SUBTREE_POINTER_TAGS`] whose on-disk type
/// code is in [`POINTER_TYPE_CODES`]. Named so its prose enumeration of the tag set, which is a
/// sibling crate's constant, is pinned in one assertion rather than drifting.
const POINTER_FIELD_REFUSAL: &str = "DNG: an Exif sub-IFD may not carry a plain field under a \
     standard pointer tag (SubIFDs, ExifIFD, GPSInfo, InteroperabilityIFD) with a pointer's own \
     type (LONG, IFD, LONG8 or IFD8), which the reader would follow as a file offset";

/// The refusal earned by a tag the directory stores **twice**: a plain field and a sub-IFD group,
/// or two sub-IFD groups.
///
/// [`Ifd`] holds fields and groups in two lists. Its setters keep a tag in one of them, but
/// [`Ifd::sub_ifds_mut`] hands out each group's `tag` to edit in place, so one tag can still sit in
/// both lists, or head two groups. The writer then emits **two entries under that tag**, which
/// TIFF 6.0 §2 — on which DNG 1.7.1 builds — does not allow, and every reader keeps exactly one
/// of them: [`gamut_ifd`] the last, libtiff the first (`tif_dirread.c`, "Mark duplicates of any
/// tag to be ignored"). Which entry survives is a property of the reader, and what the caller set
/// is lost to at least one of them.
const TAG_STORED_TWICE_REFUSAL: &str = "DNG: an Exif sub-IFD may not store one tag twice (a plain \
     field and a sub-IFD group, or two sub-IFD groups), which would be written as two entries \
     under that tag (TIFF 6.0 §2 allows one) and read back as only one of them";

/// The writer's side of what [`DngRewrite::open`](crate::DngRewrite::open) reads: refuses two
/// shapes of Exif subtree this crate could not hand back unchanged.
///
/// Two refusals, distinct because they are two different mistakes:
///
/// * a **field** under a tag in [`EXIF_SUBTREE_POINTER_TAGS`] whose on-disk type code is in
///   [`POINTER_TYPE_CODES`] — the reader follows it as an offset into a file it never came from,
///   so the open fails (`sub-IFD pointer loop`, `value offset out of bounds`) or parses a
///   directory nobody wrote. Every other type is left in place by the reader, so a `SHORT` under
///   `InteroperabilityIFD` is left alone here too;
/// * a **tag stored twice** — a group under a tag that also carries a field or heads an earlier
///   group ([`TAG_STORED_TWICE_REFUSAL`]).
///
/// Both apply at every level, because the reader resolves the flat tag list at every node: a
/// group's children are checked as the Exif directory is.
///
/// Not every subtree the reader would reject or change is refused: a sub-IFD group under a tag
/// outside [`EXIF_SUBTREE_POINTER_TAGS`] (read back as a plain `LONG` field, not a group) and
/// nesting deeper than the reader walks ([`gamut_ifd::SubtreeScope::MAX_DEPTH`]) both pass this
/// check. `gamut-tiff`'s check refuses those two as well; they are not adopted here, and neither
/// is a shape a decode → encode round trip produces: [`DngDecoder`](crate::DngDecoder) resolves
/// only the standard pointer tags, and only one level below the Exif directory.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) errorfor either refusal, at the first offender.
fn check_exif_subtree(ifd: &Ifd) -> Result<()> {
    for &tag in EXIF_SUBTREE_POINTER_TAGS {
        if ifd
            .get(tag)
            .is_some_and(|value| POINTER_TYPE_CODES.contains(&value.type_code()))
        {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                POINTER_FIELD_REFUSAL,
            ));
        }
    }
    let groups = ifd.sub_ifds();
    for (i, group) in groups.iter().enumerate() {
        if ifd.get(group.tag).is_some() || groups[..i].iter().any(|g| g.tag == group.tag) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                TAG_STORED_TWICE_REFUSAL,
            ));
        }
        for child in &group.ifds {
            check_exif_subtree(child)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use gamut_ifd::tags::INTEROPERABILITY_IFD;
    use gamut_ifd::{ByteOrder, Variant};
    use gamut_metadata::exif::ExifTag;

    use super::*;

    /// An [`Exif`] whose Exif sub-IFD carries `tag`.
    fn exif_with(tag: ExifTag, value: Value) -> Exif {
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set_tag(tag, value);
        exif
    }

    #[test]
    fn empty_metadata_writes_nothing() {
        let mut ifd = Ifd::new();
        let meta = DngMetadata::default();
        assert!(meta.is_empty());
        assert!(meta.apply(&mut ifd).is_none());
        assert!(ifd.fields().is_empty());
        assert!(meta.blocks().unwrap().is_empty());
    }

    #[test]
    fn an_exif_model_with_no_sub_ifd_content_is_empty() {
        // A bare model, and one whose *0th* IFD is populated, both leave the DNG's `ExifIFD`
        // slot with nothing to write — only the Exif sub-IFD crosses into the file.
        let mut image_only = Exif::new(ByteOrder::LittleEndian);
        image_only
            .image_mut()
            .set(271, Value::Ascii("gamut".into()));
        for exif in [Exif::new(ByteOrder::LittleEndian), image_only] {
            let meta = DngMetadata {
                exif: Some(exif),
                ..Default::default()
            };
            assert!(meta.is_empty());
            assert!(meta.apply(&mut Ifd::new()).is_none());
        }
    }

    #[test]
    fn applies_blocks_and_carries_the_exif_sub_ifd() {
        let mut ifd = Ifd::new();
        let mut exif = exif_with(ExifTag::PhotographicSensitivity, Value::Short(vec![400]));
        exif.set_tag(ExifTag::ExposureTime, Value::Rational(vec![(1, 250)]));
        let meta = DngMetadata {
            exif: Some(exif),
            xmp: Some(b"<x:xmpmeta/>".to_vec()),
            icc: Some(vec![0u8; 8]),
            c2pa: Some(vec![0u8; 16]),
            ..Default::default()
        };
        assert!(!meta.is_empty());
        let written = meta.apply(&mut ifd).expect("exif IFD");
        assert_eq!(
            ifd.get(tags::XMP),
            Some(&Value::Byte(b"<x:xmpmeta/>".to_vec()))
        );
        assert!(ifd.get(tags::ICC_PROFILE).is_some());
        // The store is the encoder's to place, so `apply` leaves the directory without it.
        assert!(ifd.get(gamut_ifd::C2PA_MANIFEST_STORE).is_none());
        assert_eq!(
            written.get(tags::ISO_SPEED_RATINGS),
            Some(&Value::Short(vec![400]))
        );
        assert_eq!(
            written.get(tags::EXPOSURE_TIME),
            Some(&Value::Rational(vec![(1, 250)]))
        );
        // Every field of the supplied directory survives, and the mandatory version is supplied.
        assert_eq!(written.fields().len(), 3);
        assert_eq!(
            written.get(tags::EXIF_VERSION),
            Some(&Value::Undefined(DEFAULT_EXIF_VERSION.to_vec()))
        );
    }

    #[test]
    fn a_supplied_exif_version_is_not_overwritten() {
        let meta = DngMetadata {
            exif: Some(exif_with(
                ExifTag::ExifVersion,
                Value::Undefined(b"0300".to_vec()),
            )),
            ..Default::default()
        };
        let written = meta.apply(&mut Ifd::new()).expect("exif IFD");
        assert_eq!(
            written.get(tags::EXIF_VERSION),
            Some(&Value::Undefined(b"0300".to_vec()))
        );
    }

    #[test]
    fn dng_is_empty_only_when_every_block_is_unset() {
        assert!(DngMetadata::default().is_empty());
        let singles = [
            DngMetadata {
                exif: Some(exif_with(
                    ExifTag::PhotographicSensitivity,
                    Value::Short(vec![100]),
                )),
                ..Default::default()
            },
            DngMetadata {
                xmp: Some(vec![1]),
                ..Default::default()
            },
            DngMetadata {
                iptc: Some(vec![1]),
                ..Default::default()
            },
            DngMetadata {
                icc: Some(vec![1]),
                ..Default::default()
            },
            DngMetadata {
                c2pa: Some(vec![1]),
                ..Default::default()
            },
        ];
        for (i, meta) in singles.iter().enumerate() {
            assert!(!meta.is_empty(), "block {i} alone must be non-empty");
        }
    }

    #[test]
    fn blocks_expose_every_byte_carrier_in_facade_form() {
        let meta = DngMetadata {
            exif: Some(exif_with(
                ExifTag::PhotographicSensitivity,
                Value::Short(vec![100]),
            )),
            xmp: Some(b"<x:xmpmeta/>".to_vec()),
            iptc: Some(vec![0x1c, 0x02, 0x05]),
            icc: Some(vec![7u8; 4]),
            c2pa: Some(b"\0\0\0\x14jumbc2pa".to_vec()),
        };
        assert_eq!(
            meta.blocks().unwrap(),
            vec![
                MetadataBlock::Xmp(b"<x:xmpmeta/>"),
                MetadataBlock::IptcIim(&[0x1c, 0x02, 0x05]),
                MetadataBlock::Icc(&[7u8; 4]),
                MetadataBlock::C2pa(b"\0\0\0\x14jumbc2pa"),
            ]
        );
    }

    #[test]
    fn metadata_assigns_the_exif_model_beside_the_parsed_byte_carriers() {
        // The EXIF model is assigned, not re-parsed; the C2PA store is the byte carrier whose
        // extraction is the identity, so the whole unified model is predictable.
        let exif = exif_with(ExifTag::PhotographicSensitivity, Value::Short(vec![100]));
        let meta = DngMetadata {
            exif: Some(exif.clone()),
            c2pa: Some(b"\0\0\0\x14jumbc2pa".to_vec()),
            ..Default::default()
        };
        let unified = meta.metadata().unwrap();
        assert_eq!(unified.exif, Some(exif));
        assert_eq!(unified.c2pa.as_deref(), Some(&b"\0\0\0\x14jumbc2pa"[..]));
    }

    /// An Exif sub-IFD carrying one ordinary field, so [`DngMetadata::check`] has a directory to
    /// judge, plus whatever a test adds.
    fn exif_dir() -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(tags::ISO_SPEED_RATINGS, Value::Short(vec![100]));
        ifd
    }

    /// The metadata carrying `exif` as its Exif sub-IFD.
    fn carrying(exif: Ifd) -> DngMetadata {
        let mut model = Exif::new(ByteOrder::LittleEndian);
        model.set_exif_ifd(exif);
        DngMetadata {
            exif: Some(model),
            ..Default::default()
        }
    }

    /// The message [`DngMetadata::check`] refused `meta` with.
    fn refusal(meta: &DngMetadata) -> String {
        meta.check().expect_err("must be refused").to_string()
    }

    /// `ifd` with a group under `tag` whose one child is `child`, attached through the setter.
    fn with_group(mut ifd: Ifd, tag: u16, child: Ifd) -> Ifd {
        ifd.set_sub_ifd(tag, vec![child]);
        ifd
    }

    /// `ifd` with its group at `index` moved under `tag` in place — the one route
    /// [`Ifd::sub_ifds_mut`] leaves to a tag stored twice, since the setters forbid it.
    fn retagged(mut ifd: Ifd, index: usize, tag: u16) -> Ifd {
        ifd.sub_ifds_mut()[index].tag = tag;
        ifd
    }

    /// Every value whose on-disk type code makes the reader follow it as a pointer: the four
    /// natural variants. `Value::Unknown` cannot carry any of the four codes, because
    /// [`gamut_ifd::UnknownValue::new`] refuses a recognised one (#608).
    fn pointer_typed_values() -> Vec<Value> {
        vec![
            Value::Long(vec![8]),
            Value::Ifd(vec![8]),
            Value::Long8(vec![8]),
            Value::Ifd8(vec![8]),
        ]
    }

    #[test]
    fn check_refuses_a_pointer_typed_field_under_every_standard_pointer_tag() {
        // The reader resolves the flat standard list at every node, so each of the four tags is a
        // pointer inside the Exif directory, and the type is judged by on-disk code.
        for &tag in EXIF_SUBTREE_POINTER_TAGS {
            for value in pointer_typed_values() {
                let mut exif = exif_dir();
                exif.set(tag, value.clone());
                assert!(
                    refusal(&carrying(exif)).contains(POINTER_FIELD_REFUSAL),
                    "{tag} = {value:?}"
                );
            }
        }
    }

    #[test]
    fn check_accepts_a_field_of_any_other_type_under_a_pointer_tag() {
        // The reader leaves every non-pointer type in place as a field, so the writer must too:
        // a `SHORT` under `InteroperabilityIFD`, and an unknown-typed entry at 14, the unclaimed
        // code adjacent to the pointer code 13.
        for &tag in EXIF_SUBTREE_POINTER_TAGS {
            for value in [
                Value::Short(vec![8]),
                Value::Unknown(
                    gamut_ifd::UnknownValue::new(
                        14,
                        1,
                        &[8, 0, 0, 0],
                        ByteOrder::LittleEndian,
                        Variant::Classic,
                    )
                    .expect("a classic word"),
                ),
            ] {
                let mut exif = exif_dir();
                exif.set(tag, value.clone());
                assert!(carrying(exif).check().is_ok(), "{tag} = {value:?}");
            }
        }
    }

    #[test]
    fn check_accepts_a_pointer_group_with_no_field_beside_it() {
        let exif = with_group(exif_dir(), INTEROPERABILITY_IFD, exif_dir());
        assert!(carrying(exif).check().is_ok());
    }

    #[test]
    fn check_refuses_a_group_under_a_tag_that_also_carries_a_field() {
        // The issue #610 shape: `InteroperabilityIFD` as a `SHORT` field *and* as a group. The
        // field is not pointer-typed, so only the stored-twice clause can refuse it.
        let mut exif = with_group(exif_dir(), u16::MAX, exif_dir());
        exif.set(INTEROPERABILITY_IFD, Value::Short(vec![1]));
        let exif = retagged(exif, 0, INTEROPERABILITY_IFD);
        assert!(refusal(&carrying(exif)).contains(TAG_STORED_TWICE_REFUSAL));
    }

    #[test]
    fn check_refuses_two_groups_under_one_tag() {
        let exif = with_group(exif_dir(), tags::GPS_INFO, exif_dir());
        let exif = with_group(exif, INTEROPERABILITY_IFD, exif_dir());
        let exif = retagged(exif, 1, tags::GPS_INFO);
        assert!(refusal(&carrying(exif)).contains(TAG_STORED_TWICE_REFUSAL));
    }

    #[test]
    fn check_judges_every_directory_nested_in_the_exif_directory() {
        // The reader resolves pointers at every node, so a child two levels down is read exactly
        // as the Exif directory is: each clause must reach it.
        let mut pointer_field = exif_dir();
        pointer_field.set(tags::GPS_INFO, Value::Long(vec![8]));
        let mut stored_twice = with_group(exif_dir(), u16::MAX, exif_dir());
        stored_twice.set(tags::GPS_INFO, Value::Short(vec![1]));
        let stored_twice = retagged(stored_twice, 0, tags::GPS_INFO);
        for (leaf, message) in [
            (pointer_field, POINTER_FIELD_REFUSAL),
            (stored_twice, TAG_STORED_TWICE_REFUSAL),
        ] {
            let middle = with_group(exif_dir(), tags::GPS_INFO, leaf);
            let exif = with_group(exif_dir(), INTEROPERABILITY_IFD, middle);
            assert!(refusal(&carrying(exif)).contains(message), "{message}");
        }
    }

    #[test]
    fn the_refused_pointer_tags_are_the_four_the_messages_name() {
        // The set is a sibling crate's constant while the refusal and the encoder's documentation
        // enumerate its four members; a fifth added upstream must not widen this crate's contract
        // without someone deciding it.
        assert_eq!(EXIF_SUBTREE_POINTER_TAGS, [330, 34665, 34853, 40965]);
        for name in ["SubIFDs", "ExifIFD", "GPSInfo", "InteroperabilityIFD"] {
            assert!(POINTER_FIELD_REFUSAL.contains(name), "{name}");
        }
    }
}
