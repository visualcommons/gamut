//! The HEIC decode surface: [`Hevc`], HEIC's binding into the shared HEIF pipeline, and
//! [`HeifDecoder`], the [`DecodeImage`] entry point that runs a file's primary item through it.
//!
//! Everything around the coded picture — the item model, the `grid`/`iden`/`iovl` derivations,
//! the [`DecodeLimits`] size and work caps, YCbCr → RGB, alpha and the `clap`/`irot`/`imir`
//! transforms — is `gamut-heif`'s. What is HEIC's is the coded `hvc1`/`hev1` item: its `hvcC`
//! configuration ([`HevcConfig`]), the still-image IRAP constraint
//! ([`HevcConfig::validate_still_payload`]), and the [`HevcCodestreamDecoder`] backends that decode
//! it.

use std::fmt;
use std::sync::{Arc, Mutex};

use gamut_codec_abi::Registry;
use gamut_color::ChromaUpsampling;
use gamut_core::{Cicp, DecodeImage, Error, ImageBuf, RawMetadata, Result, Rgba8, Rgba16};
use gamut_heif::{AlphaChroma, Codestream, DecodeLimits, DecodeOptions, DecodedFrame, HeifImage};
use gamut_isobmff::IsoBmffImage;

use crate::backend::HevcCodestreamDecoder;
#[cfg(doc)]
use crate::container::HeifContainer;
use crate::hvcc::HevcConfig;

/// The colour an HEVC item presents with when it carries no `colr`: BT.601 matrix coefficients
/// at limited range, primaries and transfer unspecified.
///
/// With no `nclx` the matrix is undefined, and libheif assumes BT.601 for an undefined or
/// unspecified matrix; the range is limited because the HEVC VUI `video_full_range_flag` defaults
/// to 0 (ITU-T H.265 §E.3.1) and still-image content is overwhelmingly studio-swing. The SPS VUI
/// is not read.
const UNSIGNALLED_COLOUR: Cicp = Cicp {
    colour_primaries: 2,
    transfer_characteristics: 2,
    matrix_coefficients: 6,
    full_range: false,
};

/// HEIC's binding into the shared HEIF decode pipeline: the `hvc1`/`hev1` coded item, its `hvcC`
/// configuration and its [`HevcCodestreamDecoder`] backends.
///
/// Never instantiated — it names the codec as a type parameter of `gamut-heif`'s surfaces, which
/// decode **any** item of a parsed file, where [`HeifDecoder`] decodes the primary one:
///
/// ```
/// use std::sync::{Arc, Mutex};
///
/// use gamut_codec_abi::Registry;
/// use gamut_color::ChromaSubsampling;
/// use gamut_core::{Dimensions, Result};
/// use gamut_heic::{DecodeLimits, DecodedFrame, HeifContainer, Hevc, HevcCodestreamDecoder, HevcConfig};
/// use gamut_isobmff::{IsoBmffImage, Item, Property, PropertyKind};
///
/// /// A stand-in for a real HEVC decoder: a mid-grey monochrome frame.
/// struct Grey;
/// impl HevcCodestreamDecoder for Grey {
///     fn supports(&mut self, _config: &HevcConfig) -> bool {
///         true
///     }
///     fn decode_still(&mut self, _config: &HevcConfig, _payload: &[u8]) -> Result<DecodedFrame> {
///         DecodedFrame::new(Dimensions::new(2, 2)?, 8, ChromaSubsampling::Cs400, vec![128; 4], vec![], vec![])
///     }
/// }
///
/// // A minimal `hvcC`: 23-byte header, lengthSizeMinusOne = 3, numOfArrays = 0.
/// let mut hvcc = vec![0u8; 23];
/// hvcc[0] = 1;
/// hvcc[21] = 0b11;
/// let file = IsoBmffImage::new(*b"heic", vec![*b"heic", *b"mif1"], 1, vec![
///     Item::new(1, *b"hvc1")
///         .with_properties(vec![Property::new(true, PropertyKind::CodecConfiguration {
///             kind: *b"hvcC",
///             data: hvcc,
///         })])
///         // 4-byte length prefix + an IDR_W_RADL NAL (type 19: header byte 0x26).
///         .with_payload(vec![0, 0, 0, 3, 0x26, 0x01, 0xDD]),
/// ])
/// .to_bytes()?;
/// let container = HeifContainer::parse(&file)?;
/// let image = container.image();
///
/// let mut backends: Registry<dyn HevcCodestreamDecoder + Send> = Registry::new();
/// backends.push(Arc::new(Mutex::new(Grey)));
/// let frame = image.decode_item_planar::<Hevc>(image.primary_item().id(), &backends, DecodeLimits::default())?;
/// assert_eq!((frame.width(), frame.height(), frame.chroma()), (2, 2, ChromaSubsampling::Cs400));
/// # Ok::<(), gamut_core::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hevc {}

