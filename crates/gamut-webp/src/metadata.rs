//! Embedded metadata: the `ICCP` colour profile and the `EXIF` / `XMP ` chunks of a WebP file
//! (RFC 9649 §2.7.2-§2.7.3), plus the `C2PA` manifest store of C2PA 2.4 §A.3.7.
//!
//! Payloads cross this boundary **verbatim**. The crate neither parses nor re-serializes them, so a
//! profile or packet read by [`metadata`] is byte-for-byte the one a writer embedded — the property
//! the typed metadata crates (`gamut-exif`, `gamut-icc`, `gamut-xmp`, and the `gamut-metadata`
//! facade) need in order to borrow the bytes without a copy or a re-frame. Encoding is the mirror
//! image: [`WebpEncoder::with_exif`](crate::WebpEncoder::with_exif),
//! [`with_xmp`](crate::WebpEncoder::with_xmp),
//! [`with_icc_profile`](crate::WebpEncoder::with_icc_profile) and
//! [`with_c2pa`](crate::WebpEncoder::with_c2pa).

use core::ops::Range;

use gamut_core::Result;
use gamut_riff::MetadataChunks;

/// Embedded metadata read from a WebP file by [`metadata`].
///
/// Each payload is the raw chunk content, in the form the dedicated metadata crates parse (and
/// [`gamut-metadata`](https://crates.io/crates/gamut-metadata)'s `MetadataBlock` borrows) directly.
/// Marked `#[non_exhaustive]` so a further carrier can be added without a breaking change.
///
/// # Example
///
/// The extracted payload is byte-for-byte identical to the one given to the encoder, so it can be
/// borrowed straight into a typed metadata facade:
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_webp::WebpEncoder;
///
/// let icc = b"opaque ICC profile bytes";
/// let pixels = [10u8, 20, 30];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(1, 1)?)?;
/// let mut file = Vec::new();
/// WebpEncoder::lossless()
///     .with_icc_profile(icc)
///     .encode_image(image, &mut file)?;
///
/// let meta = gamut_webp::metadata(&file)?;
/// assert_eq!(meta.icc.as_deref(), Some(icc.as_slice()));
/// # Ok::<(), gamut_core::Error>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct WebpMetadata {
    /// The `EXIF` chunk payload: Exif metadata, carried bare (a WebP file stores no `"Exif\0\0"`
    /// signature, unlike a JPEG APP1 segment). Feed as `gamut_metadata::MetadataBlock::Exif`.
    pub exif: Option<Vec<u8>>,
    /// The `XMP ` chunk payload: an XMP packet. Feed as `MetadataBlock::Xmp`.
    pub xmp: Option<Vec<u8>>,
    /// The `ICCP` chunk payload: an ICC colour profile. `None` means sRGB is assumed (§2.7.2). Feed
    /// as `MetadataBlock::Icc`.
    pub icc: Option<Vec<u8>>,
    /// The `C2PA` chunk payload: a C2PA manifest store, carried opaquely (C2PA 2.4 §A.3.7). Nothing
    /// in gamut parses, hashes, signs or validates it; [`c2pa_span`] reports the byte range a
    /// `c2pa.hash.data` assertion excludes so a C2PA implementation can.
    pub c2pa: Option<Vec<u8>>,
}

/// Reads a WebP file's embedded metadata chunks without decoding any pixels.
///
/// Walks the top-level RIFF chunks and collects the four metadata payloads — `EXIF` and `XMP `
/// metadata (§2.7.3), the `ICCP` colour profile (§2.7.2) and the `C2PA` manifest store (C2PA 2.4
/// §A.3.7) — copying each out verbatim. The spec
/// permits at most one chunk of each kind and lets readers keep only the first, which is what this
/// does; the `VP8X` feature flags are advisory, so a payload is surfaced because its chunk is
/// present rather than because a flag advertises it. A simple (single-bitstream) file, which cannot
/// conformantly carry metadata at all, yields [`WebpMetadata::default`].
///
/// # Errors
///
/// Returns [`Error::InvalidInput`](gamut_core::Error::InvalidInput) if `data` is not a valid
/// RIFF/WebP file, or if a chunk's declared size runs past the end of the data.
///
/// # Example
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_webp::{WebpEncoder, WebpMetadata};
///
/// let pixels = [1u8, 2, 3];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(1, 1)?)?;
/// let mut file = Vec::new();
/// WebpEncoder::lossless().encode_image(image, &mut file)?;
/// assert_eq!(gamut_webp::metadata(&file)?, WebpMetadata::default());
/// # Ok::<(), gamut_core::Error>(())
/// ```
pub fn metadata(data: &[u8]) -> Result<WebpMetadata> {
    let chunks = MetadataChunks::read(data)?;
    Ok(WebpMetadata {
        exif: chunks.exif.map(<[u8]>::to_vec),
        xmp: chunks.xmp.map(<[u8]>::to_vec),
        icc: chunks.icc.map(<[u8]>::to_vec),
        c2pa: chunks.c2pa.map(<[u8]>::to_vec),
    })
}

