//! The parsed-container handle [`GamutAvifImage`]: `AvifContainer::parse` behind an owned handle
//! whose accessors hand a C caller the primary and alpha AV1 codestreams, the `av1C` record, and
//! the metadata payloads (issue #666), so it can drive any AV1 decoder itself.

use core::ptr;

use gamut::avif::{AvifContainer, HeifImage, HeifItem, ItemKind};

use crate::boundary::{bytes_in, status_of};
use crate::guard;
use crate::status::{
    GAMUT_OK, GAMUT_STATUS_INVALID_INPUT, GAMUT_STATUS_NULL_ARGUMENT, GAMUT_STATUS_UNSUPPORTED,
    GamutStatus,
};

/// Opaque handle over a parsed AVIF file (`gamut::avif::HeifImage`, the role-typed view
/// `AvifContainer::parse` builds). It owns every payload, so it outlives the input buffer.
pub struct GamutAvifImage {
    inner: HeifImage,
}

/// A byte selector over the parsed image: `Ok(None)` is "absent" (an `OK` status with a
/// `NULL`/`0` result), `Err` a status to return.
type Select = for<'a> fn(&'a HeifImage) -> Result<Option<&'a [u8]>, GamutStatus>;

impl GamutAvifImage {
    /// Runs `body` against the live image behind `image`, after clearing and null-checking the
    /// out-parameters `body` writes through.
    ///
    /// # Safety
    ///
    /// `image` is `NULL` or a live pointer from `gamut_avif_image_parse`.
    unsafe fn read(
        image: *const Self,
        outs_present: bool,
        clear: impl FnOnce(),
        body: impl FnOnce(&HeifImage) -> GamutStatus,
    ) -> GamutStatus {
        guard::status(|| {
            if !outs_present {
                return GAMUT_STATUS_NULL_ARGUMENT;
            }
            clear();
            // SAFETY: null is rejected here; a non-null handle is live per contract.
            match unsafe { image.as_ref() } {
                Some(this) => body(&this.inner),
                None => GAMUT_STATUS_NULL_ARGUMENT,
            }
        })
    }

    /// The shared body of every byte accessor: writes the selected slice, borrowed from the
    /// handle, through `(data, len)`.
    ///
    /// # Safety
    ///
    /// `image` as in [`read`](Self::read); `data` and `len` are `NULL` or writable.
    unsafe fn bytes(
        image: *const Self,
        data: *mut *const u8,
        len: *mut usize,
        select: Select,
    ) -> GamutStatus {
        let outs_present = !data.is_null() && !len.is_null();
        // SAFETY (both closures): `read` calls them only once `outs_present` holds, and the
        // out-params are writable per this fn's contract.
        let write = |bytes: *const u8, count: usize| unsafe {
            data.write(bytes);
            len.write(count);
        };
        unsafe {
            Self::read(
                image,
                outs_present,
                || write(ptr::null(), 0),
                |inner| match select(inner) {
                    Ok(Some(bytes)) => {
                        write(bytes.as_ptr(), bytes.len());
                        GAMUT_OK
                    }
                    Ok(None) => GAMUT_OK,
                    Err(status) => status,
                },
            )
        }
    }
}

/// The AV1 codestream of `item`, if it is a coded AV1 item; a derived item (`grid`, `iden`,
/// `iovl`) or another codec carries no single codestream, so it is `GAMUT_STATUS_UNSUPPORTED`.
fn av1_payload(item: HeifItem<'_>) -> Result<&[u8], GamutStatus> {
    if item.kind() == (ItemKind::CodedImage { codec: *b"av01" }) {
        Ok(&item.as_isobmff_item().payload)
    } else {
        Err(GAMUT_STATUS_UNSUPPORTED)
    }
}

