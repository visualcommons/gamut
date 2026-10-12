//! Optional metadata embedded in a DNG: an EXIF sub-IFD plus XMP / IPTC / ICC / C2PA blocks.
//!
//! The models are the workspace's, not this crate's. EXIF is
//! [`gamut_metadata::exif::Exif`] — DNG's `ExifIFD` (34665) *is* an EXIF sub-IFD, so the facade's
//! model describes it exactly and this crate no longer redefines a hand-picked subset of its
//! fields. XMP (700), IPTC-IIM (33723), ICC (34675) and the C2PA manifest store (52545) are
//! single opaque payloads in the file, so they are carried verbatim as the byte blocks the
//! facade consumes — the same shape `gamut-png` and `gamut-webp` hand over — and
//! [`DngMetadata::blocks`] presents them as [`MetadataBlock`]s ready for
//! [`gamut_metadata::Metadata::from_blocks`].
//!
//! The C2PA store is the one carrier with a placement rule of its own (C2PA 2.4 §A.3.6: the
//! last main IFD, at the end of the file) and a signer's exclusion contract (§18.5.5); both are
//! [`gamut_ifd::c2pa`]'s, and the encoder applies them — see
//! [`DngEncoder::encode_with_report`](crate::DngEncoder::encode_with_report).

use gamut_core::{Error, Result};
use gamut_ifd::{Ifd, Value};
use gamut_metadata::MetadataBlock;
use gamut_metadata::exif::Exif;

use crate::tags;

/// The `ExifVersion` written when the supplied EXIF sub-IFD does not carry one: EXIF 2.3.
///
/// `ExifVersion` (36864) is mandatory in an EXIF IFD, so the encoder supplies it rather than
/// emit a directory a conforming reader may reject.
const DEFAULT_EXIF_VERSION: &[u8; 4] = b"0230";

/// Metadata to embed in a DNG: an EXIF sub-IFD and/or opaque XMP / IPTC / ICC / C2PA blocks.
///
/// Construct one as a struct literal — it is deliberately exhaustive (see `STATUS.md`), so the
/// five carriers a DNG holds are visible at the point of use:
///
/// ```
/// use gamut_dng::{ByteOrder, DngMetadata, Exif, ExifTag, Value};
///
/// let mut exif = Exif::new(ByteOrder::LittleEndian);
/// exif.set_tag(ExifTag::FNumber, Value::Rational(vec![(28, 10)]));
/// exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![400]));
///
/// let meta = DngMetadata {
///     exif: Some(exif),
///     xmp: None,
///     iptc: None,
///     icc: None,
///     c2pa: None,
/// };
/// ```
#[derive(Debug, Clone, Default)]
pub struct DngMetadata {
    /// EXIF capture settings, as the shared [`Exif`] model.
    ///
    /// Only the model's **Exif sub-IFD** ([`Exif::exif_ifd`]) crosses into the file, as the DNG's
    /// `ExifIFD` (34665) — that directory is the one this container owns a slot for. The model's
    /// 0th IFD, GPS sub-IFD and thumbnail describe directories the DNG container builds itself
    /// (IFD 0, and previews as `SubIFDs` entries), so the encoder does not write them and the
    /// decoder does not populate them; a DNG's own IFD-0 fields reach the caller through
    /// [`DecodedDng`](crate::DecodedDng) instead.
    pub exif: Option<Exif>,
    /// An XMP packet (UTF-8 RDF/XML), stored in the `XMP` tag (700), verbatim.
    pub xmp: Option<Vec<u8>>,
    /// A legacy IPTC-IIM dataset stream, stored in the `IPTC/NAA` tag (33723), verbatim.
    ///
    /// Kept as its own carrier rather than folded into [`xmp`](Self::xmp): IIM is a genuinely
    /// separate serialization that DNG files really do hold, and reconciling it into an XMP graph
    /// is a policy decision (see [`gamut_metadata::ConflictPolicy`]) that belongs to the caller,
    /// not to the container. [`blocks`](Self::blocks) hands it over for exactly that.
    pub iptc: Option<Vec<u8>>,
    /// An ICC profile, stored in the `ICCProfile` tag (34675), verbatim.
    pub icc: Option<Vec<u8>>,
    /// A C2PA manifest store, stored in the `C2PA` tag
    /// ([`gamut_ifd::c2pa::C2PA_MANIFEST_STORE`], 52545, type `UNDEFINED`), verbatim.
    ///
    /// **Opaque, and bound to one exact file.** A manifest store is a signed hash over the
    /// bytes *around* it (C2PA 2.4 §18.5), so the only store valid here is one an external
    /// signer computed over this encoder's own output — through
    /// [`DngEncoder::with_c2pa_reserved`](crate::DngEncoder::with_c2pa_reserved) and the
    /// exclusion ranges [`DngEncoder::encode_with_report`](crate::DngEncoder::encode_with_report)
    /// reports. A store copied out of another file is invalid by construction, which is why
    /// [`gamut_metadata::Metadata::encode`] never hands one back
    /// ([`gamut_metadata::C2paPolicy`]). The bytes are written exactly as given: the TIFF byte
    /// order does not govern them (§A.3.6). On decode this is the store the file carries in the
    /// last IFD of its main chain; its byte ranges are
    /// [`DecodedDng::c2pa_exclusions`](crate::DecodedDng::c2pa_exclusions).
    pub c2pa: Option<Vec<u8>>,
}

