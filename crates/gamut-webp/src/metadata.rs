//! Embedded metadata: the `ICCP` colour profile and the `EXIF` / `XMP ` chunks of a WebP file
//! (RFC 9649 §2.7.2-§2.7.3), plus the `C2PA` manifest store of C2PA 2.4 §A.3.7.
//!
//! Payloads cross this boundary **verbatim**. The crate neither parses nor re-serializes them, so a
//! profile or packet read by [`WebpDecoder::metadata`](crate::WebpDecoder::metadata) is
//! byte-for-byte the one a writer embedded — the property the typed metadata crates
//! (`gamut-exif`, `gamut-icc`, `gamut-xmp`, and the `gamut-metadata` facade) need in order to
//! borrow the bytes without a copy or a re-frame. Encoding is the mirror image:
//! [`WebpEncoder::with_exif`](crate::WebpEncoder::with_exif),
//! [`with_xmp`](crate::WebpEncoder::with_xmp),
//! [`with_icc_profile`](crate::WebpEncoder::with_icc_profile) and
//! [`with_c2pa`](crate::WebpEncoder::with_c2pa).
//!
//! The C2PA locators are `gamut-riff`'s — the chunk walk that finds the store is there, so the
//! facts and the words about them are stated once, beside it (issue #530) — and re-exported at the
//! crate root so `gamut inspect` and other hosts need no direct `gamut-riff` dependency:
//! [`c2pa_span`](crate::c2pa_span) is the read-side twin of the range
//! [`WebpEncoder::encode_with_report`](crate::WebpEncoder::encode_with_report) returns — the same
//! walk over the same bytes — so a file's exclusion range does not depend on who computed it.

pub use gamut_riff::{
    C2PA_NOT_VALIDATED, C2paChunkPosition, C2paStoreSummary, C2paSummary, C2paUnreadChunk,
    c2pa_span, c2pa_summary,
};

/// The `metadata` feature: the encoder half of the typed [`gamut_metadata`] wiring.
///
/// [`WebpDecoder::metadata`](crate::WebpDecoder::metadata) locates the `EXIF` / `XMP ` / `ICCP`
/// payloads as a [`gamut_core::RawMetadata`], which the facade reads through its own
/// `TypedMetadata` impl; this module makes [`WebpEncoder`](crate::WebpEncoder) a
/// [`MetadataSink`](gamut_metadata::MetadataSink) over its raw setters, so `with_metadata` /
/// `with_encoded_metadata` and their precedence are the facade's. The dependency direction is
/// `gamut-webp → gamut-metadata`, and the module is compiled only with the `metadata` Cargo
/// feature, so a plain WebP consumer never pays for the metadata crates.
#[cfg(feature = "metadata")]
mod typed {
    use gamut_core::Result;
    use gamut_metadata::{Carrier, MetadataSink};

    use crate::WebpEncoder;

