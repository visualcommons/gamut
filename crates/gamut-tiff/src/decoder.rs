//! The TIFF decoder.
//!
//! Decoding proper produces the image in the layout the file natively carries; presenting it as a
//! caller's chosen [`gamut_core::Pixel`] is then a pure conversion, delegated wholesale to
//! [`gamut_core::convert`] so TIFF applies exactly the same widening and narrowing rules as every
//! other gamut decoder. [`TiffDecoder::convert_policy`] selects which lossy conversions are
//! permitted; the default permits none.

use gamut_core::convert::{ConvertPolicy, DepthPolicy, RawImage, convert_from_raw};
use gamut_core::{
    Cmyk8, DecodeImage, Dimensions, Error, Gray8, Gray16, ImageBuf, Pixel, PixelFormat, Result,
    Rgb8, Rgb16, Rgba8, Rgba16, Sample,
};
use gamut_ifd::{ByteOrder, Ifd, read};

use crate::compression::{Compression, ccitt, deflate, lzw, packbits, predictor};
use crate::ifd::{PhotometricInterpretation, Predictor, SampleFormat};
use crate::info::{self, TiffInfo};
use crate::metadata::{self, TiffMetadata};
use crate::palette::Palette8;
use crate::tags;

/// Decoder for baseline TIFF images.
///
/// Reads chunky strips or tiles compressed with None, PackBits, LZW, Adobe Deflate, Modified
/// Huffman, or Group 4 fax. Supported layouts are 8- and 16-bit grayscale/RGB/RGBA/CMYK, 8-bit
/// palette, and 1-bit bilevel; other compression and colour modes return [`Error::Unsupported`].
///
/// # Choosing a pixel type
///
/// The [`DecodeImage`] impls *present* a page rather than insisting it already match, and every
/// such presentation goes through [`gamut_core::convert`]. Lossless requests are always satisfied:
/// grayscale replicates across RGB channels, RGB gains an opaque alpha, and 8-bit samples widen to
/// 16-bit by `×257` (exact). A request that would *lose* information — dropping alpha, narrowing
/// 16-bit samples to 8, reducing colour to luma — is refused by default; call
/// [`TiffDecoder::convert_policy`] to opt into it.
///
/// Use [`TiffDecoder::info`] to see a page's declared depth before deciding, rather than relying
/// on the conversions above.
///
/// # Refused, deliberately
///
/// Sample formats this crate's `u8`/`u16` model cannot represent — signed integer, IEEE float, and
/// 32-bit samples — return [`Error::Unsupported`] naming the offending tag, never a truncated or
/// reinterpreted image. A 16-bit *half-float* page is refused by its `SampleFormat`, not its depth.
///
/// CMYK is an ink space, not a rearrangement of RGB, so no policy converts into or out of it; a
/// 16-bit CMYK page has no 16-bit ink layout to land in and is presented as [`Cmyk8`] only when the
/// policy permits the narrowing.
#[derive(Debug, Clone, Default)]
pub struct TiffDecoder {
    policy: ConvertPolicy,
}

/// Upper bound on a decoded image's stored bytes, guarding against malformed huge dimensions and
/// decompression bombs (64 MiB — e.g. a 4096×4096 8-bit RGBA image, or a 2896×2896 16-bit RGB one).
///
/// The cap is on *stored* bytes, so a 16-bit image gets half the pixel budget of an 8-bit one at
/// the same channel count — which is the intent: the guard bounds the buffer actually allocated.
const MAX_IMAGE_BYTES: usize = 64 << 20;

/// Rejects a byte count past [`MAX_IMAGE_BYTES`].
///
/// An *allocation* guard, not a validity one: the caller is about to reserve this many bytes for a
/// buffer whose size the file's tags — not its data — declare, so a malformed or hostile header
/// must not be able to name an arbitrary allocation. A file rejected here would fail later anyway;
/// the guard only stops the reservation from happening first, which is why each call site passes
/// its own message.
fn within_size_limit(bytes: usize, message: &'static str) -> Result<()> {
    if bytes > MAX_IMAGE_BYTES {
        return Err(Error::unsupported(env!("CARGO_PKG_NAME"), message));
    }
    Ok(())
}

