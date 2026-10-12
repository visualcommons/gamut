//! The pluggable AV1 still-picture decode pipeline (issue #250): the [`Av1StillDecoder`] hook, the
//! decoder's [`DecodedFrame`] output contract, and the derivation pipeline that turns an AVIF item
//! into planar samples or a presentation-ready [`ImageBuf<Rgba8>`](gamut_core::ImageBuf).
//!
//! # Where the codec boundary sits
//!
//! `gamut-avif` owns the *container* and everything around the coded picture — item derivation
//! (`grid`/`iden`/`iovl`), colour conversion, chroma upsampling, alpha merge, and the
//! `clap`/`irot`/`imir` transforms — but not the AV1 codestream itself (OBU payload decoding is
//! codec scope, and on many platforms is better served by a hardware/OS decoder — exactly the
//! split downstream `rawshift` drives with VideoToolbox/VAAPI/MediaCodec). [`Av1StillDecoder`] is
//! that seam: a caller implements AV1 still decode once, and every compliant AVIF image — single
//! items, grids, overlays, identity derivations, alpha, and the transformative properties —
//! decodes through this crate.
//!
//! The trait name fulfils the `Av1StillDecoder` reservation the workspace codec-seam plan
//! (issue #274) recorded for this crate's decode surface; the backend registry and
//! `gamut-codec-abi` adapter around it are the deferred follow-up (issue #241's program) and land
//! additively.
//!
//! # FFI-adaptable by construction
//!
//! [`Av1StillDecoder`] is deliberately object-safe and C-shaped so a `-sys` shim can wrap a
//! platform decoder behind a function-pointer table:
//!
//! - **Plain byte-slice input.** [`decode_still`](Av1StillDecoder::decode_still) receives the raw
//!   item payload (a low-overhead OBU stream) as `&[u8]` together with the typed
//!   [`Av1Config`]; an implementation either hands the pair to a decoder that accepts
//!   `av1C` + sample data directly, or bridges them into one self-contained temporal unit with
//!   [`Av1Config::full_stream`]. The *pipeline* (not the implementation) calls
//!   [`Av1Config::validate_still_payload`] first, so a payload violating the still-image
//!   constraints never reaches the hook.
//! - **POD-ish output.** [`DecodedFrame`] is uniform `u16` sample planes plus a handful of
//!   scalars ([`ChromaFormat`] is `#[repr(u8)]`), which map onto a C struct without a translation
//!   layer.
//! - **`&mut self`.** A stateful platform decoder (reused contexts, GPU handles) fits behind
//!   `&mut self`; a pure function fits too.
//!
//! # Three presentation surfaces
//!
//! - [`AvifImage::decode_item_planar`] — the **raw** surface. It resolves an item through its
//!   derivation (coded → decoder; `iden` → source; `grid` → tile assembly in the plane domain)
//!   and returns the decoder's [`DecodedFrame`] untouched by colour or transforms. Everything a
//!   caller could want is here, at full bit depth and native chroma; the caller drives
//!   colour/transforms with the [`AvifItem`](crate::AvifItem) property accessors — the surface a
//!   consumer with its own colour pipeline (rawshift) uses. `iovl` compositing is *not* on this
//!   surface (it is defined over RGBA) — it returns [`Error::Unsupported`], directing callers to
//!   the RGBA path.
//! - [`AvifImage::decode_item_rgba8`] / [`AvifImage::decode_primary_rgba8`] — the
//!   **presentation** surface. It planar-decodes (or `iovl`-composites), converts colour, merges
//!   the alpha auxiliary, and applies the item's transformative properties in `ipma` order. It
//!   presents **8-bit coded frames only**: a deeper frame is [`Error::Unsupported`] here rather
//!   than silently narrowed.
//! - [`AvifImage::decode_item_rgba16`] / [`AvifImage::decode_primary_rgba16`] — the
//!   **high-bit-depth presentation** surface (issue #303). The same pipeline over 16-bit samples:
//!   it accepts every coded depth [`DecodedFrame`] admits (`8..=16`) and normalizes samples to the
//!   full 16-bit range, so the buffer does not carry the coded depth — read that from the planar
//!   surface or `av1C`.
//!
//! Both presentation surfaces share one colour policy, documented on
//! [`AvifImage::decode_item_rgba8`]; anything outside it is [`Error::Unsupported`] on *those*
//! surfaces while the planar surface still delivers the samples.

use gamut_av1::SequenceHeader;
use gamut_color::{BitDepth, ColorRange, MatrixCoefficients, YcbcrMatrix, ycbcr_to_rgb};
use gamut_core::{Dimensions, Error, ImageBuf, Result, Rgba8, Rgba16};
use gamut_isobmff::ColourInformation;

use crate::av1c::{Av1Config, ChromaFormat};
use crate::image::{AvifImage, AvifItem, CleanAperture, ItemKind, TransformativeProperty};

/// The maximum derivation-reference nesting depth. A derived item (`grid`/`iden`/`iovl`) may
/// reference other derived items; this bounds the recursion so a deep (or, together with the
/// per-branch cycle check, a self-referential) derivation graph errors instead of overflowing the
/// stack.
const MAX_DERIVATION_DEPTH: usize = 8;

/// The message of the error [`DecodeLimits`] raises for a dimension past
/// [`DecodeLimits::max_dimension`].
pub(crate) const DIMENSION_LIMIT: &str =
    "AVIF: image dimension exceeds DecodeLimits::max_dimension";

/// The message of the error [`DecodeLimits`] raises for a sample count past
/// [`DecodeLimits::max_pixels`].
pub(crate) const PIXEL_LIMIT: &str = "AVIF: image pixel count exceeds DecodeLimits::max_pixels";

/// The message of the error [`DecodeLimits`] raises for a coded-item decode past
/// [`DecodeLimits::max_decoded_items`].
pub(crate) const DECODED_ITEM_LIMIT: &str =
    "AVIF: coded-item decodes exceed DecodeLimits::max_decoded_items";

/// The message of the error [`DecodeLimits`] raises for a derived-item evaluation past
/// [`DecodeLimits::max_derived_items`].
pub(crate) const DERIVED_ITEM_LIMIT: &str =
    "AVIF: derived-item evaluations exceed DecodeLimits::max_derived_items";

/// Resource ceilings for the decode pipeline, checked **before** every canvas or plane the
/// pipeline sizes from the file.
///
/// Every buffer the decode surfaces allocate is sized by numbers in the file: a `grid` or `iovl`
/// declares its output canvas, an `ispe` declares a coded item's size, and an
/// [`Av1StillDecoder`] returns a frame whose size the codestream chose. A hostile file can
/// therefore ask for a 65535 × 65535 overlay — 16 GiB of 8-bit RGBA — with a few dozen bytes. These
/// caps are checked against the declared size before the buffer exists, so such a file is refused
/// with a typed error rather than serviced.
///
/// What is checked, on all three surfaces (planar, RGBA8, RGBA16):
///
/// - a coded item's `ispe`, before the [`Av1StillDecoder`] is invoked, and the frame the decoder
///   returns, before anything is derived from it;
/// - a `grid`'s output size, before any tile is decoded, and its tiled canvas
///   (`columns·tile_width × rows·tile_height` — the samples the tiles hold before the crop) once
///   the first tile fixes the tile size;
/// - an `iovl`'s output canvas, before it is allocated.
///
/// The caps apply to the image *as decoded*, before `clap`/`irot`/`imir`; none of those can grow
/// either measure (a rotation swaps the axes and both caps are axis-symmetric). Each refusal is
/// [`Error::Unsupported`] — the file may be well-formed, it is this decoder's budget that declines
/// it, as in `gamut-png` — with a message naming the field that refused it.
///
/// Separately from these caps, every full-image buffer the pipeline allocates is reserved
/// fallibly: an allocation the system refuses is [`Error::Unsupported`] too, never an abort.
///
/// **Work.** The size caps bound each image, not how many a file makes the pipeline produce: a
/// derivation may name one item many times (a `grid`'s `dimg` list may repeat a tile, and a `grid`
/// or `iovl` may reference another derived item), so a few nested references multiply into an
/// exponential number of evaluations from a small file, every one of them within the size caps.
/// Two counts therefore bound one top-level call (one `decode_item_*`/`decode_primary_*`
/// invocation, alpha auxiliary included): [`max_decoded_items`](Self::max_decoded_items) caps the
/// [`Av1StillDecoder::decode_still`] calls and [`max_derived_items`](Self::max_derived_items) the
/// `grid`/`iden`/`iovl` evaluations. An item is counted each time it is evaluated, not once per
/// file, so a repeated reference costs what it does. The refusal is raised *before* the call or
/// evaluation it would exceed, so with the size caps the work of one call is at most
/// `max_decoded_items + max_derived_items` images, each within the caps above.
///
/// The defaults are libavif's (`AVIF_DEFAULT_IMAGE_DIMENSION_LIMIT` and
/// `AVIF_DEFAULT_IMAGE_SIZE_LIMIT`, the reference decoder this crate is validated against), so a
/// file libavif decodes under its defaults is not refused here for size. The work defaults are
/// this crate's own, sized so that no file libavif decodes is refused here for its count: libavif
/// requires a `grid`'s tiles to be coded items and supports no `iovl`, so the most it decodes for
/// one still is a full `grid` plus a full alpha `grid` — 2 × 256 × 256 coded tiles, the `grid`
/// payload's 8-bit `rows_minus_one`/`columns_minus_one` (ISO/IEC 23008-12 §6.6.2.3.2) being what
/// bounds a grid at 256 × 256. The derived default, 1024, is far above the two `grid`s libavif
/// evaluates; what it stops accepting are files only this crate's wider derivation support reaches,
/// such as a `grid` of more than 1023 `iden` tiles, which libavif refuses outright. This is a
/// different type
/// from `gamut_av1::DecodeLimits`: that one caps a single AV1 frame (and its tile count) inside
/// the codec, this one caps the container's derived images, which may be larger than any one
/// frame.
///
/// Construct with [`DecodeLimits::new`] or start from [`DecodeLimits::default`] and assign a
/// field; the struct is `#[non_exhaustive]` so a further cap can be added without a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecodeLimits {
    /// The largest accepted width **or** height, in pixels. Default 32768.
    pub max_dimension: u32,
    /// The largest accepted pixel count (`width × height`), bounding the allocation independently
    /// of the aspect ratio. Default 16384 × 16384 (268 435 456).
    pub max_pixels: u64,
    /// The most [`Av1StillDecoder::decode_still`] calls one top-level decode makes. Default
    /// 131072 (2 × 256 × 256: a full `grid` and a full alpha `grid`).
    pub max_decoded_items: u32,
    /// The most derived-item (`grid`/`iden`/`iovl`) evaluations one top-level decode makes.
    /// Default 1024.
    pub max_derived_items: u32,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self::new(32768, 16384 * 16384)
    }
}

