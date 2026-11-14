//! Optional metadata embedded in a DNG: an EXIF sub-IFD plus XMP / IPTC / ICC blocks.
//!
//! DNG's `ExifIFD` (34665) *is* an EXIF sub-IFD, so EXIF is carried as the shared [`Exif`] model
//! — the type `gamut-tiff`'s `TiffMetadata::exif` and the facade's `Metadata::exif` hold — in
//! every build, through a normal `gamut-exif` dependency rather than the `metadata` feature.
//! XMP (700), IPTC-IIM (33723) and ICC (34675) are single opaque payloads in the file, carried
//! verbatim as bytes.
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
use gamut_exif::Exif;
use gamut_ifd::tags::INTEROPERABILITY_IFD;
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
/// use gamut_dng::{ByteOrder, DngMetadata, Exif, ExifTag, Value};
///
/// let mut exif = Exif::new(ByteOrder::LittleEndian);
/// exif.set_tag(ExifTag::FNumber, Value::Rational(vec![(28, 10)]));
/// exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![400]));
///
/// let meta = DngMetadata::new()
///     .with_exif(exif)
///     .with_xmp(b"<x:xmpmeta/>".to_vec());
/// assert_eq!(meta.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct DngMetadata {
    /// EXIF capture settings, as the shared [`Exif`] model — the type `gamut-tiff`'s
    /// `TiffMetadata::exif` and the facade's `Metadata::exif` hold.
    ///
    /// Only the model's **Exif sub-IFD** ([`Exif::exif_ifd`]) crosses into the file, as the DNG's
    /// `ExifIFD` (34665), with the model's **Interoperability sub-IFD** ([`Exif::interop_ifd`])
    /// attached to it as its `InteroperabilityIFD` (40965) group. The model's 0th IFD, GPS sub-IFD
    /// and thumbnail describe directories the DNG container builds itself (IFD 0, and previews as
    /// `SubIFDs` entries), so the encoder does not write them and the decoder does not populate
    /// them; a DNG's own IFD-0 fields reach the caller through
    /// [`DecodedDng`](crate::DecodedDng) instead.
    ///
    /// On read the model is in the file's byte order, its Exif sub-IFD's standard pointer fields
    /// resolved into sub-IFD groups (never raw offsets) and a single `InteroperabilityIFD` group
    /// lifted out into the model's Interoperability sub-IFD. On write its byte order does not
    /// travel — the file's is [`DngEncoder::with_byte_order`](crate::DngEncoder::with_byte_order)'s.
    pub exif: Option<Exif>,
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

    /// Returns a copy carrying `exif` as the file's EXIF (see [`exif`](Self::exif) for what of
    /// the model a DNG holds).
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

    /// The `ExifIFD` directory to write — the model's Exif sub-IFD with its Interoperability
    /// sub-IFD attached as an `InteroperabilityIFD` group — or `None` when the Exif sub-IFD holds
    /// no field worth a directory.
    fn exif_ifd(&self) -> Option<Ifd> {
        let exif = self.exif.as_ref()?;
        let mut directory = exif
            .exif_ifd()
            .filter(|ifd| !ifd.fields().is_empty())?
            .clone();
        if let Some(interop) = exif.interop_ifd() {
            directory.set_sub_ifd(INTEROPERABILITY_IFD, vec![interop.clone()]);
        }
        Some(directory)
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
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the Exif sub-IFD, or any directory nested in it,
    /// carries a pointer-typed field under a standard pointer tag, or stores one tag twice (a
    /// field and a sub-IFD group, or two groups).
    pub(crate) fn check(&self) -> Result<()> {
        self.exif_ifd()
            .map_or(Ok(()), |exif| check_exif_subtree(&exif))
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
        let mut exif = self.exif_ifd()?;
        if exif.get(tags::EXIF_VERSION).is_none() {
            exif.set(
                tags::EXIF_VERSION,
                Value::Undefined(DEFAULT_EXIF_VERSION.to_vec()),
            );
        }
        Some(exif)
    }
}