/// Parses an AVIF file into an owned image handle (`AvifContainer::parse`).
///
/// On `GAMUT_OK`, `*out` is a handle to release with `gamut_avif_image_free`; it copies what it
/// keeps, so `data` may be freed as soon as this returns. On any other status `*out` is `NULL`.
/// Returns `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` `out`, or a `NULL` `data` with a non-zero
/// `len`, and otherwise the parser's error status (`GAMUT_STATUS_INVALID_INPUT` for a malformed
/// file, `GAMUT_STATUS_UNSUPPORTED` for an out-of-scope one such as an image sequence).
///
/// # Safety
///
/// A non-`NULL` `data` points to `len` readable bytes; `out` is `NULL` or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_parse(
    data: *const u8,
    len: usize,
    out: *mut *mut GamutAvifImage,
) -> GamutStatus {
    guard::status(|| {
        if out.is_null() {
            return GAMUT_STATUS_NULL_ARGUMENT;
        }
        // SAFETY: non-null and writable per contract.
        unsafe { out.write(ptr::null_mut()) };
        // SAFETY: per this fn's contract.
        let Some(bytes) = (unsafe { bytes_in(data, len) }) else {
            return GAMUT_STATUS_NULL_ARGUMENT;
        };
        match AvifContainer::parse(bytes) {
            Ok(container) => {
                // The container borrows `data`; the semantic view it built owns its payloads,
                // so the handle keeps (a copy of) only that and no borrow crosses the boundary.
                let handle = Box::new(GamutAvifImage {
                    inner: container.image().clone(),
                });
                // SAFETY: as the clearing write above.
                unsafe { out.write(Box::into_raw(handle)) };
                GAMUT_OK
            }
            Err(error) => status_of(&error),
        }
    })
}

/// Frees an image created by `gamut_avif_image_parse`, invalidating every pointer its
/// accessors returned. No-op on `NULL`.
///
/// # Safety
///
/// `image` is `NULL` or a pointer returned by `gamut_avif_image_parse` that has not already
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_free(image: *mut GamutAvifImage) {
    guard::unit(|| {
        if !image.is_null() {
            // SAFETY: per this fn's contract.
            drop(unsafe { Box::from_raw(image) });
        }
    });
}

/// The primary item's AV1 codestream (the `av01` item payload: a low-overhead OBU stream),
/// borrowed from `image`.
///
/// Returns `GAMUT_STATUS_UNSUPPORTED` (with `*data == NULL`, `*len == 0`) when the primary item
/// is derived (`grid`, `iden`, `iovl`) or not AV1, so it has no single codestream;
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// `image` is `NULL` or a live image handle; `data` and `len` are `NULL` or writable. The
/// returned pointer is valid until `gamut_avif_image_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_primary_codestream(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            av1_payload(inner.primary_item()).map(Some)
        })
    }
}

/// The AV1 codestream of the primary item's alpha auxiliary, borrowed from `image`.
///
/// `GAMUT_OK` with `*data == NULL`, `*len == 0` when the image has no alpha. Returns
/// `GAMUT_STATUS_UNSUPPORTED` when the alpha item is derived or not AV1;
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// As `gamut_avif_image_primary_codestream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_alpha_codestream(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            inner
                .alpha_auxiliary_of(inner.primary_item().id())
                .map(av1_payload)
                .transpose()
        })
    }
}

/// The primary item's `av1C` AV1 codec-configuration record body (AV1-ISOBMFF §2.3.3: four
/// header bytes, then any `configOBUs`), borrowed from `image`.
///
/// `GAMUT_OK` with `*data == NULL`, `*len == 0` when the item carries no `av1C`. Returns
/// `GAMUT_STATUS_UNSUPPORTED` when the primary item is derived or not AV1, as
/// `gamut_avif_image_primary_codestream`; `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// As `gamut_avif_image_primary_codestream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_av1c(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            let primary = inner.primary_item();
            av1_payload(primary)?;
            Ok(primary
                .codec_configuration()
                .filter(|(kind, _)| *kind == b"av1C")
                .map(|(_, body)| body))
        })
    }
}