/// Reports the byte range the `C2PA` chunk occupies in `data`, or `None` if the file carries no
/// manifest store.
///
/// The range covers the chunk's **whole** span — the four identifier bytes, the four-byte size
/// field and the payload — as [`gamut_riff::c2pa_span`] reports it; that function's docs state the
/// span's two limits: it does not let a hash survive a store resize (the RIFF *File Size* field at
/// bytes `4..8` changes too and is hashed), and it overlaps the chunk header, which C2PA 2.4
/// §18.5.1 forbids a data-hash exclusion to do (the §18.5.1 range is `start + 8..end`). The RIFF
/// pad byte that follows an odd-length store (RFC 9649 §2.3) is outside the range; it is framing
/// the container adds, not store.
///
/// This is the read-side twin of the range
/// [`WebpEncoder::encode_with_report`](crate::WebpEncoder::encode_with_report) returns — the same
/// walk over the same bytes — so a file's exclusion range does not depend on who computed it. No
/// pixels are decoded.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`](gamut_core::Error::InvalidInput) if `data` is not a valid
/// RIFF/WebP file, or if a chunk's declared size runs past the end of the data.
///
/// # Example
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_webp::WebpEncoder;
///
/// let store = b"an opaque C2PA manifest store";
/// let pixels = [1u8, 2, 3];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(1, 1)?)?;
/// let mut file = Vec::new();
/// WebpEncoder::lossless()
///     .with_c2pa(store)
///     .encode_image(image, &mut file)?;
///
/// let span = gamut_webp::c2pa_span(&file)?.expect("the store was embedded");
/// assert_eq!(&file[span.start + 8..span.end], store.as_slice());
/// # Ok::<(), gamut_core::Error>(())
/// ```
pub fn c2pa_span(data: &[u8]) -> Result<Option<Range<usize>>> {
    gamut_riff::c2pa_span(data)
}

// The non-validating C2PA report is `gamut-riff`'s — the chunk walk that locates the store is
// there, so the facts and the words about them are stated once, beside it (issue #530) — and
// re-exported here so `gamut inspect` and other hosts need no direct `gamut-riff` dependency.
pub use gamut_riff::{
    C2PA_NOT_VALIDATED, C2paChunkPosition, C2paStoreSummary, C2paSummary, C2paUnreadChunk,
    c2pa_summary,
};

/// The `metadata` feature: typed [`gamut_metadata`] wiring over the raw chunk surface above.
///
/// [`metadata`] locates the `EXIF` / `XMP ` / `ICCP` / `C2PA` payloads as bytes; this module hands
/// them to the facade as `MetadataBlock`s and turns a facade `Metadata` back into the encoder's raw
/// setters. The dependency direction is `gamut-webp → gamut-metadata`, and the module is compiled
/// only with the `metadata` Cargo feature, so a plain WebP consumer never pays for the metadata
/// crates.
#[cfg(feature = "metadata")]
mod typed {
    use gamut_core::{Error, Result};
    use gamut_metadata::{
        EncodedMetadata, Metadata, MetadataBlock, MetadataEmbedder, TypedMetadata,
    };

    use super::WebpMetadata;
    use crate::WebpEncoder;

