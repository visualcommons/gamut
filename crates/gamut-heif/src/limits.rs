//! [`DecodeLimits`] and [`DecodeOptions`]: the budget a decode runs under and how it presents.

use gamut_color::ChromaUpsampling;
use gamut_core::{Error, Result};

/// The message of the error [`DecodeLimits`] raises for a dimension past
/// [`DecodeLimits::max_dimension`].
pub const DIMENSION_LIMIT: &str = "HEIF: image dimension exceeds DecodeLimits::max_dimension";

/// The message of the error [`DecodeLimits`] raises for a sample count past
/// [`DecodeLimits::max_pixels`].
pub const PIXEL_LIMIT: &str = "HEIF: image pixel count exceeds DecodeLimits::max_pixels";

/// The message of the error [`DecodeLimits`] raises for a coded-item decode past
/// [`DecodeLimits::max_decoded_items`].
pub const DECODED_ITEM_LIMIT: &str =
    "HEIF: coded-item decodes exceed DecodeLimits::max_decoded_items";

/// The message of the error [`DecodeLimits`] raises for a derived-item evaluation past
/// [`DecodeLimits::max_derived_items`].
pub const DERIVED_ITEM_LIMIT: &str =
    "HEIF: derived-item evaluations exceed DecodeLimits::max_derived_items";

/// Resource ceilings for the decode pipeline, checked **before** every canvas or plane the
/// pipeline sizes from the file.
///
/// Every buffer the decode surfaces allocate is sized by numbers in the file: a `grid` or `iovl`
/// declares its output canvas, an `ispe` declares a coded item's size, and a codec backend returns
/// a frame whose size the codestream chose. A hostile file can therefore ask for a 65535 × 65535
/// overlay — 16 GiB of 8-bit RGBA — with a few dozen bytes. These caps are checked against the
/// declared size before the buffer exists, so such a file is refused with a typed error rather
/// than serviced.
///
/// What is checked, on all three surfaces (planar, RGBA8, RGBA16):
///
/// - a coded item's `ispe`, before the backend is invoked, and the frame the backend returns,
///   before anything is derived from it;
/// - a `grid`'s output size, before any tile is decoded, and its tiled canvas
///   (`columns·tile_width × rows·tile_height` — the samples the tiles hold before the crop) once
///   the first tile fixes the tile size;
/// - an `iovl`'s output canvas, before it is allocated.
///
/// The caps apply to the image *as decoded*, before `clap`/`irot`/`imir`; none of those can grow
/// either measure (a rotation swaps the axes and both caps are axis-symmetric). Each refusal is
/// an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error — the file may be
/// well-formed, it is this decoder's budget that declines it — whose message is one of
/// [`DIMENSION_LIMIT`], [`PIXEL_LIMIT`], [`DECODED_ITEM_LIMIT`] and [`DERIVED_ITEM_LIMIT`].
///
/// Separately from these caps, every full-image buffer the pipeline allocates is reserved
/// fallibly: an allocation the system refuses is an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error too, never an abort.
///
/// **Work.** The size caps bound each image, not how many a file makes the pipeline produce: a
/// derivation may name one item many times (a `grid`'s `dimg` list may repeat a tile, and a `grid`
/// or `iovl` may reference another derived item), so a few nested references multiply into an
/// exponential number of evaluations from a small file, every one of them within the size caps.
/// Two counts therefore bound one top-level call (alpha auxiliary included):
/// [`max_decoded_items`](Self::max_decoded_items) caps the backend decodes and
/// [`max_derived_items`](Self::max_derived_items) the `grid`/`iden`/`iovl` evaluations. An item is
/// counted each time it is evaluated, not once per file, so a repeated reference costs what it
/// does. The refusal is raised *before* the call or evaluation it would exceed, so with the size
/// caps the work of one call is at most `max_decoded_items + max_derived_items` images, each
/// within the caps above.
///
/// The size defaults are libavif's (`AVIF_DEFAULT_IMAGE_DIMENSION_LIMIT` and
/// `AVIF_DEFAULT_IMAGE_SIZE_LIMIT`). The work defaults are sized so that no file libavif decodes
/// is refused for its count: libavif requires a `grid`'s tiles to be coded items and supports no
/// `iovl`, so the most it decodes for one still is a full `grid` plus a full alpha `grid` — 2 ×
/// 256 × 256 coded tiles, the `grid` payload's 8-bit `rows_minus_one`/`columns_minus_one`
/// (ISO/IEC 23008-12 §6.6.2.3.2) being what bounds a grid at 256 × 256. The derived default, 1024,
/// is far above the two `grid`s libavif evaluates; what it stops accepting are files only this
/// crate's wider derivation support reaches, such as a `grid` of more than 1023 `iden` tiles.
///
/// Construct with [`DecodeLimits::new`] or start from [`DecodeLimits::default`]; the struct is
/// `#[non_exhaustive]` so a further cap can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecodeLimits {
    /// The largest accepted width **or** height, in pixels. Default 32768.
    pub max_dimension: u32,
    /// The largest accepted pixel count (`width × height`), bounding the allocation independently
    /// of the aspect ratio. Default 16384 × 16384 (268 435 456).
    pub max_pixels: u64,
    /// The most backend decodes one top-level call makes. Default 131072 (2 × 256 × 256: a full
    /// `grid` and a full alpha `grid`).
    pub max_decoded_items: u32,
    /// The most derived-item (`grid`/`iden`/`iovl`) evaluations one top-level call makes.
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

    /// These limits with the work caps replaced: at most `max_decoded_items` backend decodes and
    /// `max_derived_items` derived-item evaluations per top-level call.
    #[must_use]
    pub const fn with_item_limits(self, max_decoded_items: u32, max_derived_items: u32) -> Self {
        Self {
            max_decoded_items,
            max_derived_items,
            ..self
        }
    }

    /// Refuses a `width × height` image past either cap, under `origin`.
    pub(crate) fn check(self, width: u32, height: u32, origin: &'static str) -> Result<()> {
        if width.max(height) > self.max_dimension {
            return Err(Error::unsupported(origin, DIMENSION_LIMIT));
        }
        if u64::from(width) * u64::from(height) > self.max_pixels {
            return Err(Error::unsupported(origin, PIXEL_LIMIT));
        }
        Ok(())
    }
}