/// Decoded samples at the width the file stores them in.
///
/// Sub-byte and 8-bit sources land in [`Samples::U8`] — bilevel expanded to 0/255, palette resolved
/// to RGB bytes — and 16-bit sources in [`Samples::U16`]. Keeping the native width here rather than
/// normalising to one type is what lets the 8-bit path stay a move: converting everything to `u16`
/// would make every ordinary image pay a widen and a narrow it does not need.
enum Samples {
    U8(Vec<u8>),
    U16(Vec<u16>),
}

/// An image decoded to interleaved samples in `BlackIsZero`/RGB convention.
///
/// `format` is the layout those samples are *already* in — palette indices are expanded,
/// `WhiteIsZero` is inverted, and bilevel is expanded to 0/255 during decode, so what reaches
/// [`gamut_core::convert`] is a plain grayscale, RGB, RGBA, or CMYK buffer. It is the pairing of
/// `format` with `samples` that is load-bearing: a 4-sample page is `Rgba*` or `Cmyk8` by its
/// photometric tag, which a sample count alone cannot distinguish.
struct DecodedImage {
    dims: Dimensions,
    format: PixelFormat,
    samples: Samples,
}

/// Deserialises packed 16-bit samples from the file's byte order.
///
/// Called *after* the predictor has been reversed, since §14 differencing operates on the samples
/// as the file stores them.
fn samples_u16(packed: &[u8], order: ByteOrder) -> Vec<u16> {
    packed
        .as_chunks::<2>()
        .0
        .iter()
        .map(|s| order.u16([s[0], s[1]]))
        .collect()
}

/// How a decoded image's stored samples map to output pixels.
enum Mode {
    /// Grayscale; `white_is_zero` selects which sample value is white.
    Gray { white_is_zero: bool },
    /// Interleaved RGB.
    Rgb,
    /// Interleaved RGBA (RGB + one extra alpha sample).
    Rgba,
    /// Interleaved CMYK (4 separated ink samples).
    Cmyk,
    /// Palette colour: 8-bit indices into a [`Palette8`] colour table. Boxed because the 768-byte
    /// table would otherwise dwarf the other variants.
    Palette(Box<Palette8>),
}

impl TiffDecoder {
    /// Creates a decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of pages (subfile IFDs) in a TIFF.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the file header or IFD chain is malformed.
    pub fn page_count(&self, data: &[u8]) -> Result<usize> {
        Ok(read(data)?.ifds.len())
    }

    /// Describes page 0 — the page the [`DecodeImage`] impls present — without decoding pixels.
    ///
    /// Reads tags only, so it is cheap to call before committing to a decode. A page this crate
    /// cannot decode is still described; see [`TiffInfo`].
    ///
    /// ```
    /// use gamut_core::{DecodeImage, Dimensions, EncodeImage, Gray16, ImageBuf, ImageRef};
    /// use gamut_tiff::{TiffDecoder, TiffEncoder};
    ///
    /// let dims = Dimensions { width: 2, height: 1 };
    /// let tiff = TiffEncoder::new()
    ///     .encode_to_vec(ImageRef::<Gray16>::new(&[4660, 43981], dims)?)?;
    ///
    /// // Choose the pixel type from what the file declares, rather than guessing and retrying.
    /// let info = TiffDecoder::new().info(&tiff)?;
    /// assert_eq!(info.bits_per_sample, 16);
    /// if info.bits_per_sample == 16 {
    ///     let image: ImageBuf<Gray16> = TiffDecoder::new().decode_image(&tiff)?;
    ///     assert_eq!(image.as_samples(), &[4660, 43981]);
    /// }
    /// # Ok::<(), gamut_core::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] for a malformed header or IFD chain, or
    /// [`Error::Unsupported`] for an unrecognised on-disk code or a page whose samples disagree
    /// about their depth or format.
    pub fn info(&self, data: &[u8]) -> Result<TiffInfo> {
        self.info_page(data, 0)
    }