impl DecodeLimits {
    /// Limits capping each dimension at `max_dimension` and the pixel count at `max_pixels`, with
    /// the default work caps ([`max_decoded_items`](Self::max_decoded_items) 131072,
    /// [`max_derived_items`](Self::max_derived_items) 1024); set those with
    /// [`with_item_limits`](Self::with_item_limits).
    #[must_use]
    pub const fn new(max_dimension: u32, max_pixels: u64) -> Self {
        Self {
            max_dimension,
            max_pixels,
            max_decoded_items: 2 * 256 * 256,
            max_derived_items: 1024,
        }
    }

    /// These limits with the work caps replaced: at most `max_decoded_items`
    /// [`Av1StillDecoder::decode_still`] calls and `max_derived_items` derived-item evaluations per
    /// top-level decode.
    #[must_use]
    pub const fn with_item_limits(self, max_decoded_items: u32, max_derived_items: u32) -> Self {
        Self {
            max_decoded_items,
            max_derived_items,
            ..self
        }
    }

    /// Refuses a `width × height` image past either cap.
    fn check(self, width: u32, height: u32) -> Result<()> {
        if width.max(height) > self.max_dimension {
            return Err(Error::unsupported(env!("CARGO_PKG_NAME"), DIMENSION_LIMIT));
        }
        if u64::from(width) * u64::from(height) > self.max_pixels {
            return Err(Error::unsupported(env!("CARGO_PKG_NAME"), PIXEL_LIMIT));
        }
        Ok(())
    }
}

/// A `len`-element buffer of `fill`, reserved fallibly: a request the allocator refuses (or one
/// past `isize::MAX` bytes) is [`Error::Unsupported`] rather than an abort.
fn try_filled<T: Clone>(len: usize, fill: T) -> Result<Vec<T>> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(len).map_err(|_| {
        Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "AVIF: out of memory allocating a decode buffer",
        )
    })?;
    buffer.resize(len, fill);
    Ok(buffer)
}

/// A pluggable AV1 still-picture decoder: the seam issue #250 exposes so a user can hook in a
/// platform AV1 decoder (dav1d, VideoToolbox, VAAPI, MediaCodec, a hardware block …) and decode
/// compliant AVIF images end to end through this crate.
///
/// Implement [`decode_still`](Self::decode_still) once; the crate's pipeline
/// ([`AvifImage::decode_item_planar`] / [`AvifImage::decode_item_rgba8`]) then handles item
/// derivation, colour, alpha, and the transformative properties around it. The trait is
/// object-safe (`&mut dyn Av1StillDecoder`) and byte-slice-shaped so it is straightforward to
/// adapt across an FFI boundary — see the module documentation.
pub trait Av1StillDecoder {
    /// Decodes one AV1 still picture to a planar [`DecodedFrame`].
    ///
    /// `config` is the typed `av1C` record of the item (profile/level, bit depth, chroma, and any
    /// `configOBUs`); `payload` is the raw `av01` item payload — a low-overhead OBU stream. An
    /// implementation that needs one self-contained stream (a temporal delimiter, the
    /// `configOBUs`, then the payload, every OBU sized) builds it with
    /// [`Av1Config::full_stream`]; a decoder taking `av1C` + sample data separately (the shape
    /// hardware still-decode APIs use) passes the two through unchanged. The returned frame's
    /// `width`/`height` are the **post-superres** luma dimensions the decoder outputs.
    ///
    /// The pipeline guarantees `payload` has already passed
    /// [`Av1Config::validate_still_payload`] (exactly one sequence header, first frame a shown
    /// key frame, no tile list), so an implementation need not re-check the still-image
    /// constraints.
    ///
    /// # Errors
    ///
    /// Returns a [`gamut_core::Error`] if the codestream cannot be decoded — malformed bitstream,
    /// an unsupported AV1 tool or profile, and so on. The pipeline propagates it unchanged.
    fn decode_still(&mut self, config: &Av1Config, payload: &[u8]) -> Result<DecodedFrame>;
}

/// The owned planar output of an [`Av1StillDecoder`]: a YCbCr (or monochrome) frame at native
/// chroma and bit depth.
///
/// Samples are carried uniformly as `u16` whatever the `bit_depth` (an 8-bit frame simply uses
/// the low byte), so one plane layout serves 8..=16-bit content and maps cleanly onto a C buffer.
/// The planes are row-major: `y` is `width × height`; `cb`/`cr` are the chroma-plane dimensions
/// of the [`chroma`](Self::chroma) format (see [`ChromaFormat::chroma_dimensions`]) and are
/// **empty** for [`ChromaFormat::Monochrome`].
///
/// Construct with [`DecodedFrame::new`], which validates every plane length against
/// `width`/`height`/`chroma` (chroma dimensions use ceiling division, so an odd luma dimension is
/// handled) and the `bit_depth` range — an internally inconsistent frame is unrepresentable, so
/// the pipeline downstream can trust the plane sizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    width: u32,
    height: u32,
    bit_depth: u8,
    chroma: ChromaFormat,
    y: Vec<u16>,
    cb: Vec<u16>,
    cr: Vec<u16>,
}

impl DecodedFrame {
    /// Builds a [`DecodedFrame`] from its planes, validating internal consistency.
    ///
    /// `width`/`height` are the luma dimensions (post superres). The chroma planes `cb`/`cr` must
    /// match [`ChromaFormat::chroma_dimensions`] for `(width, height)` — which uses **ceiling**
    /// division on the subsampled axes, so a 5×3 luma frame in 4:2:0 has 3×2 chroma planes — and
    /// must both be **empty** for [`ChromaFormat::Monochrome`]. `bit_depth` must be in `8..=16`
    /// (AV1 itself codes 8/10/12; the wider bound keeps the frame type usable for intermediate
    /// results).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if either dimension is zero, if `bit_depth` is outside
    /// `8..=16`, or if any plane length does not match the dimensions for `chroma`.
    pub fn new(
        width: u32,
        height: u32,
        bit_depth: u8,
        chroma: ChromaFormat,
        y: Vec<u16>,
        cb: Vec<u16>,
        cr: Vec<u16>,
    ) -> Result<Self> {
        let luma = Dimensions::new(width, height)?
            .num_pixels()
            .ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "AVIF: frame dimensions overflow usize",
                )
            })?;
        if !(8..=16).contains(&bit_depth) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: frame bit depth out of range (8..=16)",
            ));
        }
        if y.len() != luma {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: luma plane length does not match dimensions",
            ));
        }
        let (cw, ch) = chroma.chroma_dimensions(width, height);
        // Cannot overflow: each chroma dimension is at most its luma dimension, and `luma` fit
        // usize.
        let chroma_len = cw as usize * ch as usize;
        if chroma == ChromaFormat::Monochrome {
            if !cb.is_empty() || !cr.is_empty() {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "AVIF: monochrome frame must have empty chroma planes",
                ));
            }
        } else if cb.len() != chroma_len || cr.len() != chroma_len {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: chroma plane length does not match dimensions",
            ));
        }
        Ok(Self {
            width,
            height,
            bit_depth,
            chroma,
            y,
            cb,
            cr,
        })
    }

    /// The luma width in pixels (post superres).
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The luma height in pixels (post superres).
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The luma dimensions.
    #[must_use]
    pub fn dimensions(&self) -> Dimensions {
        Dimensions {
            width: self.width,
            height: self.height,
        }
    }

    /// The per-sample bit depth (`8..=16`). Samples are stored in `u16` regardless.
    #[must_use]
    pub fn bit_depth(&self) -> u8 {
        self.bit_depth
    }

    /// The chroma sampling format.
    #[must_use]
    pub fn chroma(&self) -> ChromaFormat {
        self.chroma
    }

    /// The chroma-plane dimensions `(width, height)` — `(0, 0)` for [`ChromaFormat::Monochrome`].
    #[must_use]
    pub fn chroma_dimensions(&self) -> (u32, u32) {
        self.chroma.chroma_dimensions(self.width, self.height)
    }

    /// The row-major luma plane (`width × height` samples).
    #[must_use]
    pub fn y(&self) -> &[u16] {
        &self.y
    }

    /// The row-major Cb plane (empty for [`ChromaFormat::Monochrome`]).
    #[must_use]
    pub fn cb(&self) -> &[u16] {
        &self.cb
    }

    /// The row-major Cr plane (empty for [`ChromaFormat::Monochrome`]).
    #[must_use]
    pub fn cr(&self) -> &[u16] {
        &self.cr
    }
}

