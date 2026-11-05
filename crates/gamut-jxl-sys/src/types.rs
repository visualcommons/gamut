//! Shared `#[repr(C)]` types transcribed field-for-field from the libjxl v0.12.0 public headers
//! (`jxl/types.h`, `jxl/codestream_header.h`, `jxl/color_encoding.h`, `jxl/memory_manager.h`).
//!
//! C `enum`s are `int`, so each is modelled here as a `#[repr(transparent)]` newtype over
//! [`core::ffi::c_int`] carrying associated constants, not as a Rust `enum`. That matches the C ABI
//! byte-for-byte for struct fields *and* is safe when libjxl hands back a value outside the known
//! set (a Rust `enum` would be undefined behaviour on an unlisted discriminant).

use core::ffi::c_int;

/// A portable `bool` replacement (`JXL_BOOL`, `jxl/types.h`). Actually a C `int`; its only defined
/// values are [`JxlBool::TRUE`] and [`JxlBool::FALSE`].
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlBool(pub c_int);

impl JxlBool {
    /// Portable `true` (`JXL_TRUE`, value `1`).
    pub const TRUE: Self = Self(1);
    /// Portable `false` (`JXL_FALSE`, value `0`).
    pub const FALSE: Self = Self(0);
}

/// Data type for the sample values per channel per pixel (`JxlDataType`, `jxl/types.h`).
///
/// Note the non-contiguous C discriminants (2, 3, 5) — the gaps are `JXL_TYPE_*` values removed in
/// earlier libjxl releases and are preserved here for ABI fidelity.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlDataType(pub c_int);

impl JxlDataType {
    /// 32-bit single-precision floating point, nominal range 0.0-1.0 (`JXL_TYPE_FLOAT`).
    pub const FLOAT: Self = Self(0);
    /// Unsigned 8-bit integer (`JXL_TYPE_UINT8`).
    pub const UINT8: Self = Self(2);
    /// Unsigned 16-bit integer (`JXL_TYPE_UINT16`).
    pub const UINT16: Self = Self(3);
    /// 16-bit IEEE 754 half-precision floating point (`JXL_TYPE_FLOAT16`).
    pub const FLOAT16: Self = Self(5);
}

/// Ordering of multi-byte data (`JxlEndianness`, `jxl/types.h`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlEndianness(pub c_int);

impl JxlEndianness {
    /// Use the system endianness without forcing either (`JXL_NATIVE_ENDIAN`).
    pub const NATIVE: Self = Self(0);
    /// Force little endian (`JXL_LITTLE_ENDIAN`).
    pub const LITTLE: Self = Self(1);
    /// Force big endian (`JXL_BIG_ENDIAN`).
    pub const BIG: Self = Self(2);
}

/// Interpretation of a [`JxlBitDepth`] (`JxlBitDepthType`, `jxl/types.h`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlBitDepthType(pub c_int);

impl JxlBitDepthType {
    /// Infer the bit depth from the pixel format's data type (`JXL_BIT_DEPTH_FROM_PIXEL_FORMAT`):
    /// a `UINT16` buffer is treated as full-range 16-bit.
    pub const FROM_PIXEL_FORMAT: Self = Self(0);
    /// Use the codestream header's declared bit depth (`JXL_BIT_DEPTH_FROM_CODESTREAM`): a
    /// `UINT16` buffer for an N-bit image carries values in `0 ..= 2^N - 1`.
    pub const FROM_CODESTREAM: Self = Self(1);
    /// Use the explicit `bits_per_sample`/`exponent_bits_per_sample` fields
    /// (`JXL_BIT_DEPTH_CUSTOM`).
    pub const CUSTOM: Self = Self(2);
}

/// The interpretation of integer pixel buffers handed to the encoder or produced by the decoder
/// (`JxlBitDepth`, `jxl/types.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JxlBitDepth {
    /// How the bit depth is determined; the fields below apply only to
    /// [`JxlBitDepthType::CUSTOM`].
    pub r#type: JxlBitDepthType,
    /// Custom bits per sample (`JXL_BIT_DEPTH_CUSTOM` only).
    pub bits_per_sample: u32,
    /// Custom float exponent bits per sample (`JXL_BIT_DEPTH_CUSTOM` only; 0 for integers).
    pub exponent_bits_per_sample: u32,
}

