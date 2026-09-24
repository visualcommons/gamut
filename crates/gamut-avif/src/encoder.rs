//! The AVIF still-image encoder: RGB → identity planes → AV1 temporal unit → ISOBMFF container.

use std::sync::{Arc, Mutex};

use gamut_av1::{
    Av1Colour, Av1StillConfig, EncodedStill, encode_still_intra_with, encode_still_intra16_with,
};
use gamut_color::{
    BitDepth, ChromaSubsampling, ColorRange, ColourPrimaries, MatrixCoefficients, Planar8,
    Planar16, RgbToYcbcr, TransferCharacteristics,
};
use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef, Result, Rgb8, Rgb16, Rgba8, Rgba16};
use gamut_isobmff::{
    ColourInformation, IsoBmffImage, Item, ItemReference, NclxColr, Property, PropertyKind, write,
};

use crate::backend::{Av1EncodeRequest, Av1StillEncoder, BackendPlanes, BackendSlot};
use crate::config::{AvifConfig, AvifMode};
use crate::image::ALPHA_AUX_URN;
use crate::transform::{Mirror, Rotation};

/// The encoder's display-orientation transforms, applied by a reader at display time (the stored
/// pixels are unchanged). Maps to the `irot`/`imir` item properties.
#[derive(Debug, Clone, Copy, Default)]
struct ImageTransform {
    /// `irot` rotation in 90° steps (`0..=3`), anti-clockwise. `0` writes no `irot`.
    rotation_ccw: u8,
    /// `imir` mirror axis: `Some(0)` vertical (left↔right), `Some(1)` horizontal (top↔bottom).
    mirror_axis: Option<u8>,
}

/// Encodes images to AVIF still images.
///
/// 8-bit input, mapped to AV1 planes. Construct with [`AvifEncoder::new`] (lossless),
/// [`AvifEncoder::lossless`], or [`AvifEncoder::lossy`], then encode via the
/// [`EncodeImage`](gamut_core::EncodeImage) trait, taking a typed [`ImageRef`].
/// [`AvifEncoder::with_rotation`] / [`AvifEncoder::with_mirror`] add `irot`/`imir`
/// display-orientation transforms.
///
/// # Inputs
///
/// | [`Pixel`](gamut_core::Pixel) | Coded as |
/// | --- | --- |
/// | [`Rgb8`] | one 4:4:4 colour item |
/// | [`Rgba8`] | a 4:4:4 colour item plus a **monochrome alpha auxiliary item** |
/// | [`Gray8`] | one **monochrome** item — not R=G=B replication |
/// | [`Rgb16`] | one 4:4:4 colour item at [`AvifEncoder::with_bit_depth`]'s depth |
/// | [`Rgba16`] | that, plus a monochrome alpha auxiliary at the same depth |
///
/// Alpha is coded as its own AV1 still: AVIF v1.2.0 §4.1 makes `mono_chrome = 1` and full range a
/// *shall* for an AV1 auxiliary image item, and the auxiliary is linked to the colour item by an
/// `auxl` reference and typed by an essential `auxC` property.
/// [`AvifEncoder::with_premultiplied_alpha`] declares that the colour values are already
/// multiplied by it.
///
/// The AV1 codestream comes from `gamut-av1` by default; [`AvifEncoder::push_backend`] registers
/// alternate [`Av1StillEncoder`] backends ahead of it (see [`crate::backend`] for the fallback
/// contract).
#[derive(Clone)]
pub struct AvifEncoder {
    /// Lossless/lossy mode and the lossy quality factor.
    config: AvifConfig,
    /// Optional `irot`/`imir` display-orientation transforms.
    transform: ImageTransform,
    /// An embedded ICC profile, carried verbatim into a `colr` box of type `prof`.
    icc: Option<Vec<u8>>,
    /// An Exif payload — a bare TIFF stream — carried verbatim into an `Exif` metadata item.
    exif: Option<Vec<u8>>,
    /// An XMP packet, carried verbatim into a `mime` metadata item.
    xmp: Option<Vec<u8>>,
    /// Whether the colour values are premultiplied by alpha, emitted as a `prem` item reference.
    /// Meaningless without an alpha channel, so it reaches the file only for an RGBA encode.
    premultiplied: bool,
    /// Pluggable AV1 still-encode backends, tried in push order before the `gamut-av1` tail.
    /// Shared (not copied) by [`Clone`] — see [`AvifEncoder::push_backend`].
    backends: Vec<BackendSlot>,
}

/// Hand-written because a backend registry is not [`Debug`]: the backends are opaque
/// caller-supplied objects, so they are summarized by count. Every other field is printed as the
/// derive would.
impl std::fmt::Debug for AvifEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AvifEncoder")
            .field("config", &self.config)
            .field("transform", &self.transform)
            // Payload *lengths*, not bytes: a profile is kilobytes of binary and would swamp the
            // output, while its presence and size are what a caller debugging a build wants.
            .field("icc", &self.icc.as_ref().map(Vec::len))
            .field("exif", &self.exif.as_ref().map(Vec::len))
            .field("xmp", &self.xmp.as_ref().map(Vec::len))
            .field("premultiplied", &self.premultiplied)
            .field("backends", &self.backends.len())
            .finish()
    }
}

impl Default for AvifEncoder {
    /// The default encoder is **lossless** — defined as [`AvifEncoder::lossless`].
    fn default() -> Self {
        Self::lossless()
    }
}

impl AvifEncoder {
    /// Creates an encoder with the default configuration; equivalent to [`AvifEncoder::lossless`].
    #[must_use]
    pub fn new() -> Self {
        Self::lossless()
    }

    /// Creates an encoder that produces a **lossless** still image — the decoded output is bit-exact
    /// to the input. This is the default mode, so [`AvifEncoder::new`] and [`AvifEncoder::default`]
    /// return the same encoder; it exists to pair with [`AvifEncoder::lossy`] and make intent
    /// explicit at the call site.
    #[must_use]
    pub fn lossless() -> Self {
        Self {
            // `AvifConfig::default()` outright, with no `mode:` override: lossless *is* the
            // default mode, so writing it again changed nothing and the field could be deleted
            // with the suite green (#110). `lossless_is_the_default_mode` below pins the coupling
            // this relies on, so a change to the default fails there rather than silently making
            // `lossless()` lossy.
            config: AvifConfig::default(),
            transform: ImageTransform::default(),
            icc: None,
            exif: None,
            xmp: None,
            premultiplied: false,
            backends: Vec::new(),
        }
    }

    /// Creates an encoder that produces a **lossy** still image at the given `quality` (`0..=100`,
    /// higher = larger output, closer to the source; values above `100` are clamped).
    /// Lossy stills are coded in **BT.709 YCbCr** at **4:2:0** by default (full range): the
    /// luma–chroma decorrelation is worth a large fraction of the bitrate and costs nothing in
    /// coding tools, and 4:2:0 (AV1 Profile 0) is what still-picture hardware decoders read.
    /// Override the matrix with [`AvifEncoder::with_matrix`] and the sampling with
    /// [`AvifEncoder::with_chroma`]; note the identity matrix forces 4:4:4 (§6.4.2).
    #[must_use]
    pub fn lossy(quality: u8) -> Self {
        Self {
            config: AvifConfig {
                mode: AvifMode::Lossy,
                quality,
                matrix: MatrixCoefficients::Bt709,
                chroma: ChromaSubsampling::Cs420,
                ..AvifConfig::default()
            },
            transform: ImageTransform::default(),
            icc: None,
            exif: None,
            xmp: None,
            premultiplied: false,
            backends: Vec::new(),
        }
    }

    /// Returns a snapshot of the encoder's configuration.
    #[must_use]
    pub fn config(&self) -> AvifConfig {
        self.config
    }

    /// Selects the CICP matrix the samples are coded through — the colour half of the
    /// space/quality tradeoff.
    ///
    /// Supported: [`MatrixCoefficients::Identity`] (R'G'B' as GBR planes, no decorrelation),
    /// [`MatrixCoefficients::Bt601`], [`MatrixCoefficients::Bt709`] (the lossy default) and
    /// [`MatrixCoefficients::Bt2020Ncl`]. Any other code point is rejected at encode time.
    ///
    /// **Ignored by [`AvifMode::Lossless`]**, which always uses the identity matrix — an 8-bit
    /// YCbCr round trip is not bit-exact, so a lossless encode through a luma–chroma matrix would
    /// not be lossless. This mirrors how lossless already ignores `quality`.
    ///
    /// The matrix is a *coding* choice, independent of the gamut: a BT.2020 matrix with BT.709
    /// primaries is legal CICP. A caller wanting a true BT.2020 image selects the gamut with
    /// [`AvifEncoder::with_primaries`], not with this knob.
    #[must_use]
    pub fn with_matrix(mut self, matrix: MatrixCoefficients) -> Self {
        self.config.matrix = matrix;
        self
    }

