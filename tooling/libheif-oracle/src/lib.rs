//! Dev-only differential oracle around a vendored, statically-linked **libheif** (HEVC decode via
//! **libde265**, HEVC encode via **kvazaar**), plus a direct **libde265** decode path.
//!
//! gamut-heic is a decode-only HEIF/HEIC container crate: it parses the ISO/IEC 23008-12 box tree
//! and demuxes the HEVC image items. Its conformance is checked differentially against the de-facto
//! reference reader (libheif). This crate exposes four safe entry points the tests build on:
//!
//! * [`decode_primary_rgba`] — libheif decodes the primary image to interleaved 8-bit RGBA. The
//!   pixel-conformance oracle.
//! * [`introspect`] — a small typed [`OracleStructure`] describing the container (primary item id,
//!   all items and their 4cc types, per-image dimensions / alpha / thumbnails, Exif/XMP blocks).
//! * [`decode_hevc_intra`] — feeds raw HEVC NAL units (config + picture, **no** start codes)
//!   straight to libde265, returning the reconstructed YUV planes. This bypasses libheif entirely,
//!   so it is what gamut-heic plugs in behind its pluggable-decoder trait.
//! * [`encode_rgba_to_heic`] — libheif + kvazaar encode a real HEIC (optionally with alpha, a
//!   thumbnail, Exif/XMP, and an orientation), used to generate fixtures at test time without
//!   committing any binary files.
//!
//! All `unsafe` FFI is confined to this module. The C libraries are built from the
//! `third_party/{libheif,libde265,kvazaar}` git submodules by `build.rs`.
//!
//! Licensing: libde265 and libheif are LGPL-3.0+; kvazaar is BSD-3-Clause. Acceptable here because
//! this crate is dev-only, excluded from the gamut workspace, and never linked into a shipped crate.

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]

use std::ffi::{CStr, c_void};
use std::os::raw::c_int;
use std::ptr;