impl Codestream for Hevc {
    type Config = HevcConfig;
    type Backend = dyn HevcCodestreamDecoder + Send;
    const ORIGIN: &'static str = env!("CARGO_PKG_NAME");
    const CONFIGURATION_BOX: [u8; 4] = *b"hvcC";
    /// An HEVC alpha auxiliary must be coded monochrome.
    const ALPHA: AlphaChroma = AlphaChroma::Monochrome;

    fn is_coded_item(item_type: &[u8; 4]) -> bool {
        matches!(item_type, b"hvc1" | b"hev1")
    }

    fn parse_config(data: &[u8]) -> Result<HevcConfig> {
        HevcConfig::parse(data)
    }

    fn validate_still_payload(config: &HevcConfig, payload: &[u8]) -> Result<()> {
        config.validate_still_payload(payload)
    }

    /// BT.601 matrix coefficients at limited range (primaries and transfer unspecified), whatever
    /// the codestream signals: with no `nclx` the matrix is undefined and libheif assumes BT.601,
    /// and the HEVC VUI `video_full_range_flag` defaults to 0. The SPS VUI is not read.
    fn signalled_colour(_config: &HevcConfig, _payload: &[u8]) -> Result<Cicp> {
        Ok(UNSIGNALLED_COLOUR)
    }

    fn supports(backend: &mut Self::Backend, config: &HevcConfig) -> bool {
        backend.supports(config)
    }

    fn decode(
        backend: &mut Self::Backend,
        config: &HevcConfig,
        payload: &[u8],
    ) -> Result<DecodedFrame> {
        backend.decode_still(config, payload)
    }
}

/// Decodes a HEIC file's primary image — to [`Rgba8`] or [`Rgba16`] through [`DecodeImage`], or
/// to its planar samples through [`decode_planar`](Self::decode_planar) — with the HEVC
/// codestream decoded by the [`HevcCodestreamDecoder`] backends added with
/// [`with_backend`](Self::with_backend).
///
/// The container, the item derivations and the presentation are decoded in pure Rust by
/// `gamut-heif`; only the coded HEVC picture is handed to a backend (libde265, a platform hardware
/// decoder, …). gamut ships no HEVC codestream decoder, so there is no built-in tail: a decoder
/// with no backend — or whose every backend declines the item — refuses it with
/// [`gamut_heif::NO_BACKEND`].
///
/// [`Rgba8`] presents 8-bit coded frames and [`Rgba16`] any coded depth scaled to the full 16-bit
/// range; both apply the item's colour, alpha, `iovl` compositing and `clap`/`irot`/`imir`. An
/// item with no `colr` presents as BT.601 at limited range.
///
/// # Resource limits
///
/// Every surface decodes under [`DecodeLimits`] ([`with_limits`](Self::with_limits)), checked
/// against each `grid`/`iovl` canvas, `ispe` and decoded frame before the buffer it sizes is
/// allocated, plus per-call caps on backend decodes and derived-item evaluations — so a hostile
/// file is refused with a typed error rather than exhausting memory or time.
///
/// # Cloning shares backends
///
/// Backends are held behind [`Arc`]`<`[`Mutex`]`<…>>`, so a clone drives **the same** backend
/// objects, while a backend added to one afterwards is not seen by the other.
#[derive(Clone)]
pub struct HeifDecoder {
    /// The codestream decoders, tried in the order they were added.
    backends: Registry<dyn HevcCodestreamDecoder + Send>,
    /// The limits and chroma upsampling every decode runs under.
    options: DecodeOptions,
}