    /// Writes the three carriers a RIFF/WebP file defines chunks for: EXIF to
    /// [`with_exif`](WebpEncoder::with_exif) (the bare TIFF stream an `EXIF` chunk holds,
    /// RFC 9649 §2.7.3), XMP to [`with_xmp`](WebpEncoder::with_xmp) and ICC to
    /// [`with_icc_profile`](WebpEncoder::with_icc_profile). A legacy IPTC-IIM stream is refused —
    /// RIFF defines no IPTC-IIM chunk — and so is a C2PA manifest store, which is signed over the
    /// one file it was made for and so goes through [`with_c2pa`](WebpEncoder::with_c2pa) /
    /// [`with_c2pa_reserved`](WebpEncoder::with_c2pa_reserved) instead.
    ///
    /// ```
    /// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
    /// use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
    /// use gamut_webp::{Metadata, MetadataSink, TypedMetadata, WebpDecoder, WebpEncoder};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut exif = Exif::new(ByteOrder::LittleEndian);
    /// exif.set_tag(ExifTag::PhotographicSensitivity, Value::Short(vec![400]));
    /// let typed = Metadata::from_carriers(Some(exif), None, None);
    ///
    /// let pixels = [10u8, 20, 30];
    /// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(1, 1)?)?;
    /// let mut file = Vec::new();
    /// WebpEncoder::lossless()
    ///     .with_metadata(&typed)?
    ///     .encode_image(image, &mut file)?;
    ///
    /// assert_eq!(WebpDecoder::new().metadata(&file)?.metadata()?, typed);
    /// # Ok(())
    /// # }
    /// ```
    impl MetadataSink for WebpEncoder {
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
        use gamut_core::{Dimensions, EncodeImage, ErrorKind, ImageRef, RawMetadata, Rgb8};
        use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
        use gamut_metadata::icc::{
            ColorSpace, DeviceClass, IccProfile, ProfileHeader, Tag, TagData,
        };
        use gamut_metadata::xmp::{WellKnownNs, XmpMeta};
        use gamut_metadata::{EncodedMetadata, Metadata, MetadataBlock, TypedMetadata};

        use super::*;
        use crate::WebpDecoder;

        /// The located payloads of `file`.
        fn read(file: &[u8]) -> RawMetadata {
            WebpDecoder::new().metadata(file).unwrap()
        }

        /// A typed model with the three facade-writable carriers populated, normalised through one
        /// embed → extract pass so it is an *extracted* model (the ICC header's `size` is stamped
        /// by the serializer).
        fn typed() -> Metadata {
            let mut exif = Exif::new(ByteOrder::BigEndian);
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

        /// Encodes a 1×1 RGB image with `encoder`.
        fn encode(encoder: &WebpEncoder) -> Vec<u8> {
            let pixels = [10u8, 20, 30];
            let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(1, 1).unwrap()).unwrap();
            let mut out = Vec::new();
            encoder.encode_image(image, &mut out).unwrap();
            out
        }

        #[test]
        fn typed_metadata_round_trips_through_the_container() {
            // The facade's keystone equality, extended through the RIFF chunks.
            let typed = typed();
            let file = encode(&WebpEncoder::lossless().with_metadata(&typed).unwrap());
            let read = read(&file);
            assert!(read.exif.is_some() && read.xmp.is_some() && read.icc.is_some());
            assert_eq!(read.metadata().unwrap(), typed);
        }

        #[test]
        fn exiv2_reads_what_with_metadata_embeds() {
            // Container-level differential: exiv2's own WebP reader finds each carrier where
            // RFC 9649 §2.7 puts it, not only this crate's reader.
            let typed = typed();
            let file = encode(&WebpEncoder::lossless().with_metadata(&typed).unwrap());
            assert_eq!(
                exiv2_oracle::image_exif_get(&file, "Exif.Image.Make").unwrap(),
                "gamut"
            );
            assert_eq!(
                exiv2_oracle::image_xmp_get(&file, "Xmp.xmp.CreatorTool").unwrap(),
                "gamut"
            );
            assert_eq!(
                exiv2_oracle::image_icc(&file).unwrap(),
                typed.to_blocks().unwrap().icc
            );
        }

        #[test]
        fn the_typed_reader_reads_what_exiv2_embeds() {
            // The other direction: exiv2 writes an `EXIF` and an `XMP ` chunk into a file this
            // crate encoded bare, and the typed reader finds both values.
            let bare = encode(&WebpEncoder::lossless());
            let file = exiv2_oracle::image_set(
                &bare,
                Some(("Exif.Photo.ISOSpeedRatings", "400")),
                Some(("Xmp.xmp.CreatorTool", "exiv2")),
            )
            .unwrap();
            let read = read(&file).metadata().unwrap();
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
            let file = encode(&WebpEncoder::lossless().with_metadata(&typed).unwrap());
            assert_eq!(gamut_riff::MetadataChunks::read(&file).unwrap().c2pa, None);
            typed.c2pa = None;
            assert_eq!(read(&file).metadata().unwrap(), typed);
        }