    /// Selects how chroma is sampled relative to luma — the geometry half of the space/quality
    /// tradeoff, and the half that decides which decoders can read the result.
    ///
    /// [`AvifMode::Lossy`] defaults to [`ChromaSubsampling::Cs420`] (AV1 Main profile).
    /// [`ChromaSubsampling::Cs444`] keeps full-resolution chroma but is AV1 **Profile 1**, which
    /// hardware still-image decoders frequently reject; [`ChromaSubsampling::Cs422`] is Profile 2,
    /// matches no AVIF profile brand at all, and loses the encoder half its rectangular partition
    /// set, because AV1 §6.10.4 forbids taller-than-wide blocks under it.
    ///
    /// **Ignored by [`AvifMode::Lossless`]**, which always keeps 4:4:4, and by the identity
    /// matrix, which §6.4.2 forces to 4:4:4 whatever is set here — see [`AvifConfig::chroma`].
    /// [`ChromaSubsampling::Cs400`] is rejected at encode time on the YCbCr path; under the
    /// identity matrix it is ignored along with every other value.
    #[must_use]
    pub fn with_chroma(mut self, chroma: ChromaSubsampling) -> Self {
        self.config.chroma = chroma;
        self
    }

    /// Tags the image with CICP colour **primaries** — the gamut its R'G'B' values are interpreted
    /// in. Defaults to [`ColourPrimaries::Bt709`] (sRGB's gamut).
    ///
    /// This is a **tag, not a conversion**: the encoder does not gamut-map anything, so it declares
    /// what the caller's samples already are. Unlike [`with_matrix`](Self::with_matrix) and
    /// [`with_color_range`](Self::with_color_range) it touches no sample, so it applies to
    /// [`AvifMode::Lossless`] as well as [`AvifMode::Lossy`].
    ///
    /// The value reaches both the AV1 sequence header's `color_config()` and the container's `colr`
    /// box, which are required to agree (AV1-ISOBMFF v1.3.0 §2.3.4).
    #[must_use]
    pub fn with_primaries(mut self, primaries: ColourPrimaries) -> Self {
        self.config.primaries = primaries;
        self
    }

    /// Tags the image with CICP **transfer characteristics** — the transfer function already
    /// applied to its samples. Defaults to [`TransferCharacteristics::Srgb`].
    ///
    /// A tag, not a conversion, exactly as [`with_primaries`](Self::with_primaries) is, and likewise
    /// honoured on the lossless path. Selecting [`TransferCharacteristics::Pq`] or
    /// [`TransferCharacteristics::Hlg`] *labels* samples the caller has already encoded that way; it
    /// does not by itself produce a complete HDR image, because the HDR metadata properties
    /// (`mdcv`/`clli`) are still deferred (`STATUS.md`).
    ///
    /// # Interaction with the AV1 sRGB shortcut
    ///
    /// AV1 §5.5.2 infers full range and 4:4:4 — coding no bits for either — only for the exact
    /// triple BT.709 primaries / sRGB transfer / identity matrix. Selecting any other primaries or
    /// transfer leaves that shortcut, so `color_range` is then coded explicitly. The stream stays
    /// conformant: AV1 §6.4.2's only requirement for the identity matrix is 4:4:4, which always
    /// holds here.
    #[must_use]
    pub fn with_transfer(mut self, transfer: TransferCharacteristics) -> Self {
        self.config.transfer = transfer;
        self
    }

    /// Selects the signal range the coded samples occupy, signalled in `colr` and the AV1 sequence
    /// header. Defaults to [`ColorRange::Full`], the AVIF ecosystem's default.
    ///
    /// Ignored by [`AvifMode::Lossless`] (and by an identity-matrix encode generally): AV1's
    /// §5.5.2 sRGB shortcut infers full range for BT.709/sRGB/identity and codes no bit for it.
    #[must_use]
    pub fn with_color_range(mut self, range: ColorRange) -> Self {
        self.config.range = range;
        self
    }

    /// Selects the depth **16-bit inputs** are coded at: [`BitDepth::Ten`] or
    /// [`BitDepth::Twelve`] (the default). Any other depth is rejected at encode time.
    ///
    /// # What happens to the low bits
    ///
    /// [`Rgb16`] and [`Rgba16`] carry samples on `gamut-core`'s canonical **full 16-bit scale**,
    /// while AV1 codes 8, 10 or 12. The encoder narrows by **truncation** — `sample >> (16 -
    /// depth)` — so the coded value is the top `depth` bits of the caller's sample. That makes the
    /// contract worth stating exactly:
    ///
    /// > A lossless encode of a 16-bit input is bit-exact **at the coded depth**, not to the
    /// > 16-bit input.
    ///
    /// Truncation rather than rounding keeps the mapping a pure prefix: the same source narrowed to
    /// 10 and to 12 bits agrees on the 10 bits they share, and no sample can round up out of range.
    /// A caller who wants a different tradeoff — dithering, or a rounded narrowing — applies it
    /// before handing the image over.
    ///
    /// **Ignored by the 8-bit inputs.** [`Rgb8`], [`Rgba8`] and [`Gray8`] always code 8-bit:
    /// widening them would claim precision the caller never had.
    #[must_use]
    pub fn with_bit_depth(mut self, bit_depth: BitDepth) -> Self {
        self.config.bit_depth = bit_depth;
        self
    }

    /// Declares that the colour values of an RGBA input are **premultiplied** by their alpha —
    /// i.e. already multiplied through, so a fully transparent pixel carries zeroed colour.
    ///
    /// This is a **declaration, not a conversion**: the encoder multiplies nothing and divides
    /// nothing. It records the caller's assertion as a `prem` item reference from the colour item
    /// to its alpha auxiliary (ISO/IEC 23008-12 §6), which is what tells a reader whether to
    /// un-premultiply before compositing. Getting it wrong darkens or halos the edges of the
    /// displayed image, so the default is `false` — unassociated alpha, the interpretation
    /// [`Rgba8`]'s [`ColorModel::Rgba`](gamut_core::ColorModel::Rgba) already documents.
    ///
    /// Ignored by every input without an alpha channel: with no alpha auxiliary there is nothing
    /// for the reference to target.
    #[must_use]
    pub fn with_premultiplied_alpha(mut self, premultiplied: bool) -> Self {
        self.premultiplied = premultiplied;
        self
    }

    /// The colour signalling and plane layout this encoder's configuration selects.
    ///
    /// The split is between knobs that **transform samples** and knobs that only **tag** them.
    /// `matrix` and `range` transform: an 8-bit YCbCr round trip is not bit-exact and studio range
    /// discards codes, so lossless pins identity/full and ignores both (see
    /// [`AvifEncoder::with_matrix`]). `primaries` and `transfer` touch no sample, so both modes
    /// carry whatever the caller selected.
    fn colour(&self) -> Av1Colour {
        let tagged = Av1Colour {
            primaries: self.config.primaries,
            transfer: self.config.transfer,
            ..Av1Colour::default()
        };
        match self.config.mode {
            AvifMode::Lossless => tagged,
            _ => Av1Colour {
                matrix: self.config.matrix,
                range: self.config.range,
                ..tagged
            },
        }
    }

    /// The colour signalling for a **monochrome** coded item: the caller's primaries and transfer
    /// over `gamut-av1`'s monochrome defaults.
    ///
    /// Neither [`with_matrix`](Self::with_matrix) nor [`with_color_range`](Self::with_color_range)
    /// applies here, and both are dropped rather than half-honoured. There is no chroma for a
    /// matrix to produce — AV1 §6.4.2 in fact forbids `MC_IDENTITY` on a single-plane stream — and
    /// the encoder scales no samples, so signalling limited range would describe samples it did
    /// not narrow. `primaries` and `transfer` only *tag* the samples, so they carry through as
    /// they do for a colour item.
    fn monochrome_colour(&self) -> Av1Colour {
        Av1Colour {
            primaries: self.config.primaries,
            transfer: self.config.transfer,
            ..Av1Colour::monochrome()
        }
    }

    /// The chroma sampling this configuration codes. Lossless pins 4:4:4 regardless of the
    /// configured value, for the same reason it pins the identity matrix.
    fn chroma(&self) -> ChromaSubsampling {
        match self.config.mode {
            AvifMode::Lossless => ChromaSubsampling::Cs444,
            _ => self.config.chroma,
        }
    }