impl DngMetadata {
    /// The byte-carried blocks — XMP, IPTC-IIM, ICC and the C2PA store — as [`MetadataBlock`]s,
    /// ready for
    /// [`gamut_metadata::Metadata::from_blocks`] or a
    /// [`MetadataExtractor`](gamut_metadata::MetadataExtractor) with a chosen
    /// [`ConflictPolicy`](gamut_metadata::ConflictPolicy).
    ///
    /// [`exif`](Self::exif) is deliberately absent: it is already the facade's typed model, so it
    /// is assigned straight onto [`Metadata::exif`](gamut_metadata::Metadata::exif) rather than
    /// re-serialised to bytes and parsed back.
    ///
    /// ```
    /// # use gamut_dng::DngMetadata;
    /// # fn demo(meta: &DngMetadata) -> Result<(), gamut_metadata::MetadataError> {
    /// let mut unified = gamut_metadata::Metadata::from_blocks(&meta.blocks())?;
    /// unified.exif = meta.exif.clone();
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn blocks(&self) -> Vec<MetadataBlock<'_>> {
        let mut blocks = Vec::new();
        if let Some(xmp) = &self.xmp {
            blocks.push(MetadataBlock::Xmp(xmp));
        }
        if let Some(iptc) = &self.iptc {
            blocks.push(MetadataBlock::IptcIim(iptc));
        }
        if let Some(icc) = &self.icc {
            blocks.push(MetadataBlock::Icc(icc));
        }
        if let Some(c2pa) = &self.c2pa {
            blocks.push(MetadataBlock::C2pa(c2pa));
        }
        blocks
    }

    /// The EXIF sub-IFD to write, or `None` when there is no EXIF content worth a directory.
    fn exif_ifd(&self) -> Option<&Ifd> {
        self.exif
            .as_ref()
            .and_then(Exif::exif_ifd)
            .filter(|ifd| !ifd.fields().is_empty())
    }

    /// Whether there is nothing to embed.
    pub(crate) fn is_empty(&self) -> bool {
        self.exif_ifd().is_none()
            && self.xmp.is_none()
            && self.iptc.is_none()
            && self.icc.is_none()
            && self.c2pa.is_none()
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
    /// Returns [`Error::InvalidInput`] if the Exif sub-IFD, or any directory nested in it,
    /// carries a pointer-typed field under a standard pointer tag, or stores one tag twice (a
    /// field and a sub-IFD group, or two groups).
    pub(crate) fn check(&self) -> Result<()> {
        self.exif_ifd().map_or(Ok(()), check_exif_subtree)
    }

    /// Writes the XMP / IPTC / ICC blocks into `ifd0` and returns the EXIF sub-IFD, if any.
    ///
    /// The C2PA store is deliberately **not** written here: its value must land at the end of
    /// the file (C2PA 2.4 §A.3.6), after the image data, which only the encoder can arrange once
    /// the rest of the file exists ([`gamut_ifd::c2pa::append_store`]).
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

/// The pointer tags [`DngRewrite::open`](crate::DngRewrite::open) resolves inside the Exif
/// subtree: every standard one, because it hands [`gamut_ifd::read_tree`] the flat
/// [`STANDARD_POINTER_TAGS`](gamut_ifd::tags::STANDARD_POINTER_TAGS) list, which that reader
/// resolves **at every node** — the Exif directory and anything nested in it included.
const EXIF_SUBTREE_POINTER_TAGS: &[u16] = gamut_ifd::tags::STANDARD_POINTER_TAGS;

/// The on-disk field-type codes that make a directory entry a sub-IFD pointer to the reader:
/// `LONG` (4), the typed `IFD` (13) of TIFF Technical Note 1, and BigTIFF's `LONG8` (16) /
/// `IFD8` (18) — the set [`gamut_ifd::read_tree`] follows.
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
/// check. `gamut-tiff`'s check refuses those two as well; they are not adopted here because what a
/// group under an Exif tag reads back as depends on the decoder resolving Exif pointers at all.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] for either refusal, at the first offender.
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
        assert!(meta.blocks().is_empty());
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
        assert!(ifd.get(gamut_ifd::c2pa::C2PA_MANIFEST_STORE).is_none());
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
            meta.blocks(),
            vec![
                MetadataBlock::Xmp(b"<x:xmpmeta/>"),
                MetadataBlock::IptcIim(&[0x1c, 0x02, 0x05]),
                MetadataBlock::Icc(&[7u8; 4]),
                MetadataBlock::C2pa(b"\0\0\0\x14jumbc2pa"),
            ]
        );
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