impl Default for HeifDecoder {
    /// [`HeifDecoder::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for HeifDecoder {
    /// Renders the number of backends (a backend need not be `Debug`) and the options.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeifDecoder")
            .field("backends", &self.backends.len())
            .field("options", &self.options)
            .finish()
    }
}

impl HeifDecoder {
    /// A decoder with no backend, the default [`DecodeLimits`], and
    /// [`ChromaUpsampling::Nearest`] — the nearest-neighbour, co-sited reconstruction this crate
    /// has always presented with.
    #[must_use]
    pub fn new() -> Self {
        Self {
            backends: Registry::new(),
            options: DecodeOptions::default().with_chroma_upsampling(ChromaUpsampling::Nearest),
        }
    }

    /// Adds an [`HevcCodestreamDecoder`] backend. Backends are tried in the order they are added;
    /// [`supports`](HevcCodestreamDecoder::supports) returning `false` is the only fall-through,
    /// and a backend that accepts an item and then fails returns its error unchanged.
    ///
    /// `Send` is required here, where the backend is inserted, not by the trait.
    #[must_use]
    pub fn with_backend(mut self, backend: impl HevcCodestreamDecoder + Send + 'static) -> Self {
        self.backends.push(Arc::new(Mutex::new(backend)));
        self
    }

    /// Replaces the size and work caps every decode runs under.
    #[must_use]
    pub fn with_limits(mut self, limits: DecodeLimits) -> Self {
        self.options = self.options.with_limits(limits);
        self
    }

    /// Selects how 4:2:0 and 4:2:2 chroma is reconstructed before colour conversion on the RGBA
    /// surfaces. The default is [`ChromaUpsampling::Nearest`].
    #[must_use]
    pub fn with_chroma_upsampling(mut self, upsampling: ChromaUpsampling) -> Self {
        self.options = self.options.with_chroma_upsampling(upsampling);
        self
    }

    /// Decodes the primary item to its planar frame — native bit depth and chroma, with `grid`
    /// and `iden` derivations resolved but no colour conversion, alpha, `clap`, `irot` or `imir`:
    /// the surface for a caller with its own colour pipeline.
    ///
    /// # Errors
    ///
    /// Any error [`HeifContainer::parse`] would return for `data`; [`gamut_heif::NO_BACKEND`] when
    /// no backend accepts a coded item; an accepting backend's error unchanged; and the pipeline's
    /// refusals — derivation cycles, a limit from [`with_limits`](Self::with_limits), a malformed
    /// `hvcC` or a payload with a non-IRAP picture.
    pub fn decode_planar(&self, data: &[u8]) -> Result<DecodedFrame> {
        let image = Self::parse(data)?;
        image.decode_item_planar::<Hevc>(
            image.primary_item().id(),
            &self.backends,
            self.options.limits,
        )
    }