/// The Exif payload describing the primary item, borrowed from `image` — **raw**, including
/// HEIF's 4-byte big-endian `exif_tiff_header_offset` prefix (as the payload of the
/// `HeifImage::exif` item carries it and libheif returns it); the TIFF stream starts that many
/// bytes after the prefix.
///
/// `GAMUT_OK` with `*data == NULL`, `*len == 0` when there is none;
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// As `gamut_avif_image_primary_codestream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_exif(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            Ok(inner.exif().map(|item| &item.as_isobmff_item().payload[..]))
        })
    }
}

/// The XMP packet describing the primary item, borrowed from `image`, verbatim.
///
/// `GAMUT_OK` with `*data == NULL`, `*len == 0` when there is none;
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// As `gamut_avif_image_primary_codestream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_xmp(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            Ok(inner.xmp().map(|item| &item.as_isobmff_item().payload[..]))
        })
    }
}

/// The primary item's ICC profile (a `colr` box of type `prof` or `rICC`), borrowed from
/// `image`, verbatim.
///
/// `GAMUT_OK` with `*data == NULL`, `*len == 0` when there is none;
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// As `gamut_avif_image_primary_codestream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_icc_profile(
    image: *const GamutAvifImage,
    data: *mut *const u8,
    len: *mut usize,
) -> GamutStatus {
    unsafe {
        GamutAvifImage::bytes(image, data, len, |inner| {
            Ok(inner.primary_item().icc_profile())
        })
    }
}

/// Writes whether the primary item's colour values are premultiplied by its alpha auxiliary
/// (a `prem` reference; `HeifImage::is_premultiplied`) to `*premultiplied`.
///
/// Returns `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// `image` is `NULL` or a live image handle; `premultiplied` is `NULL` or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_is_premultiplied(
    image: *const GamutAvifImage,
    premultiplied: *mut bool,
) -> GamutStatus {
    // SAFETY (both closures): `read` calls them only once `premultiplied` is non-null, and it is
    // writable per this fn's contract.
    let write = |value: bool| unsafe { premultiplied.write(value) };
    unsafe {
        GamutAvifImage::read(
            image,
            !premultiplied.is_null(),
            || write(false),
            |inner| {
                write(inner.is_premultiplied(inner.primary_item().id()));
                GAMUT_OK
            },
        )
    }
}