    /// Describes page `page` of a multi-page TIFF (page 0 is the first) without decoding pixels.
    ///
    /// # Errors
    ///
    /// As [`Self::info`], plus [`Error::InvalidInput`] for an out-of-range page.
    pub fn info_page(&self, data: &[u8], page: usize) -> Result<TiffInfo> {
        let file = read(data)?;
        let ifd = file.ifds.get(page).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: page index out of range")
        })?;
        info::page_info(ifd, file.order)
    }

    /// Reads the metadata a TIFF carries, without decoding pixels.
    ///
    /// IFD 0 supplies the XMP, IPTC-IIM and ICC payloads and the `ExifIFD` sub-IFD; the last IFD
    /// of the main chain supplies the C2PA manifest store (C2PA 2.4 §A.3.6). Every byte-carried
    /// payload comes back **verbatim** — this crate parses none of them — so a block written by
    /// [`TiffEncoder::with_metadata`](crate::TiffEncoder::with_metadata) reads back identical.
    /// The Exif directory is a directory model rather than a byte range, so what it promises is
    /// narrower and is stated on [`TiffMetadata::exif`](crate::TiffMetadata::exif); the shapes it
    /// could not promise for are refused by the encode rather than written.
    /// Use [`c2pa_exclusions`](crate::c2pa_exclusions) for *where* the store sits.
    ///
    /// ```
    /// use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef};
    /// use gamut_tiff::{TiffDecoder, TiffEncoder, TiffMetadata};
    ///
    /// let dims = Dimensions { width: 2, height: 1 };
    /// let tiff = TiffEncoder::new()
    ///     .with_metadata(TiffMetadata::new().with_xmp(b"<x:xmpmeta/>".to_vec()))
    ///     .encode_to_vec(ImageRef::<Gray8>::new(&[7, 9], dims)?)?;
    ///
    /// let meta = TiffDecoder::new().metadata(&tiff)?;
    /// assert_eq!(meta.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
    /// # Ok::<(), gamut_core::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] for a malformed header or IFD chain, or for a **followed**
    /// pointer that does not resolve into a tree: an out-of-bounds or unparseable target, two
    /// pointers naming one directory, or nesting below the `ExifIFD` → `InteroperabilityIFD` pair
    /// — two levels under IFD 0, the deepest tree those two tags legitimately reach (EXIF 2.3
    /// §4.6.3), and the same bound
    /// [`TiffEncoder::with_metadata`](crate::TiffEncoder::with_metadata) writes within.
    ///
    /// Which pointers are followed depends on the **level**, so which ones can fail this call does
    /// too. At **IFD 0** only `ExifIFD` (34665) is followed: a `SubIFDs` (330), `GPSInfo` (34853)
    /// or `InteroperabilityIFD` (40965) field on the page itself is left as the integer it was
    /// read as, so however broken it is it cannot fail this call. **Inside the returned Exif
    /// directory all four** are followed, because that directory is handed back and an unresolved
    /// pointer in it would be a raw offset into the source file — so there, unlike at IFD 0, an
    /// unreadable target under any of the four *does* fail the call. And only IFD 0's subtree is
    /// walked at all: a pointer on any later page of a multi-page document is never resolved, not
    /// even one naming a directory IFD 0's own subtree also names.
    ///
    /// **This can fail on a file [`decode_image`](DecodeImage::decode_image) decodes happily**,
    /// and that is deliberate. Pixel decoding never follows a metadata pointer, so a broken
    /// `ExifIFD` offset cannot stop it; this method does follow one, and the alternative to
    /// failing is reporting `exif: None` for a directory the file plainly declares — silent loss
    /// a caller cannot tell apart from "there is no EXIF here". A caller that wants a partial
    /// answer can walk the re-exported [`read`](crate::read) / [`gamut_ifd::read_tree`] spine
    /// itself and decide per pointer. (`gamut-dng` degrades instead of failing, because there the
    /// metadata is incidental to a raw *image* decode that must still succeed.)
    pub fn metadata(&self, data: &[u8]) -> Result<TiffMetadata> {
        metadata::read_metadata(data)
    }

    /// Selects which lossy conversions a typed decode may perform.
    ///
    /// Defaults to [`ConvertPolicy::lossless`], under which a layout that cannot hold the page
    /// exactly is refused. Pass [`ConvertPolicy::permissive`] for the "just give me RGB" behaviour
    /// an application usually wants:
    ///
    /// ```no_run
    /// use gamut_core::{convert::ConvertPolicy, DecodeImage, ImageBuf, Rgb8};
    /// use gamut_tiff::TiffDecoder;
    ///
    /// # fn main() -> gamut_core::Result<()> {
    /// # let bytes: &[u8] = &[];
    /// let decoder = TiffDecoder::new().convert_policy(ConvertPolicy::permissive());
    /// let rgb: ImageBuf<Rgb8> = decoder.decode_image(bytes)?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn convert_policy(mut self, policy: ConvertPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Decodes page `page` of a multi-page TIFF to interleaved 8-bit [`Rgb8`] (page 0 is the
    /// first). Multi-page access is TIFF-specific, so it stays inherent; the [`DecodeImage`] impls
    /// present page 0.
    ///
    /// Shorthand for [`TiffDecoder::decode_page_as`] at [`Rgb8`], and subject to the same
    /// [`ConvertPolicy`]: an RGBA, CMYK, or 16-bit page is refused unless the policy permits the
    /// loss.
    ///
    /// # Errors
    ///
    /// As [`TiffDecoder::decode_page_as`].
    pub fn decode_page(&self, data: &[u8], page: usize) -> Result<ImageBuf<Rgb8>> {
        self.decode_page_as(data, page)
    }

    /// Decodes page `page` of a multi-page TIFF and presents it as pixel layout `P`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] for malformed input or an out-of-range page,
    /// [`Error::Unsupported`] for a feature not yet implemented, or [`Error::Unsupported`] for a
    /// conversion to `P` that would lose information this decoder's [`ConvertPolicy`] does not
    /// permit.
    pub fn decode_page_as<P: Pixel>(&self, data: &[u8], page: usize) -> Result<ImageBuf<P>> {
        let img = decode_page_samples(data, page)?;
        match img.samples {
            Samples::U8(ref samples) => {
                convert_from_raw(RawImage::new(samples, img.format, img.dims)?, self.policy)
            }
            // CMYK is the one 16-bit layout with no `PixelFormat` to name it: the ink model has no
            // 16-bit member. Narrowing it to `Cmyk8` here is the same loss the engine would gate,
            // so it answers to the same policy rather than inventing a second rule.
            Samples::U16(samples) if img.format == PixelFormat::Cmyk8 => {
                if self.policy.depth() != DepthPolicy::Rescale {
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "TIFF: 16-bit CMYK needs a depth policy to reach 8-bit ink samples",
                    ));
                }
                let narrowed: Vec<u8> = samples
                    .iter()
                    .map(|&v| u8::from_full_range_u16(v))
                    .collect();
                convert_from_raw(
                    RawImage::new(&narrowed, PixelFormat::Cmyk8, img.dims)?,
                    self.policy,
                )
            }
            Samples::U16(ref samples) => {
                convert_from_raw(RawImage::new(samples, img.format, img.dims)?, self.policy)
            }
        }
    }
}