mod sys {
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

// ============================================================================================
//   Shared error handling
// ============================================================================================

/// Maps a libheif `heif_error` (returned by value from every fallible libheif call) to a `Result`,
/// carrying libheif's own message string on failure.
fn heif_check(err: sys::heif_error) -> Result<(), String> {
    if err.code == sys::heif_error_Ok {
        return Ok(());
    }
    let msg = if err.message.is_null() {
        String::new()
    } else {
        // SAFETY: libheif guarantees `message` is a valid NUL-terminated C string when non-null.
        unsafe { CStr::from_ptr(err.message) }
            .to_string_lossy()
            .into_owned()
    };
    Err(format!(
        "libheif error (code {}, subcode {}): {msg}",
        err.code as i32, err.subcode as i32
    ))
}

// ============================================================================================
//   (a) Primary-image pixel decode
// ============================================================================================

/// Decodes the **primary** image of a HEIC/HEIF file to interleaved 8-bit RGBA.
///
/// Returns `(width, height, rgba)` where `rgba` is tightly packed (`width * height * 4` bytes,
/// no row padding), row-major, `R,G,B,A` per pixel. Images without an alpha channel decode with
/// `A = 255`.
///
/// # Errors
///
/// Returns libheif's message if the bytes cannot be parsed or the primary image cannot be decoded.
pub fn decode_primary_rgba(heic: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    // SAFETY: the context/handle/image handles below are released on every return path; the input
    // slice outlives the (copying) read call.
    unsafe {
        let ctx = sys::heif_context_alloc();
        if ctx.is_null() {
            return Err("heif_context_alloc returned null".into());
        }
        let result = (|| {
            heif_check(sys::heif_context_read_from_memory(
                ctx,
                heic.as_ptr().cast::<c_void>(),
                heic.len(),
                ptr::null(),
            ))?;
            let mut handle: *mut sys::heif_image_handle = ptr::null_mut();
            heif_check(sys::heif_context_get_primary_image_handle(ctx, &mut handle))?;
            let out = decode_handle_rgba(handle);
            sys::heif_image_handle_release(handle);
            out
        })();
        sys::heif_context_free(ctx);
        result
    }
}

/// Decodes one image handle to tightly packed interleaved RGBA.
unsafe fn decode_handle_rgba(
    handle: *const sys::heif_image_handle,
) -> Result<(u32, u32, Vec<u8>), String> {
    // SAFETY: `handle` is a live libheif image handle; `img` is released before returning.
    unsafe {
        let mut img: *mut sys::heif_image = ptr::null_mut();
        heif_check(sys::heif_decode_image(
            handle,
            &mut img,
            sys::heif_colorspace_RGB,
            sys::heif_chroma_interleaved_RGBA,
            ptr::null(),
        ))?;

        let w = sys::heif_image_get_width(img, sys::heif_channel_interleaved);
        let h = sys::heif_image_get_height(img, sys::heif_channel_interleaved);
        if w <= 0 || h <= 0 {
            sys::heif_image_release(img);
            return Err(format!("decoded image has non-positive size {w}x{h}"));
        }
        let (w, h) = (w as usize, h as usize);

        let mut stride: c_int = 0;
        let plane =
            sys::heif_image_get_plane_readonly(img, sys::heif_channel_interleaved, &mut stride);
        if plane.is_null() {
            sys::heif_image_release(img);
            return Err("interleaved RGBA plane is null".into());
        }

        let mut rgba = vec![0u8; w * h * 4];
        let stride = stride as usize;
        for row in 0..h {
            let src = plane.add(row * stride);
            let dst = &mut rgba[row * w * 4..row * w * 4 + w * 4];
            // SAFETY: libheif guarantees `stride >= w*4` and `h` rows of valid data.
            dst.copy_from_slice(std::slice::from_raw_parts(src, w * 4));
        }
        sys::heif_image_release(img);
        Ok((w as u32, h as u32, rgba))
    }
}

// ============================================================================================
//   (b) Container structure introspection
// ============================================================================================

/// A single item in the `meta` box, identified by id and its four-character-code type
/// (e.g. `hvc1`, `grid`, `iden`, `Exif`, `mime`).
#[derive(Debug, Clone)]
pub struct OracleItem {
    /// The item's `item_ID`.
    pub id: u32,
    /// The item's four-character type code, as a UTF-8 string.
    pub item_type: String,
}

/// A top-level (coded) image and the structural facts libheif reports about it.
#[derive(Debug, Clone)]
pub struct OracleImage {
    /// The image item's id.
    pub id: u32,
    /// Decoded width in pixels.
    pub width: u32,
    /// Decoded height in pixels.
    pub height: u32,
    /// Whether the image has an associated alpha channel.
    pub has_alpha: bool,
    /// Whether libheif considers this the file's primary image.
    pub is_primary: bool,
    /// The item ids of this image's thumbnails.
    pub thumbnail_ids: Vec<u32>,
}

/// A metadata block (Exif or XMP) attached to an image.
#[derive(Debug, Clone)]
pub struct OracleMetadataBlock {
    /// The metadata item's id.
    pub id: u32,
    /// The item type string (`Exif` for Exif, `mime` for XMP).
    pub item_type: String,
    /// The MIME content type (`application/rdf+xml` for XMP; empty for Exif).
    pub content_type: String,
    /// The raw metadata bytes exactly as stored in the file (for Exif, the first four bytes are
    /// the `exif_tiff_header_offset`).
    pub data: Vec<u8>,
}

/// A small typed view of a HEIF container's structure, decoded by libheif.
#[derive(Debug, Clone)]
pub struct OracleStructure {
    /// The `pitm` primary item id.
    pub primary_item_id: u32,
    /// Every item in the file (id + 4cc type).
    pub items: Vec<OracleItem>,
    /// Every top-level (coded) image with its dimensions, alpha flag and thumbnails.
    pub images: Vec<OracleImage>,
    /// Exif/XMP metadata blocks attached to the primary image.
    pub primary_metadata: Vec<OracleMetadataBlock>,
}

/// Reads the container structure of a HEIC/HEIF file without decoding any pixels beyond what
/// libheif needs to report dimensions.
///
/// # Errors
///
/// Returns libheif's message if the bytes cannot be parsed.
pub fn introspect(heic: &[u8]) -> Result<OracleStructure, String> {
    // SAFETY: context + handles released on every path; input slice outlives the copying read.
    unsafe {
        let ctx = sys::heif_context_alloc();
        if ctx.is_null() {
            return Err("heif_context_alloc returned null".into());
        }
        let result = introspect_inner(ctx, heic);
        sys::heif_context_free(ctx);
        result
    }
}

unsafe fn introspect_inner(
    ctx: *mut sys::heif_context,
    heic: &[u8],
) -> Result<OracleStructure, String> {
    // SAFETY: FFI reads over a live context; every acquired handle is released before return.
    unsafe {
        heif_check(sys::heif_context_read_from_memory(
            ctx,
            heic.as_ptr().cast::<c_void>(),
            heic.len(),
            ptr::null(),
        ))?;

        // Primary item id.
        let mut primary_handle: *mut sys::heif_image_handle = ptr::null_mut();
        heif_check(sys::heif_context_get_primary_image_handle(
            ctx,
            &mut primary_handle,
        ))?;
        let primary_item_id = sys::heif_image_handle_get_item_id(primary_handle);
        let primary_metadata = read_metadata_blocks(primary_handle);
        sys::heif_image_handle_release(primary_handle);

        // All items and their 4cc types.
        let item_count = sys::heif_context_get_number_of_items(ctx);
        let mut item_ids = vec![0u32; item_count.max(0) as usize];
        let filled = sys::heif_context_get_list_of_item_IDs(ctx, item_ids.as_mut_ptr(), item_count);
        item_ids.truncate(filled.max(0) as usize);
        let items = item_ids
            .iter()
            .map(|&id| OracleItem {
                id,
                item_type: fourcc_to_string(sys::heif_item_get_item_type(ctx, id)),
            })
            .collect();

        // Top-level images with dimensions / alpha / thumbnails.
        let image_count = sys::heif_context_get_number_of_top_level_images(ctx);
        let mut image_ids = vec![0u32; image_count.max(0) as usize];
        let filled = sys::heif_context_get_list_of_top_level_image_IDs(
            ctx,
            image_ids.as_mut_ptr(),
            image_count,
        );
        image_ids.truncate(filled.max(0) as usize);

        let mut images = Vec::with_capacity(image_ids.len());
        for &id in &image_ids {
            let mut handle: *mut sys::heif_image_handle = ptr::null_mut();
            heif_check(sys::heif_context_get_image_handle(ctx, id, &mut handle))?;
            let width = sys::heif_image_handle_get_width(handle).max(0) as u32;
            let height = sys::heif_image_handle_get_height(handle).max(0) as u32;
            let has_alpha = sys::heif_image_handle_has_alpha_channel(handle) != 0;
            let is_primary = sys::heif_image_handle_is_primary_image(handle) != 0;
            let thumbnail_ids = read_thumbnail_ids(handle);
            sys::heif_image_handle_release(handle);
            images.push(OracleImage {
                id,
                width,
                height,
                has_alpha,
                is_primary,
                thumbnail_ids,
            });
        }

        Ok(OracleStructure {
            primary_item_id,
            items,
            images,
            primary_metadata,
        })
    }
}

/// Collects the thumbnail item ids of an image handle.
unsafe fn read_thumbnail_ids(handle: *const sys::heif_image_handle) -> Vec<u32> {
    // SAFETY: `handle` is a live image handle for the duration of the call.
    unsafe {
        let n = sys::heif_image_handle_get_number_of_thumbnails(handle);
        let mut ids = vec![0u32; n.max(0) as usize];
        let filled = sys::heif_image_handle_get_list_of_thumbnail_IDs(handle, ids.as_mut_ptr(), n);
        ids.truncate(filled.max(0) as usize);
        ids
    }
}

/// Collects all Exif/XMP metadata blocks attached to an image handle.
unsafe fn read_metadata_blocks(handle: *const sys::heif_image_handle) -> Vec<OracleMetadataBlock> {
    // SAFETY: `handle` is a live image handle for the duration of the call; each `get_metadata`
    // writes exactly `get_metadata_size` bytes into the preallocated buffer.
    unsafe {
        let n = sys::heif_image_handle_get_number_of_metadata_blocks(handle, ptr::null());
        let mut ids = vec![0u32; n.max(0) as usize];
        let filled = sys::heif_image_handle_get_list_of_metadata_block_IDs(
            handle,
            ptr::null(),
            ids.as_mut_ptr(),
            n,
        );
        ids.truncate(filled.max(0) as usize);

        ids.into_iter()
            .map(|id| {
                let item_type =
                    cstr_to_string(sys::heif_image_handle_get_metadata_type(handle, id));
                let content_type =
                    cstr_to_string(sys::heif_image_handle_get_metadata_content_type(handle, id));
                let size = sys::heif_image_handle_get_metadata_size(handle, id);
                let mut data = vec![0u8; size];
                if size > 0 {
                    let _ = sys::heif_image_handle_get_metadata(
                        handle,
                        id,
                        data.as_mut_ptr().cast::<c_void>(),
                    );
                }
                OracleMetadataBlock {
                    id,
                    item_type,
                    content_type,
                    data,
                }
            })
            .collect()
    }
}

/// A four-character-code (`uint32` big-endian) → `String`, keeping only printable ASCII.
fn fourcc_to_string(fourcc: u32) -> String {
    fourcc
        .to_be_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

/// A (possibly null) C string → owned `String`.
unsafe fn cstr_to_string(p: *const std::os::raw::c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: caller passes a libheif-owned NUL-terminated string valid for this call.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

// ============================================================================================
//   (c) Direct HEVC-intra decode via libde265 (bypassing libheif)
// ============================================================================================

/// Chroma sampling of a decoded HEVC picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OracleChroma {
    /// Monochrome (luma only).
    Mono,
    /// 4:2:0.
    Yuv420,
    /// 4:2:2.
    Yuv422,
    /// 4:4:4.
    Yuv444,
}

/// A decoded HEVC picture: `[Y, Cb, Cr]` planes, each tightly packed in raster order and widened
/// to `u16`. For [`OracleChroma::Mono`] the two chroma planes are empty; for subsampled chroma the
/// chroma planes are smaller than luma.
#[derive(Debug, Clone)]
pub struct OracleYuv {
    /// Luma width in pixels.
    pub width: u32,
    /// Luma height in pixels.
    pub height: u32,
    /// Chroma sampling.
    pub chroma: OracleChroma,
    /// Bits per sample (8, 10, 12).
    pub bit_depth: u8,
    /// `[Y, Cb, Cr]` planes, raster order, no padding, samples widened to `u16`.
    pub planes: [Vec<u16>; 3],
}

/// Decodes a single HEVC-intra still picture directly with libde265.
///
/// `config_nals` are the parameter-set NAL units (VPS/SPS/PPS, typically extracted from the item's
/// `hvcC`), `picture_nals` the coded slice NAL unit(s). Each NAL unit is a **raw** unit — the two
/// byte NAL header followed by the RBSP with emulation-prevention bytes intact, and **no** Annex-B
/// start code (they are pushed individually via `de265_push_NAL`). Config NALs are pushed first.
///
/// # Errors
///
/// Returns libde265's message if the decoder cannot be created or the NAL stream produces no
/// picture, or if the picture's bit depth is not 8/10/12.
pub fn decode_hevc_intra(
    config_nals: &[&[u8]],
    picture_nals: &[&[u8]],
) -> Result<OracleYuv, String> {
    // SAFETY: the decoder is freed on every return path; each pushed NAL slice outlives its
    // (copying) push call, and the decoded picture is released before returning.
    unsafe {
        let ctx = sys::de265_new_decoder();
        if ctx.is_null() {
            return Err("de265_new_decoder returned null".into());
        }
        let result = decode_hevc_inner(ctx, config_nals, picture_nals);
        sys::de265_free_decoder(ctx);
        result
    }
}

unsafe fn decode_hevc_inner(
    ctx: *mut sys::de265_decoder_context,
    config_nals: &[&[u8]],
    picture_nals: &[&[u8]],
) -> Result<OracleYuv, String> {
    // SAFETY: `ctx` is a live decoder for the whole body; a picture, if any, is released before
    // return.
    unsafe {
        for nal in config_nals.iter().chain(picture_nals.iter()) {
            let err = sys::de265_push_NAL(
                ctx,
                nal.as_ptr().cast::<c_void>(),
                nal.len() as c_int,
                0,
                ptr::null_mut(),
            );
            if sys::de265_isOK(err) == 0 {
                return Err(de265_err(err));
            }
        }
        // No more input follows: flush so the pending NALs are decoded and drained.
        let _ = sys::de265_flush_data(ctx);

        loop {
            let mut more: c_int = 0;
            let err = sys::de265_decode(ctx, &mut more);

            let pic = sys::de265_get_next_picture(ctx);
            if !pic.is_null() {
                let out = extract_de265(pic);
                sys::de265_release_next_picture(ctx);
                return out;
            }
            if more == 0 {
                // No picture and nothing left to decode: surface any hard error.
                if sys::de265_isOK(err) == 0 {
                    return Err(de265_err(err));
                }
                return Err("libde265 produced no picture from the NAL stream".into());
            }
        }
    }
}

/// Copies a decoded libde265 picture into an owned [`OracleYuv`].
unsafe fn extract_de265(pic: *const sys::de265_image) -> Result<OracleYuv, String> {
    // SAFETY: `pic` is a live picture owned by the decoder until `de265_release_next_picture`.
    unsafe {
        let bit_depth = sys::de265_get_bits_per_pixel(pic, 0) as u8;
        if !matches!(bit_depth, 8 | 10 | 12) {
            return Err(format!("unexpected bit depth: {bit_depth}-bit"));
        }
        let chroma = match sys::de265_get_chroma_format(pic) {
            sys::de265_chroma_mono => OracleChroma::Mono,
            sys::de265_chroma_420 => OracleChroma::Yuv420,
            sys::de265_chroma_422 => OracleChroma::Yuv422,
            sys::de265_chroma_444 => OracleChroma::Yuv444,
            other => return Err(format!("unexpected chroma format {other}")),
        };
        let w = sys::de265_get_image_width(pic, 0).max(0) as usize;
        let h = sys::de265_get_image_height(pic, 0).max(0) as usize;

        let y = copy_de265_plane(pic, 0, bit_depth);
        let (cb, cr) = if chroma == OracleChroma::Mono {
            (Vec::new(), Vec::new())
        } else {
            (
                copy_de265_plane(pic, 1, bit_depth),
                copy_de265_plane(pic, 2, bit_depth),
            )
        };

        Ok(OracleYuv {
            width: w as u32,
            height: h as u32,
            chroma,
            bit_depth,
            planes: [y, cb, cr],
        })
    }
}

/// Copies one strided libde265 plane into a tightly packed `u16` `Vec`.
unsafe fn copy_de265_plane(
    pic: *const sys::de265_image,
    channel: c_int,
    bit_depth: u8,
) -> Vec<u16> {
    // SAFETY: `pic` is live; `de265_get_image_plane` returns a buffer of `height` rows spaced
    // `stride` bytes apart, each holding at least `width` samples.
    unsafe {
        let w = sys::de265_get_image_width(pic, channel).max(0) as usize;
        let h = sys::de265_get_image_height(pic, channel).max(0) as usize;
        let mut stride: c_int = 0;
        let base = sys::de265_get_image_plane(pic, channel, &mut stride);
        if base.is_null() || w == 0 || h == 0 {
            return Vec::new();
        }
        let stride = stride as usize;
        let mut out = vec![0u16; w * h];
        for row in 0..h {
            let row_base = base.add(row * stride);
            for col in 0..w {
                out[row * w + col] = if bit_depth == 8 {
                    u16::from(*row_base.add(col))
                } else {
                    *row_base.cast::<u16>().add(col)
                };
            }
        }
        out
    }
}

/// libde265's human-readable string for an error code.
unsafe fn de265_err(err: sys::de265_error) -> String {
    // SAFETY: `de265_get_error_text` returns a static NUL-terminated string.
    unsafe {
        let p = sys::de265_get_error_text(err);
        cstr_to_string(p)
    }
}

// ============================================================================================
//   (d) HEIC encode via libheif + kvazaar (fixture generation)
// ============================================================================================

/// Options for [`encode_rgba_to_heic`].
///
/// The first three fields cover the core encode; the rest expose container knobs the differential
/// tests want (a thumbnail, embedded Exif/XMP, an orientation) — all optional and off by default.
#[derive(Debug, Clone, Default)]
pub struct EncodeOpts {
    /// Encode losslessly (kvazaar's lossless mode). Note: even in "lossless" mode the RGB→YCbCr
    /// color conversion libheif performs is generally **not** bit-exact end to end — see the
    /// crate's smoke tests for the measured round-trip error.
    pub lossless: bool,
    /// Lossy quality in `0..=100` (ignored when `lossless`).
    pub quality: u8,
    /// Encode the alpha channel of the input RGBA (as a HEVC auxiliary alpha image).
    pub with_alpha: bool,
    /// If set, also encode a thumbnail fitting a square of this many pixels, assigned to the image.
    pub thumbnail_bbox: Option<u32>,
    /// If set, embed this Exif payload (stored exactly as given behind libheif's 4-byte offset).
    pub exif: Option<Vec<u8>>,
    /// If set, embed this XMP packet (as a `mime` / `application/rdf+xml` item).
    pub xmp: Option<Vec<u8>>,
    /// EXIF-style orientation `1..=8` written as `irot`/`imir` (`0` or `1` ⇒ normal, no transform).
    pub orientation: u8,
    /// If set, write an explicit `nclx` colour profile (a `colr` box) with these CICP code points
    /// and range, and use it as the RGB→YCbCr conversion target. When `None` (the default) libheif
    /// omits `colr` entirely and falls back to full-range sRGB internally — so a reader whose
    /// colr-*less* default differs (e.g. limited-range BT.601) decodes the pixels differently. A
    /// container-conformance suite that compares presentation pixels therefore wants an explicit
    /// `colr` so both readers agree on the colour policy.
    pub nclx: Option<NclxProfile>,
    /// If set, attach this mastering display colour volume to the source image
    /// (`heif_image_set_mastering_display_colour_volume`), which libheif writes as an `mdcv` item
    /// property on the primary image.
    pub mdcv: Option<MasteringDisplay>,
}

/// A mastering display colour volume ([`EncodeOpts::mdcv`]) in libheif's
/// `heif_mastering_display_colour_volume` field layout and units: chromaticities in increments
/// of 0.00002, luminances in 0.0001 cd/m². The primaries are passed through in the given index
/// order; libheif does not reorder them.
#[derive(Debug, Clone, Copy)]
pub struct MasteringDisplay {
    /// `display_primaries_x[c]` for `c` in `0..3`.
    pub display_primaries_x: [u16; 3],
    /// `display_primaries_y[c]` for `c` in `0..3`.
    pub display_primaries_y: [u16; 3],
    /// `white_point_x`.
    pub white_point_x: u16,
    /// `white_point_y`.
    pub white_point_y: u16,
    /// `max_display_mastering_luminance`.
    pub max_display_mastering_luminance: u32,
    /// `min_display_mastering_luminance`.
    pub min_display_mastering_luminance: u32,
}

/// A CICP `nclx` colour profile ([`EncodeOpts::nclx`]) — the four code points libheif writes into a
/// `colr` box and uses as its RGB→YCbCr conversion target.
#[derive(Debug, Clone, Copy)]
pub struct NclxProfile {
    /// `colour_primaries` CICP code point (e.g. 1 = BT.709, 2 = unspecified).
    pub colour_primaries: u8,
    /// `transfer_characteristics` CICP code point (e.g. 13 = sRGB/IEC 61966-2-1).
    pub transfer_characteristics: u8,
    /// `matrix_coefficients` CICP code point (e.g. 6 = BT.601, 5 = BT.470BG, 0 = identity/GBR).
    pub matrix_coefficients: u8,
    /// `full_range_flag`: `true` = full (PC) range, `false` = limited (studio) range.
    pub full_range: bool,
}

/// Encodes an interleaved 8-bit RGBA image to a HEIC byte stream with libheif + kvazaar.
///
/// `rgba8` must be `width * height * 4` bytes, row-major `R,G,B,A`. When
/// [`EncodeOpts::with_alpha`] is false the alpha bytes are dropped and an RGB image is encoded.
///
/// # Errors
///
/// Returns libheif's message if encoding or serialization fails (e.g. no HEVC encoder is available,
/// which would indicate a broken build).
pub fn encode_rgba_to_heic(
    width: u32,
    height: u32,
    rgba8: &[u8],
    opts: &EncodeOpts,
) -> Result<Vec<u8>, String> {
    let (w, h) = (width as usize, height as usize);
    if rgba8.len() != w * h * 4 {
        return Err(format!(
            "rgba8 length {} != width*height*4 = {}",
            rgba8.len(),
            w * h * 4
        ));
    }
    // SAFETY: every libheif resource acquired below is released before returning on all paths.
    unsafe { encode_inner(width, height, rgba8, opts) }
}

unsafe fn encode_inner(
    width: u32,
    height: u32,
    rgba8: &[u8],
    opts: &EncodeOpts,
) -> Result<Vec<u8>, String> {
    // SAFETY: see per-resource release calls; `?` early-returns free what was acquired via the
    // trailing cleanup because each stage frees before propagating.
    unsafe {
        let ctx = sys::heif_context_alloc();
        if ctx.is_null() {
            return Err("heif_context_alloc returned null".into());
        }
        let out = encode_body(ctx, width, height, rgba8, opts);
        sys::heif_context_free(ctx);
        out
    }
}

unsafe fn encode_body(
    ctx: *mut sys::heif_context,
    width: u32,
    height: u32,
    rgba8: &[u8],
    opts: &EncodeOpts,
) -> Result<Vec<u8>, String> {
    // SAFETY: builds an image, encoder, and options; all are released before return.
    unsafe {
        let (w, h) = (width as usize, height as usize);
        let chroma = if opts.with_alpha {
            sys::heif_chroma_interleaved_RGBA
        } else {
            sys::heif_chroma_interleaved_RGB
        };
        let bpp: usize = if opts.with_alpha { 4 } else { 3 };

        // ---- Build the source image. ----
        let mut img: *mut sys::heif_image = ptr::null_mut();
        heif_check(sys::heif_image_create(
            width as c_int,
            height as c_int,
            sys::heif_colorspace_RGB,
            chroma,
            &mut img,
        ))?;
        if let Err(e) = heif_check(sys::heif_image_add_plane(
            img,
            sys::heif_channel_interleaved,
            width as c_int,
            height as c_int,
            8,
        )) {
            sys::heif_image_release(img);
            return Err(e);
        }
        let mut stride: c_int = 0;
        let plane = sys::heif_image_get_plane(img, sys::heif_channel_interleaved, &mut stride);
        if plane.is_null() {
            sys::heif_image_release(img);
            return Err("heif_image_get_plane returned null".into());
        }
        let stride = stride as usize;
        for row in 0..h {
            let dst = plane.add(row * stride);
            for col in 0..w {
                let s = (row * w + col) * 4;
                let d = dst.add(col * bpp);
                *d.add(0) = rgba8[s];
                *d.add(1) = rgba8[s + 1];
                *d.add(2) = rgba8[s + 2];
                if opts.with_alpha {
                    *d.add(3) = rgba8[s + 3];
                }
            }
        }
        if let Some(m) = opts.mdcv {
            let mdcv = sys::heif_mastering_display_colour_volume {
                display_primaries_x: m.display_primaries_x,
                display_primaries_y: m.display_primaries_y,
                white_point_x: m.white_point_x,
                white_point_y: m.white_point_y,
                max_display_mastering_luminance: m.max_display_mastering_luminance,
                min_display_mastering_luminance: m.min_display_mastering_luminance,
            };
            sys::heif_image_set_mastering_display_colour_volume(img, &mdcv);
        }

        // ---- Encoder. ----
        let mut encoder: *mut sys::heif_encoder = ptr::null_mut();
        if let Err(e) = heif_check(sys::heif_context_get_encoder_for_format(
            ctx,
            sys::heif_compression_HEVC,
            &mut encoder,
        )) {
            sys::heif_image_release(img);
            return Err(e);
        }
        if opts.lossless {
            let _ = sys::heif_encoder_set_lossless(encoder, 1);
        } else {
            let _ = sys::heif_encoder_set_lossless(encoder, 0);
            let _ = sys::heif_encoder_set_lossy_quality(encoder, i32::from(opts.quality));
        }

        // ---- Encoding options (alpha + orientation + optional nclx colr). ----
        let options = sys::heif_encoding_options_alloc();
        // An optional heap-allocated nclx profile; written into `colr` and used as the conversion
        // target. Kept alive until after encoding and freed in `cleanup`.
        let mut nclx_ptr: *mut sys::heif_color_profile_nclx = ptr::null_mut();
        if !options.is_null() {
            (*options).save_alpha_channel = u8::from(opts.with_alpha);
            if opts.orientation >= 2 && opts.orientation <= 8 {
                (*options).image_orientation = u32::from(opts.orientation);
            }
            if let Some(nclx) = opts.nclx {
                nclx_ptr = sys::heif_nclx_color_profile_alloc();
                if !nclx_ptr.is_null() {
                    (*nclx_ptr).color_primaries =
                        u32::from(nclx.colour_primaries) as sys::heif_color_primaries;
                    (*nclx_ptr).transfer_characteristics = u32::from(nclx.transfer_characteristics)
                        as sys::heif_transfer_characteristics;
                    (*nclx_ptr).matrix_coefficients =
                        u32::from(nclx.matrix_coefficients) as sys::heif_matrix_coefficients;
                    (*nclx_ptr).full_range_flag = u8::from(nclx.full_range);
                    (*options).output_nclx_profile = nclx_ptr;
                }
            }
        }

        let cleanup = |img: *mut sys::heif_image,
                       encoder: *mut sys::heif_encoder,
                       options: *mut sys::heif_encoding_options| {
            if !nclx_ptr.is_null() {
                sys::heif_nclx_color_profile_free(nclx_ptr);
            }
            if !options.is_null() {
                sys::heif_encoding_options_free(options);
            }
            sys::heif_encoder_release(encoder);
            sys::heif_image_release(img);
        };

        // ---- Encode the primary image. ----
        let mut handle: *mut sys::heif_image_handle = ptr::null_mut();
        if let Err(e) = heif_check(sys::heif_context_encode_image(
            ctx,
            img,
            encoder,
            options,
            &mut handle,
        )) {
            cleanup(img, encoder, options);
            return Err(e);
        }

        // ---- Optional thumbnail. ----
        if let Some(bbox) = opts.thumbnail_bbox {
            let mut thumb: *mut sys::heif_image_handle = ptr::null_mut();
            if let Err(e) = heif_check(sys::heif_context_encode_thumbnail(
                ctx,
                img,
                handle,
                encoder,
                options,
                bbox as c_int,
                &mut thumb,
            )) {
                if !handle.is_null() {
                    sys::heif_image_handle_release(handle);
                }
                cleanup(img, encoder, options);
                return Err(e);
            }
            if !thumb.is_null() {
                sys::heif_image_handle_release(thumb);
            }
        }

        // ---- Optional metadata. ----
        if let Some(exif) = &opts.exif
            && let Err(e) = heif_check(sys::heif_context_add_exif_metadata(
                ctx,
                handle,
                exif.as_ptr().cast::<c_void>(),
                exif.len() as c_int,
            ))
        {
            sys::heif_image_handle_release(handle);
            cleanup(img, encoder, options);
            return Err(e);
        }
        if let Some(xmp) = &opts.xmp
            && let Err(e) = heif_check(sys::heif_context_add_XMP_metadata(
                ctx,
                handle,
                xmp.as_ptr().cast::<c_void>(),
                xmp.len() as c_int,
            ))
        {
            sys::heif_image_handle_release(handle);
            cleanup(img, encoder, options);
            return Err(e);
        }

        if !handle.is_null() {
            sys::heif_image_handle_release(handle);
        }
        cleanup(img, encoder, options);

        // ---- Serialize to memory via a writer callback. ----
        write_context_to_memory(ctx)
    }
}

/// Serializes a finished `heif_context` into a `Vec<u8>` using a `heif_writer` callback (avoids
/// touching the filesystem — libheif has no direct write-to-memory entry point).
unsafe fn write_context_to_memory(ctx: *mut sys::heif_context) -> Result<Vec<u8>, String> {
    extern "C" fn write_cb(
        _ctx: *mut sys::heif_context,
        data: *const c_void,
        size: usize,
        userdata: *mut c_void,
    ) -> sys::heif_error {
        // SAFETY: `userdata` is the `&mut Vec<u8>` we pass to `heif_context_write`; `data`/`size`
        // describe a valid buffer for this call.
        unsafe {
            let buf = &mut *userdata.cast::<Vec<u8>>();
            buf.extend_from_slice(std::slice::from_raw_parts(data.cast::<u8>(), size));
        }
        sys::heif_error {
            code: sys::heif_error_Ok,
            subcode: sys::heif_suberror_Unspecified,
            message: ptr::null(),
        }
    }

    // SAFETY: `writer` lives across the call; `buf` is handed back to the callback as `userdata`.
    unsafe {
        let mut buf: Vec<u8> = Vec::new();
        let mut writer = sys::heif_writer {
            writer_api_version: 1,
            write: Some(write_cb),
        };
        heif_check(sys::heif_context_write(
            ctx,
            &mut writer,
            (&mut buf as *mut Vec<u8>).cast::<c_void>(),
        ))?;
        Ok(buf)
    }
}

// ============================================================================================
//   Smoke tests
// ============================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic 64×64 RGBA gradient. R = x*4, G = y*4, B = (x+y)*2, A = 255 (or a diagonal
    /// ramp when `alpha_ramp`).
    fn gradient(size: usize, alpha_ramp: bool) -> Vec<u8> {
        let mut v = vec![0u8; size * size * 4];
        for y in 0..size {
            for x in 0..size {
                let i = (y * size + x) * 4;
                v[i] = (x * 4) as u8;
                v[i + 1] = (y * 4) as u8;
                v[i + 2] = ((x + y) * 2) as u8;
                v[i + 3] = if alpha_ramp { ((x + y) * 2) as u8 } else { 255 };
            }
        }
        v
    }

    /// Mean and max absolute RGB difference between two RGBA buffers of the same length.
    fn rgb_diff(a: &[u8], b: &[u8]) -> (f64, u8) {
        assert_eq!(a.len(), b.len());
        let mut sum = 0u64;
        let mut count = 0u64;
        let mut max = 0u8;
        for (px_a, px_b) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
            for c in 0..3 {
                let d = px_a[c].abs_diff(px_b[c]);
                sum += u64::from(d);
                count += 1;
                max = max.max(d);
            }
        }
        (sum as f64 / count as f64, max)
    }