/// The EXIF model of a DNG's `ExifIFD` `directory`, read in `order`: the directory as the
/// model's Exif sub-IFD, a single `InteroperabilityIFD` group lifted out of it into the model's
/// own Interoperability sub-IFD — the inverse of [`DngMetadata::exif_ifd`].
pub(crate) fn exif_model(directory: Ifd, order: ByteOrder) -> Exif {
    let mut exif = Exif::new(order);
    let mut sub_ifd = Ifd::new();
    for field in directory.fields() {
        sub_ifd.set(field.tag, field.value.clone());
    }
    for group in directory.sub_ifds() {
        match (group.tag, group.ifds.as_slice()) {
            (INTEROPERABILITY_IFD, [interop]) => exif.set_interop_ifd(interop.clone()),
            _ => sub_ifd.set_sub_ifd(group.tag, group.ifds.clone()),
        }
    }
    exif.set_exif_ifd(sub_ifd);
    exif
}

/// The facade wiring behind the `metadata` feature: [`DngMetadata`]'s `TypedMetadata` impl.
#[cfg(feature = "metadata")]
mod typed {
    use gamut_metadata::{Metadata, MetadataBlock, TypedMetadata};

    use super::DngMetadata;

    /// The facade's [`TypedMetadata`] over the four carriers. The EXIF carrier is already the
    /// facade's typed model, so [`blocks`](TypedMetadata::blocks) hands over only the three byte
    /// carriers and [`metadata`](TypedMetadata::metadata) assigns the EXIF model straight onto
    /// [`Metadata::exif`] rather than re-serialising it to bytes and parsing it back.
    ///
    /// ```
    /// use gamut_dng::{ByteOrder, DngMetadata, Exif, ExifTag, TypedMetadata, Value};
    ///
    /// let mut exif = Exif::new(ByteOrder::LittleEndian);
    /// exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![400]));
    /// let meta = DngMetadata::new().with_exif(exif.clone());
    ///
    /// assert!(meta.blocks()?.is_empty()); // EXIF is a model here, not a byte block...
    /// assert_eq!(meta.metadata()?.exif, Some(exif)); // ...and reaches the unified model as one.
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
        /// [`exif`](DngMetadata::exif) assigned onto [`Metadata::exif`] as it is.
        ///
        /// # Errors
        ///
        /// The [`MetadataError`](gamut_metadata::MetadataError) naming the byte carrier whose
        /// parse failed.
        fn metadata(&self) -> gamut_metadata::Result<Metadata> {
            let mut unified = Metadata::parse_blocks(&self.blocks()?)?;
            unified.exif.clone_from(&self.exif);
            Ok(unified)
        }
    }

    #[cfg(test)]
    mod tests {
        use gamut_exif::Exif;
        use gamut_ifd::{ByteOrder, Value};

        use super::*;
        use crate::tags;

        /// An EXIF model in `order` whose Exif sub-IFD carries one ordinary field.
        fn iso_in(order: ByteOrder, speed: u16) -> Exif {
            let mut exif = Exif::new(order);
            exif.exif_ifd_mut()
                .set(tags::ISO_SPEED_RATINGS, Value::Short(vec![speed]));
            exif
        }

        /// [`iso_in`] little-endian.
        fn iso(speed: u16) -> Exif {
            iso_in(ByteOrder::LittleEndian, speed)
        }

        #[test]
        fn blocks_expose_the_three_byte_carriers_in_order() {
            let meta = DngMetadata::new()
                .with_exif(iso(100))
                .with_icc(vec![7u8; 4])
                .with_iptc(vec![0x1c, 0x02, 0x05])
                .with_xmp(b"<x:xmpmeta/>".to_vec());
            assert_eq!(
                meta.blocks().unwrap(),
                vec![
                    MetadataBlock::Xmp(b"<x:xmpmeta/>"),
                    MetadataBlock::IptcIim(&[0x1c, 0x02, 0x05]),
                    MetadataBlock::Icc(&[7u8; 4]),
                ]
            );
            assert!(DngMetadata::new().blocks().unwrap().is_empty());
        }

        #[test]
        fn metadata_assigns_the_exif_model_unchanged() {
            for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
                let meta = DngMetadata::new().with_exif(iso_in(order, 100));
                assert_eq!(meta.metadata().unwrap().exif, Some(iso_in(order, 100)));
            }
            assert_eq!(DngMetadata::new().metadata().unwrap().exif, None);
        }

        #[test]
        fn metadata_keeps_the_parsed_byte_carriers_beside_the_assigned_exif() {
            let packet: &[u8] = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
                <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
                <rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" \
                xmp:CreatorTool=\"gamut\"/></rdf:RDF></x:xmpmeta>";
            let parsed = Metadata::parse_blocks(&[MetadataBlock::Xmp(packet)]).unwrap();
            assert!(parsed.xmp.is_some());
            let unified = DngMetadata::new()
                .with_exif(iso(100))
                .with_xmp(packet.to_vec())
                .metadata()
                .unwrap();
            assert_eq!(unified.xmp, parsed.xmp);
            assert_eq!(unified.exif, Some(iso(100)));
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
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for either refusal, at the first offender.
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
    use gamut_ifd::Variant;

    use super::*;

    /// A little-endian EXIF model whose Exif sub-IFD is `directory`.
    fn model(directory: Ifd) -> Exif {
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set_exif_ifd(directory);
        exif
    }

    /// An EXIF model whose Exif sub-IFD carries `tag` = `value`.
    fn exif_with(tag: u16, value: Value) -> Exif {
        let mut directory = Ifd::new();
        directory.set(tag, value);
        model(directory)
    }

    #[test]
    fn empty_metadata_writes_nothing() {
        let mut ifd = Ifd::new();
        let meta = DngMetadata::default();
        assert!(meta.is_empty());
        assert!(meta.apply(&mut ifd).is_none());
        assert!(ifd.fields().is_empty());
    }

    #[test]
    fn an_exif_directory_with_no_fields_is_empty() {
        // An `ExifIFD` holding no field is not worth a directory, so it is not written — not
        // even when the model has an Interoperability sub-IFD to attach to it, or carries
        // directories (0th IFD, GPS) a DNG does not take from it.
        let mut interop = Ifd::new();
        interop.set(1, Value::Ascii("R98".into())); // InteroperabilityIndex
        let mut exif = model(Ifd::new());
        exif.set_interop_ifd(interop);
        exif.image_mut().set(271, Value::Ascii("gamut".into())); // Make
        exif.gps_ifd_mut().set(0, Value::Byte(vec![2, 3, 0, 0])); // GPSVersionID
        for exif in [Exif::new(ByteOrder::LittleEndian), exif] {
            let meta = DngMetadata::new().with_exif(exif);
            assert!(meta.is_empty());
            assert!(meta.apply(&mut Ifd::new()).is_none());
        }
    }

    #[test]
    fn apply_attaches_the_models_interop_ifd_as_the_exif_directorys_group() {
        let mut interop = Ifd::new();
        interop.set(1, Value::Ascii("R98".into())); // InteroperabilityIndex
        let mut exif = exif_with(tags::ISO_SPEED_RATINGS, Value::Short(vec![400]));
        exif.set_interop_ifd(interop.clone());
        let written = DngMetadata::new()
            .with_exif(exif)
            .apply(&mut Ifd::new())
            .expect("exif IFD");
        let groups = written.sub_ifds();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tag, INTEROPERABILITY_IFD);
        assert_eq!(groups[0].ifds, vec![interop]);
    }

    #[test]
    fn exif_model_lifts_one_interop_group_and_keeps_the_rest() {
        let mut interop = Ifd::new();
        interop.set(1, Value::Ascii("R98".into())); // InteroperabilityIndex
        let mut vendor = Ifd::new();
        vendor.set(7, Value::Short(vec![7]));
        let mut directory = Ifd::new();
        directory.set(tags::ISO_SPEED_RATINGS, Value::Short(vec![400]));
        directory.set_sub_ifd(INTEROPERABILITY_IFD, vec![interop.clone()]);
        directory.set_sub_ifd(0xC000, vec![vendor.clone()]);

        let exif = exif_model(directory, ByteOrder::BigEndian);
        assert_eq!(exif.byte_order(), ByteOrder::BigEndian);
        assert_eq!(exif.interop_ifd(), Some(&interop));
        let mut expected = Ifd::new();
        expected.set(tags::ISO_SPEED_RATINGS, Value::Short(vec![400]));
        expected.set_sub_ifd(0xC000, vec![vendor]);
        assert_eq!(exif.exif_ifd(), Some(&expected));
        assert!(exif.image().fields().is_empty());

        // An `InteroperabilityIFD` array is no single Interoperability sub-IFD: it stays a group.
        let mut array = Ifd::new();
        array.set_sub_ifd(INTEROPERABILITY_IFD, vec![interop.clone(), interop]);
        let exif = exif_model(array.clone(), ByteOrder::LittleEndian);
        assert_eq!(exif.interop_ifd(), None);
        assert_eq!(exif.exif_ifd(), Some(&array));
    }

    #[test]
    fn exif_model_inverts_apply_over_what_a_dng_holds() {
        let mut interop = Ifd::new();
        interop.set(1, Value::Ascii("R98".into())); // InteroperabilityIndex
        let mut exif = exif_with(tags::EXIF_VERSION, Value::Undefined(b"0230".to_vec()));
        exif.set_interop_ifd(interop);
        let written = DngMetadata::new()
            .with_exif(exif.clone())
            .apply(&mut Ifd::new())
            .expect("exif IFD");
        assert_eq!(exif_model(written, ByteOrder::LittleEndian), exif);
    }

    #[test]
    fn the_builders_set_one_carrier_each() {
        let exif = exif_with(tags::ISO_SPEED_RATINGS, Value::Short(vec![100]));
        assert_eq!(
            DngMetadata::new().with_exif(exif.clone()),
            DngMetadata {
                exif: Some(exif),
                ..Default::default()
            }
        );
        assert_eq!(
            DngMetadata::new().with_xmp(vec![1]),
            DngMetadata {
                xmp: Some(vec![1]),
                ..Default::default()
            }
        );
        assert_eq!(
            DngMetadata::new().with_iptc(vec![2]),
            DngMetadata {
                iptc: Some(vec![2]),
                ..Default::default()
            }
        );
        assert_eq!(
            DngMetadata::new().with_icc(vec![3]),
            DngMetadata {
                icc: Some(vec![3]),
                ..Default::default()
            }
        );
    }

    #[test]
    fn applies_blocks_and_carries_the_exif_sub_ifd() {
        let mut ifd = Ifd::new();
        let mut exif = exif_with(tags::ISO_SPEED_RATINGS, Value::Short(vec![400]));
        exif.exif_ifd_mut()
            .set(tags::EXPOSURE_TIME, Value::Rational(vec![(1, 250)]));
        let meta = DngMetadata {
            exif: Some(exif),
            xmp: Some(b"<x:xmpmeta/>".to_vec()),
            icc: Some(vec![0u8; 8]),
            ..Default::default()
        };
        assert!(!meta.is_empty());
        let written = meta.apply(&mut ifd).expect("exif IFD");
        assert_eq!(
            ifd.get(tags::XMP),
            Some(&Value::Byte(b"<x:xmpmeta/>".to_vec()))
        );
        assert!(ifd.get(tags::ICC_PROFILE).is_some());
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
                tags::EXIF_VERSION,
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
            DngMetadata::new()
                .with_exif(exif_with(tags::ISO_SPEED_RATINGS, Value::Short(vec![100]))),
            DngMetadata::new().with_xmp(vec![1]),
            DngMetadata::new().with_iptc(vec![1]),
            DngMetadata::new().with_icc(vec![1]),
        ];
        for (i, meta) in singles.iter().enumerate() {
            assert!(!meta.is_empty(), "block {i} alone must be non-empty");
        }
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
        DngMetadata::new().with_exif(model(exif))
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