impl DecodeImage<Rgb8> for TiffDecoder {
    /// Grayscale and palette pages widen into RGB. Dropping an alpha or ink channel, and narrowing
    /// 16-bit samples, each need the matching [`ConvertPolicy`].
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgb8>> {
        self.decode_page_as(data, 0)
    }
}

impl DecodeImage<Rgba8> for TiffDecoder {
    /// RGB gains opaque alpha; grayscale is replicated then made opaque. Narrowing 16-bit samples
    /// needs a [`DepthPolicy`].
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgba8>> {
        self.decode_page_as(data, 0)
    }
}

impl DecodeImage<Cmyk8> for TiffDecoder {
    /// Errors unless the page is CMYK; the samples pass through unchanged. CMYK is an ink space,
    /// not a rearrangement of RGB, so no policy converts into or out of it — but a 16-bit CMYK page
    /// still needs a [`DepthPolicy`] to reach 8-bit ink samples.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Cmyk8>> {
        self.decode_page_as(data, 0)
    }
}

impl DecodeImage<Gray8> for TiffDecoder {
    /// A grayscale page passes through unchanged. A colour page needs a
    /// [`LumaPolicy`](gamut_core::convert::LumaPolicy), and a 16-bit one a [`DepthPolicy`].
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Gray8>> {
        self.decode_page_as(data, 0)
    }
}

