//! The AVIF still-image encoder: RGB → identity planes → AV1 temporal unit → ISOBMFF container.

use core::num::NonZeroUsize;
use core::ops::Range;
use std::sync::{Arc, Mutex};

use gamut_av1::{
    Av1Colour, Av1StillConfig, EncodeOptions, EncodedStill, Speed, encode_still_intra_with_options,
    encode_still_intra16_with_options,
};
use gamut_color::{
    BitDepth, ChromaSubsampling, ColorRange, ColourPrimaries, MatrixCoefficients, Planar8,
    Planar16, RgbToYcbcr, TransferCharacteristics,
};
use gamut_core::convert::{AlphaPolicy, ConvertPolicy, convert, premultiply_alpha};
use gamut_core::{
    Dimensions, EncodeImage, Error, Gray8, ImageBuf, ImageRef, Pixel, Result, Rgb8, Rgb16, Rgba8,
    Rgba16, Sample,
};
use gamut_isobmff::{
    ColourInformation, IsoBmffImage, Item, ItemReference, NclxColr, Property, PropertyKind,
    TopLevelBox, write,
};

use crate::backend::{Av1EncodeRequest, Av1StillEncoder, BackendPlanes, BackendSlot};
use crate::c2pa::{
    C2PA_UUID, C2paBoxPurpose, content_provenance_payload, content_provenance_reserved,
};
use crate::clean_alpha::clean_rgba;
use crate::config::{AvifConfig, AvifMode};
use crate::container::AvifContainer;
use crate::image::ALPHA_AUX_URN;
use crate::transform::{Mirror, Rotation};

/// What the encoder writes into the store slot of the C2PA `uuid` box it emits.
#[derive(Clone, PartialEq, Eq)]
enum SlotSource {
    /// A slot of this many zero bytes, for an external signer to fill after encoding.
    Reserved(usize),
    /// A manifest store the caller has already computed over this exact output.
    Store(Vec<u8>),
}

/// Prints the slot by kind and length, never the store bytes (opaque binary that would swamp the
/// output).
impl std::fmt::Debug for SlotSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reserved(len) => write!(f, "Reserved({len})"),
            Self::Store(store) => write!(f, "Store({})", store.len()),
        }
    }
}

/// What [`AvifEncoder::encode_with_report`] found out about the file it just produced, beyond the
/// bytes: how the file splits between the coded images and the container around them, and where
/// the container placed the things a caller has to come back to.
///
/// Non-exhaustive: a later minor release may report more placements. Construct nothing here — the
/// encoder fills it in, and the type deliberately implements no [`Default`], so the only way to
/// hold one is to have been given one by [`encode_with_report`](AvifEncoder::encode_with_report).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AvifEncodeReport {
    /// The byte range of the C2PA manifest-store slot within the file — the `len` bytes of
    /// [`with_c2pa_reserved`](AvifEncoder::with_c2pa_reserved), or the store handed to
    /// [`with_c2pa`](AvifEncoder::with_c2pa) — or `None` when neither was configured.
    ///
    /// This is where a signer writes the finished store: nothing in the file after this range
    /// moves when it is overwritten with a payload of the same length, so the offset reported here
    /// is the offset the signer patches at. It is **not** a hash exclusion range — a BMFF asset's
    /// hard binding excludes by box path, not byte offset (C2PA 2.4 §18.6, §A.5.6); see
    /// [`C2paSlot`](crate::C2paSlot).
    pub c2pa: Option<Range<usize>>,
    /// The length in bytes of the **colour** image's AV1 codestream — the primary item's payload,
    /// which is the sum of its `iloc` extent lengths (ISO/IEC 14496-12 §8.11.3).
    pub color_bytes: usize,
    /// The length in bytes of the **alpha** auxiliary item's AV1 codestream (its `iloc` extent
    /// lengths, as for [`color_bytes`](Self::color_bytes)), or `0` when the file carries no alpha
    /// auxiliary — every input without an alpha channel.
    ///
    /// What is left of the file, `file.len() - color_bytes - alpha_bytes`, is the container and
    /// whatever metadata (ICC, Exif, XMP, C2PA) it carries.
    pub alpha_bytes: usize,
}

