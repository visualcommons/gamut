//! Encoder API subset of libjxl v0.12.0 (`jxl/encode.h`), the reference JPEG XL encoder that backs
//! [`gamut-jxl`](https://crates.io/crates/gamut-jxl).
//!
//! Declarations only. The function names keep libjxl's exact spelling, hence the module-level
//! `non_snake_case` allow.
#![allow(non_snake_case)]

use core::ffi::c_void;

use crate::types::{
    JxlBasicInfo, JxlBitDepth, JxlBool, JxlColorEncoding, JxlMemoryManager, JxlPixelFormat,
};

/// Opaque encoder instance (`JxlEncoder`). Created by
/// [`JxlEncoderCreate`] and destroyed by [`JxlEncoderDestroy`].
#[repr(C)]
pub struct JxlEncoder {
    _private: [u8; 0],
}

/// Opaque per-frame settings and metadata (`JxlEncoderFrameSettings`). Created by
/// [`JxlEncoderFrameSettingsCreate`] and owned by the parent encoder (freed when the encoder is
/// destroyed).
#[repr(C)]
pub struct JxlEncoderFrameSettings {
    _private: [u8; 0],
}

/// Return value for most encoder functions (`JxlEncoderStatus`, `jxl/encode.h`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlEncoderStatus(pub core::ffi::c_int);

impl JxlEncoderStatus {
    /// Call finished successfully, or encoding is finished (`JXL_ENC_SUCCESS`).
    pub const SUCCESS: Self = Self(0);
    /// An error occurred, e.g. out of memory (`JXL_ENC_ERROR`).
    pub const ERROR: Self = Self(1);
    /// The encoder needs more output buffer to continue (`JXL_ENC_NEED_MORE_OUTPUT`).
    pub const NEED_MORE_OUTPUT: Self = Self(2);
}

/// Detailed error condition, retrieved with [`JxlEncoderGetError`] after an
/// [`JxlEncoderStatus::ERROR`] (`JxlEncoderError`, `jxl/encode.h`). API-usage errors have the `0x80`
/// bit set.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlEncoderError(pub core::ffi::c_int);

impl JxlEncoderError {
    /// No error (`JXL_ENC_ERR_OK`).
    pub const OK: Self = Self(0);
    /// Generic encoder error due to unspecified cause (`JXL_ENC_ERR_GENERIC`).
    pub const GENERIC: Self = Self(1);
    /// Out of memory (`JXL_ENC_ERR_OOM`).
    pub const OOM: Self = Self(2);
    /// JPEG bitstream reconstruction data could not be represented (`JXL_ENC_ERR_JBRD`).
    pub const JBRD: Self = Self(3);
    /// Input is invalid, e.g. a corrupt JPEG file or ICC profile (`JXL_ENC_ERR_BAD_INPUT`).
    pub const BAD_INPUT: Self = Self(0x04);
    /// The encoder does not (yet) support this (`JXL_ENC_ERR_NOT_SUPPORTED`).
    pub const NOT_SUPPORTED: Self = Self(0x80);
    /// The encoder API was used incorrectly (`JXL_ENC_ERR_API_USAGE`).
    pub const API_USAGE: Self = Self(0x81);
}

/// Identifier of a per-frame encoder option for [`JxlEncoderFrameSettingsSetOption`]
/// (`JxlEncoderFrameSettingId`, `jxl/encode.h`).
///
/// Only the subset used by the [`gamut-jxl`](https://crates.io/crates/gamut-jxl) encoder is declared
/// here; additional option ids can be added as the wrapper grows. The underlying C enum is `int`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JxlEncoderFrameSettingId(pub core::ffi::c_int);

impl JxlEncoderFrameSettingId {
    /// Encoder effort/speed level, 1 (lightning) to 10 (glacier); default 7 (squirrel)
    /// (`JXL_ENC_FRAME_SETTING_EFFORT`).
    pub const EFFORT: Self = Self(0);

    /// Decoding-speed tier, `0` (slowest to decode, best quality/density; the default) to `4`
    /// (fastest to decode) (`JXL_ENC_FRAME_SETTING_DECODING_SPEED`).
    pub const DECODING_SPEED: Self = Self(1);

    /// Coding-tool selection: `-1` lets the encoder choose (the default), `0` enforces VarDCT, `1`
    /// enforces modular (`JXL_ENC_FRAME_SETTING_MODULAR`).
    pub const MODULAR: Self = Self(11);

    /// Progressive (responsive) encoding for modular mode: `-1` encoder default, `0` off, `1` on
    /// (`JXL_ENC_FRAME_SETTING_RESPONSIVE`).
    pub const RESPONSIVE: Self = Self(16);