impl DecodeImage<Gray16> for TiffDecoder {
    /// A 16-bit grayscale page passes through; an 8-bit one widens by `×257`. A colour page needs
    /// a [`LumaPolicy`](gamut_core::convert::LumaPolicy).
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Gray16>> {
        self.decode_page_as(data, 0)
    }
}

impl DecodeImage<Rgb16> for TiffDecoder {
    /// Grayscale is replicated across channels and 8-bit samples widen by `×257`. Dropping an
    /// alpha or ink channel needs the matching [`ConvertPolicy`].
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgb16>> {
        self.decode_page_as(data, 0)
    }
}

impl DecodeImage<Rgba16> for TiffDecoder {
    /// The widest target: RGB gains opaque alpha, grayscale is replicated then made opaque, and
    /// 8-bit samples widen by `×257` — all losslessly.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgba16>> {
        self.decode_page_as(data, 0)
    }
}

fn decode_page_samples(data: &[u8], page: usize) -> Result<DecodedImage> {
    let file = read(data)?;
    let ifd = file.ifds.get(page).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: page index out of range")
    })?;

    // Everything the page *declares* comes from one shared reader, so the probe and the decoder can
    // never disagree about a default; what follows here is purely which of those declarations this
    // decoder is willing to act on.
    let info = info::page_info(ifd, file.order)?;
    let (width, height) = (info.width as usize, info.height as usize);
    let compression = info.compression;
    if !matches!(
        compression,
        Compression::None
            | Compression::PackBits
            | Compression::CcittRle
            | Compression::CcittGroup4Fax
            | Compression::Lzw
            | Compression::Deflate
    ) {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "TIFF: compression not supported yet",
        ));
    }
    if ifd.get_u32(tags::PLANAR_CONFIGURATION).unwrap_or(1) != 1 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "TIFF: planar configuration not supported yet",
        ));
    }

    if ifd.get_u32(tags::FILL_ORDER).unwrap_or(1) != 1 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "TIFF: FillOrder 2 not supported",
        ));
    }
    let spp = info.samples_per_pixel as usize;
    let bps = info.bits_per_sample;

    // Sample *format* is checked before sample *depth*, and that order is the point: a 16-bit
    // half-float page (`bps = 16`, `SampleFormat = 3`) passes every depth gate below and would
    // decode to plausible nonsense if read as unsigned. Refusing by format first means the
    // diagnostic names the real problem, and no non-integer encoding can reach the sample path.
    match info.sample_format {
        SampleFormat::UnsignedInteger => {}
        SampleFormat::SignedInteger => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: signed-integer samples not supported",
            ));
        }
        SampleFormat::FloatingPoint => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: floating-point samples not supported",
            ));
        }
        SampleFormat::Undefined => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: undefined sample format not supported",
            ));
        }
    }

    if matches!(
        compression,
        Compression::CcittRle | Compression::CcittGroup4Fax
    ) && bps != 1
    {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "TIFF: CCITT coding requires a bilevel image",
        ));
    }
    let use_predictor = info.predictor == Predictor::HorizontalDifferencing;
    if use_predictor && !matches!(bps, 8 | 16) {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "TIFF: predictor requires 8- or 16-bit samples",
        ));
    }

    // Bytes of one stored (packed) row, before unpacking to output samples. Every depth this
    // decoder cannot unpack is rejected *here*, above the photometric table, so the diagnostic
    // names the depth rather than blaming the colour mode: the table's catch-all would otherwise
    // report a 32-bit RGB page as an unsupported photometric/sample combination.
    let stored_row_bytes = match bps {
        8 => width
            .checked_mul(spp)
            .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: image too large"))?,
        16 => width
            .checked_mul(spp)
            .and_then(|n| n.checked_mul(2))
            .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: image too large"))?,
        // Sub-byte samples pack across the whole row, then pad to a byte boundary. Written for any
        // `spp` rather than relying on the photometric table below to have restricted it to 1.
        1 => width
            .checked_mul(spp)
            .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: image too large"))?
            .div_ceil(8),
        32 => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: 32-bit samples not supported",
            ));
        }
        _ => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: unsupported bits per sample",
            ));
        }
    };

    // How stored samples become the decoded output (TIFF 6.0 §8 PhotometricInterpretation).
    let mode = match (spp, bps, info.photometric) {
        (1, 1 | 8 | 16, PhotometricInterpretation::WhiteIsZero) => Mode::Gray {
            white_is_zero: true,
        },
        (1, 1 | 8 | 16, PhotometricInterpretation::BlackIsZero) => Mode::Gray {
            white_is_zero: false,
        },
        (3, 8 | 16, PhotometricInterpretation::Rgb) => Mode::Rgb,
        (4, 8 | 16, PhotometricInterpretation::Rgb) => Mode::Rgba,
        (4, 8 | 16, PhotometricInterpretation::Cmyk) => Mode::Cmyk,
        (1, 8, PhotometricInterpretation::Palette) => {
            let cm = ifd.get_u32_vec(tags::COLOR_MAP).ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "TIFF: palette image missing ColorMap",
                )
            })?;
            Mode::Palette(Box::new(Palette8::from_tiff_colormap(&cm)?))
        }
        _ => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "TIFF: photometric/sample combination not supported yet",
            ));
        }
    };

    let stored_total = stored_row_bytes
        .checked_mul(height)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: image too large"))?;
    within_size_limit(stored_total, "TIFF: image exceeds the size limit")?;

    // Reassemble the stored (packed) row bytes from tiles or strips.
    let layout = Layout {
        width,
        height,
        spp,
        bps,
        stored_row_bytes,
        compression,
        order: info.byte_order,
    };
    let tiled = info.tiled;
    let mut packed = if tiled {
        decode_tiles(ifd, data, &layout, use_predictor)?
    } else {
        decode_strips(ifd, data, &layout)?
    };
    debug_assert_eq!(packed.len(), stored_total);

    // Reverse the horizontal-differencing predictor before unpacking. Tiles were already handled
    // per tile, inside `decode_tiles`, since each tile is predicted independently.
    if use_predictor && !tiled {
        if bps == 16 {
            predictor::reverse16(&mut packed, stored_row_bytes, spp, layout.order);
        } else {
            predictor::reverse(&mut packed, stored_row_bytes, spp);
        }
    }

    // Unpack the stored bytes into output samples per the photometric mode. 16-bit samples are
    // deserialised from the file's byte order here — after the predictor, which by §14 operates on
    // the samples as stored.
    let (format, samples) = match mode {
        Mode::Rgb if bps == 16 => (
            PixelFormat::Rgb16,
            Samples::U16(samples_u16(&packed, layout.order)),
        ),
        Mode::Rgb => (PixelFormat::Rgb8, Samples::U8(packed)),
        Mode::Rgba if bps == 16 => (
            PixelFormat::Rgba16,
            Samples::U16(samples_u16(&packed, layout.order)),
        ),
        Mode::Rgba => (PixelFormat::Rgba8, Samples::U8(packed)),
        // There is no `Cmyk16`, so a 16-bit ink page is tagged by its ink model and carries its
        // native width; `decode_page_as` settles the narrowing against the policy.
        Mode::Cmyk if bps == 16 => (
            PixelFormat::Cmyk8,
            Samples::U16(samples_u16(&packed, layout.order)),
        ),
        Mode::Cmyk => (PixelFormat::Cmyk8, Samples::U8(packed)),
        Mode::Gray { white_is_zero } if bps == 16 => {
            let mut px = samples_u16(&packed, layout.order);
            if white_is_zero {
                for v in &mut px {
                    *v = u16::MAX - *v;
                }
            }
            (PixelFormat::Gray16, Samples::U16(px))
        }
        Mode::Gray { white_is_zero } if bps == 8 => {
            let mut px = packed;
            if white_is_zero {
                for v in &mut px {
                    *v = 255 - *v;
                }
            }
            (PixelFormat::Gray8, Samples::U8(px))
        }
        Mode::Gray { white_is_zero } => {
            // bps == 1: expand each MSB-first bit to a 0/255 sample.
            let mut px = Vec::with_capacity(width * height);
            for y in 0..height {
                let row = &packed[y * stored_row_bytes..(y + 1) * stored_row_bytes];
                for x in 0..width {
                    let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
                    let white = if white_is_zero { bit == 0 } else { bit == 1 };
                    px.push(if white { 255 } else { 0 });
                }
            }
            (PixelFormat::Gray8, Samples::U8(px))
        }
        Mode::Palette(palette) => {
            // Each 8-bit index selects an RGB triple from the colour table.
            let mut px = Vec::with_capacity(width * height * 3);
            for &idx in &packed {
                px.extend_from_slice(&palette.entry(idx));
            }
            (PixelFormat::Rgb8, Samples::U8(px))
        }
    };

    Ok(DecodedImage {
        dims: Dimensions {
            width: width as u32,
            height: height as u32,
        },
        format,
        samples,
    })
}

