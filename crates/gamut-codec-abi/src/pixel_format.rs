//! The [`ImageDesc::pixel_format`](crate::ImageDesc::pixel_format) tag namespace.
//!
//! A tag says how the bytes behind an [`ImageDesc`](crate::ImageDesc)'s planes are laid out. The
//! `u32` space is split into three disjoint ranges, so a tag is never ambiguous about which one it
//! belongs to:
//!
//! - **Channel layouts, `0..=0xFFFF`.** The channels and sample width of a
//!   `gamut_core::PixelFormat`, whose discriminant the value is ([`GRAY8`] … [`GRAY_ALPHA16`]);
//!   16-bit samples are native-endian `u16`s. With one plane the channels are interleaved in the
//!   format's own order; with one plane per channel, the plane order is the seam's agreement for
//!   its `codec_id`. A format crate that holds a `PixelFormat` may write `format as u32` and lands
//!   on the same constant.
//! - **FourCC layouts.** A planar or packed layout named by its conventional FourCC, built with
//!   [`codec_id`] (big-endian): [`Y800`], [`I420`], [`I422`], [`I444`],
//!   [`BGRA`]. Every printable-ASCII FourCC is at least `0x2020_2020`, so none falls in the range
//!   above. The tag names the plane arrangement only; the sample storage width (bytes, or
//!   native-endian `u16`s) is the seam's own agreement for its `codec_id`, documented by the format
//!   crate that defines it, with [`ImageDesc::bit_depth`](crate::ImageDesc::bit_depth) carrying the
//!   coded precision.
//! - **[`OPAQUE_BYTES`], `u32::MAX`.** Not pixels: one plane of bytes whose meaning the stream's
//!   `codec_id` defines (e.g. a PNG IDAT filtered-scanline stream). No FourCC of printable ASCII is
//!   `0xFFFF_FFFF`.
//!
//! The constants below are the whole namespace; a tag outside it is a host/backend private
//! agreement and carries no meaning to a third party.

use crate::codec_id;

/// 8-bit grey (`gamut_core::PixelFormat::Gray8`).
pub const GRAY8: u32 = 0;
/// 1-bit bilevel samples, one per byte (`gamut_core::PixelFormat::Bilevel`).
pub const BILEVEL: u32 = 1;
/// 8-bit palette indices (`gamut_core::PixelFormat::Indexed8`).
pub const INDEXED8: u32 = 2;
/// 8-bit R, G, B (`gamut_core::PixelFormat::Rgb8`).
pub const RGB8: u32 = 3;
/// 8-bit R, G, B, A (`gamut_core::PixelFormat::Rgba8`).
pub const RGBA8: u32 = 4;
/// 8-bit C, M, Y, K (`gamut_core::PixelFormat::Cmyk8`).
pub const CMYK8: u32 = 5;
/// 8-bit grey, alpha (`gamut_core::PixelFormat::GrayAlpha8`).
pub const GRAY_ALPHA8: u32 = 6;
/// 16-bit grey (`gamut_core::PixelFormat::Gray16`).
pub const GRAY16: u32 = 7;
/// 16-bit R, G, B (`gamut_core::PixelFormat::Rgb16`).
pub const RGB16: u32 = 8;
/// 16-bit R, G, B, A (`gamut_core::PixelFormat::Rgba16`).
pub const RGBA16: u32 = 9;
/// 16-bit grey, alpha (`gamut_core::PixelFormat::GrayAlpha16`).
pub const GRAY_ALPHA16: u32 = 10;

/// Monochrome planar: one luma plane (`Y800`).
pub const Y800: u32 = codec_id(*b"Y800");
/// Planar Y, Cb, Cr with 4:2:0 chroma subsampling: three planes (`I420`).
pub const I420: u32 = codec_id(*b"I420");
/// Planar Y, Cb, Cr with 4:2:2 chroma subsampling: three planes (`I422`).
pub const I422: u32 = codec_id(*b"I422");
/// Planar Y, Cb, Cr without chroma subsampling: three planes (`I444`).
pub const I444: u32 = codec_id(*b"I444");
/// One plane of packed 8-bit samples in B, G, R, A byte order (`BGRA`).
pub const BGRA: u32 = codec_id(*b"BGRA");

/// Not pixels: one plane of bytes whose meaning the stream's `codec_id` defines.
pub const OPAQUE_BYTES: u32 = u32::MAX;