    /// The depth a 16-bit input is coded at, rejecting one AV1 cannot express.
    ///
    /// §6.4.1 defines 8, 10 and 12. [`BitDepth::Sixteen`] is a `gamut-color` depth for the
    /// interleaved 16-bit pipelines, and [`BitDepth::Eight`] would silently discard the input's
    /// whole point, so both are refused rather than quietly reinterpreted.
    fn coded_bit_depth(&self) -> Result<BitDepth> {
        match self.config.bit_depth {
            BitDepth::Ten | BitDepth::Twelve => Ok(self.config.bit_depth),
            _ => Err(gamut_core::Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: a 16-bit input codes at 10 or 12 bits (AV1 \u{a7}6.4.1); \
                 select one with AvifEncoder::with_bit_depth",
            )),
        }
    }

    /// The AV1 `base_q_idx` this configuration codes at: `0` — the lossless path — or the
    /// quality mapping's quantizer.
    fn base_q_idx(&self) -> u8 {
        match self.config.mode {
            AvifMode::Lossless => 0,
            AvifMode::Lossy => quality_to_quant(self.config.quality),
        }
    }

    /// Embeds an ICC profile, describing the image's colour space to a colour-managed reader.
    ///
    /// The bytes are carried **verbatim** — the encoder neither parses nor validates the profile —
    /// as a `colr` box of type `prof` (the unrestricted form, ISO/IEC 23008-12). `rICC` is not
    /// emitted: it asserts the profile fits HEIF's restricted subset, which cannot be checked
    /// without parsing it. Calling this twice keeps the **last** profile.
    ///
    /// The CICP `colr` box stays alongside it. It is not redundant: it describes what the AV1
    /// codestream itself declares, and the two are required to agree (AV1-ISOBMFF v1.3.0 §2.3.4).
    /// A colour-managed reader prefers the ICC profile; everything else falls back to the CICP
    /// code points.
    #[must_use]
    pub fn with_icc_profile(mut self, profile: &[u8]) -> Self {
        self.icc = Some(profile.to_vec());
        self
    }

    /// Attaches Exif metadata, describing the primary image.
    ///
    /// `exif` is a **bare TIFF stream** — a byte-order mark, `42`, and the offset of the first IFD
    /// — which is what [`gamut_exif::ExifWriter::write`] produces and what a PNG `eXIf` chunk or a
    /// WebP `EXIF` chunk carries. It is *not* the `Exif\0\0`-prefixed form. The encoder adds the
    /// 4-byte big-endian `exif_tiff_header_offset` that HEIF wraps around the stream, so a caller
    /// never has to know that framing.
    ///
    /// The bytes are carried **verbatim** — the encoder neither parses nor rewrites them — as an
    /// `Exif` item with a `cdsc` reference to the primary image. Calling this twice keeps the
    /// **last** payload.
    ///
    /// Because nothing is validated here, a malformed stream reaches the file intact, and readers
    /// do check: libavif rejects the whole file at parse time if the item is not a TIFF stream. The
    /// caller owes a well-formed one.
    ///
    /// [`gamut_exif::ExifWriter::write`]: https://docs.rs/gamut-exif
    #[must_use]
    pub fn with_exif(mut self, exif: &[u8]) -> Self {
        self.exif = Some(exif.to_vec());
        self
    }

    /// Attaches an XMP packet, describing the primary image.
    ///
    /// Takes bytes rather than `&str` because a packet may legitimately open with a byte-order
    /// mark. They are carried **verbatim** as a `mime` item whose `content_type` is
    /// `application/rdf+xml`, with a `cdsc` reference to the primary image. Calling this twice
    /// keeps the **last** payload.
    #[must_use]
    pub fn with_xmp(mut self, xmp: &[u8]) -> Self {
        self.xmp = Some(xmp.to_vec());
        self
    }

    /// Records an `irot` display [`Rotation`] applied by a reader (the stored pixels are unchanged,
    /// so this captures e.g. a camera's EXIF orientation without re-encoding rotated samples).
    /// [`Rotation::None`] writes no `irot`. Returns the updated encoder for chaining.
    #[must_use]
    pub fn with_rotation(mut self, rotation: Rotation) -> Self {
        self.transform.rotation_ccw = rotation.quarter_turns();
        self
    }

    /// Records an `imir` display [`Mirror`] applied by a reader (the stored pixels are unchanged).
    /// Returns the updated encoder for chaining.
    #[must_use]
    pub fn with_mirror(mut self, mirror: Mirror) -> Self {
        self.transform.mirror_axis = Some(mirror.axis());
        self
    }

    /// Registers an [`Av1StillEncoder`] backend for the AV1 codestream, returning `&mut self` for
    /// chaining.
    ///
    /// Backends are tried in **push order**, ahead of the built-in `gamut-av1` encoder, which is
    /// the implicit tail: for each encode the first backend whose
    /// [`supports`](Av1StillEncoder::supports) returns `true` produces the codestream, a backend
    /// that returns `false` is skipped, and if every backend declines the built-in encoder runs. A
    /// backend that accepts a job and then fails propagates its error — the built-in encoder is
    /// **not** used as a silent fallback, because substituting a different encoder would change
    /// the output bytes unpredictably. An encoder with no backends is byte-for-byte the encoder
    /// this crate shipped before backends existed.
    ///
    /// # Cloning shares backends
    ///
    /// Backends are held behind [`Arc`]`<`[`Mutex`]`<…>>`, so [`clone`](Clone::clone)ing an
    /// `AvifEncoder` yields an encoder that drives **the same** backend objects, not copies. A
    /// stateful backend therefore observes the encodes of every clone, and clones made *before* a
    /// `push_backend` call do not gain the new backend (the registry vector itself is per-encoder).
    ///
    /// ```
    /// use gamut_avif::{Av1EncodeRequest, Av1StillEncoder, AvifEncoder};
    /// use gamut_color::Planar8;
    ///
    /// // A backend that declines everything, so encoding still uses the built-in encoder.
    /// struct Decline;
    /// impl Av1StillEncoder for Decline {
    ///     fn supports(&mut self, _req: &Av1EncodeRequest) -> bool { false }
    ///     fn encode_still(
    ///         &mut self,
    ///         _req: &Av1EncodeRequest,
    ///         _planes: &Planar8,
    ///     ) -> gamut_core::Result<Vec<u8>> {
    ///         unreachable!("declined")
    ///     }
    /// }
    ///
    /// let mut encoder = AvifEncoder::new();
    /// encoder.push_backend(Decline);
    /// ```
    pub fn push_backend(&mut self, backend: impl Av1StillEncoder + 'static) -> &mut Self {
        self.backends.push(Arc::new(Mutex::new(backend)));
        self
    }

    /// Wraps the encoded AV1 temporal unit in the AVIF container, stamping `av1C`/`colr`/`ispe`/`pixi`
    /// from the AV1 configuration so the cross-box consistency requirements hold by construction
    /// (AVIF v1.2.0 §2.2, AV1-ISOBMFF v1.3.0 §2.3.4), and appending whatever colour and metadata
    /// payloads the encoder was configured with.
    ///
    /// `alpha`, when present, is the separately coded monochrome still that becomes the alpha
    /// auxiliary image item.
    fn build_avif(
        &self,
        still: &EncodedStill,
        dims: Dimensions,
        alpha: Option<&EncodedStill>,
    ) -> Result<Vec<u8>> {
        let c = &still.config;
        // av1C is essential; ispe/pixi/colr are descriptive. Order fixes the ipco/ipma indices.
        let mut properties = vec![
            Property {
                essential: true,
                kind: PropertyKind::CodecConfiguration {
                    kind: *b"av1C",
                    data: av1c_record(c).to_vec(),
                },
            },
            Property {
                essential: false,
                kind: PropertyKind::ImageSpatialExtents {
                    width: dims.width,
                    height: dims.height,
                },
            },
            Property {
                essential: false,
                kind: PropertyKind::PixelInformation {
                    bits_per_channel: pixi_channels(c),
                },
            },
            Property {
                essential: false,
                kind: PropertyKind::Colour(ColourInformation::Nclx(NclxColr {
                    colour_primaries: c.color_primaries,
                    transfer_characteristics: c.transfer_characteristics,
                    matrix_coefficients: c.matrix_coefficients,
                    full_range: c.full_range,
                })),
            },
        ];
        // The ICC profile is a *second* `colr`, of a different `colour_type` — ISO/IEC 14496-12 §12.1.5
        // allows one of each. It is appended after the four properties the v1 surface always writes, so
        // an encoder with no profile keeps exactly the `ipco`/`ipma` indices it always had.
        if let Some(icc) = &self.icc {
            properties.push(Property {
                essential: false,
                kind: PropertyKind::Colour(ColourInformation::UnrestrictedIcc(icc.clone())),
            });
        }
        // Transformative properties are essential (MIAF §7.3.6.7); applied irot-then-imir.
        if self.transform.rotation_ccw != 0 {
            properties.push(Property {
                essential: true,
                kind: PropertyKind::Rotation(self.transform.rotation_ccw),
            });
        }
        if let Some(axis) = self.transform.mirror_axis {
            properties.push(Property {
                essential: true,
                kind: PropertyKind::Mirror(axis),
            });
        }
        let mut items = vec![Item {
            id: PRIMARY_ITEM_ID,
            item_type: *b"av01",
            name: String::new(),
            content_type: None,
            content_encoding: None,
            hidden: false,
            references: vec![],
            properties,
            payload: still.obus.clone(),
        }];
        // The alpha auxiliary precedes the metadata items, so a file with alpha *and* Exif reads
        // 1 = colour, 2 = alpha, 3 = Exif — and a file without alpha keeps exactly the ids it
        // always had.
        if let Some(alpha) = alpha {
            let id = next_item_id(&items);
            items.push(alpha_item(id, alpha, dims));
            // `prem` runs colour image → alpha auxiliary, so the reference lives on the *colour*
            // item — the opposite direction to the `auxl` the auxiliary owns.
            if self.premultiplied {
                items[0].references.push(ItemReference {
                    reference_type: *b"prem",
                    to_item_ids: vec![id],
                });
            }
        }
        // Metadata items follow, taking the next free id in a fixed order. Ids come
        // from position among the items actually present, so an XMP-only file gets id 2 — the
        // primary stays id 1 (and `pitm` names it) whatever is attached.
        if let Some(exif) = &self.exif {
            // HEIF/AVIF wraps the TIFF stream in a 4-byte big-endian `exif_tiff_header_offset`;
            // `0` means the stream starts immediately after it. `AvifItem` exposes the payload
            // *including* this prefix, so both directions describe the same bytes.
            let mut payload = Vec::with_capacity(4 + exif.len());
            payload.extend_from_slice(&0u32.to_be_bytes());
            payload.extend_from_slice(exif);
            items.push(metadata_item(next_item_id(&items), *b"Exif", None, payload));
        }
        if let Some(xmp) = &self.xmp {
            items.push(metadata_item(
                next_item_id(&items),
                *b"mime",
                Some(XMP_CONTENT_TYPE.to_owned()),
                xmp.clone(),
            ));
        }
        let image = IsoBmffImage::new(*b"avif", compatible_brands(&items), PRIMARY_ITEM_ID, items);
        write(&image)
    }

    /// Encodes the **colour** planes, offering the job to the registered backends in push order
    /// before falling through to the built-in `gamut-av1` tail.
    fn colour_still(
        &self,
        planes: &Planar8,
        dims: Dimensions,
        base_q_idx: u8,
        colour: Av1Colour,
        chroma: ChromaSubsampling,
    ) -> Result<EncodedStill> {
        let request = Av1EncodeRequest::new(dims, base_q_idx, colour, chroma, BitDepth::Eight);
        match crate::backend::run_backends(&self.backends, &request, BackendPlanes::Eight(planes))?
        {
            Some(obus) => {
                crate::backend::still_from_backend_obus(obus, dims, colour, chroma, BitDepth::Eight)
            }
            None => Ok(encode_still_intra_with(planes, base_q_idx, colour)?.0),
        }
    }

    /// The high-bit-depth counterpart of [`colour_still`](Self::colour_still): the same registry,
    /// entered through [`Av1StillEncoder::encode_still16`], whose default declines so a backend
    /// written against the 8-bit contract falls through to the built-in tail.
    fn colour_still16(
        &self,
        planes: &Planar16,
        dims: Dimensions,
        base_q_idx: u8,
        colour: Av1Colour,
    ) -> Result<EncodedStill> {
        let bit_depth = planes.bit_depth();
        // Read off the buffer, not the configuration, exactly as the 8-bit path does: §6.4.2
        // forces 4:4:4 under the identity matrix and the `Rgba16` path has no 4-stride
        // downsampler, so either can carry 4:4:4 planes while `with_chroma` says otherwise. The
        // request must describe the planes it actually carries.
        let chroma = planes.subsampling();
        let request = Av1EncodeRequest::new(dims, base_q_idx, colour, chroma, bit_depth);
        match crate::backend::run_backends(&self.backends, &request, BackendPlanes::High(planes))? {
            Some(obus) => {
                crate::backend::still_from_backend_obus(obus, dims, colour, chroma, bit_depth)
            }
            None => Ok(encode_still_intra16_with(planes, base_q_idx, colour)?.0),
        }
    }

    /// Builds the container around an encoded still and appends it to `out`, returning the number
    /// of bytes written — the shared tail of every `EncodeImage` impl.
    fn emit(
        &self,
        still: &EncodedStill,
        dims: Dimensions,
        alpha: Option<&EncodedStill>,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let file = self.build_avif(still, dims, alpha)?;
        out.extend_from_slice(&file);
        Ok(file.len())
    }
}