/// The decoded image's storage parameters, shared by the strip and tile readers.
struct Layout {
    width: usize,
    height: usize,
    spp: usize,
    bps: u32,
    stored_row_bytes: usize,
    compression: Compression,
    /// The file's byte order — how 16-bit samples and the 16-bit predictor read and write them.
    order: ByteOrder,
}

/// Decompresses one strip/tile of byte-level data (`None`/PackBits/LZW/Deflate) to `want` bytes.
fn decompress_simple(raw: &[u8], want: usize, compression: Compression) -> Result<Vec<u8>> {
    match compression {
        Compression::None => raw.get(..want).map(<[u8]>::to_vec).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: block shorter than expected")
        }),
        Compression::PackBits => packbits::decode(raw, want),
        Compression::Lzw => lzw::decode(raw, want),
        Compression::Deflate => deflate::decode(raw, want),
        _ => Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "TIFF: compression not supported for this layout",
        )),
    }
}

/// Reassembles the stored row bytes from strips.
fn decode_strips(ifd: &Ifd, data: &[u8], l: &Layout) -> Result<Vec<u8>> {
    let rows_per_strip = match ifd.get_u32(tags::ROWS_PER_STRIP) {
        Some(0) | None => l.height,
        Some(r) => (r as usize).min(l.height),
    };
    let offsets = ifd.get_u32_vec(tags::STRIP_OFFSETS).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: missing StripOffsets")
    })?;
    let counts = ifd.get_u32_vec(tags::STRIP_BYTE_COUNTS).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: missing StripByteCounts")
    })?;
    let strips = l.height.div_ceil(rows_per_strip);
    if offsets.len() != strips || counts.len() != strips {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: strip count mismatch",
        ));
    }
    let mut packed = Vec::with_capacity(l.stored_row_bytes * l.height);
    for (i, (&off, &cnt)) in offsets.iter().zip(&counts).enumerate() {
        let rows = rows_per_strip.min(l.height - i * rows_per_strip);
        let want = rows * l.stored_row_bytes;
        let raw = data
            .get(off as usize..off as usize + cnt as usize)
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: strip out of bounds")
            })?;
        match l.compression {
            Compression::CcittRle => {
                packed.extend_from_slice(&ccitt::mh_decode_strip(raw, rows, l.width)?);
            }
            Compression::CcittGroup4Fax => {
                packed.extend_from_slice(&ccitt::g4_decode_strip(raw, rows, l.width)?);
            }
            other => packed.extend_from_slice(&decompress_simple(raw, want, other)?),
        }
    }
    Ok(packed)
}