    /// The facade's [`TypedMetadata`] over the located chunk payloads; `metadata` is the trait's
    /// default, [`Metadata::from_blocks`] over [`blocks`](TypedMetadata::blocks).
    ///
    /// ```
    /// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
    /// use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
    /// use gamut_webp::{Metadata, TypedMetadata, WebpEncoder};
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
    /// assert_eq!(gamut_webp::metadata(&file)?.metadata()?, typed);
    /// # Ok(())
    /// # }
    /// ```
    impl TypedMetadata for WebpMetadata {
        /// The located payloads as [`MetadataBlock`]s: the `EXIF` chunk's TIFF stream, the `XMP `
        /// packet, the `ICCP` profile and the `C2PA` manifest store, each present only when the
        /// file carried it. RIFF has no IPTC-IIM chunk, so no [`MetadataBlock::IptcIim`] is ever
        /// produced here.
        ///
        /// # Errors
        ///
        /// Never: every payload was already copied out as bytes when
        /// [`metadata`](crate::metadata) built this value. The signature is the trait's, shared
        /// with containers that resolve payloads lazily.
        fn blocks(&self) -> gamut_metadata::Result<Vec<MetadataBlock<'_>>> {
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
            if let Some(c2pa) = &self.c2pa {
                blocks.push(MetadataBlock::C2pa(c2pa));
            }
            Ok(blocks)
        }
    }

    impl WebpEncoder {
        /// Embeds a unified [`Metadata`] model: serializes it with the default
        /// [`MetadataEmbedder`] and routes each carrier to the matching raw setter
        /// ([`with_exif`](Self::with_exif), [`with_xmp`](Self::with_xmp),
        /// [`with_icc_profile`](Self::with_icc_profile)). Returns the updated encoder for
        /// chaining.
        ///
        /// The default embedder emits no legacy IPTC-IIM block (IPTC lives inside the XMP packet)
        /// and **drops** a C2PA manifest store — a store is signed over the file it came from and
        /// can never be copied into a re-encoded one; see [`gamut_metadata::C2paPolicy`]. A caller
        /// that must be told about either configures the embedder itself and calls
        /// [`with_encoded_metadata`](Self::with_encoded_metadata). Carriers absent from the model
        /// leave any earlier raw setting untouched.
        ///
        /// # Errors
        ///
        /// Returns [`Error::InvalidInput`] when the model does not serialize (the facade's message
        /// is carried as [`Error::detail`]), and whatever
        /// [`with_encoded_metadata`](Self::with_encoded_metadata) returns.
        pub fn with_metadata(self, meta: &Metadata) -> Result<Self> {
            let encoded = MetadataEmbedder::new().embed(meta).map_err(|e| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "WebP: metadata does not serialize")
                    .with_detail(e.to_string())
            })?;
            self.with_encoded_metadata(&encoded)
        }

        /// Embeds already-serialized facade blocks — the output of a [`MetadataEmbedder`]
        /// configured by the caller — routing each present carrier to the matching raw setter.
        /// Returns the updated encoder for chaining.
        ///
        /// EXIF goes to [`with_exif`](Self::with_exif) with the facade's `Exif\0\0` signature
        /// stripped (a WebP `EXIF` chunk carries the bare TIFF stream, RFC 9649 §2.7.3), XMP to
        /// [`with_xmp`](Self::with_xmp) and ICC to [`with_icc_profile`](Self::with_icc_profile).
        /// Fields that are `None` leave any earlier raw setting untouched.
        ///
        /// # Errors
        ///
        /// Returns [`Error::Unsupported`] when the blocks carry a legacy IPTC-IIM stream (RIFF
        /// defines no IPTC-IIM chunk) or a C2PA manifest store. A store is never embedded from
        /// metadata blocks: it is signed over the bytes of the one file it was made for, so a
        /// store computed for this output goes through [`with_c2pa`](Self::with_c2pa) /
        /// [`with_c2pa_reserved`](Self::with_c2pa_reserved) instead. Nothing is applied when an
        /// error is returned.
        pub fn with_encoded_metadata(mut self, encoded: &EncodedMetadata) -> Result<Self> {
            if encoded.iptc_iim.is_some() {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "WebP: IPTC-IIM has no RIFF chunk",
                ));
            }
            if encoded.c2pa.is_some() {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "WebP: a C2PA manifest store is not embedded from metadata blocks",
                ));
            }
            if let Some(exif) = &encoded.exif {
                self = self.with_exif(exif.strip_prefix(b"Exif\0\0").unwrap_or(exif));
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
        use gamut_core::{Dimensions, EncodeImage, ErrorKind, ImageRef, Rgb8};
        use gamut_metadata::exif::{ByteOrder, Exif, ExifTag, Value};
        use gamut_metadata::icc::{
            ColorSpace, DeviceClass, IccProfile, ProfileHeader, Signature, TagData,
        };
        use gamut_metadata::xmp::{WellKnownNs, XmpMeta};

        use super::*;

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
                .encode()
                .unwrap();
            Metadata::from_blocks(&[
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
        fn blocks_expose_each_located_payload_in_facade_form() {
            let meta = WebpMetadata {
                exif: Some(b"II*\0".to_vec()),
                xmp: Some(b"<x:xmpmeta/>".to_vec()),
                icc: Some(vec![7u8; 4]),
                c2pa: Some(b"jumb".to_vec()),
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
            assert!(WebpMetadata::default().blocks().unwrap().is_empty());
        }

        #[test]
        fn typed_metadata_round_trips_through_the_container() {
            // The facade's keystone equality, extended through the RIFF chunks.
            let typed = typed();
            let file = encode(&WebpEncoder::lossless().with_metadata(&typed).unwrap());
            let read = crate::metadata(&file).unwrap();
            assert!(read.exif.is_some() && read.xmp.is_some() && read.icc.is_some());
            assert_eq!(read.metadata().unwrap(), typed);
        }

        #[test]
        fn a_manifest_store_is_never_copied_forward() {
            let mut typed = typed();
            typed.c2pa = Some(b"\0\0\0\x14jumbc2pa".to_vec());
            let file = encode(&WebpEncoder::lossless().with_metadata(&typed).unwrap());
            let read = crate::metadata(&file).unwrap().metadata().unwrap();
            assert_eq!(read.c2pa, None);
            typed.c2pa = None;
            assert_eq!(read, typed);
        }

        #[test]
        fn encoded_blocks_route_to_the_raw_setters() {
            // Each present field lands in its chunk — the EXIF one without the facade's
            // signature — and an absent one leaves an earlier setting.
            let encoded = typed().encode().unwrap();
            let file = encode(
                &WebpEncoder::lossless()
                    .with_encoded_metadata(&encoded)
                    .unwrap(),
            );
            let read = crate::metadata(&file).unwrap();
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
            let file = encode(
                &WebpEncoder::lossless()
                    .with_xmp(b"<x:xmpmeta/>")
                    .with_encoded_metadata(&only_icc)
                    .unwrap(),
            );
            let read = crate::metadata(&file).unwrap();
            assert_eq!(read.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
            assert_eq!(read.icc, encoded.icc);
            assert_eq!(read.exif, None);
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
            assert_eq!(crate::metadata(&file).unwrap().exif, bare.exif);
        }

        #[test]
        fn unembeddable_carriers_are_typed_unsupported_errors() {
            let mut iim = EncodedMetadata::default();
            iim.iptc_iim = Some(vec![0x1c, 0x02, 0x05]);
            let err = WebpEncoder::lossless()
                .with_encoded_metadata(&iim)
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Unsupported);
            assert_eq!(
                err.static_message(),
                Some("WebP: IPTC-IIM has no RIFF chunk")
            );

            let mut c2pa = EncodedMetadata::default();
            c2pa.c2pa = Some(b"\0\0\0\x08jumb".to_vec());
            let err = WebpEncoder::lossless()
                .with_encoded_metadata(&c2pa)
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Unsupported);
            assert_eq!(
                err.static_message(),
                Some("WebP: a C2PA manifest store is not embedded from metadata blocks")
            );
        }

        #[test]
        fn a_model_that_does_not_serialize_is_invalid_input_with_the_facade_detail() {
            // An ICC model with a duplicate tag signature is rejected by `gamut-icc`'s writer
            // (ICC.1:2022 §7.3).
            let duplicate = (Signature(*b"wtpt"), TagData::Xyz(Vec::new()));
            let bad_icc = IccProfile {
                header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
                tags: vec![duplicate.clone(), duplicate],
            };
            let model = Metadata::from_carriers(None, None, Some(bad_icc));
            let err = WebpEncoder::lossless().with_metadata(&model).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
            assert_eq!(
                err.static_message(),
                Some("WebP: metadata does not serialize")
            );
            assert_eq!(err.detail(), Some("ICC: icc: duplicate tag signature"));
        }
    }
}
