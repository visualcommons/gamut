//! The AVIF decode surface: [`Av1`], AVIF's binding into the shared HEIF pipeline, and
//! [`AvifDecoder`], the [`DecodeImage`] entry point that runs a file's primary item through it.
//!
//! Everything around the coded picture — the item model, the `grid`/`iden`/`iovl` derivations,
//! the [`DecodeLimits`] size and work caps, YCbCr → RGB, alpha and the `clap`/`irot`/`imir`
//! transforms — is `gamut-heif`'s. What is AVIF's is the coded `av01` item: its `av1C`
//! configuration ([`Av1Config`]), the AVIF still-image payload constraints
//! ([`Av1Config::validate_still_payload`]), the colour its AV1 sequence header signals, and the
//! [`Av1CodestreamDecoder`] backends that decode it.

use std::fmt;
use std::sync::{Arc, Mutex};

use gamut_color::ChromaUpsampling;
use gamut_core::{Cicp, DecodeImage, Error, ImageBuf, RawMetadata, Result, Rgba8, Rgba16};
use gamut_heif::{AlphaChroma, Codestream, DecodeLimits, DecodeOptions, DecodedFrame, HeifImage};
use gamut_isobmff::IsoBmffImage;

use crate::av1c::Av1Config;
use crate::backend::{Av1CodestreamDecoder, DecoderRegistry};
#[cfg(doc)]
use crate::container::AvifContainer;

/// AVIF's binding into the shared HEIF decode pipeline: the `av01` coded item, its `av1C`
/// configuration and its [`Av1CodestreamDecoder`] backends.
///
/// Never instantiated — it names the codec as a type parameter of `gamut-heif`'s surfaces, which
/// decode **any** item of a parsed file, where [`AvifDecoder`] decodes the primary one:
///
/// ```
/// use std::sync::{Arc, Mutex};
///
/// use gamut_avif::{Av1, Av1CodestreamDecoder, Av1Config, AvifContainer, AvifEncoder, DecodeLimits};
/// use gamut_codec_abi::Registry;
/// use gamut_color::ChromaSubsampling;
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Result, Rgb8};
/// use gamut_heif::DecodedFrame;
///
/// /// A stand-in for a real AV1 decoder: a mid-grey frame of the configured layout.
/// struct Grey;
/// impl Av1CodestreamDecoder for Grey {
///     fn supports(&mut self, _config: &Av1Config) -> bool {
///         true
///     }
///     fn decode_still(&mut self, config: &Av1Config, _payload: &[u8]) -> Result<DecodedFrame> {
///         let size = Dimensions::new(2, 2)?;
///         let chroma = config.chroma_subsampling();
///         let (cw, ch) = chroma.chroma_dimensions(2, 2);
///         let c = vec![128; (cw * ch) as usize];
///         DecodedFrame::new(size, 8, chroma, vec![128; 4], c.clone(), c)
///     }
/// }
///
/// let pixels = [0u8; 2 * 2 * 3];
/// let file = AvifEncoder::new().encode_to_vec(ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2)?)?)?;
/// let container = AvifContainer::parse(&file)?;
/// let image = container.image();
///
/// let mut backends: Registry<dyn Av1CodestreamDecoder + Send> = Registry::new();
/// backends.push(Arc::new(Mutex::new(Grey)));
/// let frame = image.decode_item_planar::<Av1>(image.primary_item().id(), &backends, DecodeLimits::default())?;
/// assert_eq!((frame.width(), frame.height(), frame.chroma()), (2, 2, ChromaSubsampling::Cs444));
/// # Ok::<(), gamut_core::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Av1 {}

impl Codestream for Av1 {
    type Config = Av1Config;
    type Backend = dyn Av1CodestreamDecoder + Send;
    const ORIGIN: &'static str = env!("CARGO_PKG_NAME");
    const CONFIGURATION_BOX: [u8; 4] = *b"av1C";
    /// Real-world AVIF alpha items are routinely coded 4:2:0 or 4:4:4 with meaningless chroma, and
    /// libavif reads only their luma; so does this crate.
    const ALPHA: AlphaChroma = AlphaChroma::Any;

    fn is_coded_item(item_type: &[u8; 4]) -> bool {
        item_type == b"av01"
    }

    fn parse_config(data: &[u8]) -> Result<Av1Config> {
        Av1Config::parse(data)
    }