    /// The EXIF, XMP and ICC payloads the file carries for its primary item, verbatim: the `Exif`
    /// item's TIFF stream (its `exif_tiff_header_offset` applied), the XMP `mime` item's packet
    /// and the primary item's ICC `colr` profile, each present only when the file carries it.
    /// HEIF has no IPTC-IIM item type, and its C2PA store is a top-level box
    /// ([`HeifContainer::c2pa`]), not an item, so neither is among them. No backend is involved.
    ///
    /// # Errors
    ///
    /// Any error [`HeifContainer::parse`] would return for `data`; an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when the `Exif`
    /// item's offset field is short or points past the payload; and an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error when the XMP item
    /// declares a `content_encoding` (ISO/IEC 14496-12 §8.11.6) — its payload is then not the raw
    /// packet, and this crate decodes no content codings. Any declared coding is refused,
    /// `identity` included, which RFC 9110 §8.4.1 reserves for `Accept-Encoding`.
    pub fn metadata(&self, data: &[u8]) -> Result<RawMetadata> {
        let image = Self::parse(data)?;
        let mut metadata = RawMetadata::default();
        if let Some(exif) = image.exif() {
            metadata.exif = Some(exif.exif_tiff_stream()?.to_vec());
        }
        if let Some(xmp) = image.xmp() {
            let item = xmp.as_isobmff_item();
            if item.content_encoding.is_some() {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "HEIF: content-encoded XMP item is not supported",
                ));
            }
            metadata.xmp = Some(item.payload.clone());
        }
        metadata.icc = image.primary_item().icc_profile().map(<[u8]>::to_vec);
        Ok(metadata)
    }

    /// The semantic view of `data` — the parse [`HeifContainer::parse`] starts with, without its
    /// byte-accounting walk, which no decode reads.
    fn parse(data: &[u8]) -> Result<HeifImage> {
        HeifImage::new(IsoBmffImage::parse(data)?)
    }
}

impl DecodeImage<Rgba8> for HeifDecoder {
    /// Decodes the primary item to 8-bit RGBA; a frame coded deeper than 8 bits is refused.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgba8>> {
        let image = Self::parse(data)?;
        image.decode_item_rgba8::<Hevc>(image.primary_item().id(), &self.backends, self.options)
    }
}

impl DecodeImage<Rgba16> for HeifDecoder {
    /// Decodes the primary item to 16-bit RGBA, any coded depth scaled to the full 16-bit range.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgba16>> {
        let image = Self::parse(data)?;
        image.decode_item_rgba16::<Hevc>(image.primary_item().id(), &self.backends, self.options)
    }
}

#[cfg(test)]
mod tests {
    use gamut_color::ChromaSubsampling;
    use gamut_core::{Dimensions, ErrorKind};
    use gamut_isobmff::{Item, ItemReference, Property, PropertyKind};

    use super::*;

    /// A backend returning a flat 3×2 frame of the configured layout at `level`.
    struct Flat {
        level: u16,
        accept: bool,
    }

    impl HevcCodestreamDecoder for Flat {
        fn supports(&mut self, _config: &HevcConfig) -> bool {
            self.accept
        }

        fn decode_still(&mut self, config: &HevcConfig, _payload: &[u8]) -> Result<DecodedFrame> {
            let chroma = config.chroma_subsampling();
            let (cw, ch) = chroma.chroma_dimensions(3, 2);
            let c = vec![self.level; (cw * ch) as usize];
            DecodedFrame::new(
                Dimensions::new(3, 2)?,
                config.bit_depth_luma(),
                chroma,
                vec![self.level; 6],
                c.clone(),
                c,
            )
        }
    }

    /// An `hvcC` declaring 4:2:0 at `bit_depth`.
    fn hvcc(bit_depth: u8) -> Vec<u8> {
        let mut raw = vec![0u8; 23];
        raw[0] = 1;
        raw[16] = 0xFD;
        raw[17] = 0xF8 | (bit_depth - 8);
        raw[18] = 0xF8 | (bit_depth - 8);
        raw[21] = 0b11;
        raw
    }