/// Encodes **monochrome** planes, always through the built-in `gamut-av1` encoder.
///
/// The [`Av1StillEncoder`] seam is deliberately skipped. Its v1 contract fixes 8-bit 4:4:4
/// `seq_profile = 1` (see [`Av1StillEncoder::encode_still`]), and
/// [`supports`](Av1StillEncoder::supports) is handed an [`Av1EncodeRequest`] that cannot express
/// anything else — so a backend registered against that contract has no way to decline a
/// monochrome job, and would be given single-plane input it never agreed to encode. Widening the
/// request is the additive change that opens this path to backends (`STATUS.md`).
fn monochrome_still(planes: &Planar8, base_q_idx: u8, colour: Av1Colour) -> Result<EncodedStill> {
    Ok(encode_still_intra_with(planes, base_q_idx, colour)?.0)
}

/// The high-bit-depth [`monochrome_still`]; the registry is skipped for the same reason.
fn monochrome_still16(
    planes: &Planar16,
    base_q_idx: u8,
    colour: Av1Colour,
) -> Result<EncodedStill> {
    Ok(encode_still_intra16_with(planes, base_q_idx, colour)?.0)
}

/// The `pixi` bits-per-channel list for a coded item: one entry per coded plane, each carrying the
/// stream's own bit depth (AVIF v1.2.0 §2.2 requires `pixi` to describe the item's actual
/// channels). A monochrome item declares one channel, not three.
///
/// The depth is read back out of the `av1C` flags rather than passed alongside them, so `pixi` and
/// `av1C` cannot disagree.
fn pixi_channels(c: &Av1StillConfig) -> Vec<u8> {
    let bits = match (c.high_bitdepth, c.twelve_bit) {
        (false, _) => 8,
        (true, false) => 10,
        (true, true) => 12,
    };
    if c.monochrome {
        vec![bits]
    } else {
        vec![bits; 3]
    }
}

/// The AV1 `seq_profile` and `seq_level_idx[0]` an `av01` item declares, or `None` when it carries
/// no `av1C` to read them from.
///
/// Reads the record the writer is about to emit, so a brand claim is derived from the bytes in the
/// file rather than from a parallel flag. Both fields share the record's **second** byte
/// (AV1-ISOBMFF v1.3.0 §2.3.3; the first is the marker/version pair `0x81`), as [`av1c_record`]
/// writes it: `seq_profile` in the top three bits, `seq_level_idx[0]` in the low five.
fn av1_profile_level(item: &Item) -> Option<(u8, u8)> {
    /// The four-CC of the AV1 item configuration property (AV1-ISOBMFF v1.3.0 §2.2.1).
    const AV1C: [u8; 4] = *b"av1C";
    item.properties.iter().find_map(|p| match &p.kind {
        PropertyKind::CodecConfiguration { kind: AV1C, data } => {
            data.get(1).map(|&b| (b >> 5, b & 0x1f))
        }
        _ => None,
    })
}

/// AV1 `seq_profile` 1 — the High Profile, the only one the AVIF Advanced Profile admits for an
/// image item (AVIF v1.2.0 §8.3).
const HIGH_PROFILE: u8 = 1;

/// AV1 `seq_profile` 0 — the Main Profile, the only one the AVIF Baseline Profile admits (§8.2).
const MAIN_PROFILE: u8 = 0;

/// `seq_level_idx` 16 is AV1 level 6.0, the Advanced Profile's ceiling (§8.3).
const MA1A_MAX_LEVEL: u8 = 16;

/// `seq_level_idx` 13 is AV1 level 5.1, the Baseline Profile's ceiling (§8.2).
const MA1B_MAX_LEVEL: u8 = 13;

/// The `ftyp` compatible brands for a file built from `items` (AVIF v1.2.0 §8.1-8.3).
///
/// The two AVIF profile brands each constrain the **AV1** profile and level:
///
/// - `MA1B` (Baseline, §8.2) — "the AV1 profile shall be the Main Profile and the level shall be
///   5.1 or lower". Main is 4:2:0, and also monochrome — which is what an alpha auxiliary and a
///   `Gray8` primary code as.
/// - `MA1A` (Advanced, §8.3) — "the AV1 profile shall be the High Profile and the level shall be
///   6.0 or lower". High is 4:4:4.
///
/// Both constrain *every* AV1 image item in the file, not just the primary, so the test runs over
/// all of them: a 4:4:4 colour item beside a monochrome alpha auxiliary is High and Main at once
/// and can claim neither. A 4:2:2 still is AV1 Professional, which satisfies neither profile.
/// §8.1 anticipates all of this — a file whose encoding matches no defined AVIF profile simply
/// declares the general brands.
///
/// The level is checked against the spec's own thresholds rather than against what
/// [`pick_level`](gamut_av1::headers::pick_level) happens to produce, so this stays correct if that
/// table grows. An `av01` item with no `av1C` cannot be checked at all, so the file claims no
/// profile brand rather than one it has not verified.
fn compatible_brands(items: &[Item]) -> Vec<[u8; 4]> {
    let mut brands = vec![*b"avif", *b"mif1", *b"miaf"];
    let coded: Option<Vec<(u8, u8)>> = items
        .iter()
        .filter(|item| &item.item_type == b"av01")
        .map(av1_profile_level)
        .collect();
    if let Some(coded) = coded {
        let all = |profile: u8, max_level: u8| {
            !coded.is_empty() && coded.iter().all(|&(p, l)| p == profile && l <= max_level)
        };
        if all(HIGH_PROFILE, MA1A_MAX_LEVEL) {
            brands.push(*b"MA1A");
        } else if all(MAIN_PROFILE, MA1B_MAX_LEVEL) {
            brands.push(*b"MA1B");
        }
    }
    brands
}