    /// Spectral progression of the VarDCT AC coefficients: `-1` encoder default, `0` off, `1` on
    /// (`JXL_ENC_FRAME_SETTING_PROGRESSIVE_AC`).
    pub const PROGRESSIVE_AC: Self = Self(17);

    /// Quantization (least-significant-bit) progression of the VarDCT AC coefficients: `-1`
    /// encoder default, `0` off, `1` on (`JXL_ENC_FRAME_SETTING_QPROGRESSIVE_AC`).
    pub const QPROGRESSIVE_AC: Self = Self(18);

    /// Lower-resolution DC passes for VarDCT: `-1` encoder default, `0` off, `1` one extra 64x64
    /// pass, `2` a 512x512 and a 64x64 pass (`JXL_ENC_FRAME_SETTING_PROGRESSIVE_DC`).
    pub const PROGRESSIVE_DC: Self = Self(19);
}

unsafe extern "C" {
    /// Returns the encoder library version as `MAJOR*1000000 + MINOR*1000 + PATCH`
    /// (`JxlEncoderVersion`).
    ///
    /// # Safety
    ///
    /// Always safe to call: it takes no arguments and dereferences no pointers.
    pub fn JxlEncoderVersion() -> u32;

    /// Creates and initializes a [`JxlEncoder`], or returns null on failure (`JxlEncoderCreate`).
    ///
    /// # Safety
    ///
    /// `memory_manager` must be null or point to a valid, correctly initialized [`JxlMemoryManager`]
    /// that outlives the returned encoder. The returned pointer must be freed with
    /// [`JxlEncoderDestroy`].
    pub fn JxlEncoderCreate(memory_manager: *const JxlMemoryManager) -> *mut JxlEncoder;

    /// Re-initializes an encoder for reuse, keeping its memory manager (`JxlEncoderReset`).
    ///
    /// # Safety
    ///
    /// `enc` must be a valid pointer returned by [`JxlEncoderCreate`] and not yet destroyed.
    pub fn JxlEncoderReset(enc: *mut JxlEncoder);

    /// Deinitializes and frees an encoder (`JxlEncoderDestroy`).
    ///
    /// # Safety
    ///
    /// `enc` must be null or a valid pointer from [`JxlEncoderCreate`] that has not already been
    /// destroyed. After this call the pointer (and any derived frame-settings pointers) are dangling.
    pub fn JxlEncoderDestroy(enc: *mut JxlEncoder);

    /// Creates a new set of frame settings, copied from `source` or from defaults if `source` is
    /// null; owned by `enc` (`JxlEncoderFrameSettingsCreate`).
    ///
    /// # Safety
    ///
    /// `enc` must be a valid encoder. `source` must be null or a valid frame-settings pointer created
    /// for the same encoder. The returned pointer is owned by `enc` and must not be freed directly.
    pub fn JxlEncoderFrameSettingsCreate(
        enc: *mut JxlEncoder,
        source: *const JxlEncoderFrameSettings,
    ) -> *mut JxlEncoderFrameSettings;

    /// Sets the global basic image information; must be called before adding frames
    /// (`JxlEncoderSetBasicInfo`).
    ///
    /// # Safety
    ///
    /// `enc` must be a valid encoder and `info` must point to a valid, fully initialized
    /// [`JxlBasicInfo`] (initialize it with [`JxlEncoderInitBasicInfo`] first). The contents are
    /// copied internally.
    pub fn JxlEncoderSetBasicInfo(
        enc: *mut JxlEncoder,
        info: *const JxlBasicInfo,
    ) -> JxlEncoderStatus;

    /// Initializes a [`JxlBasicInfo`] to default values (8-bit RGB, no alpha)
    /// (`JxlEncoderInitBasicInfo`). Must be called before assigning fields, for forwards
    /// compatibility.
    ///
    /// # Safety
    ///
    /// `info` must point to writable, properly aligned storage for a [`JxlBasicInfo`].
    pub fn JxlEncoderInitBasicInfo(info: *mut JxlBasicInfo);

    /// Sets the original color encoding as structured information; must be called after
    /// [`JxlEncoderSetBasicInfo`] (`JxlEncoderSetColorEncoding`).
    ///
    /// # Safety
    ///
    /// `enc` must be a valid encoder and `color` must point to a valid [`JxlColorEncoding`]. The
    /// contents are copied internally.
    pub fn JxlEncoderSetColorEncoding(
        enc: *mut JxlEncoder,
        color: *const JxlColorEncoding,
    ) -> JxlEncoderStatus;

    /// Sets the original color encoding from raw ICC profile bytes; an alternative to
    /// [`JxlEncoderSetColorEncoding`] (`JxlEncoderSetICCProfile`).
    ///
    /// # Safety
    ///
    /// `enc` must be a valid encoder. `icc_profile` must point to at least `size` readable bytes.
    pub fn JxlEncoderSetICCProfile(
        enc: *mut JxlEncoder,
        icc_profile: *const u8,
        size: usize,
    ) -> JxlEncoderStatus;

    /// Sets the target Butteraugli distance for lossy compression (0 = mathematically lossless, 1.0 =
    /// visually lossless; range 0..25) (`JxlEncoderSetFrameDistance`).
    ///
    /// # Safety
    ///
    /// `frame_settings` must be a valid frame-settings pointer created for a live encoder.
    pub fn JxlEncoderSetFrameDistance(
        frame_settings: *mut JxlEncoderFrameSettings,
        distance: f32,
    ) -> JxlEncoderStatus;

    /// Sets how integer pixel buffers added for this frame are interpreted
    /// (`JxlEncoderSetFrameBitDepth`) — e.g. [`JxlBitDepthType::FROM_CODESTREAM`] reads a
    /// `UINT16` buffer as the basic info's declared N-bit code values instead of full-range
    /// 16-bit.
    ///
    /// [`JxlBitDepthType::FROM_CODESTREAM`]: crate::types::JxlBitDepthType::FROM_CODESTREAM
    ///
    /// # Safety
    ///
    /// `frame_settings` must be a valid frame-settings pointer created for a live encoder, and
    /// `bit_depth` must point to a valid [`JxlBitDepth`].
    pub fn JxlEncoderSetFrameBitDepth(
        frame_settings: *mut JxlEncoderFrameSettings,
        bit_depth: *const JxlBitDepth,
    ) -> JxlEncoderStatus;

    /// Enables or disables true lossless mode for a frame (`JxlEncoderSetFrameLossless`).
    ///
    /// # Safety
    ///
    /// `frame_settings` must be a valid frame-settings pointer created for a live encoder.
    pub fn JxlEncoderSetFrameLossless(
        frame_settings: *mut JxlEncoderFrameSettings,
        lossless: JxlBool,
    ) -> JxlEncoderStatus;

    /// Sets an integer-valued per-frame option, such as [`JxlEncoderFrameSettingId::EFFORT`]
    /// (`JxlEncoderFrameSettingsSetOption`).
    ///
    /// # Safety
    ///
    /// `frame_settings` must be a valid frame-settings pointer created for a live encoder. On an
    /// invalid option id or value the settings object is left unchanged and an error is returned.
    pub fn JxlEncoderFrameSettingsSetOption(
        frame_settings: *mut JxlEncoderFrameSettings,
        option: JxlEncoderFrameSettingId,
        value: i64,
    ) -> JxlEncoderStatus;

    /// Sets the pixel buffer for the next frame to encode; requires a prior
    /// [`JxlEncoderSetBasicInfo`] (`JxlEncoderAddImageFrame`).
    ///
    /// # Safety
    ///
    /// `frame_settings` must be valid. `pixel_format` must point to a valid [`JxlPixelFormat`].
    /// `buffer` must point to at least `size` readable bytes matching that format and the basic-info
    /// dimensions. The buffer contents are copied internally.
    pub fn JxlEncoderAddImageFrame(
        frame_settings: *const JxlEncoderFrameSettings,
        pixel_format: *const JxlPixelFormat,
        buffer: *const c_void,
        size: usize,
    ) -> JxlEncoderStatus;

    /// Sets the complete JPEG codestream to transcode for the next frame
    /// (`JxlEncoderAddJPEGFrame`). If [`JxlEncoderSetBasicInfo`]/[`JxlEncoderSetColorEncoding`] have
    /// not been called, they are implied from the JPEG parameters. With
    /// [`JxlEncoderStoreJPEGMetadata`] enabled and a single JPEG frame added, the original JPEG
    /// codestream becomes losslessly reconstructible from the output.
    ///
    /// # Safety
    ///
    /// `frame_settings` must be valid. `buffer` must point to at least `size` readable bytes forming
    /// the JPEG codestream; the contents are copied internally.
    pub fn JxlEncoderAddJPEGFrame(
        frame_settings: *const JxlEncoderFrameSettings,
        buffer: *const u8,
        size: usize,
    ) -> JxlEncoderStatus;

    /// Closes all input to the encoder, signalling that no further frames or boxes will be added
    /// (`JxlEncoderCloseInput`). Must be called before the final [`JxlEncoderProcessOutput`].
    ///
    /// # Safety
    ///
    /// `enc` must be a valid encoder.
    pub fn JxlEncoderCloseInput(enc: *mut JxlEncoder);

    /// Writes available encoded output, advancing `*next_out` and decrementing `*avail_out`
    /// (`JxlEncoderProcessOutput`). Returns [`JxlEncoderStatus::NEED_MORE_OUTPUT`] until finished;
    /// the caller must guarantee `*avail_out >= 32`.
    ///
    /// # Safety
    ///
    /// `enc` must be valid. `next_out` must point to a valid pointer to a writable buffer, and
    /// `avail_out` to the number of bytes available from `*next_out`; both are updated in place.
    pub fn JxlEncoderProcessOutput(
        enc: *mut JxlEncoder,
        next_out: *mut *mut u8,
        avail_out: *mut usize,
    ) -> JxlEncoderStatus;

    /// Returns the detailed [`JxlEncoderError`] behind the last [`JxlEncoderStatus::ERROR`]
    /// (`JxlEncoderGetError`).
    ///
    /// # Safety
    ///
    /// `enc` must be a valid encoder.
    pub fn JxlEncoderGetError(enc: *mut JxlEncoder) -> JxlEncoderError;

    /// Forces (or not) the JPEG XL container format for the output; must be set before encoding
    /// starts (`JxlEncoderUseContainer`).
    ///
    /// # Safety
    ///
    /// `enc` must be a valid encoder and no output must have been produced yet.
    pub fn JxlEncoderUseContainer(enc: *mut JxlEncoder, use_container: JxlBool)
    -> JxlEncoderStatus;

    /// Configures the encoder to store JPEG reconstruction metadata (the `jbrd` box) in the
    /// container, making the added JPEG frame losslessly reconstructible
    /// (`JxlEncoderStoreJPEGMetadata`). Must be set before encoding starts; implies container
    /// output.
    ///
    /// # Safety
    ///
    /// `enc` must be a valid encoder and no output must have been produced yet.
    pub fn JxlEncoderStoreJPEGMetadata(
        enc: *mut JxlEncoder,
        store_jpeg_metadata: JxlBool,
    ) -> JxlEncoderStatus;

    /// Declares that metadata boxes will be added with [`JxlEncoderAddBox`]
    /// (`JxlEncoderUseBoxes`). Must be called before the first [`JxlEncoderProcessOutput`]; forces
    /// container output.
    ///
    /// # Safety
    ///
    /// `enc` must be a valid encoder and no output must have been produced yet.
    pub fn JxlEncoderUseBoxes(enc: *mut JxlEncoder) -> JxlEncoderStatus;

    /// Adds an ISO BMFF metadata box (e.g. `"Exif"`, `"xml "`) to the container
    /// (`JxlEncoderAddBox`). Requires a prior [`JxlEncoderUseBoxes`]. With `compress_box` set the
    /// box is stored Brotli-compressed as a `brob` box.
    ///
    /// In C the `JxlBoxType` parameter is a `char[4]` array, which decays to a `char` pointer at
    /// the ABI level; it is declared here as `*const c_char` pointing to exactly 4 bytes.
    ///
    /// # Safety
    ///
    /// `enc` must be a valid encoder. `box_type` must point to at least 4 readable bytes (the box
    /// type, not NUL-terminated). `contents` must point to at least `size` readable bytes; the
    /// contents are copied internally.
    pub fn JxlEncoderAddBox(
        enc: *mut JxlEncoder,
        box_type: *const core::ffi::c_char,
        contents: *const u8,
        size: usize,
        compress_box: JxlBool,
    ) -> JxlEncoderStatus;

    /// Fills a [`JxlColorEncoding`] with the sRGB profile, gray or color (`JxlColorEncodingSetToSRGB`).
    ///
    /// # Safety
    ///
    /// `color_encoding` must point to writable, properly aligned storage for a [`JxlColorEncoding`].
    pub fn JxlColorEncodingSetToSRGB(color_encoding: *mut JxlColorEncoding, is_gray: JxlBool);

    /// Fills a [`JxlColorEncoding`] with the linear sRGB profile, gray or color
    /// (`JxlColorEncodingSetToLinearSRGB`).
    ///
    /// # Safety
    ///
    /// `color_encoding` must point to writable, properly aligned storage for a [`JxlColorEncoding`].
    pub fn JxlColorEncodingSetToLinearSRGB(color_encoding: *mut JxlColorEncoding, is_gray: JxlBool);
}