    fn coded(id: u32, bit_depth: u8) -> Item {
        Item::new(id, *b"hvc1")
            .with_properties(vec![Property::new(
                true,
                PropertyKind::CodecConfiguration {
                    kind: *b"hvcC",
                    data: hvcc(bit_depth),
                },
            )])
            .with_payload(vec![0, 0, 0, 3, 0x26, 0x01, 0xDD])
    }

    fn file_of(items: Vec<Item>) -> Vec<u8> {
        IsoBmffImage::new(*b"heic", vec![*b"heic", *b"mif1"], 1, items)
            .to_bytes()
            .unwrap()
    }

    fn file() -> Vec<u8> {
        file_of(vec![coded(1, 8)])
    }

    #[test]
    fn the_binding_claims_only_hevc_items() {
        assert!(Hevc::is_coded_item(b"hvc1"));
        assert!(Hevc::is_coded_item(b"hev1"));
        for other in [b"av01", b"vvc1", b"vvi1"] {
            assert!(!Hevc::is_coded_item(other));
        }
    }

    #[test]
    fn the_binding_parses_validates_and_presents_bt601_limited() {
        let config = Hevc::parse_config(&hvcc(10)).unwrap();
        assert_eq!(config.bit_depth_luma(), 10);
        Hevc::validate_still_payload(&config, &[0, 0, 0, 3, 0x26, 0x01, 0xDD]).unwrap();
        // A TRAIL_R picture (type 1) is not a still.
        assert!(Hevc::validate_still_payload(&config, &[0, 0, 0, 3, 0x02, 0x01, 0xDD]).is_err());
        assert_eq!(
            Hevc::signalled_colour(&config, &[]).unwrap(),
            UNSIGNALLED_COLOUR
        );
    }

    #[test]
    fn decodes_the_primary_through_the_first_accepting_backend() {
        let decoder = HeifDecoder::new()
            .with_backend(Flat {
                level: 1,
                accept: false,
            })
            .with_backend(Flat {
                level: 90,
                accept: true,
            });
        let frame = decoder.decode_planar(&file()).unwrap();
        assert_eq!((frame.width(), frame.height()), (3, 2));
        assert_eq!(frame.chroma(), ChromaSubsampling::Cs420);
        assert!(frame.y().iter().all(|&s| s == 90));
        let rgba: ImageBuf<Rgba8> = decoder.decode_image(&file()).unwrap();
        assert_eq!((rgba.width(), rgba.height()), (3, 2));
        assert_eq!(rgba.as_samples()[3], 255);
        let rgba16: ImageBuf<Rgba16> = decoder.decode_image(&file()).unwrap();
        assert_eq!(rgba16.as_samples()[3], u16::MAX);
    }

    #[test]
    fn rgba8_refuses_a_deep_frame_that_rgba16_presents() {
        let decoder = HeifDecoder::new().with_backend(Flat {
            level: 512,
            accept: true,
        });
        let deep = file_of(vec![coded(1, 10)]);
        let rgba: Result<ImageBuf<Rgba8>> = decoder.decode_image(&deep);
        assert_eq!(rgba.unwrap_err().kind(), ErrorKind::Unsupported);
        let rgba16: ImageBuf<Rgba16> = decoder.decode_image(&deep).unwrap();
        assert_eq!(rgba16.as_samples()[3], u16::MAX);
    }