/// The alpha **auxiliary image item** for a colour item of `dims` (AVIF v1.2.0 §4).
///
/// Hidden, because it is not independently displayable; typed by an **essential** `auxC` carrying
/// the alpha URN, so a reader that does not understand the auxiliary type refuses the item rather
/// than showing an alpha plane as a picture; and owning the `auxl` reference, which runs
/// auxiliary → master. No `colr` is stamped: §4.1 says it should be omitted on an alpha item, whose
/// samples are opacity rather than colour.
fn alpha_item(id: u32, alpha: &EncodedStill, dims: Dimensions) -> Item {
    Item {
        id,
        item_type: *b"av01",
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden: true,
        references: vec![ItemReference {
            reference_type: *b"auxl",
            to_item_ids: vec![PRIMARY_ITEM_ID],
        }],
        properties: vec![
            Property {
                essential: true,
                kind: PropertyKind::CodecConfiguration {
                    kind: *b"av1C",
                    data: av1c_record(&alpha.config).to_vec(),
                },
            },
            Property {
                essential: false,
                kind: PropertyKind::ImageSpatialExtents {
                    width: dims.width,
                    height: dims.height,
                },
            },
            Property {
                essential: false,
                kind: PropertyKind::PixelInformation {
                    bits_per_channel: pixi_channels(&alpha.config),
                },
            },
            Property {
                essential: true,
                kind: PropertyKind::AuxiliaryType {
                    aux_type: ALPHA_AUX_URN.to_owned(),
                    aux_subtype: Vec::new(),
                },
            },
        ],
        payload: alpha.obus.clone(),
    }
}

/// The 4-byte `AV1CodecConfigurationRecord` body (empty `configOBUs`) stamped into the `av1C`
/// property (AV1-ISOBMFF v1.3.0 §2.3.3/§2.3.4). Every field mirrors the AV1 sequence header.
/// Crate-visible so the `av1c` module's tests can pin writer/reader coherence.
pub(crate) fn av1c_record(c: &Av1StillConfig) -> [u8; 4] {
    [
        0x81, // marker = 1, version = 1
        (c.seq_profile << 5) + (c.seq_level_idx_0 & 0x1f),
        (c.seq_tier_0 << 7)
            + (u8::from(c.high_bitdepth) << 6)
            + (u8::from(c.twelve_bit) << 5)
            + (u8::from(c.monochrome) << 4)
            + (c.chroma_subsampling_x << 3)
            + (c.chroma_subsampling_y << 2)
            + (c.chroma_sample_position & 0x3),
        0x00, // reserved(3)=0, initial_presentation_delay_present(1)=0, reserved(4)=0
    ]
}

/// The item id of the primary (displayed) image. Fixed at 1: `pitm` names it, and metadata items
/// take the ids after it.
const PRIMARY_ITEM_ID: u32 = 1;

/// The `mime` `content_type` that identifies an XMP metadata item (ISO/IEC 23008-12) — the value
/// [`AvifImage::xmp`](crate::AvifImage::xmp) matches on when reading.
const XMP_CONTENT_TYPE: &str = "application/rdf+xml";

/// The id one past the highest already assigned. Metadata items are appended to a list that starts
/// with the primary at [`PRIMARY_ITEM_ID`], so this is simply the next position.
fn next_item_id(items: &[Item]) -> u32 {
    items.len() as u32 + PRIMARY_ITEM_ID
}

/// A metadata item describing the primary image: no properties, no pixels, and a `cdsc` reference.
///
/// `cdsc` runs **metadata → described image**, so the reference lives on this item and targets the
/// primary — the direction [`AvifImage::metadata_of`](crate::AvifImage::metadata_of) reads back.
fn metadata_item(
    id: u32,
    item_type: [u8; 4],
    content_type: Option<String>,
    payload: Vec<u8>,
) -> Item {
    Item {
        id,
        item_type,
        name: String::new(),
        content_type,
        content_encoding: None,
        hidden: false,
        references: vec![ItemReference {
            reference_type: *b"cdsc",
            to_item_ids: vec![PRIMARY_ITEM_ID],
        }],
        properties: vec![],
        payload,
    }
}

/// Maps a `0..=100` quality to an AV1 `base_q_idx` (`1..=255`); higher quality → lower index (less
/// quantization). `base_q_idx 0` (the lossless WHT path) is reserved for [`AvifEncoder::lossless`],
/// so the lossy path stays on the DCT pipeline — `lossy(100)` is the finest lossy quantizer, not
/// lossless. Finer rate control (target size/metric) is future work (see `STATUS.md`).
fn quality_to_quant(quality: u8) -> u8 {
    let q = u32::from(quality.min(100));
    (((100 - q) * 255 / 100) as u8).max(1)
}

impl EncodeImage<Rgb8> for AvifEncoder {
    /// Maps the RGB image to AV1 planes — identity GBR at 4:4:4, or YCbCr through the configured
    /// matrix at [`with_chroma`](AvifEncoder::with_chroma)'s sampling — and wraps the temporal unit
    /// in an AVIF file.
    fn encode_image(&self, image: ImageRef<'_, Rgb8>, out: &mut Vec<u8>) -> Result<usize> {
        let dims = image.dimensions();
        let colour = self.colour();
        let planes = match colour.matrix {
            MatrixCoefficients::Identity => Planar8::from_rgb8_identity_view(image),
            matrix => {
                // Rejects a matrix with no luma–chroma transform (Unspecified, YCgCo) before any
                // bytes are written.
                let m = RgbToYcbcr::new(matrix, colour.range, BitDepth::Eight)?;
                Planar8::from_rgb8_matrix_subsampled(image, m, self.chroma())?
            }
        };
        // Read off the buffer, not the configuration: §6.4.2 forces 4:4:4 under the identity
        // matrix, so `with_matrix(Identity)` on a lossy encoder produces 4:4:4 planes while
        // `self.chroma()` still says 4:2:0. `Av1EncodeRequest::chroma` promises "the planes are in
        // this layout", and `still_from_backend_obus` compares it against the stream the backend
        // returns — so the configured value would both misdescribe the buffer and reject every
        // conformant backend response.
        let chroma = planes.subsampling();
        // base_q_idx 0 is the lossless path; encode_still_intra(_, 0) is exactly what
        // encode_still_lossless_identity does, so a single call covers both modes.
        //
        // Pluggable backends first, in push order; `gamut-av1` is the implicit tail when every
        // backend declines (and the only path taken by an encoder with no backends, which is why
        // the default output is byte-identical to the pre-backend encoder).
        let still = self.colour_still(&planes, dims, self.base_q_idx(), colour, chroma)?;
        self.emit(&still, dims, None, out)
    }
}

impl EncodeImage<Rgba8> for AvifEncoder {
    /// Splits the image into colour and alpha, codes each as its own AV1 still — the colour item
    /// as [`EncodeImage<Rgb8>`] would, the alpha as a monochrome auxiliary — and wraps both in one
    /// AVIF file.
    ///
    /// Alpha is coded at the same `base_q_idx` as the colour, so a lossless encode round-trips
    /// alpha bit-exactly and a lossy one quantizes it alongside the colour. It carries no `colr`
    /// and goes through no matrix: opacity is not colour (AVIF v1.2.0 §4.1).
    ///
    /// # Chroma
    ///
    /// The colour item is coded at **4:4:4 regardless of
    /// [`with_chroma`](AvifEncoder::with_chroma)**, which the `Rgb8` path does honour. Subsampling
    /// an RGBA source needs a downsampler that reads a 4-sample stride, which `gamut-color` does
    /// not yet expose; the request describes the buffer truthfully rather than the configuration,
    /// so nothing mis-signals — an RGBA encode is simply larger than the same image without alpha.
    /// Tracked in `STATUS.md`.
    fn encode_image(&self, image: ImageRef<'_, Rgba8>, out: &mut Vec<u8>) -> Result<usize> {
        let dims = image.dimensions();
        let colour = self.colour();
        let planes = match colour.matrix {
            MatrixCoefficients::Identity => Planar8::from_rgba8_identity_view(image),
            matrix => {
                let m = RgbToYcbcr::new(matrix, colour.range, BitDepth::Eight)?;
                Planar8::from_rgba8_matrix_view(image, m)
            }
        };
        let base_q_idx = self.base_q_idx();
        let still = self.colour_still(&planes, dims, base_q_idx, colour, planes.subsampling())?;
        let alpha = monochrome_still(
            &Planar8::from_rgba8_alpha_view(image),
            base_q_idx,
            self.monochrome_colour(),
        )?;
        self.emit(&still, dims, Some(&alpha), out)
    }
}

