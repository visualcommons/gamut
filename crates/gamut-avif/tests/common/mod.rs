//! Shared AVIF test fixtures: model builders that drive `IsoBmffImage::to_bytes`, plus hand-authored
//! box byte-builders (mirroring `gamut-isobmff/tests/common`) for the accounting/meta cases that a
//! normalising writer cannot express.
#![allow(dead_code)] // each integration-test binary uses a different subset

use std::sync::{Arc, Mutex};

use gamut_avif::{
    Av1, Av1CodestreamDecoder, Av1Config, AvifContainer, DecodeLimits, DecodeOptions, DecodedFrame,
    HeifImage,
};
use gamut_codec_abi::Registry;
use gamut_core::{Dimensions, Error, ImageBuf, Result, Rgba8, Rgba16};
use gamut_isobmff::{IsoBmffImage, Item, ItemReference, Property, PropertyKind};

/// A minimal valid `av1C` record: High profile (4:4:4), level 0, 8-bit, empty `configOBUs`.
pub const AV1C_444_8BIT: [u8; 4] = [0x81, 0x20, 0x00, 0x00];

// ---- model builders (write-based fixtures) -------------------------------------------------

/// A bare item: no name, no mime info, no references, no properties.
pub fn item(id: u32, item_type: [u8; 4], payload: Vec<u8>) -> Item {
    Item::new(id, item_type).with_payload(payload)
}

/// An `av1C` codec-configuration property (essential), with the given opaque record bytes.
pub fn av1c(data: Vec<u8>) -> Property {
    Property::new(
        true,
        PropertyKind::CodecConfiguration {
            kind: *b"av1C",
            data,
        },
    )
}

/// A non-essential `ispe` property.
pub fn ispe(width: u32, height: u32) -> Property {
    Property::new(false, PropertyKind::ImageSpatialExtents { width, height })
}

/// A non-essential `auxC` property with the given aux-type URN.
pub fn auxc(aux_type: &str) -> Property {
    Property::new(
        false,
        PropertyKind::AuxiliaryType {
            aux_type: aux_type.to_string(),
            aux_subtype: vec![],
        },
    )
}

/// An outgoing item reference.
pub fn iref(reference_type: &[u8; 4], to: &[u32]) -> ItemReference {
    ItemReference::new(*reference_type, to.to_vec())
}

/// A canonical AV1 coded-image item: `av01`, one `av1C` + one `ispe`, with the given payload.
pub fn av01_item(id: u32, payload: Vec<u8>) -> Item {
    item(id, *b"av01", payload).with_properties(vec![av1c(AV1C_444_8BIT.to_vec()), ispe(64, 48)])
}

/// Wraps items into a complete `avif` file whose primary is the item with `primary_id`.
pub fn avif_image(primary_id: u32, items: Vec<Item>) -> IsoBmffImage {
    IsoBmffImage::new(
        *b"avif",
        vec![*b"avif", *b"mif1", *b"miaf"],
        primary_id,
        items,
    )
}

/// Serialises a model to a clean AVIF file (`ftyp` + `meta` + `mdat`).
pub fn clean_file(primary_id: u32, items: Vec<Item>) -> Vec<u8> {
    avif_image(primary_id, items)
        .to_bytes()
        .expect("valid model")
}

// ---- hand-authored box byte-builders -------------------------------------------------------

/// One complete box: 32-bit size + type + body.
pub fn bx(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
    out.extend_from_slice(ty);
    out.extend_from_slice(body);
    out
}

/// One complete FullBox: box header + 1-byte version + 3-byte flags + body.
pub fn full(ty: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(4 + body.len());
    inner.push(version);
    inner.extend_from_slice(&flags.to_be_bytes()[1..]);
    inner.extend_from_slice(body);
    bx(ty, &inner)
}

/// Concatenates byte chunks.
pub fn cat<T: AsRef<[u8]>>(parts: &[T]) -> Vec<u8> {
    parts.iter().flat_map(|p| p.as_ref().to_vec()).collect()
}

/// A minimal `ftyp` with the given major brand, minor version 0, no compatible brands.
pub fn ftyp(major: &[u8; 4]) -> Vec<u8> {
    bx(b"ftyp", &cat(&[&major[..], &[0, 0, 0, 0]]))
}

