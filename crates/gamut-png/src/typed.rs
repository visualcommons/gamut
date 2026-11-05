//! The `metadata` feature: the typed [`gamut_metadata`] wiring in both directions.
//!
//! The read half is the facade's [`TypedMetadata`] over the four carriers [`PngMetadata`] locates
//! as bytes — what [`metadata`](crate::metadata) returns and what
//! [`DecodedPng::metadata`](crate::DecodedPng::metadata) holds. The write half is the facade's
//! [`MetadataSink`] on [`PngEncoder`]: a facade `Metadata` is serialized and each carrier routed
//! to the encoder's raw setters under the precedence every gamut encoder shares.

use gamut_core::Result;
use gamut_metadata::{Carrier, MetadataBlock, MetadataSink, TypedMetadata};

use crate::{PngEncoder, PngMetadata};

/// The facade's [`TypedMetadata`] over the located payloads; `metadata` is the trait's default,
/// `Metadata::parse_blocks` over [`blocks`](TypedMetadata::blocks).
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef};
/// use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
/// use gamut_png::{Metadata, MetadataSink, PngEncoder, TypedMetadata};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mut exif = Exif::new(ByteOrder::LittleEndian);
/// exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![400]));
/// let typed = Metadata::from_carriers(Some(exif), None, None);
///
/// let pixels = vec![0u8; 64];
/// let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8)?)?;
/// let png = PngEncoder::new()
///     .with_metadata(&typed)?
///     .encode_to_vec(image)?;
///
/// assert_eq!(gamut_png::metadata(&png)?.metadata()?, typed);
/// # Ok(())
/// # }
/// ```
impl TypedMetadata for PngMetadata {
    /// The located payloads as [`MetadataBlock`]s, in a fixed order: the `eXIf` TIFF stream, the
    /// XMP packet (decompressed if its `iTXt` was), the `iCCP` profile (inflated; its name is not
    /// a carrier and stays in [`icc_profile`](PngMetadata::icc_profile)) and the `caBX` manifest
    /// store, each present only when the file carried it. PNG has no IPTC-IIM chunk, so no
    /// [`MetadataBlock::IptcIim`] is ever produced; the text annotations and colour chunks are
    /// PNG's own and are not facade carriers.
    ///
    /// # Errors
    ///
    /// Never: every payload was already located, and inflated, when this value was built. The
    /// signature is the trait's, shared with containers that resolve payloads lazily.
    fn blocks(&self) -> gamut_metadata::Result<Vec<MetadataBlock<'_>>> {
        let mut blocks = Vec::new();
        blocks.extend(self.exif.as_deref().map(MetadataBlock::Exif));
        blocks.extend(self.xmp().map(MetadataBlock::Xmp));
        blocks.extend(
            self.icc_profile
                .as_ref()
                .map(|icc| MetadataBlock::Icc(&icc.profile)),
        );
        blocks.extend(self.c2pa.as_deref().map(MetadataBlock::C2pa));
        Ok(blocks)
    }
}

/// The facade's [`MetadataSink`]: [`with_metadata`](MetadataSink::with_metadata) and
/// [`with_encoded_metadata`](MetadataSink::with_encoded_metadata) route the EXIF, XMP and ICC
/// carriers to [`with_exif`](PngEncoder::with_exif), [`with_xmp`](PngEncoder::with_xmp) and
/// [`with_icc_profile`](PngEncoder::with_icc_profile).
///
/// Its chunk-for-chunk sibling is [`with_png_metadata`](PngEncoder::with_png_metadata), which
/// carries a [`PngMetadata`] — every PNG chunk the read side surfaces — whereas the sink takes the
/// facade's container-agnostic model, which holds only the EXIF, XMP and ICC carriers.
///
/// PNG defines no IPTC-IIM chunk, so a legacy IPTC-IIM block is refused, as a C2PA manifest store
/// always is by the sink: a store computed for this output goes through
/// [`with_c2pa`](PngEncoder::with_c2pa) / [`with_c2pa_reserved`](PngEncoder::with_c2pa_reserved)
/// instead. A facade ICC block carries no profile name, so the profile is written under the name
/// [`with_icc_profile_name`](PngEncoder::with_icc_profile_name) set, or `"ICC profile"`.
impl MetadataSink for PngEncoder {
    const CARRIERS: &'static [Carrier] = &[Carrier::Exif, Carrier::Xmp, Carrier::Icc];

    /// The bare TIFF stream goes to the `eXIf` chunk (§11.3.4.5) unchanged.
    fn write_exif(self, tiff: &[u8]) -> Result<Self> {
        Ok(self.with_exif(tiff))
    }