impl EncodeImage<Rgb16> for AvifEncoder {
    /// Narrows the 16-bit samples to the configured coding depth and codes them as one item at
    /// [`with_chroma`](AvifEncoder::with_chroma)'s sampling — 10-bit 4:4:4 stays AV1 profile 1,
    /// anything 12-bit or subsampled moves to profile 2 (§6.4.1), and `av1C`/`pixi`/`colr` follow
    /// from the stream.
    ///
    /// The narrowing is truncation; see [`AvifEncoder::with_bit_depth`] for the contract. Chroma is
    /// averaged *after* narrowing, so the samples are averaged on the scale they are coded at.
    fn encode_image(&self, image: ImageRef<'_, Rgb16>, out: &mut Vec<u8>) -> Result<usize> {
        let dims = image.dimensions();
        let depth = self.coded_bit_depth()?;
        let colour = self.colour();
        let planes = match colour.matrix {
            // §6.4.2 forces 4:4:4 under the identity matrix, exactly as on the 8-bit path, so
            // `with_chroma` does not reach this arm.
            MatrixCoefficients::Identity => Planar16::from_rgb16_identity_view(image, depth),
            matrix => {
                let m = RgbToYcbcr::new(matrix, colour.range, depth)?;
                Planar16::from_rgb16_matrix_subsampled(image, m, self.chroma())?
            }
        };
        let still = self.colour_still16(&planes, dims, self.base_q_idx(), colour)?;
        self.emit(&still, dims, None, out)
    }
}

impl EncodeImage<Rgba16> for AvifEncoder {
    /// [`EncodeImage<Rgb16>`] plus the alpha auxiliary item of [`EncodeImage<Rgba8>`], both at the
    /// configured coding depth — AVIF v1.2.0 §4.1 requires the auxiliary to match the master's
    /// depth.
    fn encode_image(&self, image: ImageRef<'_, Rgba16>, out: &mut Vec<u8>) -> Result<usize> {
        let dims = image.dimensions();
        let depth = self.coded_bit_depth()?;
        let colour = self.colour();
        let planes = match colour.matrix {
            MatrixCoefficients::Identity => Planar16::from_rgba16_identity_view(image, depth),
            matrix => {
                let m = RgbToYcbcr::new(matrix, colour.range, depth)?;
                Planar16::from_rgba16_matrix_view(image, m)
            }
        };
        let base_q_idx = self.base_q_idx();
        let still = self.colour_still16(&planes, dims, base_q_idx, colour)?;
        let alpha = monochrome_still16(
            &Planar16::from_rgba16_alpha_view(image, depth),
            base_q_idx,
            self.monochrome_colour(),
        )?;
        self.emit(&still, dims, Some(&alpha), out)
    }
}