    fn validate_still_payload(config: &Av1Config, payload: &[u8]) -> Result<()> {
        config.validate_still_payload(payload)
    }

    /// The `color_config()` of the AV1 sequence header the item is coded under (AVIF v1.2.0
    /// §2.2.1): the payload's, else `configOBUs`'.
    fn signalled_colour(config: &Av1Config, payload: &[u8]) -> Result<Cicp> {
        Ok(config.sequence_header(payload)?.color.cicp)
    }

    fn supports(backend: &mut Self::Backend, config: &Av1Config) -> bool {
        backend.supports(config)
    }

    fn decode(
        backend: &mut Self::Backend,
        config: &Av1Config,
        payload: &[u8],
    ) -> Result<DecodedFrame> {
        backend.decode_still(config, payload)
    }
}

/// Decodes an AVIF file's primary image — to [`Rgba8`] or [`Rgba16`] through [`DecodeImage`], or
/// to its planar samples through [`decode_planar`](Self::decode_planar) — with the AV1
/// codestream decoded by the [`Av1CodestreamDecoder`] backends added with
/// [`with_backend`](Self::with_backend).
///
/// The container, the item derivations and the presentation are decoded in pure Rust by
/// `gamut-heif`; only the coded AV1 picture is handed to a backend (dav1d, a platform hardware
/// decoder, …). `gamut-av1` reconstructs no samples yet, so there is no built-in tail: a decoder
/// with no backend — or whose every backend declines the item — refuses it with
/// [`gamut_heif::NO_BACKEND`].
///
/// [`Rgba8`] presents any coded depth scaled to 8 bits and [`Rgba16`] scaled to the full 16-bit
/// range; both apply the item's colour, alpha, `iovl` compositing and `clap`/`irot`/`imir`.
///
/// # Resource limits
///
/// Every surface decodes under [`DecodeLimits`] ([`with_limits`](Self::with_limits)) — libavif's
/// default dimension and pixel caps, checked against each `grid`/`iovl` canvas, `ispe` and decoded
/// frame before the buffer it sizes is allocated, plus per-call caps on backend decodes and
/// derived-item evaluations — so a hostile file is refused with a typed error rather than
/// exhausting memory or time.
///
/// # Cloning shares backends
///
/// Backends are held behind [`Arc`]`<`[`Mutex`]`<…>>`, so a clone drives **the same** backend
/// objects, while a backend added to one afterwards is not seen by the other.
#[derive(Clone)]
pub struct AvifDecoder {
    /// The codestream decoders, tried in the order they were added.
    backends: DecoderRegistry,
    /// The limits and chroma upsampling every decode runs under.
    options: DecodeOptions,
}

impl Default for AvifDecoder {
    /// [`AvifDecoder::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for AvifDecoder {
    /// Renders the number of backends (a backend need not be `Debug`) and the options.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AvifDecoder")
            .field("backends", &self.backends.len())
            .field("options", &self.options)
            .finish()
    }
}

impl AvifDecoder {
    /// A decoder with no backend, the default [`DecodeLimits`], and
    /// [`ChromaUpsampling::Interpolated`] (libavif's default conversion).
    #[must_use]
    pub fn new() -> Self {
        Self {
            backends: DecoderRegistry::new(),
            options: DecodeOptions::default(),
        }
    }