/// How the colour of an RGBA input relates to its alpha, and so what an RGBA encode codes and
/// declares. Select it with [`AvifEncoder::with_alpha_mode`].
///
/// A file whose colour item carries a `prem` item reference to its alpha auxiliary declares the
/// colour **premultiplied** — associated with alpha (ISO/IEC 23008-12 §6); without
/// one the colour is unassociated. A reader un-premultiplies the first before compositing, so
/// declaring the wrong one darkens or halos the displayed edges.
///
/// | Mode | Input colour | Coded colour | `prem` written |
/// | --- | --- | --- | --- |
/// | [`Unassociated`](Self::Unassociated) | unassociated | as given | no |
/// | [`Premultiplied`](Self::Premultiplied) | already premultiplied | as given | yes |
/// | [`PremultiplyOnEncode`](Self::PremultiplyOnEncode) | unassociated | premultiplied by the encoder | yes |
///
/// Every mode is ignored by an input without an alpha channel: with no alpha auxiliary there is
/// no alpha to associate with and nothing for `prem` to target.
///
/// `#[non_exhaustive]`, `#[repr(u8)]` with permanent, append-only discriminants: plain
/// configuration data that must stay mechanically portable to C (see the workspace charter).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AlphaMode {
    /// The colour is **unassociated** (straight) and is coded as given, with no `prem` — the
    /// default, and the interpretation [`Rgba8`]'s
    /// [`ColorModel::Rgba`](gamut_core::ColorModel::Rgba) already documents.
    #[default]
    Unassociated = 0,
    /// The caller's colour is **already premultiplied** by its alpha. It is coded as given and the
    /// file declares it with `prem`: a declaration, not a conversion — the encoder multiplies
    /// nothing and divides nothing.
    Premultiplied = 1,
    /// The caller's colour is **unassociated**; the encoder premultiplies it — with
    /// [`gamut_core::convert::premultiply_alpha`], `round(colour * alpha / max)` at the input's
    /// own sample scale — before coding, and declares the result with `prem`.
    ///
    /// Opaque pixels code exactly as given and alpha is never touched. Every other pixel keeps
    /// only `alpha + 1` colour levels, so a reader that un-premultiplies recovers the source only
    /// to within rounding — `⌊(a + max) / 2a⌋` code values at alpha `a`, and no colour at all
    /// under a fully transparent pixel. That makes this mode **lossy in the colour channels even
    /// under [`AvifMode::Lossless`]**, which then codes the *premultiplied* colour bit-exactly.
    PremultiplyOnEncode = 2,
}

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
/// The 8-bit inputs code at 8 bits unless [`AvifEncoder::with_bit_depth`] asks for 10 or 12, in
/// which case they are widened by bit replication first; the 16-bit inputs code at 12 unless it
/// asks for 10.
///
/// An RGBA input whose alpha is fully opaque everywhere carries no information an RGB input
/// would not, so by default it is coded as one: no alpha auxiliary, byte-identical to the RGB
/// encode of its colour. [`AvifEncoder::with_keep_opaque_alpha`] keeps the auxiliary instead.
///
/// Alpha is coded as its own AV1 still: AVIF v1.2.0 §4.1 makes `mono_chrome = 1` and full range a
/// *shall* for an AV1 auxiliary image item, and the auxiliary is linked to the colour item by an
/// `auxl` reference and typed by an essential `auxC` property.
/// [`AvifEncoder::with_alpha_mode`] selects whether the colour is unassociated, already
/// premultiplied by alpha, or premultiplied by the encoder (see [`AlphaMode`]).
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
    /// The C2PA manifest-store slot — reserved zeros or a caller-computed store — written into a
    /// top-level `uuid` `ContentProvenanceBox` after `ftyp`.
    c2pa: Option<SlotSource>,
    /// How an RGBA input's colour relates to its alpha: whether it is premultiplied before coding
    /// and whether a `prem` item reference declares it. Meaningless without an alpha channel, so
    /// it reaches the file only for an RGBA encode.
    alpha_mode: AlphaMode,
    /// Whether an RGBA input whose every alpha sample is fully opaque still gets its alpha
    /// auxiliary item. `false` (the default) codes such an input as the RGB input it is
    /// indistinguishable from — see [`AvifEncoder::with_keep_opaque_alpha`].
    keep_opaque_alpha: bool,
    /// The `gamut-av1` mode-decision search preset every AV1 still of the file is coded at —
    /// colour and alpha alike. See [`AvifEncoder::with_speed`].
    speed: Speed,
    /// The most threads an encode may use; `None` is the machine's available parallelism. See
    /// [`AvifEncoder::with_threads`].
    threads: Option<NonZeroUsize>,
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
            .field("c2pa", &self.c2pa)
            .field("alpha_mode", &self.alpha_mode)
            .field("keep_opaque_alpha", &self.keep_opaque_alpha)
            .field("speed", &self.speed)
            .field("threads", &self.threads)
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
            c2pa: None,
            alpha_mode: AlphaMode::Unassociated,
            keep_opaque_alpha: false,
            speed: Speed::Thorough,
            threads: Some(NonZeroUsize::MIN),
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
            c2pa: None,
            alpha_mode: AlphaMode::Unassociated,
            keep_opaque_alpha: false,
            speed: Speed::Thorough,
            threads: Some(NonZeroUsize::MIN),
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

    /// Selects the depth the samples are coded at: [`BitDepth::Ten`] or [`BitDepth::Twelve`] for
    /// any input, or [`BitDepth::Eight`] for an 8-bit one.
    ///
    /// Without this call each input codes at its own default — **8** for [`Rgb8`], [`Rgba8`] and
    /// [`Gray8`], **12** for [`Rgb16`] and [`Rgba16`] — so an encoder that never sets it writes
    /// exactly the bytes it wrote before 8-bit inputs honoured it. A depth AV1 cannot code
    /// ([`BitDepth::Sixteen`], §6.4.1), and [`BitDepth::Eight`] for a 16-bit input, are rejected
    /// at encode time.
    ///
    /// # Widening an 8-bit input
    ///
    /// An 8-bit input coded at 10 or 12 bits is widened by **bit replication** — the byte is
    /// repeated into the low bits, `(x << 2) | (x >> 6)` at 10 bits — *before* colour conversion,
    /// so the matrix and the chroma averaging run at the coded precision. Replication maps `0` to
    /// `0` and `255` to the coded maximum, and the input byte is the top 8 bits of every coded
    /// sample, so:
    ///
    /// > A lossless encode of an 8-bit input at 10 or 12 bits is exact: narrowing the decoded
    /// > samples back to 8 bits (`sample >> (depth - 8)`) reproduces the input.
    ///
    /// The wider depth adds no information the caller did not have. What it buys on the lossy path
    /// is precision *inside the codec*: the YCbCr conversion and the quantizer round at 10 or 12
    /// bits rather than 8, which is what can reduce banding in smooth gradients. The
    /// widened copy of the image is held for the duration of the encode — two bytes per sample,
    /// on top of the planes every encode builds.
    ///
    /// # Narrowing a 16-bit input
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
    /// Either way the alpha auxiliary of an RGBA input codes at the colour item's depth, as AVIF
    /// v1.2.0 §4.1 requires.
    #[must_use]
    pub fn with_bit_depth(mut self, bit_depth: BitDepth) -> Self {
        self.config.bit_depth = Some(bit_depth);
        self
    }

    /// Codes the **alpha auxiliary item** at its own lossy `quality` (`0..=100`, higher = larger
    /// output, closer to the source; values above `100` are clamped), independent of the colour
    /// item's.
    ///
    /// The value goes through exactly the mapping [`AvifEncoder::lossy`]'s `quality` does, so
    /// `lossy(q).with_alpha_quality(q)` codes the same bytes as `lossy(q)`. Without this call
    /// alpha follows the colour quality (see [`AvifConfig::alpha_quality`]). Opacity edges are
    /// often more visible than colour error, so a caller trading colour bits for size may want
    /// alpha finer than colour; a mask that is already blocky can go coarser.
    ///
    /// **Ignored by [`AvifMode::Lossless`]**, which codes alpha bit-exactly at `base_q_idx 0`
    /// whatever is set here, exactly as it ignores the colour `quality` — and by every input
    /// without an alpha channel, which has no auxiliary item to code.
    #[must_use]
    pub fn with_alpha_quality(mut self, quality: u8) -> Self {
        self.config.alpha_quality = Some(quality);
        self
    }

    /// Selects how an RGBA input's colour relates to its alpha — unassociated, already
    /// premultiplied, or to be premultiplied by the encoder — and so whether the file declares
    /// premultiplied colour with a `prem` item reference from the colour item to its alpha
    /// auxiliary (ISO/IEC 23008-12 §6). See [`AlphaMode`] for each mode's contract; the default is
    /// [`AlphaMode::Unassociated`].
    ///
    /// [`AlphaMode::PremultiplyOnEncode`] is the one that converts samples, and it is lossy in the
    /// colour channels even on the lossless path. Ignored by every input without an alpha channel.
    #[must_use]
    pub fn with_alpha_mode(mut self, mode: AlphaMode) -> Self {
        self.alpha_mode = mode;
        self
    }

    /// Declares whether the colour values of an RGBA input are **already premultiplied** by their
    /// alpha — shorthand for [`with_alpha_mode`](Self::with_alpha_mode) with
    /// [`AlphaMode::Premultiplied`] (`true`) or [`AlphaMode::Unassociated`] (`false`).
    ///
    /// This is a **declaration, not a conversion**: the encoder multiplies nothing and divides
    /// nothing, it only records the caller's assertion as a `prem` item reference. A caller
    /// holding unassociated colour that wants the file stored premultiplied asks for
    /// [`AlphaMode::PremultiplyOnEncode`] instead. Getting the declaration wrong darkens or halos
    /// the edges of the displayed image, so the default is `false`.
    ///
    /// Ignored by every input without an alpha channel: with no alpha auxiliary there is nothing
    /// for the reference to target.
    #[must_use]
    pub fn with_premultiplied_alpha(self, premultiplied: bool) -> Self {
        self.with_alpha_mode(if premultiplied {
            AlphaMode::Premultiplied
        } else {
            AlphaMode::Unassociated
        })
    }

    /// Cleans the colour hidden under transparent pixels before a **lossy** encode: each
    /// non-opaque pixel's colour is replaced by a blurred bleed of the visible edge around it, so
    /// the coder spends no bits on colour nobody sees and the visible edge does not ring against
    /// it.
    ///
    /// What a reader displays does not change: alpha and opaque pixels are untouched, a fully
    /// transparent pixel's colour is invisible, and a semi-transparent pixel's colour moves only
    /// within the range that premultiplies to the same value. Only the decoded *unassociated*
    /// colour under transparency differs from the source, which is why it is opt-in (see
    /// [`AvifConfig::clean_alpha`]).
    ///
    /// **Ignored by [`AvifMode::Lossless`]**, which stays bit-exact; by every
    /// [`AlphaMode`] but [`AlphaMode::Unassociated`] — premultiplied colour is the composite
    /// itself, and premultiplying already zeroes the colour under a fully transparent pixel; and by
    /// every input without an alpha channel.
    #[must_use]
    pub fn with_clean_alpha(mut self, clean: bool) -> Self {
        self.config.clean_alpha = clean;
        self
    }

    /// Keeps the **alpha auxiliary item** of an RGBA input whose every alpha sample is fully
    /// opaque, instead of coding it as the RGB image it is indistinguishable from.
    ///
    /// By default (`false`) an [`Rgba8`] or [`Rgba16`] input whose alpha is the sample type's
    /// maximum everywhere — 255, or 65535 — is encoded exactly as the same pixels without their
    /// alpha channel would be through [`EncodeImage<Rgb8>`] or [`EncodeImage<Rgb16>`]: one colour
    /// item, no alpha auxiliary, no `auxl` or `prem` reference, and so byte for byte the file that
    /// encode writes. A reader displays the same picture either way — an image with no alpha
    /// auxiliary is opaque — while the file saves the auxiliary item, its properties and its
    /// monochrome codestream. That includes [`with_chroma`](Self::with_chroma), which the RGB path
    /// honours and the RGBA path does not yet.
    ///
    /// Opacity is judged on the **input** samples, before any narrowing to the coded depth: a
    /// 16-bit alpha of 65534 keeps the auxiliary even where it would truncate to the coded maximum,
    /// so an alpha channel the caller can tell apart from opaque is never dropped. One
    /// non-opaque sample anywhere keeps it, and so does every [`AlphaMode`] — premultiplying an
    /// opaque pixel changes nothing, so there is nothing for `prem` to declare.
    ///
    /// Set `true` when a consumer needs the alpha item present regardless — one that branches on
    /// it, or a pipeline comparing file structure across inputs. The check reads every alpha
    /// sample once; dropping the channel copies the colour samples once.
    #[must_use]
    pub fn with_keep_opaque_alpha(mut self, keep: bool) -> Self {
        self.keep_opaque_alpha = keep;
        self
    }

    /// Selects how much mode-decision search the AV1 encode runs: a [`Speed`] preset trading
    /// search for encode time, applied to **every** AV1 still of the file — the colour item and
    /// the alpha auxiliary alike.
    ///
    /// The preset is `gamut-av1`'s, passed through unchanged; its table on [`Speed`] lists what
    /// each one prunes. The default, [`Speed::Thorough`], is the full search and codes exactly the
    /// bytes an encoder without this call always has. Every preset produces a conformant stream,
    /// but the presets are a **time** control, not a size one: the search scores candidates by
    /// prediction error rather than rate, so a faster preset can come out smaller or larger.
    ///
    /// Only lossy coding is searched. [`AvifMode::Lossless`] has one fixed coding path and
    /// produces the same bytes at every speed. Nor does any preset move a container field:
    /// `av1C`, `colr`, `pixi` and the rest mirror the sequence header, which is the same at every
    /// speed — only the coded tile data differs.
    ///
    /// A [pushed backend](Self::push_backend) receives the preset as
    /// [`Av1EncodeRequest::speed`] — through [`AbiAv1StillEncoder`](crate::AbiAv1StillEncoder),
    /// as the second byte of the `extra` blob (see there) — and is free to map it onto its own
    /// effort scale or to ignore it.
    #[must_use]
    pub fn with_speed(mut self, speed: Speed) -> Self {
        self.speed = speed;
        self
    }

    /// The [`Speed`] preset this encoder codes at — [`Speed::Thorough`] unless
    /// [`with_speed`](Self::with_speed) chose another.
    #[must_use]
    pub fn speed(&self) -> Speed {
        self.speed
    }

    /// Caps the threads an encode may use: `Some(n)` uses at most `n`, and `None` uses
    /// [`std::thread::available_parallelism`] (one where the platform cannot report it). The
    /// default is `Some(1)`: an encoder without this call spawns no thread.
    ///
    /// **The output does not depend on it.** Every thread count codes exactly the bytes one thread
    /// does, so this is purely a time-for-memory control:
    ///
    /// - An RGBA input's colour item and alpha auxiliary are two independent AV1 stills. With two
    ///   or more threads the alpha is coded on a scoped thread while the colour is coded on the
    ///   calling one; the colour gets the larger half of the budget and the alpha the rest.
    /// - Each still is coded in `gamut-av1`'s tile layout, and its tiles are coded concurrently on
    ///   that still's share (see [`gamut_av1::EncodeOptions::threads`]). The layout is
    ///   `gamut-av1`'s default whatever the thread count — it is what decides the bytes — so a
    ///   still with fewer tiles than threads leaves the extra threads unused.
    ///
    /// Each extra thread holds its own copy of the encoder's frame state, so peak memory grows
    /// with the count. A thread the platform refuses to spawn has its work done on the calling
    /// thread instead: running out of threads costs time, never the encode. On a `wasm32` target
    /// without threads (`wasm32-unknown-unknown`), where every spawn fails, everything therefore
    /// runs on the calling thread; `gamut-av1` codes a still's tiles one after another on any
    /// `wasm32` target.
    ///
    /// A [pushed backend](Self::push_backend) runs on the calling thread and receives the colour
    /// item's share as [`Av1EncodeRequest::threads`], a hint it may ignore.
    #[must_use]
    pub fn with_threads(mut self, threads: Option<NonZeroUsize>) -> Self {
        self.threads = threads;
        self
    }

    /// The thread cap this encoder runs under — `Some(1)` unless
    /// [`with_threads`](Self::with_threads) chose another; `None` is the machine's available
    /// parallelism.
    #[must_use]
    pub fn threads(&self) -> Option<NonZeroUsize> {
        self.threads
    }

    /// The thread budget one encode runs under: the configured cap, or the machine's available
    /// parallelism for `None`.
    fn thread_budget(&self) -> NonZeroUsize {
        self.threads
            .unwrap_or_else(|| std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))
    }

    /// The `gamut-av1` options a still is coded under: this encoder's preset, `gamut-av1`'s
    /// default tile layout, and `threads`.
    fn av1_options(&self, threads: NonZeroUsize) -> EncodeOptions {
        EncodeOptions::default()
            .with_speed(self.speed)
            .with_threads(threads)
    }

    /// Codes an RGBA input's two stills — `colour` on the calling thread, `alpha` beside it on a
    /// scoped thread when the budget has room for one — each handed its share of the budget
    /// ([`split_threads`]). Returns the colour still, then the alpha one.
    ///
    /// The colour still is the one a [pushed backend](Self::push_backend) may produce, so keeping
    /// it on the calling thread keeps every backend call there. The order the two finish in is
    /// unobservable: each still is a pure function of its own inputs.
    fn colour_and_alpha<C, A>(&self, colour: C, alpha: A) -> Result<(EncodedStill, EncodedStill)>
    where
        C: FnOnce(NonZeroUsize) -> Result<EncodedStill>,
        A: Fn(NonZeroUsize) -> Result<EncodedStill> + Sync,
    {
        let (colour_threads, alpha_threads) = split_threads(self.thread_budget());
        let Some(alpha_threads) = alpha_threads else {
            return Ok((colour(colour_threads)?, alpha(colour_threads)?));
        };
        std::thread::scope(|scope| {
            let alpha = &alpha;
            let spawned = std::thread::Builder::new()
                .name("gamut-avif-alpha".into())
                .spawn_scoped(scope, move || alpha(alpha_threads));
            let colour = colour(colour_threads);
            // A thread the platform cannot spawn (every one, on a threadless `wasm32`) has its still coded
            // here instead. A panic on the alpha thread resumes on this one, as it would have had
            // the alpha been coded here.
            let alpha = match spawned {
                Ok(handle) => handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
                Err(_) => alpha(alpha_threads),
            };
            Ok((colour?, alpha?))
        })
    }

    /// The colour samples an RGBA `image` codes as instead, when its alpha is to be dropped: every
    /// alpha sample is the sample type's maximum and
    /// [`with_keep_opaque_alpha`](Self::with_keep_opaque_alpha) is unset. `None` codes `image`
    /// with its alpha auxiliary.
    fn opaque_colour<P: Pixel, Q: Pixel>(
        &self,
        image: ImageRef<'_, P>,
    ) -> Result<Option<ImageBuf<Q>>> {
        let opaque = image
            .as_samples()
            .chunks_exact(P::CHANNELS)
            .all(|pixel| pixel[P::CHANNELS - 1] == P::Sample::MAX_VALUE);
        if self.keep_opaque_alpha || !opaque {
            return Ok(None);
        }
        convert(
            image,
            ConvertPolicy::lossless().with_alpha(AlphaPolicy::Drop),
        )
        .map(Some)
    }

    /// Whether an RGBA encode cleans the colour under its transparent pixels: asked for, lossy,
    /// and unassociated (see [`AvifEncoder::with_clean_alpha`]).
    fn cleans_alpha(&self) -> bool {
        self.config.clean_alpha
            && self.config.mode == AvifMode::Lossy
            && self.alpha_mode == AlphaMode::Unassociated
    }

    /// Whether the file declares its colour premultiplied with a `prem` item reference: every
    /// [`AlphaMode`] but [`AlphaMode::Unassociated`].
    fn declares_premultiplied(&self) -> bool {
        self.alpha_mode != AlphaMode::Unassociated
    }

    /// The RGBA samples an encode codes in place of `image`, or `None` to code `image` as given:
    /// the premultiplied image under [`AlphaMode::PremultiplyOnEncode`], the cleaned one when
    /// [`cleans_alpha`](Self::cleans_alpha) applies. The two never both apply, since cleaning
    /// requires unassociated colour.
    fn coded_rgba<P>(&self, image: ImageRef<'_, P>) -> Result<Option<ImageBuf<P>>>
    where
        P: Pixel,
        P::Sample: crate::clean_alpha::Sample,
    {
        if self.alpha_mode == AlphaMode::PremultiplyOnEncode {
            let mut premultiplied = ImageBuf::new(image.as_samples().to_vec(), image.dimensions())?;
            premultiply_alpha(&mut premultiplied);
            Ok(Some(premultiplied))
        } else if self.cleans_alpha() {
            let cleaned = clean_rgba(image.as_samples(), image.width() as usize);
            ImageBuf::new(cleaned, image.dimensions()).map(Some)
        } else {
            Ok(None)
        }
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

    /// The depth a 16-bit input is coded at — 12 when unset — rejecting one AV1 cannot express.
    ///
    /// §6.4.1 defines 8, 10 and 12. [`BitDepth::Sixteen`] is a `gamut-color` depth for the
    /// interleaved 16-bit pipelines, and [`BitDepth::Eight`] would silently discard the input's
    /// whole point, so both are refused rather than quietly reinterpreted. Twelve is the default
    /// because narrowing 16 bits to 12 discards the least, and 12-bit is a depth every AVIF reader
    /// that handles high bit depth decodes.
    fn coded_bit_depth(&self) -> Result<BitDepth> {
        match self.config.bit_depth.unwrap_or(BitDepth::Twelve) {
            depth @ (BitDepth::Ten | BitDepth::Twelve) => Ok(depth),
            _ => Err(gamut_core::Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: a 16-bit input codes at 10 or 12 bits (AV1 \u{a7}6.4.1); \
                 select one with AvifEncoder::with_bit_depth",
            )),
        }
    }

    /// The depth an 8-bit input is **widened** to, or `None` to code it at 8 bits — the default,
    /// and what an explicit [`BitDepth::Eight`] asks for.
    ///
    /// [`BitDepth::Sixteen`] is refused rather than clamped: AV1 codes no 16-bit samples
    /// (§6.4.1), and quietly coding 12 would hand back a depth the caller did not ask for.
    fn widened_bit_depth(&self) -> Result<Option<BitDepth>> {
        match self.config.bit_depth {
            None | Some(BitDepth::Eight) => Ok(None),
            Some(depth @ (BitDepth::Ten | BitDepth::Twelve)) => Ok(Some(depth)),
            Some(_) => Err(gamut_core::Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: an 8-bit input codes at 8, 10 or 12 bits (AV1 \u{a7}6.4.1); \
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

    /// The AV1 `base_q_idx` the **alpha auxiliary** codes at: `0` on the lossless path, otherwise
    /// the quality mapping applied to [`AvifConfig::alpha_quality`], or to the colour quality when
    /// that is unset.
    fn alpha_base_q_idx(&self) -> u8 {
        match self.config.mode {
            AvifMode::Lossless => 0,
            AvifMode::Lossy => {
                quality_to_quant(self.config.alpha_quality.unwrap_or(self.config.quality))
            }
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

    /// Reserves `len` bytes for a C2PA manifest store, for an external signer to fill in after
    /// encoding.
    ///
    /// A manifest store cannot be handed to the encoder complete: its hard binding digests the
    /// finished file, and the file does not exist until encoding ends (C2PA 2.4 §15.12.1.1). So the
    /// encoder writes the store's container — a top-level `uuid` `ContentProvenanceBox` (§A.5.1)
    /// with `box_purpose` `manifest` and a zero merkle offset, placed after `ftyp` and before `meta`
    /// per §A.5.3 — around a slot of `len` **zero bytes**, and
    /// [`encode_with_report`](Self::encode_with_report) tells the caller where that slot landed.
    /// The signer then computes the store over the finished file and writes it into the slot. It
    /// must fit: the store may be shorter than `len` (§A.5.3 permits unused padding after it, and a
    /// store carries its own length) but never longer, since nothing in the file moves.
    ///
    /// The box is invisible to an AVIF reader — libavif decodes a file carrying it to the same
    /// pixels as one without — and its bytes are opaque to this crate, which validates nothing
    /// about them. A slot nobody fills reads back through [`AvifContainer::c2pa_slot`](crate::AvifContainer::c2pa_slot)
    /// as `len` zero bytes. Calling this, or [`with_c2pa`](Self::with_c2pa), twice keeps the
    /// **last** call — as every other payload builder on this encoder does.
    ///
    /// # `len` is checked, at encode time
    ///
    /// This builder cannot fail — it returns `Self` — so an unusable `len` is refused by the
    /// *encode* that follows, on every entry point including [`EncodeImage::encode_to_vec`]. This
    /// crate refuses two lengths, both as [`Error::InvalidInput`]: one **below 8 bytes**, the size
    /// of a JUMBF box header, which could not hold even an empty manifest store's outermost box;
    /// and one so large that the framed box exceeds what a buffer can hold. Neither is merely
    /// documented as a precondition, because in the release profile a downstream consumer builds
    /// with, the second wrapped silently and produced a well-formed AVIF whose C2PA box no locator
    /// — this crate's included — could find, while just below the wrap it panicked instead.
    ///
    /// On **64-bit** targets a **third** refusal sits between them: a top-level box carries a
    /// 32-bit size field, so a `ContentProvenanceBox` at or beyond 4 GiB cannot be written. This
    /// crate applies the bound [`gamut_isobmff::write`] enforces on every top-level box, *before*
    /// the slot is allocated, and refuses as
    /// [`Error::Unsupported`](gamut_core::Error::Unsupported) — the kind the container writer
    /// would give. It is the encoder's effective ceiling: on this framing the largest `len` that
    /// clears it is `4_294_967_250`, and `4_294_967_251` is refused. Whether that refusal should
    /// instead be an `InvalidInput` is issue #576. On **32-bit** targets (`wasm32`) a buffer
    /// holds under 2 GiB, so this crate's own `InvalidInput` is the effective ceiling and the
    /// 4 GiB refusal is never reached.
    ///
    /// Every `len` below the effective ceiling is written as asked. What remains beyond this
    /// crate's reach is the allocator's: a reservation the machine has no memory for aborts, as
    /// any oversized allocation in Rust does — measurably so on a 64-bit host just under the 4 GiB
    /// bound, where the writer's copy of the payload into the output buffer needs twice the slot.
    ///
    /// The *read* side is deliberately more permissive: it reports a degenerate slot it genuinely
    /// finds rather than hiding it, since those bytes exist and some other writer put them there.
    /// See [`AvifContainer::c2pa_slots`](crate::AvifContainer::c2pa_slots).
    ///
    /// # The encoder writes `manifest`, and only `manifest`
    ///
    /// §A.5.3 defines three `box_purpose` values for a store-bearing box, and the *read* side
    /// reports all three ([`C2paBoxPurpose`](crate::C2paBoxPurpose)). This encoder writes one:
    /// `manifest`, the purpose of an ordinary store in a file that is not mid-update. Once a file
    /// carries an `update` box, §A.5.3 requires its earlier store to be re-labelled `original`, so
    /// re-encoding such a file through this builder would label it wrongly; producing an
    /// `original`/`update` pair is a manifest-update operation on an existing file, which this
    /// crate does not offer (issue #444's scope is reserve, write and locate).
    #[must_use]
    pub fn with_c2pa_reserved(mut self, len: usize) -> Self {
        self.c2pa = Some(SlotSource::Reserved(len));
        self
    }

    /// Writes `store` — a C2PA manifest store the caller has **already computed over this exact
    /// output** — into the `ContentProvenanceBox` the encoder emits.
    ///
    /// This is the second pass of the reserve-then-fill flow, for a caller that re-encodes rather
    /// than patching: the framing is identical to [`with_c2pa_reserved`](Self::with_c2pa_reserved)
    /// with the slot holding `store` instead of zeros, so an encoder that is otherwise configured
    /// the same puts the same bytes everywhere outside the slot. It is not a way to attach a store
    /// computed over some other file — such a store's hard binding will not verify — and the
    /// workspace's metadata facade never hands one over (its C2PA policy is to locate and carry,
    /// never to originate). The bytes are carried verbatim; nothing inside them is parsed or
    /// checked. Calling this, or `with_c2pa_reserved`, twice keeps the **last** call.
    ///
    /// # `store` is trusted, and the reservation minimum does not apply to it
    ///
    /// `with_c2pa_reserved` refuses a `len` below 8 bytes; **this builder applies no minimum**.
    /// `store` is a slice the caller already holds, so it is carried as supplied — the same way
    /// every other caller-supplied metadata payload in this workspace is carried — and a store
    /// shorter than a JUMBF box header is written, and located again by
    /// [`AvifContainer::c2pa_slot`](crate::AvifContainer::c2pa_slot), rather than refused. Only
    /// the *length* is unchecked here in a way the reservation path is not; the content is
    /// unchecked on both. Whether the minimum should apply here too is issue #577.
    ///
    /// The container's own bound still applies: a `store` large enough to push the
    /// `ContentProvenanceBox` to or past 4 GiB is refused at encode time by
    /// [`gamut_isobmff::write`], as [`Error::Unsupported`](gamut_core::Error::Unsupported).
    ///
    /// The box is labelled `box_purpose = manifest`, the only purpose this encoder writes; see
    /// [`with_c2pa_reserved`](Self::with_c2pa_reserved) for why an `original`/`update` pair is a
    /// file-update operation this crate does not offer.
    #[must_use]
    pub fn with_c2pa(mut self, store: &[u8]) -> Self {
        self.c2pa = Some(SlotSource::Store(store.to_vec()));
        self
    }

    /// Encodes `image` as [`EncodeImage::encode_to_vec`] would, and additionally reports how the
    /// file splits between the colour codestream, the alpha codestream and the container
    /// ([`AvifEncodeReport::color_bytes`], [`AvifEncodeReport::alpha_bytes`]), and where the
    /// container placed what a caller has to come back to — today the C2PA manifest-store slot
    /// ([`AvifEncodeReport::c2pa`]).
    ///
    /// The bytes are exactly the bytes `encode_to_vec` produces: this is the same encode with a
    /// report alongside, not a different code path, so the object-safe `EncodeImage` entry point
    /// stays what it is. Every figure is found by parsing the finished file with
    /// [`AvifContainer::parse`] — the sizes from the colour and alpha items' `iloc` extents, the
    /// range from [`AvifContainer::c2pa_slot`] — so what the encoder reports is, by construction,
    /// what a reader of the file finds. That parse is the price of the report over a plain
    /// `encode_to_vec`: one container walk of the output, small beside the AV1 encode.
    ///
    /// # Errors
    ///
    /// As [`EncodeImage::encode_image`] — which includes [`Error::InvalidInput`] for a
    /// [`with_c2pa_reserved`](Self::with_c2pa_reserved) length that cannot be framed — and
    /// additionally an error if the file just written does not parse with
    /// [`AvifContainer::parse`], or [`Error::InvalidInput`] if a C2PA slot was configured but
    /// cannot be located in it. Either would mean the writer and the reader disagree about the
    /// container, and is never expected.
    pub fn encode_with_report<P: Pixel>(
        &self,
        image: ImageRef<'_, P>,
    ) -> Result<(Vec<u8>, AvifEncodeReport)>
    where
        Self: EncodeImage<P>,
    {
        let bytes = self.encode_to_vec(image)?;
        let container = AvifContainer::parse(&bytes)?;
        let c2pa = match self.c2pa {
            None => None,
            Some(_) => {
                let store = container.c2pa_slot().ok_or_else(|| {
                    Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "AVIF: the C2PA box this encoder wrote cannot be located in its own output",
                    )
                })?;
                Some(store.range)
            }
        };
        // The item payloads are what `iloc` resolves — the concatenation of each item's extents —
        // so their lengths are the extent-length sums a reader of this file finds.
        let image = container.image();
        let primary = image.primary_item();
        let color_bytes = primary.as_isobmff_item().payload.len();
        let alpha_bytes = image
            .alpha_auxiliary_of(primary.id())
            .map_or(0, |alpha| alpha.as_isobmff_item().payload.len());
        Ok((
            bytes,
            AvifEncodeReport {
                c2pa,
                color_bytes,
                alpha_bytes,
            },
        ))
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
            if self.declares_premultiplied() {
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
        let mut image =
            IsoBmffImage::new(*b"avif", compatible_brands(&items), PRIMARY_ITEM_ID, items);
        // The C2PA `ContentProvenanceBox` is a top-level box, not an item: `push_top_level_box`
        // places it `AfterFtyp` — between `ftyp` and `meta`, so before the first `mdat` (C2PA 2.4
        // §A.5.3) — and the slot is zeros or the caller's store behind the §A.5.1 framing.
        if let Some(slot) = &self.c2pa {
            let payload = match slot {
                // The reserved zeros are written once, into the payload buffer itself. A length
                // that cannot be framed is refused here rather than truncated: see
                // `content_provenance_reserved`.
                SlotSource::Reserved(len) => {
                    content_provenance_reserved(C2paBoxPurpose::Manifest, *len)?
                }
                SlotSource::Store(store) => {
                    content_provenance_payload(C2paBoxPurpose::Manifest, store)
                }
            };
            image.push_top_level_box(TopLevelBox::uuid(C2PA_UUID, payload));
        }
        write(&image)
    }

    /// Encodes the **colour** planes, offering the job to the registered backends in push order
    /// before falling through to the built-in `gamut-av1` tail, on at most `threads` threads.
    fn colour_still(
        &self,
        planes: &Planar8,
        dims: Dimensions,
        base_q_idx: u8,
        colour: Av1Colour,
        chroma: ChromaSubsampling,
        threads: NonZeroUsize,
    ) -> Result<EncodedStill> {
        let request = Av1EncodeRequest::new(
            dims,
            base_q_idx,
            colour,
            chroma,
            BitDepth::Eight,
            self.speed,
            threads,
        );
        match crate::backend::run_backends(&self.backends, &request, BackendPlanes::Eight(planes))?
        {
            Some(obus) => {
                crate::backend::still_from_backend_obus(obus, dims, colour, chroma, BitDepth::Eight)
            }
            None => Ok(encode_still_intra_with_options(
                planes,
                base_q_idx,
                colour,
                self.av1_options(threads),
            )?
            .0),
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
        threads: NonZeroUsize,
    ) -> Result<EncodedStill> {
        let bit_depth = planes.bit_depth();
        // Read off the buffer, not the configuration, exactly as the 8-bit path does: §6.4.2
        // forces 4:4:4 under the identity matrix and the `Rgba16` path has no 4-stride
        // downsampler, so either can carry 4:4:4 planes while `with_chroma` says otherwise. The
        // request must describe the planes it actually carries.
        let chroma = planes.subsampling();
        let request = Av1EncodeRequest::new(
            dims, base_q_idx, colour, chroma, bit_depth, self.speed, threads,
        );
        match crate::backend::run_backends(&self.backends, &request, BackendPlanes::High(planes))? {
            Some(obus) => {
                crate::backend::still_from_backend_obus(obus, dims, colour, chroma, bit_depth)
            }
            None => Ok(encode_still_intra16_with_options(
                planes,
                base_q_idx,
                colour,
                self.av1_options(threads),
            )?
            .0),
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
fn monochrome_still(
    planes: &Planar8,
    base_q_idx: u8,
    colour: Av1Colour,
    options: EncodeOptions,
) -> Result<EncodedStill> {
    Ok(encode_still_intra_with_options(planes, base_q_idx, colour, options)?.0)
}

/// The high-bit-depth [`monochrome_still`]; the registry is skipped for the same reason.
fn monochrome_still16(
    planes: &Planar16,
    base_q_idx: u8,
    colour: Av1Colour,
    options: EncodeOptions,
) -> Result<EncodedStill> {
    Ok(encode_still_intra16_with_options(planes, base_q_idx, colour, options)?.0)
}

/// Splits a thread `budget` between an RGBA input's colour still and its alpha auxiliary: the
/// colour gets the larger half (it codes three planes to the alpha's one) and the alpha the rest,
/// so the two together never exceed the budget. `None` for the alpha — a budget of one — codes the
/// two one after the other, each on the whole budget.
fn split_threads(budget: NonZeroUsize) -> (NonZeroUsize, Option<NonZeroUsize>) {
    let alpha = budget.get() / 2;
    // `budget - alpha` is `ceil(budget / 2) >= 1`, so the fallback is never taken.
    let colour = NonZeroUsize::new(budget.get() - alpha).unwrap_or(budget);
    (colour, NonZeroUsize::new(alpha))
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
    ///
    /// At a [`with_bit_depth`](AvifEncoder::with_bit_depth) of 10 or 12 the image is first widened
    /// to [`Rgb16`] and coded exactly as [`EncodeImage<Rgb16>`] codes it, which narrows back to the
    /// coded depth: `x * 257 >> (16 - depth)` is the bit replication of `x` to that depth.
    fn encode_image(&self, image: ImageRef<'_, Rgb8>, out: &mut Vec<u8>) -> Result<usize> {
        if self.widened_bit_depth()?.is_some() {
            let wide = convert::<Rgb8, Rgb16>(image, ConvertPolicy::lossless())?;
            return EncodeImage::<Rgb16>::encode_image(self, wide.as_ref(), out);
        }
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
        let still = self.colour_still(
            &planes,
            dims,
            self.base_q_idx(),
            colour,
            chroma,
            self.thread_budget(),
        )?;
        self.emit(&still, dims, None, out)
    }
}

impl EncodeImage<Rgba8> for AvifEncoder {
    /// Splits the image into colour and alpha, codes each as its own AV1 still — the colour item
    /// as [`EncodeImage<Rgb8>`] would, the alpha as a monochrome auxiliary — and wraps both in one
    /// AVIF file.
    ///
    /// Alpha is coded at the colour's `base_q_idx` unless
    /// [`with_alpha_quality`](AvifEncoder::with_alpha_quality) gives it its own, so a lossless
    /// encode round-trips alpha bit-exactly and a lossy one quantizes it at the alpha quality
    /// (defaulting to the colour quality). It carries no `colr`
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
    ///
    /// At a [`with_bit_depth`](AvifEncoder::with_bit_depth) of 10 or 12 the image — alpha
    /// included — is widened to [`Rgba16`] and coded exactly as [`EncodeImage<Rgba16>`] codes it,
    /// so the alpha auxiliary keeps the colour item's depth (AVIF v1.2.0 §4.1).
    ///
    /// An image whose every alpha sample is 255 is coded as [`EncodeImage<Rgb8>`] codes its colour,
    /// with no alpha auxiliary, unless
    /// [`with_keep_opaque_alpha`](AvifEncoder::with_keep_opaque_alpha) asks to keep it.
    fn encode_image(&self, image: ImageRef<'_, Rgba8>, out: &mut Vec<u8>) -> Result<usize> {
        if let Some(rgb) = self.opaque_colour::<Rgba8, Rgb8>(image)? {
            return EncodeImage::<Rgb8>::encode_image(self, rgb.as_ref(), out);
        }
        if self.widened_bit_depth()?.is_some() {
            let wide = convert::<Rgba8, Rgba16>(image, ConvertPolicy::lossless())?;
            return EncodeImage::<Rgba16>::encode_image(self, wide.as_ref(), out);
        }
        let coded = self.coded_rgba(image)?;
        let image = coded.as_ref().map_or(image, ImageBuf::as_ref);
        let dims = image.dimensions();
        let colour = self.colour();
        let planes = match colour.matrix {
            MatrixCoefficients::Identity => Planar8::from_rgba8_identity_view(image),
            matrix => {
                let m = RgbToYcbcr::new(matrix, colour.range, BitDepth::Eight)?;
                Planar8::from_rgba8_matrix_view(image, m)
            }
        };
        let alpha_planes = Planar8::from_rgba8_alpha_view(image);
        let (still, alpha) = self.colour_and_alpha(
            |threads| {
                self.colour_still(
                    &planes,
                    dims,
                    self.base_q_idx(),
                    colour,
                    planes.subsampling(),
                    threads,
                )
            },
            |threads| {
                monochrome_still(
                    &alpha_planes,
                    self.alpha_base_q_idx(),
                    self.monochrome_colour(),
                    self.av1_options(threads),
                )
            },
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
        let still = self.colour_still16(
            &planes,
            dims,
            self.base_q_idx(),
            colour,
            self.thread_budget(),
        )?;
        self.emit(&still, dims, None, out)
    }
}

impl EncodeImage<Rgba16> for AvifEncoder {
    /// [`EncodeImage<Rgb16>`] plus the alpha auxiliary item of [`EncodeImage<Rgba8>`], both at the
    /// configured coding depth — AVIF v1.2.0 §4.1 requires the auxiliary to match the master's
    /// depth.
    ///
    /// An image whose every alpha sample is 65535 is coded as [`EncodeImage<Rgb16>`] codes its
    /// colour, with no alpha auxiliary, unless
    /// [`with_keep_opaque_alpha`](AvifEncoder::with_keep_opaque_alpha) asks to keep it.
    fn encode_image(&self, image: ImageRef<'_, Rgba16>, out: &mut Vec<u8>) -> Result<usize> {
        if let Some(rgb) = self.opaque_colour::<Rgba16, Rgb16>(image)? {
            return EncodeImage::<Rgb16>::encode_image(self, rgb.as_ref(), out);
        }
        let coded = self.coded_rgba(image)?;
        let image = coded.as_ref().map_or(image, ImageBuf::as_ref);
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
        let alpha_planes = Planar16::from_rgba16_alpha_view(image, depth);
        let (still, alpha) = self.colour_and_alpha(
            |threads| self.colour_still16(&planes, dims, self.base_q_idx(), colour, threads),
            |threads| {
                monochrome_still16(
                    &alpha_planes,
                    self.alpha_base_q_idx(),
                    self.monochrome_colour(),
                    self.av1_options(threads),
                )
            },
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
    ///
    /// At a [`with_bit_depth`](AvifEncoder::with_bit_depth) of 10 or 12 the samples are widened by
    /// bit replication and coded as a monochrome item at that depth.
    fn encode_image(&self, image: ImageRef<'_, Gray8>, out: &mut Vec<u8>) -> Result<usize> {
        let dims = image.dimensions();
        let options = self.av1_options(self.thread_budget());
        let still = match self.widened_bit_depth()? {
            None => monochrome_still(
                &Planar8::from_gray8_view(image),
                self.base_q_idx(),
                self.monochrome_colour(),
                options,
            )?,
            Some(depth) => monochrome_still16(
                &gray8_widened(image, depth)?,
                self.base_q_idx(),
                self.monochrome_colour(),
                options,
            )?,
        };
        self.emit(&still, dims, None, out)
    }
}

/// A [`Gray8`] image widened to `depth` by bit replication, as a single-plane monochrome buffer.
///
/// `gamut-color` has no grey constructor for [`Planar16`], so the image goes through the two
/// conversions that already own each half rather than a third copy of either: `gamut-core`'s
/// lossless widening replicates the grey into an [`Rgb16`] image at full 16-bit scale, and
/// [`Planar16::from_rgb16_identity_view`] narrows it to `depth`, whose `Y = G` plane is the grey.
/// The two chroma planes it also builds are discarded. That is three times the transient memory a
/// dedicated grey constructor would need, paid only on this opt-in path.
fn gray8_widened(image: ImageRef<'_, Gray8>, depth: BitDepth) -> Result<Planar16> {
    let wide = convert::<Gray8, Rgb16>(image, ConvertPolicy::lossless())?;
    let gbr = Planar16::from_rgb16_identity_view(wide.as_ref(), depth);
    Planar16::from_planes_subsampled(
        image.width(),
        image.height(),
        ChromaSubsampling::Cs400,
        depth,
        [gbr.plane(0).to_vec(), Vec::new(), Vec::new()],
    )
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
    fn alpha_base_q_idx_follows_alpha_quality_then_colour_quality() {
        // Unset: alpha codes at the colour item's quantizer, so the pre-knob bytes hold.
        let follows = AvifEncoder::lossy(50);
        assert_eq!(follows.config().alpha_quality, None);
        assert_eq!(follows.alpha_base_q_idx(), 127);
        assert_eq!(follows.alpha_base_q_idx(), follows.base_q_idx());
        // Set: its own value through the same mapping, leaving the colour quantizer alone.
        let own = AvifEncoder::lossy(50).with_alpha_quality(90);
        assert_eq!(own.config().alpha_quality, Some(90));
        assert_eq!(own.alpha_base_q_idx(), 25);
        assert_eq!(own.base_q_idx(), 127);
        // The clamp is the colour quality's: above 100 behaves as 100.
        assert_eq!(
            AvifEncoder::lossy(50)
                .with_alpha_quality(200)
                .alpha_base_q_idx(),
            1
        );
        // Lossless ignores it and codes alpha on the lossless path.
        assert_eq!(
            AvifEncoder::lossless()
                .with_alpha_quality(10)
                .alpha_base_q_idx(),
            0
        );
    }

    #[test]
    fn cleans_alpha_only_when_asked_lossy_and_unassociated() {
        assert!(!AvifEncoder::lossy(50).cleans_alpha(), "opt-in");
        assert!(AvifEncoder::lossy(50).with_clean_alpha(true).cleans_alpha());
        assert!(
            !AvifEncoder::lossy(50)
                .with_clean_alpha(true)
                .with_clean_alpha(false)
                .cleans_alpha(),
            "the last call wins"
        );
        assert!(
            !AvifEncoder::lossless()
                .with_clean_alpha(true)
                .cleans_alpha(),
            "lossless stays bit-exact"
        );
        assert!(
            !AvifEncoder::lossy(50)
                .with_clean_alpha(true)
                .with_premultiplied_alpha(true)
                .cleans_alpha(),
            "premultiplied colour is the composite itself"
        );
        assert!(
            !AvifEncoder::lossy(50)
                .with_clean_alpha(true)
                .with_alpha_mode(AlphaMode::PremultiplyOnEncode)
                .cleans_alpha(),
            "premultiplying already zeroes the hidden colour"
        );
    }

    #[test]
    fn alpha_mode_discriminants_are_the_frozen_c_abi_values() {
        // Plain configuration data bound for the C boundary: append-only discriminants.
        assert_eq!(AlphaMode::Unassociated as u8, 0);
        assert_eq!(AlphaMode::Premultiplied as u8, 1);
        assert_eq!(AlphaMode::PremultiplyOnEncode as u8, 2);
        assert_eq!(AlphaMode::default(), AlphaMode::Unassociated);
    }

    #[test]
    fn prem_is_declared_by_every_alpha_mode_but_unassociated() {
        let declares = |mode| {
            AvifEncoder::new()
                .with_alpha_mode(mode)
                .declares_premultiplied()
        };
        assert!(!AvifEncoder::new().declares_premultiplied(), "the default");
        assert!(!declares(AlphaMode::Unassociated));
        assert!(declares(AlphaMode::Premultiplied));
        assert!(declares(AlphaMode::PremultiplyOnEncode));
    }

    #[test]
    fn with_premultiplied_alpha_selects_the_declaring_modes() {
        let mode = |premultiplied| {
            AvifEncoder::new()
                .with_alpha_mode(AlphaMode::PremultiplyOnEncode)
                .with_premultiplied_alpha(premultiplied)
                .alpha_mode
        };
        assert_eq!(mode(true), AlphaMode::Premultiplied);
        assert_eq!(mode(false), AlphaMode::Unassociated);
    }

    #[test]
    fn rgba8_encode_codes_the_premultiplied_colour_on_premultiply_on_encode() {
        let px = banded_rgba8();
        let mut premultiplied = ImageBuf::<Rgba8>::new(px.clone(), BANDED).unwrap();
        premultiply_alpha(&mut premultiplied);
        assert_ne!(
            premultiplied.as_samples(),
            &px[..],
            "the fixture is not opaque"
        );
        let encode = |enc: AvifEncoder, px: &[u8]| {
            enc.with_clean_alpha(true)
                .encode_to_vec(ImageRef::<Rgba8>::new(px, BANDED).unwrap())
                .unwrap()
        };
        // Coded as a caller handing over the premultiplied image would have it coded — on both
        // paths, and with cleaning (asked for here) not applied on top.
        for enc in [AvifEncoder::lossless(), AvifEncoder::lossy(50)] {
            assert_eq!(
                encode(
                    enc.clone().with_alpha_mode(AlphaMode::PremultiplyOnEncode),
                    &px
                ),
                encode(
                    enc.with_alpha_mode(AlphaMode::Premultiplied),
                    premultiplied.as_samples()
                )
            );
        }
    }

    #[test]
    fn rgba16_encode_codes_the_premultiplied_colour_on_premultiply_on_encode() {
        let px: Vec<u16> = banded_rgba8()
            .into_iter()
            .map(|s| u16::from(s) * 257)
            .collect();
        let mut premultiplied = ImageBuf::<Rgba16>::new(px.clone(), BANDED).unwrap();
        premultiply_alpha(&mut premultiplied);
        assert_ne!(premultiplied.as_samples(), &px[..]);
        let encode = |enc: AvifEncoder, px: &[u16]| {
            enc.encode_to_vec(ImageRef::<Rgba16>::new(px, BANDED).unwrap())
                .unwrap()
        };
        assert_eq!(
            encode(
                AvifEncoder::lossless().with_alpha_mode(AlphaMode::PremultiplyOnEncode),
                &px
            ),
            encode(
                AvifEncoder::lossless().with_alpha_mode(AlphaMode::Premultiplied),
                premultiplied.as_samples()
            )
        );
    }

    #[test]
    fn widened_rgba8_encode_premultiplies_after_widening_on_premultiply_on_encode() {
        let px = banded_rgba8();
        let narrow = ImageRef::<Rgba8>::new(&px, BANDED).unwrap();
        // Widen first, then premultiply at 16-bit precision (decision 5 of #710) ...
        let mut wide = convert::<Rgba8, Rgba16>(narrow, ConvertPolicy::lossless()).unwrap();
        premultiply_alpha(&mut wide);
        // ... which differs from premultiplying at 8 bits and widening the result, so the
        // comparison below pins the order and not just that premultiplication happened.
        let mut narrow_first = ImageBuf::<Rgba8>::new(px.clone(), BANDED).unwrap();
        premultiply_alpha(&mut narrow_first);
        let narrow_first =
            convert::<Rgba8, Rgba16>(narrow_first.as_ref(), ConvertPolicy::lossless()).unwrap();
        assert_ne!(wide.as_samples(), narrow_first.as_samples());
        for depth in [BitDepth::Ten, BitDepth::Twelve] {
            let enc = AvifEncoder::lossless().with_bit_depth(depth);
            assert_eq!(
                enc.clone()
                    .with_alpha_mode(AlphaMode::PremultiplyOnEncode)
                    .encode_to_vec(narrow)
                    .unwrap(),
                enc.with_alpha_mode(AlphaMode::Premultiplied)
                    .encode_to_vec(wide.as_ref())
                    .unwrap(),
                "{depth:?}"
            );
        }
    }

    /// An 8×8 RGBA image: an opaque, semi-transparent and transparent band, with noisy colour
    /// everywhere — including under the transparency, where cleaning has something to remove.
    fn banded_rgba8() -> Vec<u8> {
        let mut px = Vec::with_capacity(8 * 8 * 4);
        for y in 0u32..8 {
            for x in 0u32..8 {
                let alpha = [255, 255, 128, 40, 0, 0, 0, 0][x as usize];
                px.extend_from_slice(&[
                    (x * 37 + y * 101) as u8,
                    (x * y * 53 + 7) as u8,
                    (x ^ y).wrapping_mul(29) as u8,
                    alpha,
                ]);
            }
        }
        px
    }

    const BANDED: Dimensions = Dimensions {
        width: 8,
        height: 8,
    };

    #[test]
    fn rgba8_encode_codes_the_cleaned_colour_when_cleaning_applies() {
        let px = banded_rgba8();
        let cleaned = clean_rgba(&px, 8);
        assert_ne!(
            cleaned, px,
            "the fixture hides colour for cleaning to change"
        );
        let encode = |enc: AvifEncoder, px: &[u8]| {
            enc.encode_to_vec(ImageRef::<Rgba8>::new(px, BANDED).unwrap())
                .unwrap()
        };
        assert_eq!(
            encode(AvifEncoder::lossy(50).with_clean_alpha(true), &px),
            encode(AvifEncoder::lossy(50), &cleaned)
        );
        // Not applying is coding the source as given.
        assert_eq!(
            encode(AvifEncoder::lossless().with_clean_alpha(true), &px),
            encode(AvifEncoder::lossless(), &px)
        );
    }

    #[test]
    fn rgba16_encode_codes_the_cleaned_colour_when_cleaning_applies() {
        let px: Vec<u16> = banded_rgba8()
            .into_iter()
            .map(|s| u16::from(s) * 257)
            .collect();
        let cleaned = clean_rgba(&px, 8);
        assert_ne!(cleaned, px);
        let encode = |enc: AvifEncoder, px: &[u16]| {
            enc.encode_to_vec(ImageRef::<Rgba16>::new(px, BANDED).unwrap())
                .unwrap()
        };
        assert_eq!(
            encode(AvifEncoder::lossy(50).with_clean_alpha(true), &px),
            encode(AvifEncoder::lossy(50), &cleaned)
        );
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
        // The C2PA slot prints its kind and length — a reserved slot is distinguishable from a
        // caller-supplied store of the same size, and neither dumps bytes.
        assert!(bare.contains("c2pa: None"), "{bare}");
        let reserved = format!("{:?}", AvifEncoder::new().with_c2pa_reserved(4096));
        assert!(
            reserved.contains("c2pa: Some(Reserved(4096))"),
            "{reserved}"
        );
        let store = format!("{:?}", AvifEncoder::new().with_c2pa(&payload(0x55, 3)));
        assert!(store.contains("c2pa: Some(Store(3))"), "{store}");
    }

    #[test]
    fn an_unusable_c2pa_reservation_is_refused_by_the_object_safe_encode_path() {
        // `with_c2pa_reserved` returns `Self`, so the refusal has to arrive at the encode — and at
        // the `EncodeImage` entry point most callers reach for, not only at `encode_with_report`.
        // Before the length was checked, the release profile wrapped the framing addition here and
        // returned `Ok` with a file whose C2PA box the crate's own locator could not find, so an
        // `Ok` on either length is the outcome this pins shut. The two lengths fail for different
        // reasons, so each is matched on the reason it gives.
        let rgb = vec![0u8; 34 * 18 * 3];
        let dims = Dimensions {
            width: 34,
            height: 18,
        };
        let img = ImageRef::<Rgb8>::new(&rgb, dims).expect("the ramp fits the dimensions");
        for (len, reason) in [
            (0usize, "at least 8 bytes"),
            (usize::MAX, "exceeds the largest"),
        ] {
            let mut out = Vec::new();
            let err = AvifEncoder::new()
                .with_c2pa_reserved(len)
                .encode_image(img, &mut out)
                .expect_err("an unusable reservation is refused");
            assert!(format!("{err}").contains(reason), "{len}: {err}");
        }
    }

    #[test]
    fn with_c2pa_reserved_emits_one_content_provenance_box_after_ftyp() {
        use gamut_isobmff::{TopLevelPosition, read};
        // The slot is a *top-level* box, never an item: the item list and the primary's
        // properties are exactly what an unconfigured encoder writes, and `top_level_boxes` gains
        // one `uuid` box with the §A.5.1 user type, placed `AfterFtyp` (§A.5.3), whose payload is
        // the §A.5.1.2 framing around `len` zero bytes.
        let img = read(&encode_with(
            AvifEncoder::new().with_c2pa_reserved(96),
            34,
            18,
        ))
        .expect("parses");
        assert_eq!(img.items.len(), 1, "no item is added");
        assert_eq!(img.items[0].properties.len(), 4, "no property is added");
        assert_eq!(img.top_level_boxes.len(), 1);
        let top = &img.top_level_boxes[0];
        assert_eq!(top.ty, *b"uuid");
        assert_eq!(top.user_type, Some(C2PA_UUID));
        assert_eq!(top.position, TopLevelPosition::AfterFtyp);
        assert_eq!(
            top.payload,
            content_provenance_payload(C2paBoxPurpose::Manifest, &[0u8; 96])
        );
        // `with_c2pa` writes the store where the zeros would be, under the same framing.
        let store = payload(0x66, 96);
        let img = read(&encode_with(AvifEncoder::new().with_c2pa(&store), 34, 18)).expect("parses");
        assert_eq!(
            img.top_level_boxes[0].payload,
            content_provenance_payload(C2paBoxPurpose::Manifest, &store)
        );
        // Neither knob set: no top-level box at all (the byte-identity guarantee).
        let img = read(&encode_with(AvifEncoder::new(), 34, 18)).expect("parses");
        assert!(img.top_level_boxes.is_empty());
    }

    #[test]
    fn the_last_c2pa_call_wins_whichever_kind_it_is() {
        // The two knobs set one slot, so the later call replaces the earlier one in both orders.
        let store = payload(0x77, 8);
        assert_eq!(
            AvifEncoder::new()
                .with_c2pa(&store)
                .with_c2pa_reserved(16)
                .c2pa,
            Some(SlotSource::Reserved(16))
        );
        assert_eq!(
            AvifEncoder::new()
                .with_c2pa_reserved(16)
                .with_c2pa(&store)
                .c2pa,
            Some(SlotSource::Store(store))
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

    /// The widened `Gray8` path (`monochrome_still16` over `gray8_widened`) codes its still at the
    /// encoder's preset: at 10 and 12 bits the primary item is `gamut-av1`'s own output for the
    /// widened plane at that preset. Reads the private `gray8_widened`, so it lives here.
    #[test]
    fn gray8_high_bit_depth_codes_its_still_at_the_chosen_preset() {
        const W: u32 = 40;
        const H: u32 = 24;
        let px: Vec<u8> = (0..H)
            .flat_map(|y| (0..W).map(move |x| ((x * x + y * 29) & 0xff) as u8))
            .collect();
        let image = ImageRef::<Gray8>::new(
            &px,
            Dimensions {
                width: W,
                height: H,
            },
        )
        .unwrap();
        for depth in [BitDepth::Ten, BitDepth::Twelve] {
            let primary_at = |speed| {
                let enc = AvifEncoder::lossy(50)
                    .with_bit_depth(depth)
                    .with_speed(speed);
                let bytes = enc.encode_to_vec(image).unwrap();
                let container = AvifContainer::parse(&bytes).expect("parses");
                container
                    .image()
                    .primary_item()
                    .as_isobmff_item()
                    .payload
                    .clone()
            };
            let want = gamut_av1::encode_still_intra16_with_speed(
                &gray8_widened(image, depth).unwrap(),
                127,
                Av1Colour::monochrome(),
                Speed::Fastest,
            )
            .unwrap()
            .0
            .obus;
            let fastest = primary_at(Speed::Fastest);
            assert_eq!(fastest, want, "{depth:?}");
            assert_ne!(
                fastest,
                primary_at(Speed::Thorough),
                "{depth:?}: the fixture separates the presets"
            );
        }
    }

    /// The colour still gets `ceil(budget / 2)` threads and the alpha `floor(budget / 2)`, so the
    /// two never exceed the budget; a budget of one runs them one after the other. The alpha share
    /// is invisible in the output, so it is pinned here.
    #[test]
    fn split_threads_gives_the_colour_the_larger_half() {
        let split = |budget| {
            let (colour, alpha) = split_threads(NonZeroUsize::new(budget).unwrap());
            (colour.get(), alpha.map(NonZeroUsize::get))
        };
        assert_eq!(split(1), (1, None));
        assert_eq!(split(2), (1, Some(1)));
        assert_eq!(split(3), (2, Some(1)));
        assert_eq!(split(4), (2, Some(2)));
        assert_eq!(split(7), (4, Some(3)));
    }

    /// With a budget of two the alpha still is coded on its own scoped thread; a panic there
    /// reaches the caller with its own payload, as it would had the alpha been coded inline,
    /// rather than being swallowed or replaced by the join's error.
    #[test]
    fn colour_and_alpha_resumes_an_alpha_thread_panic_on_the_caller() {
        let encoder = AvifEncoder::lossless().with_threads(NonZeroUsize::new(2));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            encoder.colour_and_alpha(
                |_| {
                    Err(gamut_core::Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "colour stand-in",
                    ))
                },
                |_| -> Result<EncodedStill> { panic!("alpha thread panicked") },
            )
        }));
        let payload = caught.expect_err("the alpha thread's panic reaches the caller");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"alpha thread panicked")
        );
    }
}