/// Reassembles the stored row bytes from tiles (8-bit only), cropping the edge-tile padding.
fn decode_tiles(ifd: &Ifd, data: &[u8], l: &Layout, use_predictor: bool) -> Result<Vec<u8>> {
    if !matches!(l.bps, 8 | 16) {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "TIFF: tiled images require 8- or 16-bit samples",
        ));
    }
    // Every offset below is a byte offset, so the per-pixel stride carries the sample width too.
    let pixel_bytes = l.spp * (l.bps as usize / 8);
    let tw = ifd
        .get_u32(tags::TILE_WIDTH)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: missing TileWidth"))?
        as usize;
    let th = ifd
        .get_u32(tags::TILE_LENGTH)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: missing TileLength"))?
        as usize;
    if tw == 0 || th == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: zero tile dimension",
        ));
    }
    let offsets = ifd
        .get_u32_vec(tags::TILE_OFFSETS)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: missing TileOffsets"))?;
    let counts = ifd.get_u32_vec(tags::TILE_BYTE_COUNTS).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: missing TileByteCounts")
    })?;
    let across = l.width.div_ceil(tw);
    let down = l.height.div_ceil(th);
    if offsets.len() != across * down || counts.len() != across * down {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: tile count mismatch",
        ));
    }
    let tile_row_bytes = tw
        .checked_mul(pixel_bytes)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: tile too large"))?;
    let tile_size = th
        .checked_mul(tile_row_bytes)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: tile too large"))?;
    within_size_limit(tile_size, "TIFF: tile exceeds the size limit")?;
    let mut packed = vec![0u8; l.stored_row_bytes * l.height];
    for ty in 0..down {
        for tx in 0..across {
            let idx = ty * across + tx;
            let (off, cnt) = (offsets[idx] as usize, counts[idx] as usize);
            let raw = data.get(off..off + cnt).ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: tile out of bounds")
            })?;
            let mut tile = decompress_simple(raw, tile_size, l.compression)?;
            // Each tile is predicted independently, so the predictor is reversed here — before the
            // crop-and-blit, while the tile's own rows are still intact.
            if use_predictor {
                if l.bps == 16 {
                    predictor::reverse16(&mut tile, tile_row_bytes, l.spp, l.order);
                } else {
                    predictor::reverse(&mut tile, tile_row_bytes, l.spp);
                }
            }
            let copy_cols = tw.min(l.width - tx * tw);
            for r in 0..th {
                let dst_row = ty * th + r;
                if dst_row >= l.height {
                    break;
                }
                let src = r * tile_row_bytes;
                let dst = dst_row * l.stored_row_bytes + tx * tw * pixel_bytes;
                packed[dst..dst + copy_cols * pixel_bytes]
                    .copy_from_slice(&tile[src..src + copy_cols * pixel_bytes]);
            }
        }
    }
    Ok(packed)
}

#[cfg(test)]
mod tests {
    use gamut_core::{EncodeImage, ImageRef};
    use gamut_ifd::ByteOrder;

    use super::*;
    use crate::encoder::TiffEncoder;

    #[test]
    fn rejects_truncated_file() {
        let dec = TiffDecoder::new();
        let got: Result<ImageBuf<Rgb8>> = dec.decode_image(&[]);
        assert!(got.is_err());
    }

    #[test]
    fn gray_roundtrips_both_orders() {
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            let dims = Dimensions {
                width: 5,
                height: 3,
            };
            let pixels: Vec<u8> = (0..15).collect();
            let mut tiff = Vec::new();
            TiffEncoder::new()
                .with_byte_order(order)
                .encode_image(ImageRef::<Gray8>::new(&pixels, dims).unwrap(), &mut tiff)
                .expect("encode");
            let got: ImageBuf<Gray8> = TiffDecoder::new().decode_image(&tiff).expect("decode");
            assert_eq!(got.dimensions(), dims);
            assert_eq!(got.as_samples(), pixels.as_slice());
        }
    }
}