    /// Adds an [`Av1CodestreamDecoder`] backend. Backends are tried in the order they are added;
    /// [`supports`](Av1CodestreamDecoder::supports) returning `false` is the only fall-through,
    /// and a backend that accepts an item and then fails returns its error unchanged.
    ///
    /// `Send` is required here, where the backend is inserted, not by the trait.
    #[must_use]
    pub fn with_backend(mut self, backend: impl Av1CodestreamDecoder + Send + 'static) -> Self {
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
    /// surfaces. The default is [`ChromaUpsampling::Interpolated`], libavif's.
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
    /// Any error [`AvifContainer::parse`] would return for `data`; [`gamut_heif::NO_BACKEND`] when no backend accepts a
    /// coded item; an accepting backend's error unchanged; and the pipeline's refusals —
    /// derivation cycles, a limit from [`with_limits`](Self::with_limits), a malformed `av1C` or
    /// a payload outside the AVIF still-image constraints.
    pub fn decode_planar(&self, data: &[u8]) -> Result<DecodedFrame> {
        let image = Self::parse(data)?;
        image.decode_item_planar::<Av1>(
            image.primary_item().id(),
            &self.backends,
            self.options.limits,
        )
    }

    /// The EXIF, XMP and ICC payloads the file carries for its primary item, verbatim: the `Exif`
    /// item's TIFF stream (its `exif_tiff_header_offset` applied), the XMP `mime` item's packet
    /// and the primary item's ICC `colr` profile, each present only when the file carries it.
    /// AVIF has no IPTC-IIM item type, and its C2PA store is a top-level box
    /// ([`AvifContainer::c2pa_slot`]), not an item, so neither is among them. No backend is
    /// involved.
    ///
    /// # Errors
    ///
    /// Any error [`AvifContainer::parse`] would return for `data`; an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when the `Exif`
    /// item's offset field is short or points past the payload; and an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error when the XMP item
    /// declares a `content_encoding` (ISO/IEC 14496-12 §8.11.6) — its payload is then not the raw
    /// packet, and this crate decodes no content codings.
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
                    "AVIF: content-encoded XMP item is not supported",
                ));
            }
            metadata.xmp = Some(item.payload.clone());
        }
        metadata.icc = image.primary_item().icc_profile().map(<[u8]>::to_vec);
        Ok(metadata)
    }

    /// The semantic view of `data` — the parse [`AvifContainer::parse`] starts with, without its
    /// byte-accounting walk, which no decode reads.
    fn parse(data: &[u8]) -> Result<HeifImage> {
        HeifImage::new(IsoBmffImage::parse(data)?)
    }
}

impl DecodeImage<Rgba8> for AvifDecoder {
    /// Decodes the primary item to 8-bit RGBA, any coded depth scaled to 8 bits.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgba8>> {
        let image = Self::parse(data)?;
        image.decode_item_rgba8::<Av1>(image.primary_item().id(), &self.backends, self.options)
    }
}

impl DecodeImage<Rgba16> for AvifDecoder {
    /// Decodes the primary item to 16-bit RGBA, any coded depth scaled to the full 16-bit range.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgba16>> {
        let image = Self::parse(data)?;
        image.decode_item_rgba16::<Av1>(image.primary_item().id(), &self.backends, self.options)
    }
}

#[cfg(test)]
mod tests {
    use gamut_color::ChromaSubsampling;
    use gamut_core::{Dimensions, EncodeImage, ErrorKind, ImageRef, Rgb8, Rgba8};

    use super::*;
    use crate::{AvifContainer, AvifEncoder};

    /// A backend returning a flat frame of the configured layout at `level`, and counting calls.
    struct Flat {
        level: u16,
        accept: bool,
    }

    impl Av1CodestreamDecoder for Flat {
        fn supports(&mut self, _config: &Av1Config) -> bool {
            self.accept
        }

        fn decode_still(&mut self, config: &Av1Config, payload: &[u8]) -> Result<DecodedFrame> {
            let header = config.sequence_header(payload)?;
            let (w, h) = (header.max_frame_width, header.max_frame_height);
            let chroma = config.chroma_subsampling();
            let (cw, ch) = chroma.chroma_dimensions(w, h);
            let c = vec![self.level; (cw * ch) as usize];
            DecodedFrame::new(
                Dimensions::new(w, h)?,
                config.bit_depth(),
                chroma,
                vec![self.level; (w * h) as usize],
                c.clone(),
                c,
            )
        }
    }

    fn file() -> Vec<u8> {
        let pixels = [7u8; 3 * 2 * 3];
        AvifEncoder::new()
            .encode_to_vec(ImageRef::<Rgb8>::new(&pixels, Dimensions::new(3, 2).unwrap()).unwrap())
            .unwrap()
    }

    #[test]
    fn the_binding_claims_only_av01_items() {
        assert!(Av1::is_coded_item(b"av01"));
        for other in [b"hvc1", b"hev1", b"vvc1", b"vvi1"] {
            assert!(!Av1::is_coded_item(other));
        }
    }