/// A spec `hdlr` with handler type `pict`.
pub fn hdlr() -> Vec<u8> {
    full(
        b"hdlr",
        0,
        0,
        &cat(&[&[0u8; 4][..], b"pict", &[0u8; 12], &[0u8]]),
    )
}

/// A `pitm` v0 naming `id`.
pub fn pitm_v0(id: u16) -> Vec<u8> {
    full(b"pitm", 0, 0, &id.to_be_bytes())
}

/// An `infe` v2 for an unprotected item; `flags & 1` marks it hidden.
pub fn infe_v2(id: u16, item_type: &[u8; 4], hidden: bool) -> Vec<u8> {
    full(
        b"infe",
        2,
        u32::from(hidden),
        &cat(&[&id.to_be_bytes()[..], &[0, 0], item_type, &[0]]),
    )
}

/// An `iinf` v0 wrapping the given `infe` boxes.
pub fn iinf_v0(infes: &[Vec<u8>]) -> Vec<u8> {
    let mut body = (infes.len() as u16).to_be_bytes().to_vec();
    for infe in infes {
        body.extend_from_slice(infe);
    }
    full(b"iinf", 0, 0, &body)
}

/// A `meta` FullBox with the given children.
pub fn meta(children: &[Vec<u8>]) -> Vec<u8> {
    full(b"meta", 0, 0, &cat(children))
}

// ---- oracle bridges -------------------------------------------------------------------------

/// [`Av1CodestreamDecoder`] implemented with the real dav1d decoder (`dav1d-oracle`), bridged exactly
/// the way a platform decoder would be: the typed `av1C` and the raw item payload are assembled
/// into one self-contained temporal unit with [`Av1Config::full_stream`] and handed to a Section-5
/// decoder.
///
/// Shared here because more than one integration test decodes a file it just encoded; each binary
/// that does declares `mod common;` and the rest carry it under this file's `allow(dead_code)`.
#[derive(Default)]
pub struct Dav1dDecoder;

impl Av1CodestreamDecoder for Dav1dDecoder {
    fn supports(&mut self, _config: &Av1Config) -> bool {
        true
    }

    fn decode_still(&mut self, config: &Av1Config, payload: &[u8]) -> Result<DecodedFrame> {
        let mut stream = Vec::new();
        config.full_stream(payload, &mut stream)?;
        let picture = dav1d_oracle::decode_obu(&stream).map_err(|_| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "common: dav1d rejected the stream")
        })?;
        let [y, u, v] = picture.planes;
        // `DecodedFrame::new` validates the plane lengths against the av1C-derived chroma, so a
        // config/codestream mismatch fails loudly here rather than decoding to nonsense.
        DecodedFrame::new(
            Dimensions::new(picture.width, picture.height)?,
            picture.bit_depth,
            config.chroma_subsampling(),
            y,
            u,
            v,
        )
    }
}

// ---- decoding through one test decoder ------------------------------------------------------

/// The `irot` angle (anti-clockwise quarter turns) item `item` carries, as on the wire.
pub fn irot(item: &gamut_avif::HeifItem<'_>) -> Option<u8> {
    item.transformative_properties()
        .iter()
        .find_map(|p| match p {
            gamut_heif::TransformativeProperty::Rotation(turns) => Some(*turns),
            _ => None,
        })
}

/// The `imir` axis item `item` carries, as on the wire.
pub fn imir(item: &gamut_avif::HeifItem<'_>) -> Option<u8> {
    item.transformative_properties()
        .iter()
        .find_map(|p| match p {
            gamut_heif::TransformativeProperty::Mirror(axis) => Some(*axis),
            _ => None,
        })
}

/// [`DecodedFrame::new`] over a `width × height` luma size, refusing a zero side as the frame does.
pub fn decoded_frame(
    width: u32,
    height: u32,
    bit_depth: u8,
    chroma: gamut_avif::ChromaSubsampling,
    y: Vec<u16>,
    cb: Vec<u16>,
    cr: Vec<u16>,
) -> Result<DecodedFrame> {
    DecodedFrame::new(
        Dimensions::new(width, height)?,
        bit_depth,
        chroma,
        y,
        cb,
        cr,
    )
}