/// Writes the primary item's stored dimensions (its `ispe` property, before any `clap`/`irot`
/// display transform) to `*width` and `*height`.
///
/// Returns `GAMUT_STATUS_INVALID_INPUT` (with both `0`) when the primary item has no usable
/// `ispe`, which every image item must carry (ISO/IEC 23008-12 §6.5.3);
/// `GAMUT_STATUS_NULL_ARGUMENT` for a `NULL` argument.
///
/// # Safety
///
/// `image` is `NULL` or a live image handle; `width` and `height` are `NULL` or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gamut_avif_image_dimensions(
    image: *const GamutAvifImage,
    width: *mut u32,
    height: *mut u32,
) -> GamutStatus {
    // SAFETY (both closures): `read` calls them only once both out-params are non-null, and
    // they are writable per this fn's contract.
    let write = |w: u32, h: u32| unsafe {
        width.write(w);
        height.write(h);
    };
    unsafe {
        GamutAvifImage::read(
            image,
            !width.is_null() && !height.is_null(),
            || write(0, 0),
            |inner| match inner.primary_item().dimensions() {
                Some(dims) => {
                    write(dims.width(), dims.height());
                    GAMUT_OK
                }
                None => GAMUT_STATUS_INVALID_INPUT,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    //! The entry points are called exactly as C calls them. The oracle is `AvifContainer`, the
    //! parser the handle is built from (issue #666); the fixtures come from the Rust
    //! `AvifEncoder`, so a defect in the C encode entry points cannot fail these tests.

    use gamut::avif::{AlphaMode, AvifEncoder};
    use gamut::core::{Dimensions, EncodeImage, ImageRef, Pixel, Rgb8, Rgba8};

    use super::*;

    /// A 4×4 file the Rust encoder writes for a gradient of `P` samples, configured by `encoder`.
    fn fixture<P: Pixel<Sample = u8>>(encoder: AvifEncoder) -> Vec<u8>
    where
        AvifEncoder: EncodeImage<P>,
    {
        let samples: Vec<u8> = (0..16 * P::CHANNELS)
            .map(|i| (i * 13 % 256) as u8)
            .collect();
        let image = ImageRef::<P>::new(&samples, Dimensions::new(4, 4).unwrap()).unwrap();
        encoder.encode_to_vec(image).unwrap()
    }

    /// Parses through the C entry point, panicking on failure.
    fn c_parse(bytes: &[u8]) -> *mut GamutAvifImage {
        let mut image = ptr::null_mut();
        assert_eq!(
            unsafe { gamut_avif_image_parse(bytes.as_ptr(), bytes.len(), &mut image) },
            GAMUT_OK
        );
        assert!(!image.is_null());
        image
    }

    /// One byte accessor's result, copied out of the handle.
    type Accessor =
        unsafe extern "C" fn(*const GamutAvifImage, *mut *const u8, *mut usize) -> GamutStatus;

    fn read(
        image: *const GamutAvifImage,
        accessor: Accessor,
    ) -> Result<Option<Vec<u8>>, GamutStatus> {
        let mut data = ptr::null();
        let mut len = usize::MAX;
        let status = unsafe { accessor(image, &mut data, &mut len) };
        if status != GAMUT_OK {
            assert!(
                data.is_null() && len == 0,
                "a failed accessor returns nothing"
            );
            return Err(status);
        }
        if data.is_null() {
            assert_eq!(len, 0, "an absent payload has no length");
            return Ok(None);
        }
        Ok(Some(
            unsafe { core::slice::from_raw_parts(data, len) }.to_vec(),
        ))
    }

    #[test]
    fn parse_accessors_agree_with_avif_container() {
        let tiff = b"MM\0\x2a\0\0\0\x08\0\0".as_slice();
        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'/>".as_slice();
        let icc = b"not-really-an-icc-profile".as_slice();
        let file = fixture::<Rgba8>(
            AvifEncoder::new()
                .with_exif(tiff)
                .with_xmp(xmp)
                .with_icc_profile(icc)
                .with_alpha_mode(AlphaMode::Premultiplied),
        );

        let container = AvifContainer::parse(&file).unwrap();
        let oracle = container.image();
        let primary = oracle.primary_item();
        let alpha = oracle.alpha_auxiliary_of(primary.id()).unwrap();

        let image = c_parse(&file);
        let payload = |item: HeifItem<'_>| Some(item.as_isobmff_item().payload.clone());
        assert_eq!(
            read(image, gamut_avif_image_primary_codestream),
            Ok(payload(primary))
        );
        assert_eq!(
            read(image, gamut_avif_image_alpha_codestream),
            Ok(payload(alpha))
        );
        assert_ne!(
            payload(primary),
            payload(alpha),
            "the two codestreams are told apart"
        );
        assert_eq!(
            read(image, gamut_avif_image_av1c),
            Ok(primary.codec_configuration().map(|(_, body)| body.to_vec()))
        );
        assert_eq!(
            read(image, gamut_avif_image_exif),
            Ok(oracle.exif().and_then(payload))
        );
        // The raw Exif payload is the caller's TIFF stream behind a zero header offset.
        assert_eq!(
            read(image, gamut_avif_image_exif),
            Ok(Some([&[0; 4], tiff].concat()))
        );
        assert_eq!(read(image, gamut_avif_image_xmp), Ok(Some(xmp.to_vec())));
        assert_eq!(
            read(image, gamut_avif_image_icc_profile),
            Ok(Some(icc.to_vec()))
        );

        let mut premultiplied = false;
        assert_eq!(
            unsafe { gamut_avif_image_is_premultiplied(image, &mut premultiplied) },
            GAMUT_OK
        );
        assert!(premultiplied);
        let (mut width, mut height) = (0, 0);
        assert_eq!(
            unsafe { gamut_avif_image_dimensions(image, &mut width, &mut height) },
            GAMUT_OK
        );
        assert_eq!((width, height), (4, 4));
        unsafe { gamut_avif_image_free(image) };
    }

    #[test]
    fn parse_reports_absent_payloads_as_null() {
        let file = fixture::<Rgb8>(AvifEncoder::new());

        let image = c_parse(&file);
        for accessor in [
            gamut_avif_image_alpha_codestream as Accessor,
            gamut_avif_image_exif,
            gamut_avif_image_xmp,
            gamut_avif_image_icc_profile,
        ] {
            assert_eq!(read(image, accessor), Ok(None));
        }
        let mut premultiplied = true;
        assert_eq!(
            unsafe { gamut_avif_image_is_premultiplied(image, &mut premultiplied) },
            GAMUT_OK
        );
        assert!(!premultiplied);
        unsafe { gamut_avif_image_free(image) };
    }

    #[test]
    fn parse_boundary_rejects_null_and_malformed_input() {
        let mut image = ptr::NonNull::<GamutAvifImage>::dangling().as_ptr();
        assert_eq!(
            unsafe { gamut_avif_image_parse(ptr::null(), 0, ptr::null_mut()) },
            GAMUT_STATUS_NULL_ARGUMENT
        );
        assert_eq!(
            unsafe { gamut_avif_image_parse(ptr::null(), 8, &mut image) },
            GAMUT_STATUS_NULL_ARGUMENT
        );
        assert!(image.is_null(), "a failed parse clears the out-param");

        let junk = [0u8; 16];
        image = ptr::NonNull::dangling().as_ptr();
        assert_eq!(
            unsafe { gamut_avif_image_parse(junk.as_ptr(), junk.len(), &mut image) },
            GAMUT_STATUS_INVALID_INPUT
        );
        assert!(image.is_null());

        // Every accessor rejects a null handle and null out-params.
        for accessor in [
            gamut_avif_image_primary_codestream as Accessor,
            gamut_avif_image_alpha_codestream,
            gamut_avif_image_av1c,
            gamut_avif_image_exif,
            gamut_avif_image_xmp,
            gamut_avif_image_icc_profile,
        ] {
            assert_eq!(read(ptr::null(), accessor), Err(GAMUT_STATUS_NULL_ARGUMENT));
            let mut len = 0;
            let mut data = ptr::null();
            assert_eq!(
                unsafe { accessor(ptr::null(), ptr::null_mut(), &mut len) },
                GAMUT_STATUS_NULL_ARGUMENT
            );
            assert_eq!(
                unsafe { accessor(ptr::null(), &mut data, ptr::null_mut()) },
                GAMUT_STATUS_NULL_ARGUMENT
            );
        }
        let mut flag = true;
        assert_eq!(
            unsafe { gamut_avif_image_is_premultiplied(ptr::null(), &mut flag) },
            GAMUT_STATUS_NULL_ARGUMENT
        );
        assert!(
            !flag,
            "the out-param is cleared before the handle is checked"
        );
        let (mut width, mut height) = (7, 7);
        assert_eq!(
            unsafe { gamut_avif_image_dimensions(ptr::null(), &mut width, &mut height) },
            GAMUT_STATUS_NULL_ARGUMENT
        );
        assert_eq!((width, height), (0, 0));
        assert_eq!(
            unsafe { gamut_avif_image_dimensions(ptr::null(), &mut width, ptr::null_mut()) },
            GAMUT_STATUS_NULL_ARGUMENT
        );

        // `_free` is null-tolerant.
        unsafe { gamut_avif_image_free(ptr::null_mut()) };
    }
}