impl AvifImage {
    /// Decodes an item to a raw planar [`DecodedFrame`] — the surface that delivers everything,
    /// with no colour conversion or transformative properties applied (drive those from the
    /// item's [`AvifItem`](crate::AvifItem) accessors).
    ///
    /// The item is resolved through its derivation, recursively, depth-limited (a self- or
    /// mutually-referential `dimg` graph errors rather than recursing forever) and work-limited (a
    /// repeated reference is evaluated again and counted again, against
    /// [`DecodeLimits::max_decoded_items`] and [`DecodeLimits::max_derived_items`]):
    ///
    /// - **Coded** (`av01`): the payload is validated against the AVIF still-image constraints
    ///   ([`Av1Config::validate_still_payload`]) and handed to `decoder`.
    /// - **`iden`**: the frame of its single `dimg` source.
    /// - **`grid`**: every tile is decoded (row-major `dimg` order) and assembled in the plane
    ///   domain onto a `columns·tile_w × rows·tile_h` canvas, then cropped to the grid's output
    ///   size (ISO/IEC 23008-12 §6.6.2.3.2). All tiles must share chroma format, bit depth, and
    ///   dimensions, else [`Error::Unsupported`]; the arithmetic is checked so a hostile grid
    ///   cannot amplify allocation.
    /// - **`iovl`**: [`Error::Unsupported`] — overlay compositing is defined over RGBA.
    ///
    /// Transformative properties (`clap`/`irot`/`imir`) are **not** applied here — read them via
    /// [`AvifItem::transformative_properties`](crate::AvifItem::transformative_properties).
    ///
    /// Decodes under [`DecodeLimits::default`]; [`decode_item_planar_with_limits`](Self::decode_item_planar_with_limits)
    /// takes caller-chosen limits.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] for a missing item, a malformed `av1C`, a payload
    /// violating the still-image constraints, a derivation cycle, or a malformed derivation;
    /// [`Error::Unsupported`] for an `iovl`, a non-AV1 coded item, an item with an unrecognised
    /// essential property, a non-uniform grid, a derivation nested past the internal depth
    /// limit, an image or a count of decodes/derivations past the [`DecodeLimits`], or an
    /// allocation the system refuses; and
    /// propagates the `decoder`'s errors.
    pub fn decode_item_planar(
        &self,
        id: u32,
        decoder: &mut dyn Av1StillDecoder,
    ) -> Result<DecodedFrame> {
        self.decode_item_planar_with_limits(id, decoder, DecodeLimits::default())
    }

    /// [`decode_item_planar`](Self::decode_item_planar) under caller-chosen [`DecodeLimits`].
    ///
    /// # Errors
    ///
    /// As [`decode_item_planar`](Self::decode_item_planar), with `limits` as the budget.
    pub fn decode_item_planar_with_limits(
        &self,
        id: u32,
        decoder: &mut dyn Av1StillDecoder,
        limits: DecodeLimits,
    ) -> Result<DecodedFrame> {
        let mut walk = Walk::default();
        self.decode_planar_inner(id, decoder, &mut walk, limits)
    }

    /// Decodes an item to a presentation-ready [`ImageBuf<Rgba8>`]: planar decode (or `iovl`
    /// compositing), colour conversion, alpha-auxiliary merge, then the item's transformative
    /// properties applied in `ipma` order.
    ///
    /// **Colour policy.** Shared with [`decode_item_rgba16`](Self::decode_item_rgba16); this
    /// surface additionally presents **8-bit coded frames only**. Anything outside the policy is
    /// [`Error::Unsupported`] *here* while [`decode_item_planar`](Self::decode_item_planar) still
    /// delivers the samples:
    ///
    /// - **nclx matrix 0 (identity / GBR)** — requires 4:4:4; samples are mapped directly
    ///   (`R=Cr, G=Y, B=Cb`) with no range expansion. This is what the crate's **lossless**
    ///   encoder emits, so a gamut-encoded lossless AVIF decodes here bit-exactly.
    /// - **nclx matrix 1 (BT.709), 5/6 (BT.601, BT.470 System B,G), 9 (BT.2020 non-constant
    ///   luminance)** — via the general H.273 §8.3 derivation, with the nclx `full_range` flag
    ///   selecting [`ColorRange`]. BT.709 is what this crate's own lossy encoder emits.
    /// - **nclx matrix 2 (unspecified)** — treated as BT.601, matching libavif's fallback for
    ///   unspecified matrix coefficients (real-world AVIFs commonly stamp CICP 2/2/2).
    /// - **Monochrome** — luma replicated to gray, range-expanded for limited range.
    /// - **Missing `colr`** — the matrix and range come from the AV1 sequence header's
    ///   `color_config` (details below), then follow the rules above.
    /// - Anything else — YCgCo, the chromaticity-derived matrices (12/13/14), an ICC-only `colr`,
    ///   or a coded depth CICP does not model — is [`Error::Unsupported`]. On *this* surface a
    ///   `bit_depth > 8` frame is too: narrowing it would trade an honest error for silent quality
    ///   loss, so use [`decode_item_rgba16`](Self::decode_item_rgba16) instead.
    ///
    /// **Which conversion runs.** BT.601 at 8 bits uses [`gamut_color::ycbcr_to_rgb`], gamut-color's
    /// libwebp-exact integer inverse; **every other (matrix, depth) pair** uses the H.273-derived
    /// bit-depth-generic [`YcbcrMatrix`]. The two agree to within a rounding unit — the split exists
    /// so the 8-bit BT.601 output this crate has always produced, which the libavif differential
    /// bound pins, stays byte-identical.
    ///
    /// **Missing `colr`.** AVIF v1.2.0 §2.2.1 defers an item without a `colr` to the AV1 sequence
    /// header's `color_config`, and so does this surface: `matrix_coefficients` and `color_range`
    /// are read from the header [`av1_sequence_header`](Self::av1_sequence_header) parses — for a
    /// `grid` the first tile's, for an `iden` its source's, as libavif does. A header with
    /// `color_description_present_flag = 0` signals matrix 2 (unspecified) and so presents as
    /// BT.601, at the range it codes. A `colr` that *is* present always wins over the header,
    /// including when the two disagree. A sequence header that does not parse is an error here,
    /// never a guessed default; the planar surface still delivers the samples.
    ///
    /// **Chroma upsampling** is nearest-neighbour, co-sited (4:2:0 / 4:2:2 → 4:4:4) — the
    /// deliberate space/time tradeoff for a container-layer presentation helper; a caller wanting
    /// a better resampler works from the planar surface.
    ///
    /// **Alpha.** The item's alpha auxiliary (if any) is decoded through the same planar path and
    /// must match the master's **pre-transform** dimensions; its **luma plane** supplies the
    /// alpha samples, scaled from its bit depth to 8-bit. Unlike the HEIF surface, a
    /// non-monochrome alpha auxiliary is accepted (its chroma planes are ignored): real-world
    /// AVIF alpha items are routinely coded 4:2:0/4:4:4 with meaningless chroma, and libavif
    /// likewise reads only the Y plane. An absent alpha is opaque (255). Premultiplication is
    /// **not** undone — query [`AvifImage::is_premultiplied`] and un-premultiply yourself if
    /// needed (an [`Rgba8`] buffer cannot carry the flag).
    ///
    /// **Transforms** are applied to the interleaved RGBA in `ipma` order (ISO/IEC 23008-12 §7):
    /// even when the order violates the MIAF constraint (check with
    /// [`AvifItem::is_miaf_transform_ordered`](crate::AvifItem::is_miaf_transform_ordered)) they
    /// are applied as listed. `clap` crops per ISO/IEC 14496-12 §12.1.4, `irot` rotates
    /// anti-clockwise, `imir` mirrors top↔bottom (axis 0) or left↔right (axis 1) per ISO/IEC
    /// 23008-12:2022 §6.5.12.
    ///
    /// **Limits.** Decodes under [`DecodeLimits::default`], checked before every canvas and plane
    /// (see [`DecodeLimits`]); [`decode_item_rgba8_with_limits`](Self::decode_item_rgba8_with_limits)
    /// takes caller-chosen limits.
    ///
    /// # Errors
    ///
    /// As [`decode_item_planar`](Self::decode_item_planar), plus [`Error::Unsupported`] for an
    /// unsupported colour configuration and [`Error::InvalidInput`] for a mismatched alpha
    /// auxiliary or an out-of-range/non-integer `clap`.
    pub fn decode_item_rgba8(
        &self,
        id: u32,
        decoder: &mut dyn Av1StillDecoder,
    ) -> Result<ImageBuf<Rgba8>> {
        self.decode_item_rgba8_with_limits(id, decoder, DecodeLimits::default())
    }

    /// [`decode_item_rgba8`](Self::decode_item_rgba8) under caller-chosen [`DecodeLimits`].
    ///
    /// # Errors
    ///
    /// As [`decode_item_rgba8`](Self::decode_item_rgba8), with `limits` as the budget.
    pub fn decode_item_rgba8_with_limits(
        &self,
        id: u32,
        decoder: &mut dyn Av1StillDecoder,
        limits: DecodeLimits,
    ) -> Result<ImageBuf<Rgba8>> {
        let mut walk = Walk::default();
        let (rgba, dims) = self.decode_rgba_inner::<u8>(id, decoder, &mut walk, limits)?;
        ImageBuf::<Rgba8>::new(rgba, dims)
    }

