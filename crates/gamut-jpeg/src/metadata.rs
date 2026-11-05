//! The `metadata` feature: the encoder half of the typed [`gamut_metadata`] wiring.
//!
//! [`JpegDecoder::metadata`](crate::JpegDecoder::metadata) locates the APP1 EXIF / APP1 XMP / APP2
//! `ICC_PROFILE` payloads as a [`gamut_core::RawMetadata`], which the facade reads through its own
//! `TypedMetadata` impl; this module makes [`JpegEncoder`] a [`MetadataSink`] over its raw setters,
//! so `with_metadata` / `with_encoded_metadata` and their precedence are the facade's. The
//! dependency direction is `gamut-jpeg → gamut-metadata` (the facade never learns about JPEG
//! segments), and the module is compiled only with the `metadata` Cargo feature so a plain JPEG
//! consumer never pays for the metadata crates.
//!
//! The block boundaries are the ones the facade documents: the EXIF block is the **TIFF stream**
//! (`II`/`MM` first; [`JpegEncoder::with_exif`] re-adds the `Exif\0\0` APP1 signature), the XMP
//! block is the `xpacket` with the namespace URI stripped, and the ICC block is the profile the
//! encoder splits across its APP2 chunks.

use gamut_core::Result;
use gamut_metadata::{Carrier, MetadataSink};

use crate::JpegEncoder;

/// Writes the three carriers JPEG's APP segments hold: EXIF to
/// [`with_exif`](JpegEncoder::with_exif) (APP1), XMP to [`with_xmp`](JpegEncoder::with_xmp) (APP1)
/// and ICC to [`with_icc_profile`](JpegEncoder::with_icc_profile) (APP2). The size caps of those
/// segments are still checked at encode time, exactly as for the raw setters. A legacy IPTC-IIM
/// stream is refused (APP13 carriage is not implemented — see `STATUS.md`), and so is a C2PA
/// manifest store (APP11 carriage is not implemented, and a store is never copied forward).
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef};
/// use gamut_jpeg::{JpegDecoder, JpegEncoder, MetadataSink, TypedMetadata};
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
/// let read = JpegDecoder::new().metadata(&jpeg)?.metadata()?;
/// assert_eq!(read, typed);
/// # Ok(())
/// # }
/// ```
impl MetadataSink for JpegEncoder {
    const CARRIERS: &'static [Carrier] = &[Carrier::Exif, Carrier::Xmp, Carrier::Icc];

    fn write_exif(self, tiff: &[u8]) -> Result<Self> {
        Ok(self.with_exif(tiff))
    }

    fn write_xmp(self, packet: &[u8]) -> Result<Self> {
        Ok(self.with_xmp(packet))
    }

    fn write_icc(self, profile: &[u8]) -> Result<Self> {
        Ok(self.with_icc_profile(profile))
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef, RawMetadata};
    use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
    use gamut_metadata::icc::{ColorSpace, DeviceClass, IccProfile, ProfileHeader};
    use gamut_metadata::xmp::{WellKnownNs, XmpMeta};
    use gamut_metadata::{EncodedMetadata, Metadata, MetadataBlock, TypedMetadata};

    use super::*;
    use crate::JpegDecoder;

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
    fn encode(encoder: JpegEncoder) -> Vec<u8> {
        let pixels = vec![128u8; 64];
        let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(8, 8).unwrap()).unwrap();
        encoder.encode_to_vec(image).unwrap()
    }

    /// The located payloads of `jpeg`.
    fn read(jpeg: &[u8]) -> RawMetadata {
        JpegDecoder::new().metadata(jpeg).unwrap()
    }

    #[test]
    fn typed_metadata_round_trips_through_the_stream() {
        // The facade's keystone equality, extended through the APP segments: every carrier the
        // model holds comes back as the same typed model.
        let typed = typed();
        let raw = read(&encode(JpegEncoder::new().with_metadata(&typed).unwrap()));
        assert!(raw.exif.is_some() && raw.xmp.is_some() && raw.icc.is_some());
        assert_eq!(raw.metadata().unwrap(), typed);
    }

    #[test]
    fn an_empty_model_embeds_nothing() {
        let jpeg = encode(
            JpegEncoder::new()
                .with_metadata(&Metadata::default())
                .unwrap(),
        );
        assert_eq!(read(&jpeg), RawMetadata::default());
    }

    #[test]
    fn a_bare_exif_stream_is_written_unchanged() {
        // A TIFF stream that never carried the `Exif\0\0` signature is stored as given.
        let mut bare = EncodedMetadata::default();
        bare.exif = Some(b"MM\0*\0\0\0\x08".to_vec());
        let raw = read(&encode(
            JpegEncoder::new().with_encoded_metadata(&bare).unwrap(),
        ));
        assert_eq!(raw.exif, bare.exif);
    }

    #[test]
    fn carriers_without_an_app_segment_are_refused() {
        // `CARRIERS` lists exactly the three APP carriers: an IPTC-IIM stream (no APP13 writer)
        // and a manifest store are refused, and nothing is applied.
        let mut iim = EncodedMetadata::default();
        iim.iptc_iim = Some(vec![0x1c, 0x02, 0x05]);
        let mut c2pa = EncodedMetadata::default();
        c2pa.c2pa = Some(b"\0\0\0\x08jumb".to_vec());
        for blocks in [iim, c2pa] {
            let err = JpegEncoder::new()
                .with_encoded_metadata(&blocks)
                .unwrap_err();
            assert_eq!(err.kind(), gamut_core::ErrorKind::Unsupported);
        }
    }

    #[test]
    fn encoded_blocks_route_to_the_raw_setters() {
        // Each present field lands in its APP segment; an absent one leaves an earlier setting.
        let encoded = typed().to_blocks().unwrap();
        let raw = read(&encode(
            JpegEncoder::new().with_encoded_metadata(&encoded).unwrap(),
        ));
        // `EncodedMetadata::exif` carries the `Exif\0\0` signature; the stream stores the TIFF.
        assert_eq!(
            raw.exif.as_deref(),
            encoded
                .exif
                .as_deref()
                .and_then(|e| e.strip_prefix(b"Exif\0\0"))
        );
        assert_eq!(raw.xmp, encoded.xmp);
        assert_eq!(raw.icc, encoded.icc);

        let mut only_icc = EncodedMetadata::default();
        only_icc.icc = encoded.icc.clone();
        let raw = read(&encode(
            JpegEncoder::new()
                .with_xmp(b"<x:xmpmeta/>")
                .with_encoded_metadata(&only_icc)
                .unwrap(),
        ));
        assert_eq!(raw.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        assert_eq!(raw.icc, encoded.icc);
        assert_eq!(raw.exif, None);
    }
}