    #[test]
    fn the_binding_reads_the_sequence_header_colour() {
        let file = file();
        let container = AvifContainer::parse(&file).unwrap();
        let item = container.image().primary_item().as_isobmff_item().clone();
        let (_, av1c) = container
            .image()
            .primary_item()
            .codec_configuration()
            .unwrap();
        let config = Av1::parse_config(av1c).unwrap();
        Av1::validate_still_payload(&config, &item.payload).unwrap();
        // Lossless: BT.709 / sRGB / identity, full range — the §5.5.2 sRGB shortcut.
        let cicp = Av1::signalled_colour(&config, &item.payload).unwrap();
        assert_eq!(
            (
                cicp.colour_primaries,
                cicp.transfer_characteristics,
                cicp.matrix_coefficients,
                cicp.full_range
            ),
            (1, 13, 0, true)
        );
    }

    #[test]
    fn decodes_the_primary_through_the_first_accepting_backend() {
        let decoder = AvifDecoder::new()
            .with_backend(Flat {
                level: 1,
                accept: false,
            })
            .with_backend(Flat {
                level: 9,
                accept: true,
            });
        let frame = decoder.decode_planar(&file()).unwrap();
        assert_eq!((frame.width(), frame.height()), (3, 2));
        assert_eq!(frame.chroma(), ChromaSubsampling::Cs444);
        assert!(frame.y().iter().all(|&s| s == 9));
        let rgba: ImageBuf<Rgba8> = decoder.decode_image(&file()).unwrap();
        assert_eq!((rgba.width(), rgba.height()), (3, 2));
        // Identity matrix, flat 9: every channel 9, opaque.
        assert_eq!(&rgba.as_samples()[..4], &[9, 9, 9, 255]);
        let rgba16: ImageBuf<Rgba16> = decoder.decode_image(&file()).unwrap();
        assert_eq!(rgba16.as_samples()[3], u16::MAX);
        assert_eq!(rgba16.as_samples()[0], 9 * 257);
    }