impl EncodeImage<Gray8> for AvifEncoder {
    /// Codes the grayscale image as a **single monochrome** AV1 item — one luma plane, no chroma,
    /// and a `pixi` declaring one channel.
    ///
    /// Replicating the samples into three equal planes would code two constant chroma planes and
    /// then claim three channels for an image that has one. The matrix and range knobs do not
    /// apply (see [`AvifEncoder::with_matrix`]); the primaries and transfer tags do.
    fn encode_image(&self, image: ImageRef<'_, Gray8>, out: &mut Vec<u8>) -> Result<usize> {
        let dims = image.dimensions();
        let still = monochrome_still(
            &Planar8::from_gray8_view(image),
            self.base_q_idx(),
            self.monochrome_colour(),
        )?;
        self.emit(&still, dims, None, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `lossless()` carries no explicit mode because the default already is one, so that has to
    /// stay true. If the default ever changes, this fails here rather than turning `lossless()`
    /// lossy in silence.
    #[test]
    fn lossless_is_the_default_mode() {
        assert_eq!(AvifConfig::default().mode, AvifMode::Lossless);
        assert_eq!(AvifEncoder::lossless().config().mode, AvifMode::Lossless);
        assert_eq!(
            AvifEncoder::new().config(),
            AvifEncoder::lossless().config()
        );
    }

    #[test]
    fn lossless_pins_four_four_four_whatever_the_chroma_knob_says() {
        // Lossless ignores `with_chroma` exactly as it ignores `with_matrix` and `quality`:
        // discarding three quarters of the chroma is not lossless, and AV1 §6.4.2 forbids the
        // identity matrix below 4:4:4 anyway — so honouring the request would produce a stream
        // that cannot be built at all.
        for requested in [
            ChromaSubsampling::Cs444,
            ChromaSubsampling::Cs422,
            ChromaSubsampling::Cs420,
        ] {
            let enc = AvifEncoder::lossless().with_chroma(requested);
            assert_eq!(
                enc.config().chroma,
                requested,
                "the knob records the request"
            );
            assert_eq!(
                enc.chroma(),
                ChromaSubsampling::Cs444,
                "but lossless codes 4:4:4 regardless"
            );
        }
        // Lossy honours it, so the pinning is specific to the lossless path.
        for requested in [
            ChromaSubsampling::Cs444,
            ChromaSubsampling::Cs422,
            ChromaSubsampling::Cs420,
        ] {
            assert_eq!(
                AvifEncoder::lossy(50).with_chroma(requested).chroma(),
                requested
            );
        }
        // And the default lossy format is 4:2:0 — AV1 Main, the profile hardware decoders accept.
        assert_eq!(AvifEncoder::lossy(50).chroma(), ChromaSubsampling::Cs420);
        assert_eq!(AvifEncoder::lossless().chroma(), ChromaSubsampling::Cs444);
    }

    /// A coded `av01` item declaring `seq_profile` / `seq_level_idx[0]` through its `av1C`, which
    /// is the only thing [`compatible_brands`] reads.
    fn coded_item(seq_profile: u8, seq_level_idx_0: u8) -> Item {
        Item {
            id: PRIMARY_ITEM_ID,
            item_type: *b"av01",
            name: String::new(),
            content_type: None,
            content_encoding: None,
            hidden: false,
            references: vec![],
            properties: vec![Property {
                essential: true,
                kind: PropertyKind::CodecConfiguration {
                    kind: *b"av1C",
                    data: av1c_record(&Av1StillConfig {
                        seq_profile,
                        seq_level_idx_0,
                        seq_tier_0: 0,
                        high_bitdepth: false,
                        twelve_bit: false,
                        monochrome: false,
                        chroma_subsampling_x: 0,
                        chroma_subsampling_y: 0,
                        chroma_sample_position: 0,
                        color_primaries: 1,
                        transfer_characteristics: 13,
                        matrix_coefficients: 1,
                        full_range: true,
                    })
                    .to_vec(),
                },
            }],
            payload: vec![],
        }
    }

    fn general() -> Vec<[u8; 4]> {
        vec![*b"avif", *b"mif1", *b"miaf"]
    }

    #[test]
    fn profile_brands_require_both_the_av1_profile_and_its_level() {
        // AVIF §8.2/§8.3 constrain the AV1 profile *and* the level: `MA1B` needs Main at ≤ 5.1
        // (`seq_level_idx` ≤ 13), `MA1A` needs High at ≤ 6.0 (≤ 16). §8.1: a file matching neither
        // declares only the general brands. `pick_level` never yields above 16 today, so the level
        // guards are checked here rather than through an encode.
        let brands = |profile, level| compatible_brands(&[coded_item(profile, level)]);

        // Main within level 5.1 earns MA1B; one level above loses it.
        let mut with_b = general();
        with_b.push(*b"MA1B");
        assert_eq!(brands(0, 13), with_b);
        assert_eq!(brands(0, 12), with_b);
        assert_eq!(brands(0, 14), general());

        // High within level 6.0 earns MA1A; above it, general brands only.
        let mut with_a = general();
        with_a.push(*b"MA1A");
        assert_eq!(brands(1, 16), with_a);
        assert_eq!(brands(1, 17), general());

        // Professional (4:2:2, and any 12-bit stream) matches neither profile at any level.
        assert_eq!(brands(2, 0), general());
        assert_eq!(brands(2, 16), general());
    }

    #[test]
    fn profile_brands_constrain_every_coded_item_not_just_the_primary() {
        // §8.2/§8.3 both say "the AV1 profile" of the file, and MIAF applies a brand's constraints
        // to every image item — so an alpha auxiliary or a `Gray8` primary, which code as Main
        // monochrome, participate in the claim rather than riding on the primary's.
        let mut with_a = general();
        with_a.push(*b"MA1A");
        let mut with_b = general();
        with_b.push(*b"MA1B");

        // A 4:4:4 colour item beside a monochrome alpha auxiliary is High *and* Main at once, so
        // the file can claim neither. This is the case the alpha surface introduces.
        assert_eq!(
            compatible_brands(&[coded_item(1, 16), coded_item(0, 13)]),
            general()
        );
        // All-Main — a 4:2:0 colour item with its alpha auxiliary — still earns MA1B.
        assert_eq!(
            compatible_brands(&[coded_item(0, 13), coded_item(0, 13)]),
            with_b
        );
        // One item above the level ceiling costs the whole file the claim.
        assert_eq!(
            compatible_brands(&[coded_item(1, 16), coded_item(1, 17)]),
            general()
        );
        // All-High stays MA1A.
        assert_eq!(
            compatible_brands(&[coded_item(1, 16), coded_item(1, 16)]),
            with_a
        );
    }

    #[test]
    fn a_coded_item_without_an_av1c_forfeits_every_profile_brand() {
        // The claim is derived from the bytes in the file. An `av01` item carrying no `av1C` has
        // no profile to check, so no brand is claimed rather than one that was never verified —
        // and vacuously claiming a brand for a file with no coded item at all is refused too.
        let mut bare = coded_item(1, 16);
        bare.properties.clear();
        assert_eq!(compatible_brands(&[bare]), general());
        assert_eq!(compatible_brands(&[]), general());
    }

    #[test]
    fn non_av01_items_do_not_participate_in_the_brand_claim() {
        // Exif and XMP are metadata items, not image items: §8.2/§8.3 constrain the AV1 profile of
        // the *coded* items, so a metadata item beside a High primary must not cost it MA1A.
        let mut with_a = general();
        with_a.push(*b"MA1A");
        let exif = metadata_item(2, *b"Exif", None, vec![0, 0, 0, 0]);
        assert_eq!(compatible_brands(&[coded_item(1, 16), exif]), with_a);
    }

    #[test]
    fn av1c_record_encodes_every_field() {
        // Distinct, non-zero values in every field so each shift, mask, and `+` is observable (a
        // zero term would hide its operator: `0 + x == 0 - x`, `0 << n == 0 >> n`).
        let c = Av1StillConfig {
            seq_profile: 5,        // 0b101
            seq_level_idx_0: 0x15, // 0b10101
            seq_tier_0: 1,
            high_bitdepth: true,
            twelve_bit: true,
            monochrome: true,
            chroma_subsampling_x: 1,
            chroma_subsampling_y: 1,
            chroma_sample_position: 2, // 0b10
            // colr fields are irrelevant to av1C but needed to build the config.
            color_primaries: 2,
            transfer_characteristics: 3,
            matrix_coefficients: 5,
            full_range: true,
        };
        // marker/version 0x81; (seq_profile<<5)+(level&0x1f) = 0xA0+0x15 = 0xB5; the flags byte sets
        // tier/high_bitdepth/twelve_bit/monochrome/subsampling_x/_y plus chroma position 2:
        // 0x80+0x40+0x20+0x10+0x08+0x04+0x02 = 0xFE; trailing reserved 0x00.
        assert_eq!(av1c_record(&c), [0x81, 0xB5, 0xFE, 0x00]);
    }

    #[test]
    fn quality_maps_to_quant() {
        // 0..=100, higher quality = lower base_q_idx (less quantization). base_q_idx 0 is reserved
        // for the lossless path, so the lossy mapping floors at 1 and never returns 0.
        assert_eq!(
            quality_to_quant(100),
            1,
            "best quality = finest lossy quantizer"
        );
        assert_eq!(
            quality_to_quant(0),
            255,
            "worst quality = coarsest quantizer"
        );
        assert_eq!(quality_to_quant(50), 127);
        assert_eq!(
            quality_to_quant(200),
            1,
            "out-of-range quality is clamped to 100"
        );
        // The constructors set the mode; lossless never consults the quality field.
        assert_eq!(AvifEncoder::lossless().config().mode, AvifMode::Lossless);
        let lossy = AvifEncoder::lossy(80).config();
        assert_eq!(lossy.mode, AvifMode::Lossy);
        assert_eq!(lossy.quality, 80);
    }

    #[test]
    fn container_carries_av1_config_and_layout() {
        use gamut_isobmff::{ColourInformation, PropertyKind, read};
        // Both modes wrap the AV1 unit in the same well-formed container; only the mdat payload
        // differs. Parsing it back (gamut-isobmff round-trips its own output) pins the brands, the
        // primary `av01` item, and the av1C-derived `ispe`/`pixi`/`colr` the encoder stamps — none
        // of which a box-presence check would catch if a field were wrong.
        // `(encoder, primaries, transfer, matrix_coefficients, full_range, profile brand)`:
        // lossless is pinned to identity/full/4:4:4, lossy defaults to BT.709/full/4:2:0, and
        // the knobs override the lossy defaults. The brand follows the AV1 profile the chroma
        // format selects — `MA1B` requires Main (4:2:0), `MA1A` requires High (4:4:4), and
        // 4:2:2 is Professional, which matches neither.
        let cases = [
            (
                AvifEncoder::lossless(),
                1u16,
                13u16,
                0u16,
                true,
                Some(*b"MA1A"),
            ),
            (AvifEncoder::lossy(50), 1, 13, 1, true, Some(*b"MA1B")),
            (
                AvifEncoder::lossy(50).with_chroma(ChromaSubsampling::Cs444),
                1,
                13,
                1,
                true,
                Some(*b"MA1A"),
            ),
            (
                AvifEncoder::lossy(50).with_chroma(ChromaSubsampling::Cs422),
                1,
                13,
                1,
                true,
                None,
            ),
            (
                AvifEncoder::lossy(50).with_matrix(MatrixCoefficients::Bt601),
                1,
                13,
                6,
                true,
                Some(*b"MA1B"),
            ),
            // Studio range reaches `colr` — and can only be signalled outside the §5.5.2 sRGB
            // shortcut, which is why it pairs with a real matrix.
            (
                AvifEncoder::lossy(50).with_color_range(ColorRange::Limited),
                1,
                13,
                1,
                false,
                Some(*b"MA1B"),
            ),
            // …but none of the knobs apply on the lossless path, which ignores them as it ignores
            // quality: an 8-bit YCbCr round trip is not bit-exact, studio range discards codes, and
            // subsampled chroma is not lossless at all.
            (
                AvifEncoder::lossless()
                    .with_matrix(MatrixCoefficients::Bt709)
                    .with_color_range(ColorRange::Limited)
                    .with_chroma(ChromaSubsampling::Cs420),
                1,
                13,
                0,
                true,
                Some(*b"MA1A"),
            ),
            // Primaries and transfer are tags, not transforms, so — unlike matrix and range — they
            // reach `colr` on **both** paths. Lossless keeps identity/full alongside them.
            (
                AvifEncoder::lossless()
                    .with_primaries(ColourPrimaries::Bt2020)
                    .with_transfer(TransferCharacteristics::Pq),
                9,
                16,
                0,
                true,
                Some(*b"MA1A"),
            ),
            (
                AvifEncoder::lossy(50)
                    .with_primaries(ColourPrimaries::DisplayP3)
                    .with_transfer(TransferCharacteristics::Hlg)
                    .with_matrix(MatrixCoefficients::Bt2020Ncl),
                12,
                18,
                9,
                true,
                Some(*b"MA1B"),
            ),
        ];
        for (
            enc,
            want_primaries,
            want_transfer,
            want_matrix,
            want_full_range,
            want_profile_brand,
        ) in cases
        {
            let img = read(&encode_with(enc, 34, 18)).expect("emitted AVIF parses");
            assert_eq!(img.major_brand, *b"avif");
            for brand in [*b"avif", *b"mif1", *b"miaf"] {
                assert!(
                    img.compatible_brands.contains(&brand),
                    "missing brand {brand:?}"
                );
            }
            // Exactly one profile brand, or none — never both, and never one the AV1 profile
            // does not satisfy.
            for brand in [*b"MA1A", *b"MA1B"] {
                assert_eq!(
                    img.compatible_brands.contains(&brand),
                    want_profile_brand == Some(brand),
                    "brand {brand:?} presence"
                );
            }
            assert_eq!(img.primary_item_id, 1);
            let item = &img.items[0];
            assert_eq!(item.item_type, *b"av01");
            let props = &item.properties;
            let ispe = props.iter().find_map(|p| match p.kind {
                PropertyKind::ImageSpatialExtents { width, height } => Some((width, height)),
                _ => None,
            });
            assert_eq!(ispe, Some((34, 18)), "ispe = display dimensions");
            let pixi = props.iter().find_map(|p| match &p.kind {
                PropertyKind::PixelInformation { bits_per_channel } => {
                    Some(bits_per_channel.clone())
                }
                _ => None,
            });
            assert_eq!(pixi, Some(vec![8u8, 8, 8]), "three 8-bit channels (4:4:4)");
            let nclx = props
                .iter()
                .find_map(|p| match &p.kind {
                    PropertyKind::Colour(ColourInformation::Nclx(n)) => Some(n),
                    _ => None,
                })
                .expect("colr nclx present");
            // Every CICP field is whatever the configuration selected (AVIF v1.2.0 §2.2; mc = 0
            // additionally requires 4:4:4). `colr` must mirror the sequence header exactly, which
            // the backend seam re-checks for a foreign stream (AV1-ISOBMFF v1.3.0 §2.3.4).
            assert_eq!(nclx.colour_primaries, want_primaries);
            assert_eq!(nclx.transfer_characteristics, want_transfer);
            assert_eq!(nclx.matrix_coefficients, want_matrix);
            assert_eq!(nclx.full_range, want_full_range);
        }
    }

    /// A deterministic non-trivial payload: distinct from the pixel ramp, and long enough that a
    /// truncation or an off-by-one prefix would show.
    fn payload(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| seed.wrapping_add((i * 31) as u8))
            .collect()
    }

    #[test]
    fn icc_profile_is_a_second_colr_of_type_prof() {
        use gamut_isobmff::{ColourInformation, PropertyKind, read};
        let icc = payload(0x11, 300);
        let bytes = encode_with(AvifEncoder::new().with_icc_profile(&icc), 34, 18);
        let img = read(&bytes).expect("emitted AVIF parses");
        let colrs: Vec<&ColourInformation> = img.items[0]
            .properties
            .iter()
            .filter_map(|p| match &p.kind {
                PropertyKind::Colour(c) => Some(c),
                _ => None,
            })
            .collect();
        // Two `colr` boxes of different `colour_type` (ISO/IEC 14496-12 §12.1.5 allows one each),
        // CICP first: it describes the codestream, which the container must agree with, and keeping
        // it first leaves a profile-free file's property indices untouched.
        assert_eq!(colrs.len(), 2, "CICP nclx and the ICC profile");
        assert!(matches!(colrs[0], ColourInformation::Nclx(_)), "nclx first");
        // `prof` (unrestricted), not `rICC`: the encoder does not parse the profile, so it cannot
        // assert that it fits HEIF's restricted subset.
        assert_eq!(
            colrs[1],
            &ColourInformation::UnrestrictedIcc(icc.clone()),
            "profile carried verbatim as `prof`"
        );
        // …and it is reachable through the crate's own role lens, which `colour()` alone cannot do
        // because that returns whichever `colr` comes first.
        let container = crate::AvifContainer::parse(&bytes).expect("parses");
        assert_eq!(
            container.image().primary_item().icc_profile(),
            Some(icc.as_slice())
        );
    }

    #[test]
    fn last_icc_profile_wins() {
        use gamut_isobmff::{ColourInformation, PropertyKind, read};
        let img = read(&encode_with(
            AvifEncoder::new()
                .with_icc_profile(&payload(0x11, 64))
                .with_icc_profile(&payload(0x22, 96)),
            4,
            4,
        ))
        .expect("parses");
        let icc = img.items[0].properties.iter().find_map(|p| match &p.kind {
            PropertyKind::Colour(ColourInformation::UnrestrictedIcc(icc)) => Some(icc.clone()),
            _ => None,
        });
        assert_eq!(icc, Some(payload(0x22, 96)), "the last profile is kept");
    }

    #[test]
    fn metadata_items_describe_the_primary_with_cdsc() {
        use gamut_isobmff::read;
        let exif = payload(0x33, 120);
        let xmp = payload(0x44, 80);
        let bytes = encode_with(AvifEncoder::new().with_exif(&exif).with_xmp(&xmp), 34, 18);
        let img = read(&bytes).expect("emitted AVIF parses");
        // The primary keeps id 1 at index 0 and stays what `pitm` names, whatever is attached.
        assert_eq!(img.primary_item_id, 1);
        assert_eq!(img.items[0].id, 1);
        assert_eq!(img.items[0].item_type, *b"av01");
        assert_eq!(img.items.len(), 3, "primary + Exif + XMP");

        let exif_item = &img.items[1];
        assert_eq!(exif_item.id, 2);
        assert_eq!(exif_item.item_type, *b"Exif");
        assert_eq!(exif_item.content_type, None, "only `mime` items carry one");
        // HEIF wraps the TIFF stream in a 4-byte big-endian `exif_tiff_header_offset`; the caller
        // hands over a bare stream and the encoder adds it.
        let mut want = 0u32.to_be_bytes().to_vec();
        want.extend_from_slice(&exif);
        assert_eq!(
            exif_item.payload, want,
            "4-byte offset prefix, then the TIFF"
        );

        let xmp_item = &img.items[2];
        assert_eq!(xmp_item.id, 3);
        assert_eq!(xmp_item.item_type, *b"mime");
        assert_eq!(
            xmp_item.content_type.as_deref(),
            Some("application/rdf+xml"),
            "the content type `AvifImage::xmp` matches on"
        );
        assert_eq!(xmp_item.payload, xmp, "packet carried verbatim");

        // `cdsc` runs metadata → described image, so it lives on each metadata item and targets the
        // primary — not the other way round.
        for item in &img.items[1..] {
            assert_eq!(item.references.len(), 1);
            assert_eq!(item.references[0].reference_type, *b"cdsc");
            assert_eq!(item.references[0].to_item_ids, vec![1]);
        }
        assert!(
            img.items[0].references.is_empty(),
            "the primary owns no references"
        );

        // …and the crate's own lenses find them by that relationship.
        let container = crate::AvifContainer::parse(&bytes).expect("parses");
        let image = container.image();
        assert_eq!(
            image.exif().map(|i| i.as_isobmff_item().payload.clone()),
            Some(want)
        );
        assert_eq!(
            image.xmp().map(|i| i.as_isobmff_item().payload.clone()),
            Some(xmp)
        );
    }

    #[test]
    fn metadata_item_ids_follow_the_items_actually_present() {
        use gamut_isobmff::read;
        // Ids are positional among the items present, not fixed per kind: with no Exif, XMP takes
        // id 2 rather than leaving a hole at 2 and claiming 3.
        let img = read(&encode_with(
            AvifEncoder::new().with_xmp(&payload(0x44, 40)),
            4,
            4,
        ))
        .expect("parses");
        assert_eq!(img.items.len(), 2, "primary + XMP");
        assert_eq!(img.items[1].id, 2);
        assert_eq!(img.items[1].item_type, *b"mime");
        assert_eq!(img.items[1].references[0].to_item_ids, vec![1]);
    }

    #[test]
    fn debug_summarizes_the_payloads_by_length() {
        // The payloads are opaque binary that would swamp the output, so `Debug` reports their
        // size. An unset knob has to stay distinguishable from an empty payload.
        let bare = format!("{:?}", AvifEncoder::new());
        assert!(bare.contains("icc: None"), "{bare}");
        assert!(bare.contains("exif: None"), "{bare}");
        assert!(bare.contains("xmp: None"), "{bare}");

        let set = format!(
            "{:?}",
            AvifEncoder::new()
                .with_icc_profile(&payload(0x11, 512))
                .with_exif(&payload(0x33, 64))
                .with_xmp(&[])
        );
        assert!(set.contains("icc: Some(512)"), "{set}");
        assert!(set.contains("exif: Some(64)"), "{set}");
        assert!(
            set.contains("xmp: Some(0)"),
            "an empty payload is still set: {set}"
        );
    }

    #[test]
    fn an_unconfigured_encoder_adds_no_items_or_properties() {
        use gamut_isobmff::read;
        // The guard behind the crate's byte-identity promise: every knob added here is inert until
        // it is set, so the default file keeps its item count *and* its `ipco`/`ipma` indices.
        let img = read(&encode_with(AvifEncoder::new(), 34, 18)).expect("parses");
        assert_eq!(img.items.len(), 1, "the primary alone");
        assert_eq!(
            img.items[0].properties.len(),
            4,
            "av1C, ispe, pixi, colr — and nothing else"
        );
        assert!(img.items[0].references.is_empty(), "so `iref` is omitted");
    }

    #[test]
    fn appends_without_clobbering() {
        let mut out = vec![0xAA, 0xBB];
        let rgb = vec![128u8; 4 * 4 * 3];
        let n = AvifEncoder::new()
            .encode_image(
                ImageRef::<Rgb8>::new(
                    &rgb,
                    Dimensions {
                        width: 4,
                        height: 4,
                    },
                )
                .unwrap(),
                &mut out,
            )
            .unwrap();
        assert_eq!(out.len(), 2 + n);
        assert_eq!(&out[0..2], &[0xAA, 0xBB]);
    }

    fn encode_with(enc: AvifEncoder, w: u32, h: u32) -> Vec<u8> {
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        for (i, b) in rgb.iter_mut().enumerate() {
            *b = (i * 37) as u8;
        }
        let mut out = Vec::new();
        let dims = Dimensions {
            width: w,
            height: h,
        };
        enc.encode_image(ImageRef::<Rgb8>::new(&rgb, dims).unwrap(), &mut out)
            .unwrap();
        out
    }

    #[test]
    fn with_rotation_emits_irot_and_none_is_omitted() {
        // A rotation emits an `irot` whose body byte is the angle. `irot` lives in `meta`, which
        // precedes `mdat`, so the first occurrence is the property box (not stray OBU bytes).
        let f = encode_with(AvifEncoder::new().with_rotation(Rotation::Ccw90), 4, 4);
        let p = f
            .windows(4)
            .position(|w| w == b"irot")
            .expect("irot present");
        assert_eq!(f[p + 4] & 0x03, 1, "Ccw90 ⇒ irot angle = 1");
        // Rotation::None writes no `irot`.
        let f0 = encode_with(AvifEncoder::new().with_rotation(Rotation::None), 4, 4);
        assert!(
            !f0.windows(4).any(|w| w == b"irot"),
            "Rotation::None ⇒ no irot"
        );
    }

    #[test]
    fn with_mirror_emits_imir_axis() {
        // ISO/IEC 23008-12:2022 §6.5.12: axis 1 exchanges left/right, axis 0 top/bottom.
        for (mirror, axis) in [(Mirror::LeftRight, 1u8), (Mirror::TopBottom, 0)] {
            let f = encode_with(AvifEncoder::new().with_mirror(mirror), 4, 4);
            let p = f
                .windows(4)
                .position(|w| w == b"imir")
                .expect("imir present");
            assert_eq!(f[p + 4] & 0x01, axis, "{mirror:?} ⇒ imir axis = {axis}");
            assert!(!f.windows(4).any(|w| w == b"irot"), "mirror only ⇒ no irot");
        }
    }
}
