//! AVIF (AV1 Image File Format) encoder and **container decoder** — AV1 intra-frame bitstreams
//! wrapped in an ISOBMFF/MIAF container.
//!
//! The encode surface is [`AvifEncoder`], which implements [`gamut_core::EncodeImage`] for
//! [`Rgb8`](gamut_core::Rgb8), [`Rgba8`](gamut_core::Rgba8), [`Gray8`](gamut_core::Gray8),
//! [`Rgb16`](gamut_core::Rgb16) and [`Rgba16`](gamut_core::Rgba16), so
//! the input is a typed [`ImageRef`](gamut_core::ImageRef) and handing it an unsupported pixel
//! layout is a compile error. The crate is orchestration only: [`gamut_color`] maps pixels to
//! planes — identity GBR at 4:4:4, or YCbCr through a CICP matrix at the configured chroma
//! sampling, for colour; monochrome for alpha and grayscale — [`gamut_av1`] encodes each AV1
//! temporal unit, and [`gamut_isobmff`] writes the container. An alpha channel becomes its own
//! monochrome **auxiliary image item** (AVIF v1.2.0 §4.1), not a fourth plane — unless every
//! alpha sample is fully opaque, in which case the input is coded as the RGB image it is
//! indistinguishable from, with no auxiliary at all
//! ([`AvifEncoder::with_keep_opaque_alpha`] keeps it).
//!
//! A caller already holding YCbCr planes skips the colour conversion entirely:
//! [`AvifEncoder::encode_raw_planes`] codes a [`RawPlanes`] view — borrowed
//! [`Planar8`](gamut_color::Planar8) or 10/12-bit [`Planar16`](gamut_color::Planar16) planes, an
//! optional alpha plane, and the CICP matrix and range they were produced under — as given, and
//! writes `colr nclx` from that declaration.
//!
//! # The decode surface (issue #250)
//!
//! The read side mirrors the surface [`gamut-heic`](https://docs.rs/gamut-heic) established for
//! HEIF: the crate decodes the **container** — and everything around the coded picture — in pure
//! Rust, while the AV1 codestream itself is decoded by a pluggable [`Av1StillDecoder`] the caller
//! supplies (a platform hardware decoder, dav1d, …). Two layers:
//!
//! - [`AvifContainer`] — the **total, byte-accounting** representation: every input byte maps to
//!   exactly one [`Segment`] (box / appended motion-photo stream / trailer), and unconsumed
//!   `meta` boxes surface as [`UnknownBox`]es — it is structurally impossible to ignore bits.
//! - [`AvifImage`] / [`AvifItem`] — the **role-typed semantic view** over
//!   [`gamut_isobmff::IsoBmffImage`]: the validated primary item, alpha/depth auxiliaries,
//!   thumbnails, Exif/XMP payloads, grid/overlay derivations, and typed properties including the
//!   [`Av1Config`] `av1C` record ([`AvifItem::av1_config`]), the parsed AV1
//!   [`SequenceHeader`] ([`AvifImage::av1_sequence_header`], with its [`ColorConfig`] and
//!   [`Subsampling`] field types), and the OBU layer
//!   ([`iter_obus`]/[`ObuType`]).
//!
//! Around the seam, [`AvifImage::decode_item_planar`] resolves item derivation (`grid`/`iden`)
//! and returns the decoder's raw planar [`DecodedFrame`] — the surface for callers with their own
//! colour pipeline — while [`AvifImage::decode_item_rgba8`] /
//! [`AvifImage::decode_primary_rgba8`] add colour conversion, alpha merge, `iovl` compositing,
//! and the `clap`/`irot`/`imir` transforms for a presentation-ready
//! [`ImageBuf<Rgba8>`](gamut_core::ImageBuf). [`AvifImage::decode_item_rgba16`] /
//! [`AvifImage::decode_primary_rgba16`] are the same pipeline for high-bit-depth content: they take
//! any coded depth from 8 to 16 bits and normalize the samples to the full 16-bit range. The whole
//! pipeline is validated differentially
//! against **libavif + dav1d** (`tests/conformance.rs`).
//!
//! # Examples
//!
//! Parse an AVIF file and decode it through a caller-supplied [`Av1StillDecoder`]. The stub here
//! returns a solid-gray monochrome frame; a real decoder wraps a platform AV1 decoder (dav1d,
//! VideoToolbox, VAAPI, MediaCodec, …), typically bridging the config + payload into one stream
//! with [`Av1Config::full_stream`].
//!
//! ```
//! use gamut_avif::{Av1Config, Av1StillDecoder, AvifContainer, ChromaFormat, DecodedFrame};
//! use gamut_core::Result;
//! use gamut_isobmff::{IsoBmffImage, Item, Property, PropertyKind, write};
//!
//! // A stub AV1 decoder: ignores the codestream and returns a 2x2 solid-gray monochrome frame.
//! struct GrayStub;
//! impl Av1StillDecoder for GrayStub {
//!     fn decode_still(&mut self, _config: &Av1Config, _payload: &[u8]) -> Result<DecodedFrame> {
//!         DecodedFrame::new(2, 2, 8, ChromaFormat::Monochrome, vec![128; 4], vec![], vec![])
//!     }
//! }
//!
//! // A minimal AV1 still: one av01 item with a monochrome av1C and a conforming payload
//! // (a reduced-still-picture sequence header OBU + a frame OBU).
//! let img = IsoBmffImage::new(
//!     *b"avif",
//!     vec![*b"avif", *b"mif1", *b"miaf"],
//!     1,
//!     vec![Item {
//!         id: 1,
//!         item_type: *b"av01",
//!         name: String::new(),
//!         content_type: None,
//!         content_encoding: None,
//!         hidden: false,
//!         references: vec![],
//!         properties: vec![Property {
//!             essential: true,
//!             kind: PropertyKind::CodecConfiguration {
//!                 kind: *b"av1C",
//!                 // marker+version; profile 0; monochrome, subsampling (1,1)
//!                 data: vec![0x81, 0x00, 0x1C, 0x00],
//!             },
//!         }],
//!         // Sequence header OBU (reduced_still_picture_header = 1, 2x2, 8-bit monochrome, no
//!         // colour description, studio range) + frame OBU.
//!         payload: vec![0x0A, 0x04, 0x18, 0x00, 0x30, 0x11, 0x32, 0x03, 0xAA, 0xBB, 0xCC],
//!     }],
//! );
//! let bytes = write(&img).unwrap();
//!
//! let container = AvifContainer::parse(&bytes).unwrap();
//! assert!(container.image().is_av1_still());
//! assert_eq!(container.image().primary_item().id(), 1);
//!
//! // The item has no `colr`, so the RGBA surface takes its colour from this sequence header.
//! let header = container.image().av1_sequence_header(1).unwrap();
//! assert_eq!((header.max_frame_width, header.max_frame_height), (2, 2));
//! assert_eq!(header.color.bit_depth, 8);
//! assert!(!header.color.full_range);
//!
//! // Decode the primary image to RGBA through the stub decoder.
//! let rgba = container.decode_primary_rgba8(&mut GrayStub).unwrap();
//! assert_eq!((rgba.width(), rgba.height()), (2, 2));
//! assert_eq!(rgba.as_samples()[3], 255); // opaque (no alpha auxiliary)
//! ```
//!
//! # The encode-backend seam (issue #274)
//!
//! Symmetrically, the AV1 codestream on the **write** side is pluggable: [`AvifEncoder`] encodes
//! with [`gamut_av1`] by default, and [`AvifEncoder::push_backend`] registers
//! [`Av1StillEncoder`] backends that are tried ahead of it in push order (`gamut-av1` is the
//! implicit tail). [`AbiAv1StillEncoder`] bridges the shared [`gamut_codec_abi`] seam — and hence
//! any C/`-sys` encoder — onto that trait. An encoder with no pushed backend is byte-for-byte the
//! encoder this crate has always been. See [`backend`] for the full fallback contract.
//!
//! Encoding:
//!
//! ```
//! use gamut_avif::AvifEncoder;
//! use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
//!
//! // A 2×2 8-bit RGB image, row-major (red, green, blue, yellow).
//! let pixels = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
//! let image = ImageRef::<Rgb8>::new(&pixels, Dimensions { width: 2, height: 2 })?;
//!
//! // Lossless by default; `AvifEncoder::lossy(quality)` trades fidelity for a smaller file.
//! let avif = AvifEncoder::new().encode_to_vec(image)?;
//! assert_eq!(&avif[4..8], b"ftyp");
//! # Ok::<(), gamut_core::Error>(())
//! ```
//!
//! # Content credentials (C2PA)
//!
//! A C2PA manifest store is bound to the finished file, so it cannot be handed to the encoder
//! complete. The encoder therefore **reserves**: [`AvifEncoder::with_c2pa_reserved`] writes the
//! C2PA `ContentProvenanceBox` — a top-level `uuid` box with user type [`C2PA_UUID`], after
//! `ftyp` (C2PA 2.4 §A.5.1, §A.5.3) — around a slot of zero bytes, and
//! [`AvifEncoder::encode_with_report`] returns the bytes `encode_to_vec` would plus the slot's
//! range ([`AvifEncodeReport::c2pa`]), which an external signer patches in place; nothing after
//! the slot moves. [`AvifEncoder::with_c2pa`] writes a store already computed over this exact
//! output. On read, [`AvifContainer::c2pa_slot`] / [`AvifContainer::c2pa_slots`] report each
//! located [`C2paSlot`] — slot bytes, file range and [`C2paBoxPurpose`]. The store is
//! opaque throughout: nothing here validates it, and the range is for patching and byte
//! accounting, not a hash exclusion range (BMFF assets bind by box path, §18.6).
//!
//! # Supported / deferred
//!
//! gamut is image-first, so only the still-image (intra) subset of AV1 is in scope — no sequences or
//! animation. **Supported:** 8- and 16-bit RGB and RGBA input and 8-bit grayscale (the five
//! layouts above), and caller-supplied 8/10/12-bit YCbCr or GBR planes ([`RawPlanes`]); **lossless** (the default, decoded output bit-exact to the input at the coded depth)
//! and **lossy** ([`AvifEncoder::lossy`], `quality` `0..=100`) AV1 intra coding — lossless at
//! 4:4:4 through the identity matrix, lossy through **BT.709 YCbCr** by default, with BT.601 /
//! BT.2020-NCL and studio range selectable ([`AvifEncoder::with_matrix`] /
//! [`AvifEncoder::with_color_range`]) at 4:2:0 (the lossy default), 4:2:2 or 4:4:4
//! ([`AvifEncoder::with_chroma`]); a coded depth of 8, 10 or 12 bits
//! ([`AvifEncoder::with_bit_depth`]); **alpha** as a monochrome auxiliary item with its own
//! quality and premultiplication mode ([`AvifEncoder::with_alpha_quality`] /
//! [`AvifEncoder::with_alpha_mode`]); an encode-**speed** preset trading mode-decision search for
//! time ([`AvifEncoder::with_speed`], `gamut-av1`'s [`Speed`]); `irot`/`imir` display orientation ([`AvifEncoder::with_rotation`] /
//! [`AvifEncoder::with_mirror`]); the **colour and metadata surface** — CICP primaries and transfer
//! ([`AvifEncoder::with_primaries`] / [`AvifEncoder::with_transfer`]), HDR `clli` / `mdcv`
//! metadata ([`AvifEncoder::with_content_light_level`] /
//! [`AvifEncoder::with_mastering_display`]), an embedded ICC profile
//! ([`AvifEncoder::with_icc_profile`]), Exif / XMP items
//! ([`AvifEncoder::with_exif`] / [`AvifEncoder::with_xmp`]), and a reserved or written C2PA
//! manifest store (above); and the **container decode surface** above (full read of items,
//! properties, derivations, and metadata; planar, 8-bit and high-bit-depth RGBA presentation around
//! a caller decoder). Output is validated end-to-end against `libavif` (its dav1d-backed reference
//! container decoder); the wrapped AV1 bitstream is cross-checked against `libaom` — the AV1
//! reference codec — and `dav1d` via [`gamut_av1`].
//!
//! **Deferred, planned** (tracked row-by-row against the specs in `STATUS.md`, whose disposition
//! ledger is the authority): the HDR surface beyond CICP tagging and `clli`/`mdcv` (the
//! `cclv`/`amve`/`reve`/`ndwt` properties and film grain), `grid` / `tmap` (gain-map) / `sato` derivations and the remaining
//! container transforms on the encode side, layered/progressive still images, rate control, a **built-in AV1 codestream decoder** behind [`Av1StillDecoder`], and the decoder
//! backend registry / `gamut-codec-abi` adapter around the seam. On the decoder: `gamut-av1`
//! ships the pure-Rust one (its default `decode` feature), but today it parses only the header
//! and tile-framing layer and reconstructs no samples, so gamut-avif does not yet register it as
//! the implicit tail behind [`Av1StillDecoder`] — until it does, the caller supplies that decoder
//! (which will then become optional). All of these land semver-minor — the v1 surface is designed
//! so no deferred feature reshapes it.
//!
//! **Permanently out of scope** (workspace charter — gamut is image-first): image sequences and
//! tracks (the `avis`/`avio` brands) and AV1 inter-frame coding. An `avis` file that also carries
//! a `meta` primary still item is read for that still, which decodes like any other;
//! [`AvifContainer::has_image_sequence`] reports the track it leaves undecoded.
#![forbid(unsafe_code)]