/// Description of an interleaved pixel buffer for encode input / decode output (`JxlPixelFormat`,
/// `jxl/types.h`). Pixels are laid out row by row, left to right, top to bottom.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JxlPixelFormat {
    /// Number of channels per pixel: 1 = grayscale/single, 2 = gray+alpha, 3 = RGB, 4 = RGBA.
    pub num_channels: u32,
    /// Data type of each channel.
    pub data_type: JxlDataType,
    /// Byte order for multi-byte data types (applies to `UINT16` and `FLOAT`).
    pub endianness: JxlEndianness,
    /// Scanline alignment in bytes, or 0/1 for no alignment.
    pub align: usize,
}

/// Image orientation metadata; values 1..8 match the EXIF definitions (`JxlOrientation`,
/// `jxl/codestream_header.h`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlOrientation(pub c_int);

impl JxlOrientation {
    /// No transform (`JXL_ORIENT_IDENTITY`).
    pub const IDENTITY: Self = Self(1);
    /// Flip horizontally (`JXL_ORIENT_FLIP_HORIZONTAL`).
    pub const FLIP_HORIZONTAL: Self = Self(2);
    /// Rotate 180 degrees (`JXL_ORIENT_ROTATE_180`).
    pub const ROTATE_180: Self = Self(3);
    /// Flip vertically (`JXL_ORIENT_FLIP_VERTICAL`).
    pub const FLIP_VERTICAL: Self = Self(4);
    /// Transpose (`JXL_ORIENT_TRANSPOSE`).
    pub const TRANSPOSE: Self = Self(5);
    /// Rotate 90 degrees clockwise (`JXL_ORIENT_ROTATE_90_CW`).
    pub const ROTATE_90_CW: Self = Self(6);
    /// Anti-transpose (`JXL_ORIENT_ANTI_TRANSPOSE`).
    pub const ANTI_TRANSPOSE: Self = Self(7);
    /// Rotate 90 degrees counter-clockwise (`JXL_ORIENT_ROTATE_90_CCW`).
    pub const ROTATE_90_CCW: Self = Self(8);
}

/// The codestream preview header (`JxlPreviewHeader`, `jxl/codestream_header.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JxlPreviewHeader {
    /// Preview width in pixels.
    pub xsize: u32,
    /// Preview height in pixels.
    pub ysize: u32,
}

/// The codestream animation header (`JxlAnimationHeader`, `jxl/codestream_header.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JxlAnimationHeader {
    /// Numerator of ticks per second of a single animation frame time unit.
    pub tps_numerator: u32,
    /// Denominator of ticks per second of a single animation frame time unit.
    pub tps_denominator: u32,
    /// Number of animation loops, or 0 to repeat infinitely.
    pub num_loops: u32,
    /// Whether animation time codes are present at animation frames in the codestream.
    pub have_timecodes: JxlBool,
}

