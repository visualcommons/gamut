//! The `metadata` feature: the typed [`gamut_metadata`] wiring in both directions.
//!
//! The read half is the facade's [`TypedMetadata`] over [`TiffMetadata`] — what
//! [`TiffDecoder::metadata`](crate::TiffDecoder::metadata) returns. The write half is the facade's
//! [`MetadataSink`] on [`TiffEncoder`]: a facade `Metadata` is serialized and each carrier routed
//! to the encoder's raw setters under the precedence every gamut encoder shares.
//!
//! Split from `metadata` and `encoder` (issue #1082) so the facade wiring sits in one file rather
//! than after each module's tests, as `gamut-png`'s `typed.rs` does.

use gamut_core::Result;
use gamut_metadata::{Carrier, Metadata, MetadataBlock, MetadataSink, TypedMetadata};

use crate::{TiffEncoder, TiffMetadata};

/// The facade's [`TypedMetadata`] over the five carriers. Like `gamut-dng`'s,
/// [`exif`](TiffMetadata::exif) is already the facade's EXIF model rather than bytes, so
/// [`blocks`](TypedMetadata::blocks) hands over only the four byte carriers and
/// [`metadata`](TypedMetadata::metadata) assigns the model straight onto [`Metadata::exif`].
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef};
/// use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
/// use gamut_tiff::{Metadata, MetadataSink, TiffEncoder, TypedMetadata};
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

    /// The unified model: [`Metadata::parse_blocks`] over [`blocks`](TypedMetadata::blocks)
    /// (the IPTC-IIM block reconciled into the XMP graph under the default
    /// [`ConflictPolicy`](gamut_metadata::iptc::ConflictPolicy)), with
    /// [`exif`](TiffMetadata::exif) assigned onto [`Metadata::exif`] as it is.
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

/// The facade's [`MetadataSink`]: [`with_metadata`](MetadataSink::with_metadata) and
/// [`with_encoded_metadata`](MetadataSink::with_encoded_metadata) route the EXIF, XMP, ICC and —
/// unlike most wired containers — legacy IPTC-IIM carriers to
/// [`with_exif`](TiffEncoder::with_exif), [`with_xmp`](TiffEncoder::with_xmp),
/// [`with_icc_profile`](TiffEncoder::with_icc_profile) and
/// [`with_iptc_iim`](TiffEncoder::with_iptc_iim).
///
/// Its whole-struct sibling is [`with_tiff_metadata`](TiffEncoder::with_tiff_metadata), which
/// replaces every carrier; the sink sets only the carriers a model holds. EXIF is parsed, because
/// a TIFF stores it as directories of the page rather than as bytes, and what of it a TIFF cannot
/// hold — a thumbnail, a 0th-IFD field IFD 0 holds as the encoder's own — is left behind and
/// reported by [`metadata_notices`](TiffEncoder::metadata_notices) rather than refused. A C2PA
/// manifest store is always refused by the sink: a store computed for this output goes through
/// [`with_c2pa`](TiffEncoder::with_c2pa) / [`with_c2pa_reserved`](TiffEncoder::with_c2pa_reserved)
/// instead.
impl MetadataSink for TiffEncoder {
    const CARRIERS: &'static [Carrier] =
        &[Carrier::Exif, Carrier::Xmp, Carrier::Icc, Carrier::IptcIim];

    /// The TIFF stream is parsed into [`TiffMetadata::exif`].
    ///
    /// # Errors
    ///
    /// As [`with_exif`](TiffEncoder::with_exif): a stream that does not parse.
    fn write_exif(self, tiff: &[u8]) -> Result<Self> {
        self.with_exif(tiff)
    }

    /// The packet goes to the `XMP` tag (700) of IFD 0.
    fn write_xmp(self, packet: &[u8]) -> Result<Self> {
        Ok(self.with_xmp(packet))
    }

    /// The profile goes to the `ICCProfile` tag (34675) of IFD 0.
    fn write_icc(self, profile: &[u8]) -> Result<Self> {
        Ok(self.with_icc_profile(profile))
    }