    /// Decodes the primary item to a presentation-ready [`ImageBuf<Rgba8>`].
    ///
    /// Equivalent to [`decode_item_rgba8`](Self::decode_item_rgba8) on the primary item id.
    ///
    /// # Errors
    ///
    /// As [`decode_item_rgba8`](Self::decode_item_rgba8).
    pub fn decode_primary_rgba8(
        &self,
        decoder: &mut dyn Av1StillDecoder,
    ) -> Result<ImageBuf<Rgba8>> {
        self.decode_primary_rgba8_with_limits(decoder, DecodeLimits::default())
    }

    /// [`decode_primary_rgba8`](Self::decode_primary_rgba8) under caller-chosen [`DecodeLimits`].
    ///
    /// # Errors
    ///
    /// As [`decode_item_rgba8`](Self::decode_item_rgba8), with `limits` as the budget.
    pub fn decode_primary_rgba8_with_limits(
        &self,
        decoder: &mut dyn Av1StillDecoder,
        limits: DecodeLimits,
    ) -> Result<ImageBuf<Rgba8>> {
        self.decode_item_rgba8_with_limits(self.as_isobmff().primary_item_id, decoder, limits)
    }

    /// Decodes an item to a presentation-ready **high-bit-depth** [`ImageBuf<Rgba16>`]: the same
    /// pipeline as [`decode_item_rgba8`](Self::decode_item_rgba8) — planar decode (or `iovl`
    /// compositing), colour conversion, alpha merge, then the transformative properties in `ipma`
    /// order — widened to 16-bit samples and the full matrix set.
    ///
    /// **Sample scale.** Samples are normalized to the **full 16-bit range** `0..=65535` whatever
    /// the coded depth: a 10-bit sample `s` becomes `(s·65535 + 511)/1023`, a 12-bit sample
    /// `(s·65535 + 2047)/4095`, and an 8-bit sample exactly `s·257`. The buffer therefore carries
    /// no residual notion of "this was 10-bit" — an [`ImageBuf`] has nowhere to put one, and every
    /// other `Rgba16` producer in the workspace means full-range too. Read the coded depth from
    /// [`decode_item_planar`](Self::decode_item_planar)'s [`DecodedFrame::bit_depth`] or the item's
    /// `av1C`.
    ///
    /// **Coded depth.** Any depth [`DecodedFrame`] admits (`8..=16`) is accepted; 8-bit content
    /// widens losslessly, so a caller whose decoder emits either depth need not branch. The
    /// matrixed paths additionally require a CICP-modeled depth (8, 10, 12 or 16), while
    /// monochrome and identity work at every depth.
    ///
    /// **Colour policy.** As [`decode_item_rgba8`](Self::decode_item_rgba8), which documents the
    /// shared policy in full: nclx matrices 1 (BT.709), 2/5/6 (BT.601 and its aliases) and 9
    /// (BT.2020 non-constant luminance), identity (0) at 4:4:4, monochrome, and a missing `colr`
    /// resolved from the AV1 sequence header. Beyond 8 bits every matrix — BT.601 included — goes
    /// through the H.273-derived [`YcbcrMatrix`].
    ///
    /// **Limits.** As [`decode_item_rgba8`](Self::decode_item_rgba8):
    /// [`DecodeLimits::default`], or caller-chosen through
    /// [`decode_item_rgba16_with_limits`](Self::decode_item_rgba16_with_limits).
    ///
    /// # Errors
    ///
    /// As [`decode_item_rgba8`](Self::decode_item_rgba8), except that a `bit_depth > 8` frame is no
    /// longer rejected.
    pub fn decode_item_rgba16(
        &self,
        id: u32,
        decoder: &mut dyn Av1StillDecoder,
    ) -> Result<ImageBuf<Rgba16>> {
        self.decode_item_rgba16_with_limits(id, decoder, DecodeLimits::default())
    }

    /// [`decode_item_rgba16`](Self::decode_item_rgba16) under caller-chosen [`DecodeLimits`].
    ///
    /// # Errors
    ///
    /// As [`decode_item_rgba16`](Self::decode_item_rgba16), with `limits` as the budget.
    pub fn decode_item_rgba16_with_limits(
        &self,
        id: u32,
        decoder: &mut dyn Av1StillDecoder,
        limits: DecodeLimits,
    ) -> Result<ImageBuf<Rgba16>> {
        let mut walk = Walk::default();
        let (rgba, dims) = self.decode_rgba_inner::<u16>(id, decoder, &mut walk, limits)?;
        ImageBuf::<Rgba16>::new(rgba, dims)
    }

    /// Decodes the primary item to a presentation-ready [`ImageBuf<Rgba16>`].
    ///
    /// Equivalent to [`decode_item_rgba16`](Self::decode_item_rgba16) on the primary item id.
    ///
    /// # Errors
    ///
    /// As [`decode_item_rgba16`](Self::decode_item_rgba16).
    pub fn decode_primary_rgba16(
        &self,
        decoder: &mut dyn Av1StillDecoder,
    ) -> Result<ImageBuf<Rgba16>> {
        self.decode_primary_rgba16_with_limits(decoder, DecodeLimits::default())
    }

    /// [`decode_primary_rgba16`](Self::decode_primary_rgba16) under caller-chosen
    /// [`DecodeLimits`].
    ///
    /// # Errors
    ///
    /// As [`decode_item_rgba16`](Self::decode_item_rgba16), with `limits` as the budget.
    pub fn decode_primary_rgba16_with_limits(
        &self,
        decoder: &mut dyn Av1StillDecoder,
        limits: DecodeLimits,
    ) -> Result<ImageBuf<Rgba16>> {
        self.decode_item_rgba16_with_limits(self.as_isobmff().primary_item_id, decoder, limits)
    }

    // ---- planar pipeline ---------------------------------------------------------------------