/// Basic image information, available from the file signature and first part of the codestream
/// header (`JxlBasicInfo`, `jxl/codestream_header.h`).
///
/// Transcribed field-for-field, including the trailing 100-byte forwards-compatibility padding.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JxlBasicInfo {
    /// Whether the codestream is embedded in the container format.
    pub have_container: JxlBool,
    /// Width of the image in pixels, before applying orientation.
    pub xsize: u32,
    /// Height of the image in pixels, before applying orientation.
    pub ysize: u32,
    /// Original image color channel bit depth.
    pub bits_per_sample: u32,
    /// Original image color channel floating point exponent bits, or 0 for unsigned integer.
    pub exponent_bits_per_sample: u32,
    /// Upper bound on the intensity level present in the image, in nits.
    pub intensity_target: f32,
    /// Lower bound on the intensity level present in the image, in nits.
    pub min_nits: f32,
    /// Whether [`Self::linear_below`] is relative to the maximum display brightness.
    pub relative_to_max_display: JxlBool,
    /// Brightness below which tone mapping leaves pixels unchanged (see `relative_to_max_display`).
    pub linear_below: f32,
    /// Whether codestream data is encoded in the attached original color profile.
    pub uses_original_profile: JxlBool,
    /// Indicates a preview image exists near the beginning of the codestream.
    pub have_preview: JxlBool,
    /// Indicates animation frames exist in the codestream.
    pub have_animation: JxlBool,
    /// Image orientation, values 1-8 matching EXIF.
    pub orientation: JxlOrientation,
    /// Number of encoded color channels: 1 (grayscale) or 3 (color). Excludes alpha/extra channels.
    pub num_color_channels: u32,
    /// Number of additional image channels, including the main alpha channel.
    pub num_extra_channels: u32,
    /// Bit depth of the encoded alpha channel, or 0 if there is no alpha channel.
    pub alpha_bits: u32,
    /// Alpha channel floating point exponent bits, or 0 if unsigned.
    pub alpha_exponent_bits: u32,
    /// Whether the alpha channel is premultiplied.
    pub alpha_premultiplied: JxlBool,
    /// Dimensions of the encoded preview image; only used if [`Self::have_preview`] is true.
    pub preview: JxlPreviewHeader,
    /// Global animation properties; only used if [`Self::have_animation`] is true.
    pub animation: JxlAnimationHeader,
    /// Intrinsic (recommended display) width of the image.
    pub intrinsic_xsize: u32,
    /// Intrinsic (recommended display) height of the image.
    pub intrinsic_ysize: u32,
    /// Padding for forwards-compatibility, in case more fields are exposed in a future version.
    pub padding: [u8; 100],
}

/// Color space of the image data (`JxlColorSpace`, `jxl/color_encoding.h`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlColorSpace(pub c_int);

impl JxlColorSpace {
    /// Tristimulus RGB (`JXL_COLOR_SPACE_RGB`).
    pub const RGB: Self = Self(0);
    /// Luminance-based; the primaries must be ignored (`JXL_COLOR_SPACE_GRAY`).
    pub const GRAY: Self = Self(1);
    /// XYB (opsin) color space (`JXL_COLOR_SPACE_XYB`).
    pub const XYB: Self = Self(2);
    /// None of the other entries describe the color space (`JXL_COLOR_SPACE_UNKNOWN`).
    pub const UNKNOWN: Self = Self(3);
}

/// Built-in white points for color encoding (`JxlWhitePoint`, `jxl/color_encoding.h`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlWhitePoint(pub c_int);

impl JxlWhitePoint {
    /// CIE Standard Illuminant D65 (`JXL_WHITE_POINT_D65`).
    pub const D65: Self = Self(1);
    /// White point read from the numerical `white_point_xy` field (`JXL_WHITE_POINT_CUSTOM`).
    pub const CUSTOM: Self = Self(2);
    /// CIE Standard Illuminant E, equal-energy (`JXL_WHITE_POINT_E`).
    pub const E: Self = Self(10);
    /// DCI-P3 from SMPTE RP 431-2 (`JXL_WHITE_POINT_DCI`).
    pub const DCI: Self = Self(11);
}

/// Built-in RGB primaries for color encoding (`JxlPrimaries`, `jxl/color_encoding.h`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlPrimaries(pub c_int);

impl JxlPrimaries {
    /// sRGB primaries (`JXL_PRIMARIES_SRGB`).
    pub const SRGB: Self = Self(1);
    /// Primaries read from the numerical `primaries_*_xy` fields (`JXL_PRIMARIES_CUSTOM`).
    pub const CUSTOM: Self = Self(2);
    /// Rec. ITU-R BT.2100-1 primaries (`JXL_PRIMARIES_2100`).
    pub const BT2100: Self = Self(9);
    /// SMPTE RP 431-2 (P3) primaries (`JXL_PRIMARIES_P3`).
    pub const P3: Self = Self(11);
}

/// Built-in transfer functions for color encoding (`JxlTransferFunction`, `jxl/color_encoding.h`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlTransferFunction(pub c_int);