    #[test]
    fn roundtrip_lossy_quality_90() {
        let src = gradient(64, false);
        let heic = encode_rgba_to_heic(
            64,
            64,
            &src,
            &EncodeOpts {
                quality: 90,
                ..Default::default()
            },
        )
        .expect("encode q90");
        assert!(!heic.is_empty());
        // Sanity: a real HEIF file starts with an `ftyp` box.
        assert_eq!(&heic[4..8], b"ftyp", "output is not a HEIF file");

        let (w, h, dec) = decode_primary_rgba(&heic).expect("decode q90");
        assert_eq!((w, h), (64, 64));
        let (mean, max) = rgb_diff(&src, &dec);
        // Lossy YCbCr 4:2:0 at q90 over a smooth gradient stays well within these bounds.
        assert!(mean < 4.0, "q90 mean abs diff too high: {mean}");
        assert!(max < 24, "q90 max abs diff too high: {max}");
        eprintln!("q90 roundtrip: mean={mean:.3} max={max}");
    }

    #[test]
    fn roundtrip_lossless_measures_exactness() {
        let src = gradient(64, false);
        let heic = encode_rgba_to_heic(
            64,
            64,
            &src,
            &EncodeOpts {
                lossless: true,
                ..Default::default()
            },
        )
        .expect("encode lossless");
        let (w, h, dec) = decode_primary_rgba(&heic).expect("decode lossless");
        assert_eq!((w, h), (64, 64));
        let (mean, max) = rgb_diff(&src, &dec);
        let bit_exact = max == 0;
        eprintln!("lossless roundtrip: mean={mean:.3} max={max} bit_exact={bit_exact}");
        // kvazaar's lossless HEVC is exact in YCbCr, but libheif's RGB↔YCbCr 4:2:0 conversion is
        // not reversible, so the end-to-end RGB round-trip is only near-exact. Assert a tight
        // bound rather than exact equality (see the printed measurement).
        assert!(mean < 3.0, "lossless mean abs diff too high: {mean}");
        assert!(max <= 16, "lossless max abs diff too high: {max}");
    }