    #[test]
    fn no_accepting_backend_is_refused() {
        let declined = HeifDecoder::new().with_backend(Flat {
            level: 0,
            accept: false,
        });
        for decoder in [HeifDecoder::new(), declined] {
            let err = decoder.decode_planar(&file()).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Unsupported);
            assert_eq!(err.static_message(), Some(gamut_heif::NO_BACKEND));
        }
    }

    #[test]
    fn the_defaults_and_knobs_reach_the_pipeline() {
        assert_eq!(
            HeifDecoder::default().options.chroma_upsampling,
            ChromaUpsampling::Nearest
        );
        assert_eq!(HeifDecoder::new().options.limits, DecodeLimits::default());
        let decoder = HeifDecoder::new()
            .with_backend(Flat {
                level: 1,
                accept: true,
            })
            .with_limits(DecodeLimits::new(2, 100))
            .with_chroma_upsampling(ChromaUpsampling::Interpolated);
        assert_eq!(decoder.options.limits, DecodeLimits::new(2, 100));
        assert_eq!(
            decoder.options.chroma_upsampling,
            ChromaUpsampling::Interpolated
        );
        let err = decoder.decode_planar(&file()).unwrap_err();
        assert_eq!(err.static_message(), Some(gamut_heif::DIMENSION_LIMIT));
        let rgba: Result<ImageBuf<Rgba8>> = decoder.decode_image(&file());
        assert_eq!(
            rgba.unwrap_err().static_message(),
            Some(gamut_heif::DIMENSION_LIMIT)
        );
        let rgba16: Result<ImageBuf<Rgba16>> = decoder.decode_image(&file());
        assert_eq!(
            rgba16.unwrap_err().static_message(),
            Some(gamut_heif::DIMENSION_LIMIT)
        );
    }

    #[test]
    fn debug_counts_backends() {
        let decoder = HeifDecoder::new().with_backend(Flat {
            level: 0,
            accept: true,
        });
        assert!(format!("{decoder:?}").contains("backends: 1"));
    }

    /// `metadata` over a file whose primary item 1 is described by `metadata`.
    mod raw_metadata {
        use super::*;

        /// An item of `item_type` carrying `payload`, describing item 1 through `cdsc`.
        fn item(id: u32, item_type: [u8; 4], payload: &[u8]) -> Item {
            Item::new(id, item_type)
                .with_references(vec![ItemReference::new(*b"cdsc", vec![1])])
                .with_payload(payload.to_vec())
        }

        fn read(metadata: Vec<Item>) -> Result<RawMetadata> {
            let mut items = vec![coded(1, 8)];
            items.extend(metadata);
            HeifDecoder::new().metadata(&file_of(items))
        }

        #[test]
        fn metadata_hands_over_the_tiff_stream_the_packet_and_the_profile() {
            let mut xmp = item(3, *b"mime", b"<x:xmpmeta/>");
            xmp.content_type = Some("application/rdf+xml".to_owned());
            let raw = read(vec![item(2, *b"Exif", b"\0\0\0\0MM\0*"), xmp]).unwrap();
            assert_eq!(raw.exif.as_deref(), Some(&b"MM\0*"[..]));
            assert_eq!(raw.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
            assert_eq!(raw.icc, None);
            assert_eq!(read(Vec::new()).unwrap(), RawMetadata::default());

            let icc = coded(1, 8).with_properties(vec![
                Property::new(
                    true,
                    PropertyKind::CodecConfiguration {
                        kind: *b"hvcC",
                        data: hvcc(8),
                    },
                ),
                Property::new(
                    false,
                    PropertyKind::Colour(gamut_isobmff::ColourInformation::UnrestrictedIcc(
                        b"profile".to_vec(),
                    )),
                ),
            ]);
            let raw = HeifDecoder::new().metadata(&file_of(vec![icc])).unwrap();
            assert_eq!(raw.icc.as_deref(), Some(&b"profile"[..]));
        }

        #[test]
        fn an_unlocatable_exif_stream_is_a_container_error() {
            let err = read(vec![item(2, *b"Exif", b"\0\0\0\x09MM\0*")]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
        }

        #[test]
        fn a_content_encoded_xmp_item_is_unsupported() {
            let mut xmp = item(2, *b"mime", b"compressed");
            xmp.content_type = Some("application/rdf+xml".to_owned());
            xmp.content_encoding = Some("deflate".to_owned());
            let err = read(vec![xmp]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Unsupported);
            assert_eq!(
                err.static_message(),
                Some("HEIF: content-encoded XMP item is not supported")
            );
        }
    }
}