/// Runs `decode` with `decoder` as the one backend of an `Av1` registry, handing the decoder back
/// afterwards — so a test can inspect what a stateful decoder recorded, as it could when the
/// pipeline borrowed it.
pub fn with_backend<D, R>(
    decoder: &mut D,
    decode: impl FnOnce(&Registry<dyn Av1CodestreamDecoder + Send>) -> R,
) -> R
where
    D: Av1CodestreamDecoder + Send + Default + 'static,
{
    let shared = Arc::new(Mutex::new(std::mem::take(decoder)));
    let mut backends: Registry<dyn Av1CodestreamDecoder + Send> = Registry::new();
    backends.push(shared.clone());
    let out = decode(&backends);
    drop(backends);
    *decoder = Arc::try_unwrap(shared)
        .ok()
        .expect("the registry was dropped")
        .into_inner()
        .expect("the decoder did not panic");
    out
}

/// The `gamut-heif` decode surfaces bound to [`Av1`] and one test decoder, in the shapes this
/// suite's assertions were written against: an item (or the primary one), a decoder, and
/// optionally limits or options.
pub trait Decode {
    /// The semantic view decoded from.
    fn heif(&self) -> &HeifImage;

    fn planar<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        id: u32,
        d: &mut D,
    ) -> Result<DecodedFrame> {
        self.planar_with_limits(id, d, DecodeLimits::default())
    }

    fn planar_with_limits<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        id: u32,
        d: &mut D,
        limits: DecodeLimits,
    ) -> Result<DecodedFrame> {
        with_backend(d, |b| self.heif().decode_item_planar::<Av1>(id, b, limits))
    }

    fn rgba8<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        id: u32,
        d: &mut D,
    ) -> Result<ImageBuf<Rgba8>> {
        self.rgba8_with_options(id, d, DecodeOptions::default())
    }

    fn rgba8_with_limits<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        id: u32,
        d: &mut D,
        limits: DecodeLimits,
    ) -> Result<ImageBuf<Rgba8>> {
        self.rgba8_with_options(id, d, DecodeOptions::default().with_limits(limits))
    }

    fn rgba8_with_options<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        id: u32,
        d: &mut D,
        options: DecodeOptions,
    ) -> Result<ImageBuf<Rgba8>> {
        with_backend(d, |b| self.heif().decode_item_rgba8::<Av1>(id, b, options))
    }

    fn rgba16<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        id: u32,
        d: &mut D,
    ) -> Result<ImageBuf<Rgba16>> {
        self.rgba16_with_options(id, d, DecodeOptions::default())
    }

    fn rgba16_with_limits<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        id: u32,
        d: &mut D,
        limits: DecodeLimits,
    ) -> Result<ImageBuf<Rgba16>> {
        self.rgba16_with_options(id, d, DecodeOptions::default().with_limits(limits))
    }

    fn rgba16_with_options<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        id: u32,
        d: &mut D,
        options: DecodeOptions,
    ) -> Result<ImageBuf<Rgba16>> {
        with_backend(d, |b| self.heif().decode_item_rgba16::<Av1>(id, b, options))
    }

    fn primary_rgba8<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        d: &mut D,
    ) -> Result<ImageBuf<Rgba8>> {
        self.rgba8(self.heif().primary_item().id(), d)
    }

    fn primary_rgba8_with_limits<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        d: &mut D,
        limits: DecodeLimits,
    ) -> Result<ImageBuf<Rgba8>> {
        self.rgba8_with_limits(self.heif().primary_item().id(), d, limits)
    }

    fn primary_rgba8_with_options<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        d: &mut D,
        options: DecodeOptions,
    ) -> Result<ImageBuf<Rgba8>> {
        self.rgba8_with_options(self.heif().primary_item().id(), d, options)
    }

    fn primary_rgba16<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        d: &mut D,
    ) -> Result<ImageBuf<Rgba16>> {
        self.rgba16(self.heif().primary_item().id(), d)
    }

    fn primary_rgba16_with_limits<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        d: &mut D,
        limits: DecodeLimits,
    ) -> Result<ImageBuf<Rgba16>> {
        self.rgba16_with_limits(self.heif().primary_item().id(), d, limits)
    }

    fn primary_rgba16_with_options<D: Av1CodestreamDecoder + Send + Default + 'static>(
        &self,
        d: &mut D,
        options: DecodeOptions,
    ) -> Result<ImageBuf<Rgba16>> {
        self.rgba16_with_options(self.heif().primary_item().id(), d, options)
    }
}

impl Decode for HeifImage {
    fn heif(&self) -> &HeifImage {
        self
    }
}

impl Decode for AvifContainer<'_> {
    fn heif(&self) -> &HeifImage {
        self.image()
    }
}