    /// The datasets go to the `IPTC/NAA` tag (33723) of IFD 0.
    fn write_iptc_iim(self, datasets: &[u8]) -> Result<Self> {
        Ok(self.with_iptc_iim(datasets))
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::{Dimensions, EncodeImage, ErrorKind, Gray8, ImageRef};
    use gamut_ifd::{ByteOrder, Ifd};
    use gamut_metadata::EncodedMetadata;
    use gamut_metadata::exif::{Exif, ExifTag, Value};
    use gamut_metadata::icc::{ColorSpace, DeviceClass, IccProfile, ProfileHeader, Tag, TagData};
    use gamut_metadata::xmp::{WellKnownNs, XmpMeta};

    use super::*;
    use crate::{MetadataNotice, TiffDecoder, tags};

    /// A typed model with EXIF (a 0th-IFD `Make` and `Orientation`, an Exif sub-IFD field and
    /// a GPS sub-IFD field — one in each directory a TIFF carries), XMP and ICC populated,
    /// normalised through one embed → extract pass so it is an *extracted* model (the ICC
    /// header's `size` is stamped by the serializer).
    fn typed(order: ByteOrder) -> Metadata {
        let mut exif = Exif::new(order);
        exif.set_tag(ExifTag::Make, Value::Ascii("gamut".into()));
        exif.set_tag(ExifTag::Orientation, Value::Short(vec![6]));
        exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![400]));
        exif.set_tag(ExifTag::GpsLatitudeRef, Value::Ascii("N".into()));
        let mut xmp = XmpMeta::new();
        xmp.set_text(WellKnownNs::Xmp.uri(), "CreatorTool", "gamut");
        let icc = IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Gray),
            tags: Vec::new(),
        };
        let encoded = Metadata::from_carriers(Some(exif), Some(xmp), Some(icc))
            .to_blocks()
            .unwrap();
        Metadata::parse_blocks(&[
            MetadataBlock::Exif(encoded.exif.as_deref().unwrap()),
            MetadataBlock::Xmp(encoded.xmp.as_deref().unwrap()),
            MetadataBlock::Icc(encoded.icc.as_deref().unwrap()),
        ])
        .unwrap()
    }

    /// Encodes an 8×8 grayscale image with `encoder`.
    fn encode(encoder: &TiffEncoder) -> Vec<u8> {
        let pixels = vec![128u8; 64];
        let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
        encoder.encode_to_vec(image).unwrap()
    }

    #[test]
    fn blocks_hand_over_the_byte_carriers_only() {
        let mut meta = TiffMetadata::new()
            .with_exif(Exif::new(ByteOrder::LittleEndian))
            .with_xmp(b"<x:xmpmeta/>".to_vec())
            .with_iptc(vec![0x1c, 0x02, 0x05])
            .with_icc(vec![7; 4]);
        meta.c2pa = Some(b"jumb".to_vec());
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

    #[test]
    fn metadata_assigns_the_exif_model_as_it_is() {
        let mut exif = Exif::new(ByteOrder::BigEndian);
        exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![400]));
        let mut meta = TiffMetadata::new().with_exif(exif.clone());
        meta.c2pa = Some(b"jumb".to_vec());
        let unified = meta.metadata().unwrap();
        assert_eq!(unified.exif, Some(exif));
        // The byte carriers still go through `parse_blocks` beside the assigned model.
        assert_eq!(unified.c2pa.as_deref(), Some(&b"jumb"[..]));
        assert_eq!(TiffMetadata::new().metadata().unwrap(), Metadata::default());
    }

    #[test]
    fn typed_metadata_round_trips_through_the_container_in_either_byte_order() {
        // The facade's keystone equality, extended through IFD 0 — and the EXIF model's byte
        // order is the file's, so each order is written by an encoder set to it.
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            let typed = typed(order);
            let tiff = encode(
                &TiffEncoder::new()
                    .with_byte_order(order)
                    .with_metadata(&typed)
                    .unwrap(),
            );
            let read = TiffDecoder::new().metadata(&tiff).unwrap();
            assert_eq!(read.exif.as_ref().map(Exif::byte_order), Some(order));
            assert_eq!(read.metadata().unwrap(), typed, "{order:?}");
        }
    }

    #[test]
    fn exiv2_reads_what_with_metadata_embeds() {
        // Container-level differential: exiv2's own TIFF reader finds the `ExifIFD`, the XMP
        // (700) and the `ICCProfile` (34675) tags this encoder writes into IFD 0.
        let typed = typed(ByteOrder::BigEndian);
        let tiff = encode(
            &TiffEncoder::new()
                .with_byte_order(ByteOrder::BigEndian)
                .with_metadata(&typed)
                .unwrap(),
        );
        assert_eq!(
            exiv2_oracle::image_exif_get(&tiff, "Exif.Photo.ISOSpeedRatings").unwrap(),
            "400"
        );
        // The 0th-IFD fields are the page's own tags, and the GPS sub-IFD hangs off it.
        assert_eq!(
            exiv2_oracle::image_exif_get(&tiff, "Exif.Image.Make").unwrap(),
            "gamut"
        );
        assert_eq!(
            exiv2_oracle::image_exif_get(&tiff, "Exif.Image.Orientation").unwrap(),
            "6"
        );
        assert_eq!(
            exiv2_oracle::image_exif_get(&tiff, "Exif.GPSInfo.GPSLatitudeRef").unwrap(),
            "N"
        );
        assert_eq!(
            exiv2_oracle::image_xmp_get(&tiff, "Xmp.xmp.CreatorTool").unwrap(),
            "gamut"
        );
        assert_eq!(
            exiv2_oracle::image_icc(&tiff).unwrap(),
            typed.to_blocks().unwrap().icc
        );
    }

    #[test]
    fn the_typed_reader_reads_what_exiv2_embeds() {
        // The other direction: exiv2 adds an `ExifIFD` and an XMP tag to a file this crate
        // encoded bare, and the typed reader finds both values.
        let bare = encode(&TiffEncoder::new());
        let tiff = exiv2_oracle::image_set(
            &bare,
            Some(("Exif.Photo.ISOSpeedRatings", "400")),
            Some(("Xmp.xmp.CreatorTool", "exiv2")),
        )
        .unwrap();
        let read = TiffDecoder::new()
            .metadata(&tiff)
            .unwrap()
            .metadata()
            .unwrap();
        assert_eq!(
            read.exif.unwrap().get_tag(ExifTag::PhotographicSensitivity),
            Some(&Value::Short(vec![400]))
        );
        assert_eq!(
            read.xmp
                .unwrap()
                .get_text(WellKnownNs::Xmp.uri(), "CreatorTool"),
            Some("exiv2")
        );
    }

    #[test]
    fn a_manifest_store_is_never_copied_forward() {
        // The default embedder drops a model's store, and the sink refuses blocks carrying one.
        let mut typed = typed(ByteOrder::LittleEndian);
        typed.c2pa = Some(b"\0\0\0\x14jumbc2pa".to_vec());
        let tiff = encode(&TiffEncoder::new().with_metadata(&typed).unwrap());
        let read = TiffDecoder::new().metadata(&tiff).unwrap();
        assert_eq!(read.c2pa, None);
        typed.c2pa = None;
        assert_eq!(read.metadata().unwrap(), typed);

        let mut c2pa = EncodedMetadata::default();
        c2pa.c2pa = Some(b"\0\0\0\x08jumb".to_vec());
        let err = TiffEncoder::new().with_encoded_metadata(&c2pa).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn encoded_blocks_route_to_the_raw_setters() {
        // Each present field lands in its tag — IPTC-IIM included — and an absent one leaves
        // an earlier setting.
        let mut encoded = typed(ByteOrder::LittleEndian).to_blocks().unwrap();
        encoded.iptc_iim = Some(vec![0x1c, 0x02, 0x05]);
        let tiff = encode(&TiffEncoder::new().with_encoded_metadata(&encoded).unwrap());
        let read = TiffDecoder::new().metadata(&tiff).unwrap();
        assert_eq!(read.xmp, encoded.xmp);
        assert_eq!(read.icc, encoded.icc);
        assert_eq!(read.iptc, encoded.iptc_iim);
        let exif = read.exif.unwrap();
        assert_eq!(
            exif.get_tag(ExifTag::PhotographicSensitivity),
            Some(&Value::Short(vec![400]))
        );
        assert_eq!(
            exif.image().get(tags::MAKE),
            Some(&Value::Ascii("gamut".into()))
        );
        assert_eq!(
            exif.get_tag(ExifTag::GpsLatitudeRef),
            Some(&Value::Ascii("N".into()))
        );

        let mut only_icc = EncodedMetadata::default();
        only_icc.icc = encoded.icc.clone();
        let tiff = encode(
            &TiffEncoder::new()
                .with_xmp(b"<x:xmpmeta/>")
                .with_encoded_metadata(&only_icc)
                .unwrap(),
        );
        let read = TiffDecoder::new().metadata(&tiff).unwrap();
        assert_eq!(read.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        assert_eq!(read.icc, encoded.icc);
        assert_eq!(read.exif, None);
    }

    /// `YCbCrPositioning` (531), a TIFF 6.0 §21 tag this crate names nowhere else.
    const Y_CB_CR_POSITIONING: u16 = 531;

    #[test]
    fn what_a_tiff_cannot_hold_of_a_model_is_a_notice_not_an_error() {
        // A camera's EXIF keeps `YCbCrPositioning` in the 0th IFD, which in a TIFF describes a
        // YCbCr coding this encoder never writes: it is left behind with a notice and the carried
        // `Make` still lands.
        let mut camera = Exif::new(ByteOrder::LittleEndian);
        camera.set_tag(ExifTag::Make, Value::Ascii("gamut".to_owned()));
        camera.set_tag(ExifTag::YCbCrPositioning, Value::Short(vec![1]));
        let model = Metadata::from_carriers(Some(camera), None, None);
        let encoder = TiffEncoder::new().with_metadata(&model).unwrap();
        assert_eq!(
            encoder.metadata_notices(),
            vec![MetadataNotice::ExifImageField {
                tag: Y_CB_CR_POSITIONING
            }]
        );
        let read = TiffDecoder::new().metadata(&encode(&encoder)).unwrap();
        let page: &Ifd = read.exif.as_ref().unwrap().image();
        assert_eq!(page.get(tags::MAKE), Some(&Value::Ascii("gamut".into())));
        assert_eq!(page.get(Y_CB_CR_POSITIONING), None);
    }

    #[test]
    fn an_exif_block_that_does_not_parse_is_invalid_input_with_the_parsers_detail() {
        let mut garbage = EncodedMetadata::default();
        garbage.exif = Some(b"not a tiff stream".to_vec());
        let err = TiffEncoder::new()
            .with_encoded_metadata(&garbage)
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.static_message(),
            Some("TIFF: EXIF block does not parse")
        );
        assert!(err.detail().is_some());
    }

    #[test]
    fn a_model_that_does_not_serialize_is_invalid_input() {
        // An ICC model with a duplicate tag signature is rejected by `gamut-icc`'s writer
        // (ICC.1:2022 §7.3), before any carrier reaches the encoder.
        let duplicate = Tag::new(*b"wtpt", TagData::Xyz(Vec::new()));
        let bad_icc = IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Gray),
            tags: vec![duplicate.clone(), duplicate],
        };
        let model = Metadata::from_carriers(None, None, Some(bad_icc));
        let err = TiffEncoder::new().with_metadata(&model).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn the_sink_writes_exactly_the_four_carriers_a_tiff_has_a_tag_for() {
        assert_eq!(
            <TiffEncoder as MetadataSink>::CARRIERS,
            &[Carrier::Exif, Carrier::Xmp, Carrier::Icc, Carrier::IptcIim]
        );
    }
}