    #[test]
    fn no_accepting_backend_is_refused() {
        let declined = AvifDecoder::new().with_backend(Flat {
            level: 0,
            accept: false,
        });
        for decoder in [AvifDecoder::new(), declined] {
            let err = decoder.decode_planar(&file()).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Unsupported);
            assert_eq!(err.static_message(), Some(gamut_heif::NO_BACKEND));
        }
    }

    #[test]
    fn the_limits_and_upsampling_reach_the_pipeline() {
        let decoder = AvifDecoder::new()
            .with_backend(Flat {
                level: 1,
                accept: true,
            })
            .with_limits(DecodeLimits::new(2, 100))
            .with_chroma_upsampling(ChromaUpsampling::Nearest);
        assert_eq!(decoder.options.limits, DecodeLimits::new(2, 100));
        assert_eq!(decoder.options.chroma_upsampling, ChromaUpsampling::Nearest);
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

    /// The pinned-seed driver of `invariants::decoding_stays_within_limits` over generated
    /// coded, `grid` and `iovl` files; the fuzz tier's `avif_parse_decode` drives the same law
    /// over arbitrary bytes.
    mod limits_law {
        use gamut_isobmff::{
            ImageGrid, ImageOverlay, IsoBmffImage, Item, ItemReference, OverlayOffset, Property,
            PropertyKind,
        };
        use proptest::collection;
        use proptest::prelude::{Strategy, prop_assert_eq, prop_oneof, proptest};
        use proptest::test_runner::{Config, RngSeed};

        use crate::DecodeLimits;
        use crate::invariants::decoding_stays_within_limits;

        /// Monochrome 8-bit `av1C`, matching [`SEQUENCE_HEADER`].
        const AV1C: [u8; 4] = [0x81, 0x00, 0x1C, 0x00];
        /// A reduced-still-picture sequence header OBU: 8-bit monochrome, no colour description.
        const SEQUENCE_HEADER: [u8; 6] = [0x0A, 0x04, 0x18, 0x00, 0x30, 0x11];
        /// The looser run's limits: well past anything generated here.
        const CEILING: DecodeLimits = DecodeLimits::new(1024, 1 << 20);

        /// The workspace property-test configuration (`docs/testing.md`): pinned seed, bounded
        /// shrinking, no failure persistence.
        fn config() -> Config {
            Config {
                cases: 512,
                max_shrink_iters: 2048,
                max_shrink_time: 10_000,
                rng_seed: RngSeed::Fixed(0x6761_6D75_745F_6C69),
                failure_persistence: None,
                ..Config::default()
            }
        }

        fn item(id: u32, item_type: [u8; 4], hidden: bool) -> Item {
            Item::new(id, item_type).with_hidden(hidden)
        }

        /// A coded item the stub decodes to `(1 + w % 16) × (1 + h % 16)`, optionally declaring
        /// `ispe`.
        fn coded(id: u32, (w, h): (u8, u8), ispe: Option<(u32, u32)>, hidden: bool) -> Item {
            let mut properties = vec![Property::new(
                true,
                PropertyKind::CodecConfiguration {
                    kind: *b"av1C",
                    data: AV1C.to_vec(),
                },
            )];
            if let Some((width, height)) = ispe {
                properties.push(Property::new(
                    false,
                    PropertyKind::ImageSpatialExtents { width, height },
                ));
            }
            let mut payload = SEQUENCE_HEADER.to_vec();
            payload.extend_from_slice(&[0x32, 0x03, 0xAA, w, h]);
            item(id, *b"av01", hidden)
                .with_properties(properties)
                .with_payload(payload)
        }

        fn derived(id: u32, item_type: [u8; 4], payload: Vec<u8>, sources: &[u32]) -> Item {
            item(id, item_type, false)
                .with_references(vec![ItemReference::new(*b"dimg", sources.to_vec())])
                .with_payload(payload)
        }

        fn file(items: Vec<Item>) -> Vec<u8> {
            IsoBmffImage::new(*b"avif", vec![*b"avif", *b"mif1", *b"miaf"], 1, items)
                .to_bytes()
                .expect("the generated model is writable")
        }

        fn frame_bytes() -> impl Strategy<Value = (u8, u8)> {
            (0u8..=255, 0u8..=255)
        }

        fn coded_file() -> impl Strategy<Value = Vec<u8>> {
            let ispe = proptest::option::of((1u32..=48, 1u32..=48));
            (frame_bytes(), ispe).prop_map(|(frame, ispe)| file(vec![coded(1, frame, ispe, false)]))
        }

        fn grid_file() -> impl Strategy<Value = Vec<u8>> {
            (1u16..=3, 1u16..=3, frame_bytes(), 1u32..=48, 1u32..=48).prop_map(
                |(rows, columns, frame, ow, oh)| {
                    let tiles: Vec<u32> = (2..2 + u32::from(rows * columns)).collect();
                    let payload = ImageGrid {
                        rows,
                        columns,
                        output_width: ow,
                        output_height: oh,
                    }
                    .to_bytes()
                    .expect("a generated grid is encodable");
                    let mut items = vec![derived(1, *b"grid", payload, &tiles)];
                    items.extend(tiles.iter().map(|&id| coded(id, frame, None, true)));
                    file(items)
                },
            )
        }

        fn overlay_file() -> impl Strategy<Value = Vec<u8>> {
            let input = (frame_bytes(), -8i32..=40, -8i32..=40);
            (1u32..=48, 1u32..=48, collection::vec(input, 1..=3)).prop_map(|(ow, oh, inputs)| {
                let ids: Vec<u32> = (2..2 + inputs.len() as u32).collect();
                let payload = ImageOverlay {
                    canvas_fill_value: [1000, 2000, 3000, 65535],
                    output_width: ow,
                    output_height: oh,
                    offsets: inputs
                        .iter()
                        .map(|&(_, horizontal, vertical)| OverlayOffset {
                            horizontal,
                            vertical,
                        })
                        .collect(),
                }
                .to_bytes()
                .expect("a generated overlay is encodable");
                let mut items = vec![derived(1, *b"iovl", payload, &ids)];
                items.extend(
                    ids.iter()
                        .zip(&inputs)
                        .map(|(&id, &(frame, _, _))| coded(id, frame, None, true)),
                );
                file(items)
            })
        }

        /// The property is vacuous if the generated files never decode: pin that one of each
        /// shape does, at the size the stub and the derivation give it.
        #[test]
        fn the_generated_files_decode() {
            use gamut_core::DecodeImage;

            use crate::AvifDecoder;
            use crate::invariants::StubDecoder;

            let sizes = |bytes: Vec<u8>| {
                let rgba: gamut_core::ImageBuf<gamut_core::Rgba16> = AvifDecoder::new()
                    .with_backend(StubDecoder)
                    .with_limits(CEILING)
                    .decode_image(&bytes)
                    .unwrap();
                (rgba.width(), rgba.height())
            };
            assert_eq!(sizes(file(vec![coded(1, (3, 4), None, false)])), (4, 5));
            let grid = ImageGrid {
                rows: 1,
                columns: 2,
                output_width: 7,
                output_height: 5,
            };
            assert_eq!(
                sizes(file(vec![
                    derived(1, *b"grid", grid.to_bytes().unwrap(), &[2, 3]),
                    coded(2, (3, 4), None, true),
                    coded(3, (3, 4), None, true),
                ])),
                (7, 5)
            );
            let overlay = ImageOverlay {
                canvas_fill_value: [0; 4],
                output_width: 9,
                output_height: 2,
                offsets: vec![OverlayOffset {
                    horizontal: 0,
                    vertical: 0,
                }],
            };
            assert_eq!(
                sizes(file(vec![
                    derived(1, *b"iovl", overlay.to_bytes().unwrap(), &[2]),
                    coded(2, (3, 4), None, true),
                ])),
                (9, 2)
            );
        }

        proptest! {
            #![proptest_config(config())]

            #[test]
            fn decoding_never_returns_an_image_past_its_limits(
                bytes in prop_oneof![coded_file(), grid_file(), overlay_file()],
                max_dimension in 1u32..=40,
                max_pixels in 1u64..=1600,
            ) {
                let limits = DecodeLimits::new(max_dimension, max_pixels);
                prop_assert_eq!(decoding_stays_within_limits(&bytes, limits, CEILING), Ok(()));
            }
        }
    }

    /// `metadata` over a file whose primary item 1 is described by `metadata`.
    mod raw_metadata {
        use gamut_isobmff::{IsoBmffImage, Item, ItemReference};

        use super::*;

        /// An item of `item_type` carrying `payload`, describing item 1 through `cdsc`.
        fn item(id: u32, item_type: [u8; 4], payload: &[u8]) -> Item {
            Item::new(id, item_type)
                .with_references(vec![ItemReference::new(*b"cdsc", vec![1])])
                .with_payload(payload.to_vec())
        }

        fn read(metadata: Vec<Item>) -> Result<RawMetadata> {
            let mut items = vec![Item::new(1, *b"av01").with_payload(vec![0])];
            items.extend(metadata);
            let file = IsoBmffImage::new(*b"avif", vec![*b"avif"], 1, items)
                .to_bytes()
                .unwrap();
            AvifDecoder::new().metadata(&file)
        }

        #[test]
        fn metadata_hands_over_the_tiff_stream_and_the_packet() {
            let mut xmp = item(3, *b"mime", b"<x:xmpmeta/>");
            xmp.content_type = Some("application/rdf+xml".to_owned());
            let raw = read(vec![item(2, *b"Exif", b"\0\0\0\0MM\0*"), xmp]).unwrap();
            assert_eq!(raw.exif.as_deref(), Some(&b"MM\0*"[..]));
            assert_eq!(raw.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
            assert_eq!(raw.icc, None);
            assert_eq!(read(Vec::new()).unwrap(), RawMetadata::default());
        }

        #[test]
        fn an_unlocatable_exif_stream_is_a_container_error() {
            let err = read(vec![item(2, *b"Exif", b"\0\0\0\x09MM\0*")]).unwrap_err();
            assert_eq!(
                err.static_message(),
                Some("ISOBMFF: Exif item tiff-header offset out of range")
            );
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
                Some("AVIF: content-encoded XMP item is not supported")
            );
        }

        #[test]
        fn the_primary_items_icc_profile_is_read() {
            let file = AvifEncoder::new()
                .with_icc_profile(b"profile")
                .encode_to_vec(
                    ImageRef::<Rgb8>::new(&[0; 3], Dimensions::new(1, 1).unwrap()).unwrap(),
                )
                .unwrap();
            let raw = AvifDecoder::new().metadata(&file).unwrap();
            assert_eq!(raw.icc.as_deref(), Some(&b"profile"[..]));
        }
    }

    #[test]
    fn debug_counts_backends() {
        let decoder = AvifDecoder::new().with_backend(Flat {
            level: 0,
            accept: true,
        });
        assert!(format!("{decoder:?}").contains("backends: 1"));
    }
}