    /// The packet goes to the XMP `iTXt` chunk.
    ///
    /// # Errors
    ///
    /// As [`with_xmp`](PngEncoder::with_xmp): a packet that is not UTF-8, which an `iTXt` text
    /// field must be (§11.3.3.4).
    fn write_xmp(self, packet: &[u8]) -> Result<Self> {
        self.with_xmp(packet)
    }

    /// The profile goes to the `iCCP` chunk.
    fn write_icc(self, profile: &[u8]) -> Result<Self> {
        Ok(self.with_icc_profile(profile))
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::{Dimensions, EncodeImage, ErrorKind, Gray8, ImageRef};
    use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
    use gamut_metadata::icc::{ColorSpace, DeviceClass, IccProfile, ProfileHeader, Tag, TagData};
    use gamut_metadata::xmp::{WellKnownNs, XmpMeta};
    use gamut_metadata::{EncodedMetadata, Metadata};

    use super::*;
    use crate::decoded::{TextChunk, TextChunkKind, XMP_KEYWORD};
    use crate::text::DEFAULT_ICC_PROFILE_NAME;

    #[test]
    fn png_metadata_blocks_expose_each_located_payload_in_facade_form() {
        let meta = PngMetadata {
            exif: Some(b"II*\0".to_vec()),
            texts: vec![TextChunk {
                keyword: XMP_KEYWORD.to_owned(),
                text: "<x:xmpmeta/>".to_owned(),
                language: None,
                translated_keyword: None,
                kind: TextChunkKind::International,
            }],
            icc_profile: Some(crate::IccProfile {
                name: "ICC profile".to_owned(),
                profile: vec![7u8; 4],
            }),
            c2pa: Some(b"jumb".to_vec()),
            ..PngMetadata::default()
        };
        assert_eq!(
            meta.blocks().unwrap(),
            vec![
                MetadataBlock::Exif(b"II*\0"),
                MetadataBlock::Xmp(b"<x:xmpmeta/>"),
                MetadataBlock::Icc(&[7u8; 4]),
                MetadataBlock::C2pa(b"jumb"),
            ]
        );
        assert!(PngMetadata::default().blocks().unwrap().is_empty());
    }

    /// A typed model with the three facade-writable carriers populated, normalised through one
    /// embed → extract pass so it is an *extracted* model (the ICC header's `size` is stamped by
    /// the serializer). The profile is a greyscale one, matching the image `encode` writes
    /// (§11.3.2.3).
    fn typed() -> Metadata {
        let mut exif = Exif::new(ByteOrder::BigEndian);
        exif.set_tag(ExifTag::Make, Value::Ascii("gamut".to_owned()));
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
    fn encode(encoder: &PngEncoder) -> Vec<u8> {
        let pixels = vec![128u8; 64];
        let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
        encoder.encode_to_vec(image).unwrap()
    }

    #[test]
    fn typed_metadata_round_trips_through_the_datastream() {
        // The facade's keystone equality, extended through the `eXIf` / `iTXt` / `iCCP` chunks.
        let typed = typed();
        let png = encode(&PngEncoder::new().with_metadata(&typed).unwrap());
        let read = crate::metadata(&png).unwrap();
        assert!(read.exif.is_some() && read.xmp().is_some() && read.icc_profile.is_some());
        assert_eq!(read.metadata().unwrap(), typed);
    }

    #[test]
    fn exiv2_reads_what_with_metadata_embeds() {
        // Container-level differential: exiv2's own PNG reader finds the `eXIf`, the XMP `iTXt`
        // and the `iCCP` this encoder writes, not only this crate's reader.
        let typed = typed();
        let png = encode(&PngEncoder::new().with_metadata(&typed).unwrap());
        assert_eq!(
            exiv2_oracle::image_exif_get(&png, "Exif.Image.Make").unwrap(),
            "gamut"
        );
        assert_eq!(
            exiv2_oracle::image_xmp_get(&png, "Xmp.xmp.CreatorTool").unwrap(),
            "gamut"
        );
        assert_eq!(
            exiv2_oracle::image_icc(&png).unwrap(),
            typed.to_blocks().unwrap().icc
        );
    }

    #[test]
    fn the_typed_reader_reads_what_exiv2_embeds() {
        // The other direction: exiv2 writes an `eXIf` and an XMP `iTXt` into a file this crate
        // encoded bare, and the typed reader finds both values.
        let bare = encode(&PngEncoder::new());
        let png = exiv2_oracle::image_set(
            &bare,
            Some(("Exif.Photo.ISOSpeedRatings", "400")),
            Some(("Xmp.xmp.CreatorTool", "exiv2")),
        )
        .unwrap();
        let read = crate::metadata(&png).unwrap().metadata().unwrap();
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
        let mut typed = typed();
        typed.c2pa = Some(b"\0\0\0\x14jumbc2pa".to_vec());
        let png = encode(&PngEncoder::new().with_metadata(&typed).unwrap());
        let read = crate::metadata(&png).unwrap().metadata().unwrap();
        assert_eq!(read.c2pa, None);
        typed.c2pa = None;
        assert_eq!(read, typed);
    }

    #[test]
    fn encoded_blocks_route_to_the_raw_setters() {
        // Each present field lands in its chunk — the EXIF one without the facade's signature,
        // the profile under the default name — and an absent one leaves an earlier setting.
        let encoded = typed().to_blocks().unwrap();
        let png = encode(&PngEncoder::new().with_encoded_metadata(&encoded).unwrap());
        let read = crate::metadata(&png).unwrap();
        assert_eq!(
            read.exif.as_deref(),
            encoded
                .exif
                .as_deref()
                .and_then(|e| e.strip_prefix(b"Exif\0\0"))
        );
        assert_eq!(read.xmp(), encoded.xmp.as_deref());
        let icc = read.icc_profile.unwrap();
        assert_eq!(icc.name, DEFAULT_ICC_PROFILE_NAME);
        assert_eq!(Some(icc.profile), encoded.icc);

        let mut only_icc = EncodedMetadata::default();
        only_icc.icc = encoded.icc.clone();
        let png = encode(
            &PngEncoder::new()
                .with_xmp(b"<x:xmpmeta/>")
                .unwrap()
                .with_icc_profile_name("named")
                .with_encoded_metadata(&only_icc)
                .unwrap(),
        );
        let read = crate::metadata(&png).unwrap();
        assert_eq!(read.xmp(), Some(&b"<x:xmpmeta/>"[..]));
        let icc = read.icc_profile.unwrap();
        assert_eq!(
            (icc.name.as_str(), Some(icc.profile)),
            ("named", encoded.icc)
        );
        assert_eq!(read.exif, None);
    }

    #[test]
    fn the_default_profile_name_draws_no_notice() {
        // The default name must itself satisfy §11.3.2.3, or every typed ICC embed would be
        // reported as bent.
        let mut only_icc = EncodedMetadata::default();
        only_icc.icc = typed().to_blocks().unwrap().icc;
        let encoder = PngEncoder::new().with_encoded_metadata(&only_icc).unwrap();
        assert!(encoder.metadata_notices().is_empty());
    }

    #[test]
    fn a_bare_exif_stream_is_written_unchanged() {
        // The sink's `strip_prefix` must not eat a stream that never carried the signature.
        let mut bare = EncodedMetadata::default();
        bare.exif = Some(b"MM\0*\0\0\0\x08".to_vec());
        let png = encode(&PngEncoder::new().with_encoded_metadata(&bare).unwrap());
        assert_eq!(crate::metadata(&png).unwrap().exif, bare.exif);
    }

    #[test]
    fn unembeddable_carriers_are_typed_errors() {
        let mut iim = EncodedMetadata::default();
        iim.iptc_iim = Some(vec![0x1c, 0x02, 0x05]);
        let err = PngEncoder::new().with_encoded_metadata(&iim).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert_eq!(
            err.static_message(),
            Some("metadata: the encoder writes no IPTC-IIM carrier")
        );

        let mut c2pa = EncodedMetadata::default();
        c2pa.c2pa = Some(b"\0\0\0\x08jumb".to_vec());
        let err = PngEncoder::new().with_encoded_metadata(&c2pa).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert_eq!(
            err.static_message(),
            Some("metadata: a C2PA manifest store is not embedded from metadata blocks")
        );

        let mut latin1 = EncodedMetadata::default();
        latin1.xmp = Some(vec![0xff, 0xfe]);
        latin1.exif = Some(b"MM\0*\0\0\0\x08".to_vec());
        let err = PngEncoder::new()
            .with_encoded_metadata(&latin1)
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.static_message(), Some("PNG: XMP packet is not UTF-8"));
    }

    #[test]
    fn a_model_that_does_not_serialize_is_invalid_input_with_the_facade_detail() {
        // An ICC model with a duplicate tag signature is rejected by `gamut-icc`'s writer
        // (ICC.1:2022 §7.3).
        let duplicate = Tag::new(*b"wtpt", TagData::Xyz(Vec::new()));
        let bad_icc = IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Gray),
            tags: vec![duplicate.clone(), duplicate],
        };
        let model = Metadata::from_carriers(None, None, Some(bad_icc));
        let err = PngEncoder::new().with_metadata(&model).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.detail(), Some("icc: duplicate tag signature"));
    }
}