    #[test]
    fn encode_with_alpha_is_reported_by_introspection() {
        let src = gradient(64, true);
        let heic = encode_rgba_to_heic(
            64,
            64,
            &src,
            &EncodeOpts {
                quality: 90,
                with_alpha: true,
                ..Default::default()
            },
        )
        .expect("encode alpha");

        let st = introspect(&heic).expect("introspect");
        assert_ne!(st.primary_item_id, 0);
        assert!(!st.items.is_empty());
        let primary = st
            .images
            .iter()
            .find(|i| i.is_primary)
            .expect("a primary image");
        assert_eq!((primary.width, primary.height), (64, 64));
        assert!(primary.has_alpha, "alpha channel not reported");
    }

    #[test]
    fn thumbnail_and_metadata_survive_roundtrip() {
        let src = gradient(64, false);
        let exif = b"\x00\x00\x00\x00II*\x00fake-exif".to_vec();
        let xmp = br#"<?xpacket?><x:xmpmeta xmlns:x="adobe:ns:meta/"></x:xmpmeta>"#.to_vec();
        let heic = encode_rgba_to_heic(
            64,
            64,
            &src,
            &EncodeOpts {
                quality: 85,
                thumbnail_bbox: Some(32),
                exif: Some(exif.clone()),
                xmp: Some(xmp.clone()),
                ..Default::default()
            },
        )
        .expect("encode with thumb+metadata");

        let st = introspect(&heic).expect("introspect");
        let primary = st.images.iter().find(|i| i.is_primary).expect("primary");
        assert_eq!(
            primary.thumbnail_ids.len(),
            1,
            "expected exactly one thumbnail"
        );
        let types: Vec<_> = st
            .primary_metadata
            .iter()
            .map(|m| m.item_type.as_str())
            .collect();
        assert!(types.contains(&"Exif"), "Exif block missing: {types:?}");
        assert!(
            types.contains(&"mime"),
            "XMP (mime) block missing: {types:?}"
        );
        // The XMP payload is stored verbatim.
        let xmp_block = st
            .primary_metadata
            .iter()
            .find(|m| m.item_type == "mime")
            .unwrap();
        assert_eq!(xmp_block.content_type, "application/rdf+xml");
        assert_eq!(xmp_block.data, xmp);
    }
}