        #[test]
        fn encoded_blocks_route_to_the_raw_setters() {
            // Each present field lands in its chunk — the EXIF one without the facade's
            // signature — and an absent one leaves an earlier setting.
            let encoded = typed().to_blocks().unwrap();
            let file = encode(
                &WebpEncoder::lossless()
                    .with_encoded_metadata(&encoded)
                    .unwrap(),
            );
            let raw = read(&file);
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
            let file = encode(
                &WebpEncoder::lossless()
                    .with_xmp(b"<x:xmpmeta/>")
                    .with_encoded_metadata(&only_icc)
                    .unwrap(),
            );
            let raw = read(&file);
            assert_eq!(raw.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
            assert_eq!(raw.icc, encoded.icc);
            assert_eq!(raw.exif, None);
        }

        #[test]
        fn a_bare_exif_stream_is_written_unchanged() {
            // `strip_prefix` must not eat a stream that never carried the signature.
            let mut bare = EncodedMetadata::default();
            bare.exif = Some(b"MM\0*\0\0\0\x08".to_vec());
            let file = encode(
                &WebpEncoder::lossless()
                    .with_encoded_metadata(&bare)
                    .unwrap(),
            );
            assert_eq!(read(&file).exif, bare.exif);
        }

        #[test]
        fn carriers_without_a_riff_chunk_are_refused() {
            // `CARRIERS` lists exactly the three chunk carriers: an IPTC-IIM stream (RIFF has no
            // chunk for it) and a manifest store are refused, and nothing is applied.
            assert_eq!(
                <WebpEncoder as MetadataSink>::CARRIERS,
                &[Carrier::Exif, Carrier::Xmp, Carrier::Icc]
            );
            let mut iim = EncodedMetadata::default();
            iim.iptc_iim = Some(vec![0x1c, 0x02, 0x05]);
            let mut c2pa = EncodedMetadata::default();
            c2pa.c2pa = Some(b"\0\0\0\x08jumb".to_vec());
            for blocks in [iim, c2pa] {
                let err = WebpEncoder::lossless()
                    .with_encoded_metadata(&blocks)
                    .unwrap_err();
                assert_eq!(err.kind(), ErrorKind::Unsupported);
            }
        }

        #[test]
        fn a_model_that_does_not_serialize_is_an_error() {
            // An ICC model with a duplicate tag signature is rejected by `gamut-icc`'s writer
            // (ICC.1:2022 §7.3); the sink hands the facade's error back rather than embedding.
            let duplicate = Tag::new(*b"wtpt", TagData::Xyz(Vec::new()));
            let bad_icc = IccProfile {
                header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
                tags: vec![duplicate.clone(), duplicate],
            };
            let model = Metadata::from_carriers(None, None, Some(bad_icc));
            assert!(WebpEncoder::lossless().with_metadata(&model).is_err());
        }

        #[test]
        fn the_sink_setters_are_the_raw_setters() {
            let file = encode(
                &WebpEncoder::lossless()
                    .write_exif(b"MM\0*")
                    .unwrap()
                    .write_xmp(b"<x/>")
                    .unwrap()
                    .write_icc(b"icc")
                    .unwrap(),
            );
            let mut expected = RawMetadata::default();
            expected.exif = Some(b"MM\0*".to_vec());
            expected.xmp = Some(b"<x/>".to_vec());
            expected.icc = Some(b"icc".to_vec());
            assert_eq!(read(&file), expected);
            // The typed reader is the facade's over the same value.
            assert_eq!(
                read(&file).blocks().unwrap(),
                vec![
                    MetadataBlock::Exif(b"MM\0*"),
                    MetadataBlock::Xmp(b"<x/>"),
                    MetadataBlock::Icc(b"icc"),
                ]
            );
        }
    }
}