    fn decode_planar_inner(
        &self,
        id: u32,
        decoder: &mut dyn Av1StillDecoder,
        walk: &mut Walk,
        limits: DecodeLimits,
    ) -> Result<DecodedFrame> {
        let item = self.item(id).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "AVIF: item id names no item")
        })?;
        if item.has_unsupported_essential_property() {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: item has an unrecognised essential property",
            ));
        }
        enter_derivation(&mut walk.stack, id)?;
        let out = self.decode_planar_kind(id, item, decoder, walk, limits);
        walk.stack.pop();
        out
    }

    fn decode_planar_kind(
        &self,
        id: u32,
        item: AvifItem<'_>,
        decoder: &mut dyn Av1StillDecoder,
        walk: &mut Walk,
        limits: DecodeLimits,
    ) -> Result<DecodedFrame> {
        match item.kind() {
            ItemKind::CodedImage { .. } if item.kind().is_av1() => {
                let config = item.av1_config().ok_or_else(|| {
                    Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "AVIF: AV1 item has no av1C configuration",
                    )
                })??;
                let payload = &item.as_isobmff_item().payload;
                // The pipeline enforces the still-image constraints before the codec hook sees
                // the payload (a non-conforming stream never reaches `decode_still`).
                config.validate_still_payload(payload)?;
                // The declared size is refused before the decoder is asked to allocate it, and
                // the delivered one before anything is derived from it: a decoder need not honour
                // `ispe` (or the file need not carry one), so the second check is not redundant.
                if let Some(declared) = item.dimensions() {
                    limits.check(declared.width, declared.height)?;
                }
                walk.charge_decode(limits)?;
                let frame = decoder.decode_still(&config, payload)?;
                limits.check(frame.width, frame.height)?;
                Ok(frame)
            }
            ItemKind::CodedImage { .. } => Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: only AV1 coded items are decodable here",
            )),
            ItemKind::Identity => {
                let [source] = item.derivation_target_ids() else {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "AVIF: iden item must reference exactly one source",
                    ));
                };
                walk.charge_derivation(limits)?;
                self.decode_planar_inner(*source, decoder, walk, limits)
            }
            ItemKind::Grid => {
                walk.charge_derivation(limits)?;
                self.decode_grid(id, item, decoder, walk, limits)
            }
            ItemKind::Overlay => Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: overlay compositing is not available on the planar surface (use the RGBA path)",
            )),
            ItemKind::Exif | ItemKind::Mime | ItemKind::Unknown(_) => Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: item is not a decodable image",
            )),
        }
    }

    fn decode_grid(
        &self,
        id: u32,
        item: AvifItem<'_>,
        decoder: &mut dyn Av1StillDecoder,
        walk: &mut Walk,
        limits: DecodeLimits,
    ) -> Result<DecodedFrame> {
        // `grid` validates the payload and that the tile count equals rows * columns.
        let grid = self.grid(id)?;
        let cols = usize::from(grid.columns);
        let rows = usize::from(grid.rows);
        let (ow, oh) = (grid.output_width, grid.output_height);
        // The declared output is refused before a single tile is decoded.
        limits.check(ow, oh)?;

        // A grid always has >= 1 tile (rows, columns are >= 1), and the first fixes the tile
        // size, so the canvas the tiles fill is checked before a second tile is decoded and every
        // later tile is held to the first's shape as it arrives: the tiles held at once never
        // outgrow the limits.
        let mut tiles: Vec<DecodedFrame> = Vec::with_capacity(cols * rows);
        for &tile_id in item.derivation_target_ids() {
            let tile = self.decode_planar_inner(tile_id, decoder, walk, limits)?;
            match tiles.first() {
                None => {
                    check_grid_canvas((cols, rows), (tile.width, tile.height), (ow, oh), limits)?
                }
                Some(first)
                    if tile.chroma != first.chroma
                        || tile.bit_depth != first.bit_depth
                        || tile.width != first.width
                        || tile.height != first.height =>
                {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "AVIF: grid tiles are not uniform in dimensions, chroma, or bit depth",
                    ));
                }
                Some(_) => {}
            }
            tiles.push(tile);
        }
        let first = &tiles[0];
        let (chroma, bit_depth, tw, th) =
            (first.chroma, first.bit_depth, first.width, first.height);

        let y = assemble_plane(
            &tiles,
            DecodedFrame::y,
            cols,
            tw as usize,
            th as usize,
            ow as usize,
            oh as usize,
        )?;
        let (cb, cr) = if chroma == ChromaFormat::Monochrome {
            (Vec::new(), Vec::new())
        } else {
            let (tcw, tch) = chroma.chroma_dimensions(tw, th);
            let (ocw, och) = chroma.chroma_dimensions(ow, oh);
            let cb = assemble_plane(
                &tiles,
                DecodedFrame::cb,
                cols,
                tcw as usize,
                tch as usize,
                ocw as usize,
                och as usize,
            )?;
            let cr = assemble_plane(
                &tiles,
                DecodedFrame::cr,
                cols,
                tcw as usize,
                tch as usize,
                ocw as usize,
                och as usize,
            )?;
            (cb, cr)
        };
        DecodedFrame::new(ow, oh, bit_depth, chroma, y, cb, cr)
    }

    // ---- RGBA pipeline -----------------------------------------------------------------------

    /// The shared RGBA pipeline, generic over the presentation sample width. Returns the
    /// interleaved buffer and its post-transform dimensions rather than an `ImageBuf`, so the
    /// overlay recursion can composite sub-images without allocating and revalidating one per
    /// sub-item; the two public entry points wrap the result.
    fn decode_rgba_inner<T: RgbaSample>(
        &self,
        id: u32,
        decoder: &mut dyn Av1StillDecoder,
        walk: &mut Walk,
        limits: DecodeLimits,
    ) -> Result<(Vec<T>, Dimensions)> {
        let item = self.item(id).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "AVIF: item id names no item")
        })?;
        if item.has_unsupported_essential_property() {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: item has an unrecognised essential property",
            ));
        }

        let (rgba, w, h) = if matches!(item.kind(), ItemKind::Overlay) {
            walk.charge_derivation(limits)?;
            enter_derivation(&mut walk.stack, id)?;
            let composited = self.composite_overlay(id, item, decoder, walk, limits);
            walk.stack.pop();
            composited?
        } else {
            // The planar path applies its own cycle/depth guard for `id`, and its own limits: a
            // frame it returns is within them, so the RGBA buffer sized from it is too.
            let frame = self.decode_planar_inner(id, decoder, walk, limits)?;
            let mut rgba = frame_to_rgba(&frame, self.colour_params(&item)?)?;
            let (w, h) = (frame.width, frame.height);
            if let Some(alpha) = self.alpha_auxiliary_of(id) {
                let alpha_frame = self.decode_planar_inner(alpha.id(), decoder, walk, limits)?;
                apply_alpha(&mut rgba, w as usize, h as usize, &alpha_frame)?;
            }
            (rgba, w, h)
        };

        let (rgba, w, h) = apply_transforms(item.transformative_properties(), rgba, w, h)?;
        Ok((rgba, Dimensions::new(w, h)?))
    }

    fn composite_overlay<T: RgbaSample>(
        &self,
        id: u32,
        item: AvifItem<'_>,
        decoder: &mut dyn Av1StillDecoder,
        walk: &mut Walk,
        limits: DecodeLimits,
    ) -> Result<(Vec<T>, u32, u32)> {
        // `overlay` validates the payload holds exactly one offset per `dimg` reference.
        let overlay = self.overlay(id)?;
        let (ow, oh) = (overlay.output_width, overlay.output_height);
        // The declared canvas is refused before it is allocated (or any input decoded).
        limits.check(ow, oh)?;
        let len = (ow as usize)
            .checked_mul(oh as usize)
            .and_then(|p| p.checked_mul(4))
            .filter(|_| ow != 0 && oh != 0)
            .ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "AVIF: overlay canvas is empty or too large",
                )
            })?;
        // The `canvas_fill_value` channels are 16-bit; narrowing to a smaller sample takes the
        // most significant bits (ISO/IEC 23008-12 §6.6.2.4), deliberately *not* the
        // round-to-nearest rescale used for coded samples.
        let fill = [
            T::from_canvas_fill(overlay.canvas_fill_value[0]),
            T::from_canvas_fill(overlay.canvas_fill_value[1]),
            T::from_canvas_fill(overlay.canvas_fill_value[2]),
            T::from_canvas_fill(overlay.canvas_fill_value[3]),
        ];
        let mut canvas = try_filled(len, T::default())?;
        for px in canvas.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&fill);
        }
        for (&input_id, &(dx, dy)) in item.derivation_target_ids().iter().zip(&overlay.offsets) {
            let (src, dims) = self.decode_rgba_inner::<T>(input_id, decoder, walk, limits)?;
            composite_over(
                &mut canvas,
                (ow, oh),
                &src,
                (dims.width, dims.height),
                (dx, dy),
            );
        }
        Ok((canvas, ow, oh))
    }
}

// ---- derivation recursion guard --------------------------------------------------------------

/// The state of one top-level decode's walk through the derivation graph: the current branch
/// (for the cycle and depth guard) and the work spent so far against the [`DecodeLimits`] work
/// caps. One is created per public `decode_*` call and threaded through every recursive step, so
/// the alpha auxiliary and every repeated reference draw on the same budget.
#[derive(Debug, Default)]
struct Walk {
    /// The items being evaluated on the current branch, outermost first.
    stack: Vec<u32>,
    /// [`Av1StillDecoder::decode_still`] calls made so far.
    decoded: u32,
    /// Derived-item evaluations started so far.
    derived: u32,
}

impl Walk {
    /// Counts one [`Av1StillDecoder::decode_still`] call, refusing it if `limits` allow no more.
    fn charge_decode(&mut self, limits: DecodeLimits) -> Result<()> {
        self.decoded = charge(self.decoded, limits.max_decoded_items, DECODED_ITEM_LIMIT)?;
        Ok(())
    }

    /// Counts one derived-item evaluation, refusing it if `limits` allow no more.
    fn charge_derivation(&mut self, limits: DecodeLimits) -> Result<()> {
        self.derived = charge(self.derived, limits.max_derived_items, DERIVED_ITEM_LIMIT)?;
        Ok(())
    }
}

/// `spent + 1`, or the work-limit refusal `message` if `spent` has already reached `cap`.
fn charge(spent: u32, cap: u32, message: &'static str) -> Result<u32> {
    if spent >= cap {
        return Err(Error::unsupported(env!("CARGO_PKG_NAME"), message));
    }
    Ok(spent + 1)
}

/// Cycle- and depth-checks a derivation step, then records `id` on the recursion `stack`. The
/// caller pops `id` when the step completes.
fn enter_derivation(stack: &mut Vec<u32>, id: u32) -> Result<()> {
    if stack.contains(&id) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: derivation reference cycle",
        ));
    }
    if stack.len() >= MAX_DERIVATION_DEPTH {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "AVIF: derivation nested too deeply",
        ));
    }
    stack.push(id);
    Ok(())
}

// ---- grid plane assembly ---------------------------------------------------------------------

/// Checks a grid's tiled canvas — `columns·tile_w × rows·tile_h`, the samples its tiles hold
/// before the crop — once the first tile has fixed `tile_w × tile_h`: the arithmetic is checked,
/// the declared output must be non-empty and fit within the canvas (ISO/IEC 23008-12
/// §6.6.2.3.2), and the canvas itself must be within `limits`.
///
/// Holding the canvas, not only the output, to the limits is what bounds the memory the decoded
/// tiles occupy together. It refuses a grid whose tiles overhang its output by enough to cross a
/// limit the output itself respects; raise the limits to accept one.
fn check_grid_canvas(
    (cols, rows): (usize, usize),
    (tw, th): (u32, u32),
    (ow, oh): (u32, u32),
    limits: DecodeLimits,
) -> Result<()> {
    let canvas_w = (cols as u32).checked_mul(tw).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "AVIF: grid canvas width overflow")
    })?;
    let canvas_h = (rows as u32).checked_mul(th).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "AVIF: grid canvas height overflow")
    })?;
    if ow == 0 || oh == 0 || ow > canvas_w || oh > canvas_h {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: grid output dimensions exceed the tiled canvas",
        ));
    }
    limits.check(canvas_w, canvas_h)
}

/// Assembles one plane of a grid directly into its cropped output: for each output sample it
/// reads the covering tile's plane, so the tile canvas (`columns·tile_pw × rows·tile_ph`) is
/// sampled and cropped to `out_pw × out_ph` in a single pass. `out_pw <= columns·tile_pw` and
/// `out_ph <= rows·tile_ph` are guaranteed by the caller, so the tile index is always in range.
fn assemble_plane(
    tiles: &[DecodedFrame],
    plane: fn(&DecodedFrame) -> &[u16],
    cols: usize,
    tile_pw: usize,
    tile_ph: usize,
    out_pw: usize,
    out_ph: usize,
) -> Result<Vec<u16>> {
    let mut out = try_filled(out_pw * out_ph, 0u16)?;
    for oy in 0..out_ph {
        let (tile_row, in_y) = (oy / tile_ph, oy % tile_ph);
        for ox in 0..out_pw {
            let (tile_col, in_x) = (ox / tile_pw, ox % tile_pw);
            let tile = &tiles[tile_row * cols + tile_col];
            out[oy * out_pw + ox] = plane(tile)[in_y * tile_pw + in_x];
        }
    }
    Ok(out)
}

// ---- presentation sample width ---------------------------------------------------------------

mod sample_sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
}

