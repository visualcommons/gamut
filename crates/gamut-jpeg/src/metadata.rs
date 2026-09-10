//! The `metadata` feature: typed [`gamut_metadata`] wiring over the raw APP-segment surface.
//!
//! [`crate::metadata`] locates the APP1 EXIF / APP1 XMP / APP2 `ICC_PROFILE` payloads as bytes;
//! this module hands them to the facade as [`MetadataBlock`]s and turns a facade [`Metadata`] back
//! into the encoder's raw setters. The dependency direction is `gamut-jpeg → gamut-metadata`
//! (the facade never learns about JPEG segments), and the module is compiled only with the
//! `metadata` Cargo feature so a plain JPEG consumer never pays for the metadata crates.
//!
//! The block boundaries are the ones the facade documents: the EXIF block is the **TIFF stream**
//! (`II`/`MM` first; the `Exif\0\0` APP1 signature is already stripped by [`crate::metadata`] and
//! re-added by [`JpegEncoder::with_exif`]), the XMP block is the `xpacket` with the namespace URI
//! stripped, and the ICC block is the profile **reassembled** from its APP2 chunks.

use gamut_core::{Error, Result};
use gamut_metadata::{EncodedMetadata, Metadata, MetadataBlock, MetadataEmbedder};

use crate::{JpegEncoder, JpegMetadata};

impl JpegMetadata {
    /// The located payloads as [`MetadataBlock`]s, ready for [`Metadata::from_blocks`] or a
    /// [`MetadataExtractor`](gamut_metadata::MetadataExtractor) with a chosen
    /// [`ConflictPolicy`](gamut_metadata::ConflictPolicy): the EXIF TIFF stream, the XMP packet
    /// and the reassembled ICC profile, each present only when the stream carried it.
    ///
    /// JPEG's legacy IPTC-IIM carrier (APP13) and the C2PA APP11 carriage are not located by this
    /// crate (see `STATUS.md`), so no [`MetadataBlock::IptcIim`] / [`MetadataBlock::C2pa`] is ever
    /// produced here.
    #[must_use]
    pub fn blocks(&self) -> Vec<MetadataBlock<'_>> {
        let mut blocks = Vec::new();
        if let Some(exif) = &self.exif {
            blocks.push(MetadataBlock::Exif(exif));
        }
        if let Some(xmp) = &self.xmp {
            blocks.push(MetadataBlock::Xmp(xmp));
        }
        if let Some(icc) = &self.icc {
            blocks.push(MetadataBlock::Icc(icc));
        }
        blocks
    }

    /// Parses the located payloads into the unified [`Metadata`] model —
    /// [`Metadata::from_blocks`] over [`blocks`](Self::blocks).
    ///
    /// # Errors
    ///
    /// Returns the facade's [`MetadataError`](gamut_metadata::MetadataError) naming the carrier
    /// whose parse failed.
    ///
    /// # Example
    ///
    /// ```
    /// use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef};
    /// use gamut_jpeg::JpegEncoder;
    /// use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
    /// use gamut_metadata::Metadata;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut exif = Exif::new(ByteOrder::LittleEndian);
    /// exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![400]));
    /// let typed = Metadata::from_carriers(Some(exif), None, None);
    ///
    /// let pixels = vec![0u8; 64];
    /// let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8)?)?;
    /// let jpeg = JpegEncoder::new().with_metadata(&typed)?.encode_to_vec(image)?;
    ///
    /// let read = gamut_jpeg::metadata(&jpeg)?.metadata()?;
    /// assert_eq!(read, typed);
    /// # Ok(())
    /// # }
    /// ```
    pub fn metadata(&self) -> gamut_metadata::Result<Metadata> {
        Metadata::from_blocks(&self.blocks())
    }
}

