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

#[cfg(test)]
mod tests {
    use gamut_ifd::ByteOrder;
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
}