/// Options for the decode surfaces: the [`DecodeLimits`] the decode runs under and the
/// [`ChromaUpsampling`] the RGBA conversion uses.
///
/// [`DecodeOptions::default`] holds the default limits and [`ChromaUpsampling::Interpolated`]
/// (libavif's bilinear filter). A codec crate whose reference decoder upsamples otherwise starts
/// from its own default — HEIC presents with [`ChromaUpsampling::Nearest`]. The struct is
/// `#[non_exhaustive]`; build it from the default with the `with_*` methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// The size and work caps the decode runs under. Default [`DecodeLimits::default`].
    pub limits: DecodeLimits,
    /// How 4:2:0 and 4:2:2 chroma is reconstructed before colour conversion. Default
    /// [`ChromaUpsampling::Interpolated`].
    pub chroma_upsampling: ChromaUpsampling,
}

impl DecodeOptions {
    /// These options with the limits replaced.
    #[must_use]
    pub const fn with_limits(self, limits: DecodeLimits) -> Self {
        Self { limits, ..self }
    }

    /// These options with the chroma upsampling replaced.
    #[must_use]
    pub const fn with_chroma_upsampling(self, chroma_upsampling: ChromaUpsampling) -> Self {
        Self {
            chroma_upsampling,
            ..self
        }
    }
}

/// A `len`-element buffer of `fill`, reserved fallibly: a request the allocator refuses (or one
/// past `isize::MAX` bytes) is an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported)
/// error under `origin` rather than an abort.
pub(crate) fn try_filled<T: Clone>(len: usize, fill: T, origin: &'static str) -> Result<Vec<T>> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(len).map_err(|_| {
        Error::unsupported(origin, "HEIF: out of memory allocating a decode buffer")
    })?;
    buffer.resize(len, fill);
    Ok(buffer)
}