// Compiles README.md's code blocks as doctests; exists only while rustdoc collects them.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod av1c;
pub mod backend;
mod c2pa;
mod clean_alpha;
mod config;
mod container;
mod decode;
mod encoder;
mod image;
/// Executable laws for this crate's sample transforms, shared by its property tests and the
/// out-of-tree fuzz tier (`test-support`). Not part of the stable API; see `docs/testing.md`.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod invariants;
mod obu;
mod transform;

pub use av1c::{Av1Config, ChromaFormat};
pub use backend::{AV1_CODEC_ID, AbiAv1StillEncoder, Av1EncodeRequest, Av1StillEncoder};
pub use c2pa::{
    C2PA_NOT_VALIDATED, C2PA_UUID, C2paBoxPosition, C2paBoxPurpose, C2paMerkleBox, C2paSlot,
    C2paStoreSummary, C2paSummary, C2paUnreadBox, C2paUnreadReason,
};
pub use config::{AvifConfig, AvifMode};
pub use container::{AvifContainer, Segment, SegmentKind, UnknownBox, UnknownBoxLocation};
pub use decode::{Av1StillDecoder, DecodedFrame};
pub use encoder::{AlphaMode, AvifEncodeReport, AvifEncoder, RawPlanes};
pub use gamut_av1::{ColorConfig, SequenceHeader, Speed, Subsampling};
pub use gamut_core::Dimensions;
pub use image::{
    AvifImage, AvifItem, CleanAperture, ContentLightLevel, ItemKind, MasteringDisplay,
    PixelAspectRatio, TransformativeProperty,
};
pub use obu::{Obu, ObuHeader, ObuIter, ObuType, iter_obus};
pub use transform::{Mirror, Rotation};