/// The interleaved-RGBA sample type of a presentation surface: `u8` for [`Rgba8`], `u16` for
/// [`Rgba16`]. Sealed — those two surfaces are the only implementors.
///
/// Every presentation surface carries samples over its type's **full** range, so [`Self::MAX`] is
/// both the opaque-alpha value and the unity of the source-over blend.
trait RgbaSample: sample_sealed::Sealed + Copy + Default + Eq {
    /// The all-ones sample: `255` for `u8`, `65535` for `u16`.
    const MAX: u32;

    /// The deepest coded frame this surface will present. The `u8` surface stops at 8 so that
    /// narrowing a 10/12-bit frame is an explicit [`Error::Unsupported`] rather than silent
    /// quality loss; the `u16` surface accepts everything [`DecodedFrame`] can carry.
    const MAX_CODED_DEPTH: u8;

    /// The name of the entry point that does present deeper frames, for that error message.
    const WIDER_SURFACE: &'static str;

    /// Narrows a value the caller has already bounded by [`Self::MAX`].
    fn narrow(v: u32) -> Self;

    /// Widens a sample for arithmetic.
    fn widen(self) -> u32;

    /// Narrows one `iovl` `canvas_fill_value` channel to this sample width by taking the **most
    /// significant bits** (ISO/IEC 23008-12 §6.6.2.4): `v >> 8` for `u8`, `v` unchanged for `u16`.
    fn from_canvas_fill(v: u16) -> Self;
}

impl RgbaSample for u8 {
    const MAX: u32 = 255;
    const MAX_CODED_DEPTH: u8 = 8;
    const WIDER_SURFACE: &'static str =
        "AVIF: >8-bit RGBA presentation needs decode_item_rgba16 (or the planar surface)";

    fn narrow(v: u32) -> Self {
        v as Self
    }

    fn widen(self) -> u32 {
        u32::from(self)
    }

    fn from_canvas_fill(v: u16) -> Self {
        (v >> 8) as Self
    }
}

impl RgbaSample for u16 {
    const MAX: u32 = 65535;
    const MAX_CODED_DEPTH: u8 = 16;
    const WIDER_SURFACE: &'static str = "AVIF: bit depth is out of range (use the planar surface)";

    fn narrow(v: u32) -> Self {
        v as Self
    }

    fn widen(self) -> u32 {
        u32::from(self)
    }

    fn from_canvas_fill(v: u16) -> Self {
        v
    }
}

/// Rescales one coded sample from a plane whose white level is `max_in` onto the presentation
/// sample's full range, round-to-nearest, saturating a sample that exceeds its declared depth.
///
/// This is the single place the surfaces' sample-scale policy lives. `rescale::<u8>(s, 255)` is the
/// identity, and `rescale::<u16>(s, 255)` is exactly `s * 257` — so widening 8-bit content to the
/// 16-bit surface is lossless.
///
/// Round-to-nearest rather than bit replication: the two differ (at 10-bit, sample 15 gives 961
/// here and 960 replicated), and this is the formula the alpha merge has always used.
fn rescale<T: RgbaSample>(s: u16, max_in: u32) -> T {
    let s = u64::from(u32::from(s).min(max_in));
    let max = u64::from(max_in);
    T::narrow(((s * u64::from(T::MAX) + max / 2) / max) as u32)
}

// ---- colour conversion -----------------------------------------------------------------------

impl AvifImage {
    /// The presentation matrix coefficients and signal range for `item`: its nclx `colr` when it
    /// carries one, else the `color_config` of the AV1 sequence header its samples were coded
    /// under (AVIF v1.2.0 §2.2.1; see `decode_item_rgba8`).
    fn colour_params(&self, item: &AvifItem<'_>) -> Result<(u16, ColorRange)> {
        let (matrix, full_range) = match item.colour() {
            None => {
                let colour = self.sample_sequence_header(item.id())?.color;
                (u16::from(colour.matrix_coefficients), colour.full_range)
            }
            Some(ColourInformation::Nclx(nclx)) => (nclx.matrix_coefficients, nclx.full_range),
            Some(_) => {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "AVIF: ICC-profile colr is not supported on the RGBA surface (use the planar \
                     surface)",
                ));
            }
        };
        let range = if full_range {
            ColorRange::Full
        } else {
            ColorRange::Limited
        };
        Ok((matrix, range))
    }

    /// The sequence header of the coded item whose samples item `id` presents: `id` itself when
    /// it is coded; for a `grid`, its **first** tile (the one libavif harvests an absent `colr`
    /// from); for an `iden`, its sole source — followed down to a coded item.
    ///
    /// Carries no cycle or depth guard of its own because it is only reached after
    /// [`decode_planar_inner`](Self::decode_planar_inner) has walked these same `dimg` edges under
    /// both, so the walk is known to be finite.
    fn sample_sequence_header(&self, id: u32) -> Result<SequenceHeader> {
        if let Some(item) = self.item(id)
            && let (ItemKind::Grid | ItemKind::Identity, [first, ..]) =
                (item.kind(), item.derivation_target_ids())
        {
            return self.sample_sequence_header(*first);
        }
        self.av1_sequence_header(id)
    }
}

/// The per-frame YCbCr → RGB conversion this crate's colour policy selected.
enum Converter {
    /// 8-bit BT.601 through [`ycbcr_to_rgb`], gamut-color's libwebp-exact integer inverse — the
    /// conversion this crate has always used at 8 bits, kept so shipped output stays byte-identical.
    Bt601Legacy8(ColorRange),
    /// The bit-depth- and matrix-generic H.273 §8.3 de-matrixing.
    Generic(YcbcrMatrix),
}

impl Converter {
    /// The converter for an nclx `matrix_coefficients` code point at `range` and `bit_depth`, or
    /// [`Error::Unsupported`] if this surface does not present that combination.
    ///
    /// Matrix 0 (identity / GBR) never reaches here — it is a plane permutation, handled inline.
    /// Matrix 2 (unspecified) resolves to BT.601, matching libavif's presentation default.
    fn select(matrix: u16, range: ColorRange, bit_depth: u8) -> Result<Self> {
        // BT.601 at 8 bits keeps the libwebp-exact inverse; every other pair takes the generic path.
        if matches!(matrix, 2 | 5 | 6) && bit_depth == 8 {
            return Ok(Converter::Bt601Legacy8(range));
        }
        let unsupported = || {
            Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: unsupported matrix coefficients or bit depth on the RGBA surface (BT.709, \
                 BT.601/BT.470 B,G, BT.2020 NCL, identity, and monochrome are supported at 8, 10, \
                 12 and 16 bits)",
            )
        };
        // Unspecified defaults to BT.601 here — including from a `colr`-less item whose sequence
        // header codes no colour description.
        let coefficients = if matrix == 2 {
            MatrixCoefficients::Bt601
        } else {
            MatrixCoefficients::from_code_point(matrix).ok_or_else(unsupported)?
        };
        // `DecodedFrame` admits any depth in 8..=16, but only the CICP-modeled depths have a
        // de-matrixing; an unmodeled depth is an explicit refusal, never a silent reinterpretation.
        let depth = BitDepth::from_bits(u32::from(bit_depth)).ok_or_else(unsupported)?;
        YcbcrMatrix::new(coefficients, range, depth).map(Converter::Generic)
    }

    /// Converts one triple, returning samples at the **input** depth's scale, so the caller
    /// rescales from the frame's white level either way.
    fn to_rgb(&self, y: u16, cb: u16, cr: u16) -> (u16, u16, u16) {
        match self {
            Converter::Bt601Legacy8(range) => {
                let (r, g, b) = ycbcr_to_rgb(y as u8, cb as u8, cr as u8, *range);
                (u16::from(r), u16::from(g), u16::from(b))
            }
            Converter::Generic(matrix) => matrix.to_rgb(y, cb, cr),
        }
    }
}

/// Converts a planar [`DecodedFrame`] to an interleaved RGBA buffer (opaque alpha) at the
/// presentation sample width `T`, applying this crate's colour policy (see
/// [`AvifImage::decode_item_rgba8`]) to the item's resolved `(matrix_coefficients, range)`.
fn frame_to_rgba<T: RgbaSample>(
    frame: &DecodedFrame,
    (matrix, range): (u16, ColorRange),
) -> Result<Vec<T>> {
    if frame.bit_depth > T::MAX_CODED_DEPTH {
        return Err(Error::unsupported(env!("CARGO_PKG_NAME"), T::WIDER_SURFACE));
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    let max_in = (1u32 << frame.bit_depth) - 1;
    let opaque = T::narrow(T::MAX);
    let len = w
        .checked_mul(h)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: image is too large to present",
            )
        })?;
    let mut out = try_filled(len, T::default())?;

    if frame.chroma == ChromaFormat::Monochrome {
        for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let g = expand_gray::<T>(frame.y[i], frame.bit_depth, range);
            px.copy_from_slice(&[g, g, g, opaque]);
        }
        return Ok(out);
    }

    // Identity / GBR (mc = 0): plane order Y=G, Cb=B, Cr=R; requires 4:4:4, no range expansion.
    // The configuration gamut-avif's own lossless encoder emits.
    if matrix == 0 {
        if frame.chroma != ChromaFormat::Yuv444 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "AVIF: identity matrix (mc = 0) requires 4:4:4 chroma",
            ));
        }
        for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px.copy_from_slice(&[
                rescale::<T>(frame.cr[i], max_in),
                rescale::<T>(frame.y[i], max_in),
                rescale::<T>(frame.cb[i], max_in),
                opaque,
            ]);
        }
        return Ok(out);
    }

    let converter = Converter::select(matrix, range, frame.bit_depth)?;
    let (cw, _) = frame.chroma.chroma_dimensions(frame.width, frame.height);
    let cw = cw as usize;
    for y in 0..h {
        let cy = match frame.chroma {
            ChromaFormat::Yuv420 => y / 2,
            _ => y,
        };
        for x in 0..w {
            let cx = match frame.chroma {
                ChromaFormat::Yuv444 => x,
                _ => x / 2,
            };
            let ci = cy * cw + cx;
            let (r, g, b) = converter.to_rgb(frame.y[y * w + x], frame.cb[ci], frame.cr[ci]);
            let o = (y * w + x) * 4;
            out[o..o + 4].copy_from_slice(&[
                rescale::<T>(r, max_in),
                rescale::<T>(g, max_in),
                rescale::<T>(b, max_in),
                opaque,
            ]);
        }
    }
    Ok(out)
}