/// `spent + 1`, or the work-limit refusal `message` under `origin` if `spent` has already
/// reached `cap`.
pub(crate) fn charge(
    spent: u32,
    cap: u32,
    message: &'static str,
    origin: &'static str,
) -> Result<u32> {
    if spent >= cap {
        return Err(Error::unsupported(origin, message));
    }
    Ok(spent + 1)
}

#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    const ORIGIN: &str = "gamut-caller";

    /// The refusal `result` carries, as `(kind, message)`, checking it names the caller.
    fn refusal(result: Result<()>) -> (ErrorKind, Option<&'static str>) {
        let error = result.unwrap_err();
        assert_eq!(error.origin(), Some(ORIGIN));
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
    fn options_default_to_the_default_limits_and_interpolated_chroma() {
        let options = DecodeOptions::default();
        assert_eq!(options.limits, DecodeLimits::default());
        assert_eq!(options.chroma_upsampling, ChromaUpsampling::Interpolated);
        let options = options
            .with_limits(DecodeLimits::new(1, 1))
            .with_chroma_upsampling(ChromaUpsampling::Nearest);
        assert_eq!(options.limits, DecodeLimits::new(1, 1));
        assert_eq!(options.chroma_upsampling, ChromaUpsampling::Nearest);
    }

    #[test]
    fn charge_counts_up_to_the_cap_and_refuses_at_it() {
        assert_eq!(charge(0, 2, DECODED_ITEM_LIMIT, ORIGIN).unwrap(), 1);
        assert_eq!(charge(1, 2, DECODED_ITEM_LIMIT, ORIGIN).unwrap(), 2);
        assert_eq!(
            refusal(charge(2, 2, DECODED_ITEM_LIMIT, ORIGIN).map(drop)),
            (ErrorKind::Unsupported, Some(DECODED_ITEM_LIMIT))
        );
        // A zero cap admits nothing, and a saturated counter cannot wrap past the cap.
        assert!(charge(0, 0, DECODED_ITEM_LIMIT, ORIGIN).is_err());
        assert!(charge(u32::MAX, u32::MAX, DECODED_ITEM_LIMIT, ORIGIN).is_err());
    }

    #[test]
    fn check_accepts_an_image_exactly_at_both_caps() {
        let limits = DecodeLimits::new(10, 50);
        assert!(limits.check(10, 5, ORIGIN).is_ok());
        assert!(limits.check(5, 10, ORIGIN).is_ok());
    }

    #[test]
    fn check_refuses_a_width_or_a_height_one_past_the_dimension_cap() {
        let limits = DecodeLimits::new(10, 1000);
        let past = (ErrorKind::Unsupported, Some(DIMENSION_LIMIT));
        assert_eq!(refusal(limits.check(11, 1, ORIGIN)), past);
        assert_eq!(refusal(limits.check(1, 11, ORIGIN)), past);
    }

    #[test]
    fn check_refuses_a_pixel_count_past_the_cap_with_both_dimensions_inside() {
        // 8 x 7 = 56 > 50, while each side is within 10, and 8 + 7 and 8 / 7 are not past 50.
        let limits = DecodeLimits::new(10, 50);
        assert_eq!(
            refusal(limits.check(8, 7, ORIGIN)),
            (ErrorKind::Unsupported, Some(PIXEL_LIMIT))
        );
    }

    #[test]
    fn check_counts_pixels_without_overflowing_u32() {
        // 65536 x 65536 = 2^32: a u32 product would wrap to 0 and pass.
        let limits = DecodeLimits::new(u32::MAX, u64::from(u32::MAX));
        assert_eq!(
            refusal(limits.check(65536, 65536, ORIGIN)),
            (ErrorKind::Unsupported, Some(PIXEL_LIMIT))
        );
    }

    #[test]
    fn try_filled_returns_the_requested_buffer() {
        assert_eq!(try_filled(3, 7u16, ORIGIN).unwrap(), vec![7, 7, 7]);
    }

    #[test]
    fn try_filled_refuses_an_impossible_allocation_instead_of_aborting() {
        // usize::MAX u64s is past isize::MAX bytes, so the reservation fails without allocating.
        let error = try_filled(usize::MAX, 0u64, ORIGIN).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(error.origin(), Some(ORIGIN));
    }
}