impl JpegEncoder {
    /// Embeds a unified [`Metadata`] model: serializes it with the default
    /// [`MetadataEmbedder`] and routes each carrier to the matching raw setter
    /// ([`with_exif`](Self::with_exif), [`with_xmp`](Self::with_xmp),
    /// [`with_icc_profile`](Self::with_icc_profile)). Returns the updated encoder for chaining.
    ///
    /// The default embedder emits no legacy IPTC-IIM block (IPTC lives inside the XMP packet) and
    /// **drops** a C2PA manifest store — a store is signed over the file it came from and can never
    /// be copied into a re-encoded one; see [`gamut_metadata::C2paPolicy`]. A caller that must be
    /// told about either configures the embedder itself and calls
    /// [`with_encoded_metadata`](Self::with_encoded_metadata). Carriers absent from the model leave
    /// any earlier raw setting untouched.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] when the model does not serialize (the facade's message is
    /// carried as [`Error::detail`]), and whatever
    /// [`with_encoded_metadata`](Self::with_encoded_metadata) returns.
    pub fn with_metadata(self, meta: &Metadata) -> Result<Self> {
        let encoded = MetadataEmbedder::new().embed(meta).map_err(|e| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: metadata does not serialize")
                .with_detail(e.to_string())
        })?;
        self.with_encoded_metadata(&encoded)
    }

    /// Embeds already-serialized facade blocks — the output of a [`MetadataEmbedder`] configured by
    /// the caller — routing each present carrier to the matching raw setter. Returns the updated
    /// encoder for chaining.
    ///
    /// Only the carriers JPEG can write are accepted: EXIF (APP1), XMP (APP1) and ICC (APP2). The
    /// size caps of those segments are still checked at encode time, exactly as for the raw
    /// setters. Fields that are `None` leave any earlier raw setting untouched.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when the blocks carry a legacy IPTC-IIM stream (JPEG's APP13
    /// carriage is not implemented — see `STATUS.md`) or a C2PA manifest store (APP11 carriage is
    /// not implemented, and a store must not be copied forward in any case). Nothing is applied
    /// when an error is returned.
    pub fn with_encoded_metadata(mut self, encoded: &EncodedMetadata) -> Result<Self> {
        if encoded.iptc_iim.is_some() {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JPEG: IPTC-IIM (APP13) embedding is not supported",
            ));
        }
        if encoded.c2pa.is_some() {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "JPEG: C2PA manifest store (APP11) embedding is not supported",
            ));
        }
        if let Some(exif) = &encoded.exif {
            self = self.with_exif(exif);
        }
        if let Some(xmp) = &encoded.xmp {
            self = self.with_xmp(xmp);
        }
        if let Some(icc) = &encoded.icc {
            self = self.with_icc_profile(icc);
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::{Dimensions, EncodeImage, ErrorKind, Gray8, ImageRef};
    use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
    use gamut_metadata::icc::{
        ColorSpace, DeviceClass, IccProfile, ProfileHeader, Signature, TagData,
    };
    use gamut_metadata::xmp::{WellKnownNs, XmpMeta};

    use super::*;

    /// A typed model with all three JPEG-writable carriers populated, normalised through one
    /// embed → extract pass so it is an *extracted* model: the facade's keystone equality is
    /// extract → embed → extract, and a hand-built model differs from its parsed form in fields the
    /// serializer stamps (the ICC header's `size`).
    fn typed() -> Metadata {
        let mut exif = Exif::new(ByteOrder::LittleEndian);
        exif.set_tag(ExifTag::Make, Value::Ascii("gamut".to_owned()));
        let mut xmp = XmpMeta::new();
        xmp.set_text(WellKnownNs::Xmp.uri(), "CreatorTool", "gamut");
        let icc = IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
            tags: Vec::new(),
        };
        let encoded = Metadata::from_carriers(Some(exif), Some(xmp), Some(icc))
            .encode()
            .unwrap();
        Metadata::from_blocks(&[
            MetadataBlock::Exif(encoded.exif.as_deref().unwrap()),
            MetadataBlock::Xmp(encoded.xmp.as_deref().unwrap()),
            MetadataBlock::Icc(encoded.icc.as_deref().unwrap()),
        ])
        .unwrap()
    }

    /// Encodes an 8×8 grayscale image with `encoder`.
    fn encode(encoder: JpegEncoder) -> Vec<u8> {
        let pixels = vec![128u8; 64];
        let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
        encoder.encode_to_vec(image).unwrap()
    }

    #[test]
    fn blocks_expose_each_located_payload_in_facade_form() {
        let meta = JpegMetadata {
            exif: Some(b"II*\0".to_vec()),
            xmp: Some(b"<x:xmpmeta/>".to_vec()),
            icc: Some(vec![7u8; 4]),
        };
        assert_eq!(
            meta.blocks(),
            vec![
                MetadataBlock::Exif(b"II*\0"),
                MetadataBlock::Xmp(b"<x:xmpmeta/>"),
                MetadataBlock::Icc(&[7u8; 4]),
            ]
        );
        assert!(JpegMetadata::default().blocks().is_empty());
    }

    #[test]
    fn typed_metadata_round_trips_through_the_stream() {
        // The facade's keystone equality, extended through the APP segments: every carrier the
        // model holds comes back as the same typed model.
        let typed = typed();
        let jpeg = encode(JpegEncoder::new().with_metadata(&typed).unwrap());
        let read = crate::metadata(&jpeg).unwrap();
        assert!(read.exif.is_some() && read.xmp.is_some() && read.icc.is_some());
        assert_eq!(read.metadata().unwrap(), typed);
    }

    #[test]
    fn an_empty_model_embeds_nothing() {
        let jpeg = encode(
            JpegEncoder::new()
                .with_metadata(&Metadata::default())
                .unwrap(),
        );
        assert_eq!(crate::metadata(&jpeg).unwrap(), JpegMetadata::default());
    }

    #[test]
    fn a_manifest_store_is_never_copied_forward() {
        // The facade's policy, observed at this crate's boundary: the model's C2PA store produces
        // no segment, and the other carriers are unaffected.
        let mut typed = typed();
        typed.c2pa = Some(b"\0\0\0\x14jumbc2pa".to_vec());
        let jpeg = encode(JpegEncoder::new().with_metadata(&typed).unwrap());
        let read = crate::metadata(&jpeg).unwrap().metadata().unwrap();
        assert_eq!(read.c2pa, None);
        typed.c2pa = None;
        assert_eq!(read, typed);
    }

    #[test]
    fn unwritable_carriers_are_typed_unsupported_errors() {
        let mut iim = EncodedMetadata::default();
        iim.iptc_iim = Some(vec![0x1c, 0x02, 0x05]);
        let err = JpegEncoder::new().with_encoded_metadata(&iim).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert_eq!(
            err.static_message(),
            Some("JPEG: IPTC-IIM (APP13) embedding is not supported")
        );

        let mut c2pa = EncodedMetadata::default();
        c2pa.c2pa = Some(vec![0u8; 4]);
        let err = JpegEncoder::new().with_encoded_metadata(&c2pa).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert_eq!(
            err.static_message(),
            Some("JPEG: C2PA manifest store (APP11) embedding is not supported")
        );
    }

    #[test]
    fn encoded_blocks_route_to_the_raw_setters() {
        // Each present field lands in its APP segment; an absent one leaves an earlier setting.
        let encoded = typed().encode().unwrap();
        let jpeg = encode(JpegEncoder::new().with_encoded_metadata(&encoded).unwrap());
        let read = crate::metadata(&jpeg).unwrap();
        // `EncodedMetadata::exif` carries the `Exif\0\0` signature; the stream stores the TIFF.
        assert_eq!(
            read.exif.as_deref(),
            encoded
                .exif
                .as_deref()
                .and_then(|e| e.strip_prefix(b"Exif\0\0"))
        );
        assert_eq!(read.xmp, encoded.xmp);
        assert_eq!(read.icc, encoded.icc);

        let mut only_icc = EncodedMetadata::default();
        only_icc.icc = encoded.icc.clone();
        let jpeg = encode(
            JpegEncoder::new()
                .with_xmp(b"<x:xmpmeta/>")
                .with_encoded_metadata(&only_icc)
                .unwrap(),
        );
        let read = crate::metadata(&jpeg).unwrap();
        assert_eq!(read.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        assert_eq!(read.icc, encoded.icc);
        assert_eq!(read.exif, None);
    }

    #[test]
    fn a_model_that_does_not_serialize_is_invalid_input_with_the_facade_detail() {
        // The facade's own serialization failure: an ICC model with a duplicate tag signature is
        // rejected by `gamut-icc`'s writer (ICC.1:2022 §7.3), and the encoder reports it as this
        // crate's error with the facade's message carried as detail.
        let duplicate = (Signature(*b"wtpt"), TagData::Xyz(Vec::new()));
        let bad_icc = IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
            tags: vec![duplicate.clone(), duplicate],
        };
        let model = Metadata::from_carriers(None, None, Some(bad_icc));
        let err = JpegEncoder::new().with_metadata(&model).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.static_message(),
            Some("JPEG: metadata does not serialize")
        );
        assert_eq!(err.detail(), Some("ICC: icc: duplicate tag signature"));
    }
}