/// Expands one monochrome luma sample at `bit_depth` to a display gray value on the presentation
/// sample's full range: the identity for full range, and the studio-swing expansion
/// `(y - 16·2^(bit_depth-8)) · max / (219·2^(bit_depth-8))` (rounded, clamped) for limited range.
///
/// Like every other path here the expansion happens **at the coded depth** and the result is then
/// rescaled, so one precision model covers the whole surface and 8-bit content widens onto the
/// 16-bit surface exactly. At `bit_depth = 8` the limited-range arm is exactly
/// `((y - 16)·255 + 109)/219`, since `219/2 == 109` in integer division.
fn expand_gray<T: RgbaSample>(y: u16, bit_depth: u8, range: ColorRange) -> T {
    let max_in = (1u32 << bit_depth) - 1;
    let coded = match range {
        ColorRange::Full => u32::from(y).min(max_in),
        ColorRange::Limited => {
            let s = i64::from(1u32 << (bit_depth - 8));
            let black = 16 * s;
            let span = 219 * s;
            let max = i64::from(max_in);
            let expanded = ((i64::from(y) - black) * max + span / 2) / span;
            expanded.clamp(0, max) as u32
        }
    };
    rescale::<T>(coded as u16, max_in)
}

/// Merges an alpha auxiliary into the RGBA buffer's alpha channel.
///
/// Only the auxiliary's **luma plane** is read: unlike the HEIF surface, a non-monochrome alpha
/// item is accepted (real-world AVIF alpha auxiliaries are routinely coded 4:2:0/4:4:4 with
/// meaningless chroma, and libavif likewise reads only Y). The dimensions must match the master's
/// pre-transform dimensions; samples are rescaled from their native bit depth to 8-bit,
/// round-to-nearest.
fn apply_alpha<T: RgbaSample>(
    rgba: &mut [T],
    w: usize,
    h: usize,
    alpha: &DecodedFrame,
) -> Result<()> {
    if alpha.width as usize != w || alpha.height as usize != h {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: alpha auxiliary dimensions do not match the master image",
        ));
    }
    let max = (1u32 << alpha.bit_depth) - 1;
    for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        // Depth 8 onto the `u8` surface needs no special case: `(s * 255 + 127) / 255 == s` for
        // every `s` in `0..=255`, so the general rescale is already the identity there.
        px[3] = rescale::<T>(alpha.y[i], max);
    }
    Ok(())
}

// ---- transformative properties (on interleaved RGBA8) ----------------------------------------

/// Applies transformative properties to an interleaved RGBA buffer in `ipma` (list) order,
/// returning the transformed buffer and its dimensions.
fn apply_transforms<T: Copy + Default>(
    props: Vec<TransformativeProperty>,
    rgba: Vec<T>,
    w: u32,
    h: u32,
) -> Result<(Vec<T>, u32, u32)> {
    let (mut cur, mut cw, mut ch) = (rgba, w, h);
    for tp in props {
        match tp {
            TransformativeProperty::Rotation(turns) => {
                // `turns` is 0..=3 by construction — the isobmff `irot` parser masks the angle
                // field with `& 0x03` — so each unit is exactly one 90° anti-clockwise rotation
                // and no modulo normalisation is needed here.
                for _ in 0..turns {
                    let rotated = rotate90_ccw(&cur, cw, ch)?;
                    cur = rotated.0;
                    cw = rotated.1;
                    ch = rotated.2;
                }
            }
            TransformativeProperty::Mirror(axis) => {
                cur = mirror(&cur, cw, ch, axis)?;
            }
            TransformativeProperty::CleanAperture(clap) => {
                let cropped = crop_clap(&cur, cw, ch, &clap)?;
                cur = cropped.0;
                cw = cropped.1;
                ch = cropped.2;
            }
        }
    }
    Ok((cur, cw, ch))
}

/// Rotates an interleaved RGBA buffer 90° anti-clockwise: output `(ox, oy)` reads input
/// `(w - 1 - oy, ox)`, giving output dimensions `(h, w)`.
fn rotate90_ccw<T: Copy + Default>(src: &[T], w: u32, h: u32) -> Result<(Vec<T>, u32, u32)> {
    let (wu, hu) = (w as usize, h as usize);
    let (nw, nh) = (hu, wu);
    let mut out = try_filled(nw * nh * 4, T::default())?;
    for oy in 0..nh {
        for ox in 0..nw {
            let (sx, sy) = (wu - 1 - oy, ox);
            let si = (sy * wu + sx) * 4;
            let oi = (oy * nw + ox) * 4;
            out[oi..oi + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    Ok((out, h, w))
}

/// Mirrors an interleaved RGBA buffer per the `imir` axis semantics of ISO/IEC 23008-12:2022
/// §6.5.12: `axis = 0` exchanges the top and bottom parts, `axis = 1` exchanges the left and
/// right parts (the reading libheif and libavif implement). Dimensions are unchanged; any other
/// `axis` value is treated as axis 0 (the `imir` field is a single bit).
fn mirror<T: Copy + Default>(src: &[T], w: u32, h: u32, axis: u8) -> Result<Vec<T>> {
    let (wu, hu) = (w as usize, h as usize);
    let mut out = try_filled(wu * hu * 4, T::default())?;
    for y in 0..hu {
        for x in 0..wu {
            let (sx, sy) = if axis == 1 {
                (wu - 1 - x, y)
            } else {
                (x, hu - 1 - y)
            };
            let si = (sy * wu + sx) * 4;
            let oi = (y * wu + x) * 4;
            out[oi..oi + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    Ok(out)
}

/// Crops an interleaved RGBA buffer to its `clap` clean aperture (ISO/IEC 14496-12 §12.1.4).
///
/// The clean aperture is a `cleanApertureWidth × cleanApertureHeight` rectangle centred at
/// `((w - 1)/2 + horizOff, (h - 1)/2 + vertOff)`, so its top-left corner is
/// `left = (w - cleanApertureWidth)/2 + horizOff` and
/// `top = (h - cleanApertureHeight)/2 + vertOff` (the width/height and offset centre terms
/// combine exactly, matching libavif's integer clap conversion). The crop must be integer-valued
/// and lie fully inside the image; otherwise it is rejected.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if any denominator is zero, if the width/height or the
/// computed top-left corner is not integer-valued, or if the crop window falls outside the image.
fn crop_clap<T: Copy + Default>(
    src: &[T],
    w: u32,
    h: u32,
    clap: &CleanAperture,
) -> Result<(Vec<T>, u32, u32)> {
    if clap.width_d == 0 || clap.height_d == 0 || clap.horiz_off_d == 0 || clap.vert_off_d == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: clap has a zero denominator",
        ));
    }
    if !clap.width_n.is_multiple_of(clap.width_d) || !clap.height_n.is_multiple_of(clap.height_d) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: clap width/height is not integer-valued",
        ));
    }
    let crop_w = clap.width_n / clap.width_d;
    let crop_h = clap.height_n / clap.height_d;
    if crop_w == 0 || crop_h == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: clap has a zero-sized crop",
        ));
    }
    let left = clap_offset(
        i64::from(w),
        i64::from(crop_w),
        clap.horiz_off_n,
        clap.horiz_off_d,
    )?;
    let top = clap_offset(
        i64::from(h),
        i64::from(crop_h),
        clap.vert_off_n,
        clap.vert_off_d,
    )?;
    if left < 0
        || top < 0
        || left + i64::from(crop_w) > i64::from(w)
        || top + i64::from(crop_h) > i64::from(h)
    {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: clap crop window lies outside the image",
        ));
    }

    let (wu, left, top) = (w as usize, left as usize, top as usize);
    let (cw, chh) = (crop_w as usize, crop_h as usize);
    let mut out = try_filled(cw * chh * 4, T::default())?;
    for y in 0..chh {
        for x in 0..cw {
            let si = ((top + y) * wu + (left + x)) * 4;
            let oi = (y * cw + x) * 4;
            out[oi..oi + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    Ok((out, crop_w, crop_h))
}

/// The integer top-left offset of a clean-aperture crop along one axis:
/// `left = (dim - crop)/2 + off_n/off_d`, evaluated exactly as
/// `((dim - crop)·off_d + 2·off_n) / (2·off_d)`. `off_n` is signed (two's-complement bits);
/// `off_d` is a positive denominator.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the offset is not integer-valued.
fn clap_offset(dim: i64, crop: i64, off_n: u32, off_d: u32) -> Result<i64> {
    let num = (dim - crop) * i64::from(off_d) + 2 * i64::from(off_n as i32);
    let den = 2 * i64::from(off_d);
    if num % den != 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "AVIF: clap offset is not integer-valued",
        ));
    }
    Ok(num / den)
}

// ---- overlay compositing ---------------------------------------------------------------------