impl JxlTransferFunction {
    /// ITU-R BT.709-6 (`JXL_TRANSFER_FUNCTION_709`).
    pub const BT709: Self = Self(1);
    /// None of the other entries describe the transfer function (`JXL_TRANSFER_FUNCTION_UNKNOWN`).
    pub const UNKNOWN: Self = Self(2);
    /// Linear, gamma exponent 1 (`JXL_TRANSFER_FUNCTION_LINEAR`).
    pub const LINEAR: Self = Self(8);
    /// IEC 61966-2-1 sRGB (`JXL_TRANSFER_FUNCTION_SRGB`).
    pub const SRGB: Self = Self(13);
    /// SMPTE ST 2084 (PQ) (`JXL_TRANSFER_FUNCTION_PQ`).
    pub const PQ: Self = Self(16);
    /// SMPTE ST 428-1 (`JXL_TRANSFER_FUNCTION_DCI`).
    pub const DCI: Self = Self(17);
    /// Rec. ITU-R BT.2100-1 (HLG) (`JXL_TRANSFER_FUNCTION_HLG`).
    pub const HLG: Self = Self(18);
    /// Power law given by the numerical `gamma` field (`JXL_TRANSFER_FUNCTION_GAMMA`).
    pub const GAMMA: Self = Self(65535);
}

/// Rendering intent for color encoding, per ISO 15076-1:2010 (`JxlRenderingIntent`,
/// `jxl/color_encoding.h`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlRenderingIntent(pub c_int);

impl JxlRenderingIntent {
    /// Perceptual, vendor-specific (`JXL_RENDERING_INTENT_PERCEPTUAL`).
    pub const PERCEPTUAL: Self = Self(0);
    /// Media-relative colorimetric (`JXL_RENDERING_INTENT_RELATIVE`).
    pub const RELATIVE: Self = Self(1);
    /// Saturation, vendor-specific (`JXL_RENDERING_INTENT_SATURATION`).
    pub const SATURATION: Self = Self(2);
    /// ICC-absolute colorimetric (`JXL_RENDERING_INTENT_ABSOLUTE`).
    pub const ABSOLUTE: Self = Self(3);
}

/// Color encoding of the image as structured information (`JxlColorEncoding`,
/// `jxl/color_encoding.h`). All CIE units are for the standard 1931 2-degree observer.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JxlColorEncoding {
    /// Color space of the image data.
    pub color_space: JxlColorSpace,
    /// Built-in white point; if [`JxlWhitePoint::CUSTOM`], use `white_point_xy`.
    pub white_point: JxlWhitePoint,
    /// Numerical white point in CIE xy space.
    pub white_point_xy: [f64; 2],
    /// Built-in RGB primaries; if [`JxlPrimaries::CUSTOM`], use the numerical primaries fields.
    pub primaries: JxlPrimaries,
    /// Numerical red primary in CIE xy space.
    pub primaries_red_xy: [f64; 2],
    /// Numerical green primary in CIE xy space.
    pub primaries_green_xy: [f64; 2],
    /// Numerical blue primary in CIE xy space.
    pub primaries_blue_xy: [f64; 2],
    /// Transfer function if `gamma` is unused.
    pub transfer_function: JxlTransferFunction,
    /// Gamma value used when `transfer_function` is [`JxlTransferFunction::GAMMA`].
    pub gamma: f64,
    /// Rendering intent defined for the color profile.
    pub rendering_intent: JxlRenderingIntent,
}

/// Memory manager struct (`JxlMemoryManager`, `jxl/memory_manager.h`).
///
/// Passed (as `*const`) to [`crate::encode::JxlEncoderCreate`] and
/// the `oracle` feature's `JxlDecoderCreate`. This crate only ever passes a null pointer (the default
/// malloc/free allocator), so the allocator callback fields are typed as opaque
/// [`core::ffi::c_void`] pointers rather than transcribed function-pointer types.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JxlMemoryManager {
    /// Opaque pointer passed as the first argument to `alloc`/`free`.
    pub opaque: *mut core::ffi::c_void,
    /// Allocation function pointer (`jpegxl_alloc_func`), or null to use the default allocator.
    pub alloc: *mut core::ffi::c_void,
    /// Free function pointer (`jpegxl_free_func`), or null to use the default allocator.
    pub free: *mut core::ffi::c_void,
}