/// Composites a source RGBA image over the canvas at signed offset `(dx, dy)`, clipping to the
/// canvas, with unassociated-alpha source-over blending.
///
/// Source-over in integer math (`sa`/`da` the source/destination alpha, `sc`/`dc` a channel):
/// `out_a = sa + round(da·(255 - sa)/255)`, and each channel
/// `out_c = round((sc·sa + round(dc·da·(255 - sa)/255)) / out_a)`, or a fully transparent pixel
/// when `out_a = 0`. Rounding is round-half-up (`+ half`).
fn composite_over<T: RgbaSample>(
    canvas: &mut [T],
    canvas_dims: (u32, u32),
    src: &[T],
    src_dims: (u32, u32),
    offset: (i32, i32),
) {
    let (cw, ch) = canvas_dims;
    let (sw, sh) = src_dims;
    let (dx, dy) = offset;
    // The intermediates are `u64`: at 16-bit samples `dc·da·inv` reaches ~2.8e14, far past `u32`.
    // Every 8-bit intermediate already fitted `u32` and the arithmetic is exact, so widening cannot
    // change an 8-bit result.
    let max = u64::from(T::MAX);
    let transparent = T::narrow(0);
    for sy in 0..sh as i64 {
        let y = i64::from(dy) + sy;
        if y < 0 || y >= i64::from(ch) {
            continue;
        }
        for sx in 0..sw as i64 {
            let x = i64::from(dx) + sx;
            if x < 0 || x >= i64::from(cw) {
                continue;
            }
            let si = ((sy * i64::from(sw) + sx) * 4) as usize;
            let di = ((y * i64::from(cw) + x) * 4) as usize;
            let sa = u64::from(src[si + 3].widen());
            let inv = max - sa;
            let da = u64::from(canvas[di + 3].widen());
            let da_contrib = (da * inv + max / 2) / max;
            let out_a = sa + da_contrib;
            if out_a == 0 {
                canvas[di..di + 4].copy_from_slice(&[transparent; 4]);
                continue;
            }
            for c in 0..3 {
                let sc = u64::from(src[si + c].widen());
                let dc = u64::from(canvas[di + c].widen());
                let num = sc * sa + (dc * da * inv + max / 2) / max;
                canvas[di + c] = T::narrow(((num + out_a / 2) / out_a).min(max) as u32);
            }
            canvas[di + 3] = T::narrow(out_a as u32);
        }
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::{
        DECODED_ITEM_LIMIT, DIMENSION_LIMIT, DecodeLimits, PIXEL_LIMIT, charge, check_grid_canvas,
        try_filled,
    };

    /// The refusal `result` carries, as `(kind, message)`.
    fn refusal(result: gamut_core::Result<()>) -> (ErrorKind, Option<&'static str>) {
        let error = result.unwrap_err();
        (error.kind(), error.static_message())
    }

    #[test]
    fn default_limits_are_libavifs_dimension_and_size_limits() {
        // AVIF_DEFAULT_IMAGE_DIMENSION_LIMIT and AVIF_DEFAULT_IMAGE_SIZE_LIMIT (libavif avif.h).
        let limits = DecodeLimits::default();
        assert_eq!(limits.max_dimension, 32768);
        assert_eq!(limits.max_pixels, 16384 * 16384);
    }

    #[test]
    fn default_work_caps_admit_a_full_grid_and_a_full_alpha_grid() {
        // A grid's rows/columns are 8-bit minus-one fields: at most 256 x 256 tiles, twice over.
        let limits = DecodeLimits::default();
        assert_eq!(limits.max_decoded_items, 2 * 256 * 256);
        assert_eq!(limits.max_derived_items, 1024);
    }

    #[test]
    fn with_item_limits_sets_the_work_caps_and_keeps_the_size_caps() {
        let limits = DecodeLimits::new(10, 50).with_item_limits(3, 7);
        assert_eq!(
            (
                limits.max_dimension,
                limits.max_pixels,
                limits.max_decoded_items,
                limits.max_derived_items
            ),
            (10, 50, 3, 7)
        );
    }

    #[test]
    fn charge_counts_up_to_the_cap_and_refuses_at_it() {
        assert_eq!(charge(0, 2, DECODED_ITEM_LIMIT).unwrap(), 1);
        assert_eq!(charge(1, 2, DECODED_ITEM_LIMIT).unwrap(), 2);
        assert_eq!(
            refusal(charge(2, 2, DECODED_ITEM_LIMIT).map(drop)),
            (ErrorKind::Unsupported, Some(DECODED_ITEM_LIMIT))
        );
        // A zero cap admits nothing, and a saturated counter cannot wrap past the cap.
        assert!(charge(0, 0, DECODED_ITEM_LIMIT).is_err());
        assert!(charge(u32::MAX, u32::MAX, DECODED_ITEM_LIMIT).is_err());
    }

    #[test]
    fn check_accepts_an_image_exactly_at_both_caps() {
        let limits = DecodeLimits::new(10, 50);
        assert!(limits.check(10, 5).is_ok());
        assert!(limits.check(5, 10).is_ok());
    }

    #[test]
    fn check_refuses_a_width_or_a_height_one_past_the_dimension_cap() {
        let limits = DecodeLimits::new(10, 1000);
        let past = (ErrorKind::Unsupported, Some(DIMENSION_LIMIT));
        assert_eq!(refusal(limits.check(11, 1)), past);
        assert_eq!(refusal(limits.check(1, 11)), past);
    }

    #[test]
    fn check_refuses_a_pixel_count_past_the_cap_with_both_dimensions_inside() {
        // 8 x 7 = 56 > 50, while each side is within 10, and 8 + 7 and 8 / 7 are not past 50.
        let limits = DecodeLimits::new(10, 50);
        assert_eq!(
            refusal(limits.check(8, 7)),
            (ErrorKind::Unsupported, Some(PIXEL_LIMIT))
        );
    }

    #[test]
    fn check_counts_pixels_without_overflowing_u32() {
        // 65536 x 65536 = 2^32: a u32 product would wrap to 0 and pass.
        let limits = DecodeLimits::new(u32::MAX, u64::from(u32::MAX));
        assert_eq!(
            refusal(limits.check(65536, 65536)),
            (ErrorKind::Unsupported, Some(PIXEL_LIMIT))
        );
    }

    #[test]
    fn grid_canvas_past_the_limits_is_refused_though_the_output_is_inside_them() {
        // Two 6x1 tiles make a 12x1 canvas cropped to 7x1: the output respects a 10-pixel
        // dimension cap, the canvas the tiles fill does not.
        assert_eq!(
            refusal(check_grid_canvas(
                (2, 1),
                (6, 1),
                (7, 1),
                DecodeLimits::new(10, 100)
            )),
            (ErrorKind::Unsupported, Some(DIMENSION_LIMIT))
        );
        assert!(check_grid_canvas((2, 1), (5, 1), (7, 1), DecodeLimits::new(10, 100)).is_ok());
    }

    #[test]
    fn try_filled_returns_the_requested_buffer() {
        assert_eq!(try_filled(3, 7u16).unwrap(), vec![7, 7, 7]);
    }

    #[test]
    fn try_filled_refuses_an_impossible_allocation_instead_of_aborting() {
        // usize::MAX u64s is past isize::MAX bytes, so the reservation fails without allocating.
        let error = try_filled(usize::MAX, 0u64).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    /// The pinned-seed driver of `invariants::decoding_stays_within_limits` over generated
    /// coded, `grid` and `iovl` files; the fuzz tier's `avif_parse_decode` drives the same law
    /// over arbitrary bytes.
    mod limits_law {
        use gamut_isobmff::{
            ImageGrid, ImageOverlay, IsoBmffImage, Item, ItemReference, Property, PropertyKind,
            write,
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
            Item {
                id,
                item_type,
                name: String::new(),
                content_type: None,
                content_encoding: None,
                hidden,
                references: vec![],
                properties: vec![],
                payload: vec![],
            }
        }

        /// A coded item the stub decodes to `(1 + w % 16) × (1 + h % 16)`, optionally declaring
        /// `ispe`.
        fn coded(id: u32, (w, h): (u8, u8), ispe: Option<(u32, u32)>, hidden: bool) -> Item {
            let mut properties = vec![Property {
                essential: true,
                kind: PropertyKind::CodecConfiguration {
                    kind: *b"av1C",
                    data: AV1C.to_vec(),
                },
            }];
            if let Some((width, height)) = ispe {
                properties.push(Property {
                    essential: false,
                    kind: PropertyKind::ImageSpatialExtents { width, height },
                });
            }
            let mut payload = SEQUENCE_HEADER.to_vec();
            payload.extend_from_slice(&[0x32, 0x03, 0xAA, w, h]);
            Item {
                properties,
                payload,
                ..item(id, *b"av01", hidden)
            }
        }

        fn derived(id: u32, item_type: [u8; 4], payload: Vec<u8>, sources: &[u32]) -> Item {
            Item {
                references: vec![ItemReference {
                    reference_type: *b"dimg",
                    to_item_ids: sources.to_vec(),
                }],
                payload,
                ..item(id, item_type, false)
            }
        }

        fn file(items: Vec<Item>) -> Vec<u8> {
            write(&IsoBmffImage::new(
                *b"avif",
                vec![*b"avif", *b"mif1", *b"miaf"],
                1,
                items,
            ))
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
                    offsets: inputs.iter().map(|&(_, dx, dy)| (dx, dy)).collect(),
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
            use crate::AvifContainer;
            use crate::invariants::StubDecoder;

            let sizes = |bytes: Vec<u8>| {
                let image = AvifContainer::parse(&bytes).unwrap().image().clone();
                let rgba = image
                    .decode_primary_rgba16_with_limits(&mut StubDecoder, CEILING)
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
                offsets: vec![(0, 0)],
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
}
