//! The TIFF decoder.
//!
//! Decoding proper produces the image in the layout the file natively carries; presenting it as a
//! caller's chosen [`gamut_core::Pixel`] is then a pure conversion, delegated wholesale to
//! [`gamut_core::convert`] so TIFF applies exactly the same widening and narrowing rules as every
//! other gamut decoder. [`TiffDecoder::with_convert_policy`] selects which lossy conversions are
//! permitted; the default permits none. [`TiffDecoder::with_limits`] bounds what a page may make
//! it allocate.

use gamut_core::convert::{
    ConvertPolicy, DepthPolicy, RawImage, convert_from_raw, unpremultiply_alpha,
};
use gamut_core::{
    Bilevel, Cmyk8, DecodeImage, DecodeLimits, Dimensions, Error, Gray8, Gray16, GrayAlpha8,
    GrayAlpha16, ImageBuf, Indexed8, Palette, Pixel, PixelFormat, Result, Rgb8, Rgb16, Rgba8,
    Rgba16, Sample,
};
use gamut_ifd::{ByteOrder, Ifd, ZeroRowsPerStrip, extent_slices, read, rows_per_strip};

use crate::compression::{Compression, ccitt, deflate, lzw, packbits, predictor};
use crate::ifd::{PhotometricInterpretation, Predictor, SampleFormat};
use crate::info::{self, ExtraSample, TiffInfo};
use crate::metadata::{self, TiffMetadata};
use crate::palette::Palette8;
use crate::tags;

/// Decoder for baseline TIFF images.
///
/// Reads chunky strips or tiles compressed with None, PackBits, LZW, Adobe Deflate, Modified
/// Huffman, or Group 4 fax. Supported layouts are 8- and 16-bit grayscale/grey + alpha/RGB/RGBA/
/// CMYK, 8-bit palette, and 1-bit bilevel; other compression and colour modes return an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error.
///
/// # Alpha
///
/// An alpha sample is read by its `ExtraSamples` code ([`TiffInfo::extra_sample`]) and always
/// presented as *unassociated* alpha, the only alpha layout gamut's pixel types name: associated
/// (premultiplied) colour is un-premultiplied by
/// [`unpremultiply_alpha`](gamut_core::convert::unpremultiply_alpha), and unassociated alpha
/// passes through. As libtiff reads them, a four-sample RGB page declaring `ExtraSamples = 0`
/// (unspecified), or no `ExtraSamples` at all, is read as associated alpha; a grey page carries
/// alpha only under codes 1 and 2. Un-premultiplying is exact only up to the
/// precision premultiplying kept, so re-premultiplying the result reproduces a well-formed
/// associated page exactly.
///
/// # Choosing a pixel type
///
/// The [`DecodeImage`] impls *present* a page rather than insisting it already match, and every
/// such presentation goes through [`gamut_core::convert`]. Lossless requests are always satisfied:
/// grayscale replicates across RGB channels, RGB gains an opaque alpha, and 8-bit samples widen to
/// 16-bit by `×257` (exact). A request that would *lose* information — dropping alpha, narrowing
/// 16-bit samples to 8, reducing colour to luma — is refused by default; call
/// [`TiffDecoder::with_convert_policy`] to opt into it.
///
/// A 1-bit page is natively [`Bilevel`] (0/255 samples), so it passes through to that layout and
/// widens losslessly to every grey or colour one. A palette page is natively [`Indexed8`]: that
/// layout receives its bare indices, every other layout the RGB they select, and
/// [`TiffDecoder::decode_page_indexed8`] returns the indices with their colour table.
///
/// Use [`TiffDecoder::info`] to see a page's declared depth before deciding, rather than relying
/// on the conversions above.
///
/// # Refused, deliberately
///
/// Sample formats this crate's `u8`/`u16` model cannot represent — signed integer, IEEE float, and
/// 32-bit samples — return an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error
/// naming the offending tag, never a truncated or
/// reinterpreted image. A 16-bit *half-float* page is refused by its `SampleFormat`, not its depth.
///
/// CMYK is an ink space, not a rearrangement of RGB, so no policy converts into or out of it; a
/// 16-bit CMYK page has no 16-bit ink layout to land in and is presented as [`Cmyk8`] only when the
/// policy permits the narrowing.
#[derive(Debug, Clone, Default)]
pub struct TiffDecoder {
    policy: ConvertPolicy,
    limits: DecodeLimits,
}

/// Rejects a byte count past `max_bytes` — [`DecodeLimits::max_image_bytes`], opt-in and unbounded
/// by default, when the dimension and pixel caps are what bound a page.
///
/// An *allocation* guard, not a validity one: the caller is about to reserve this many bytes for a
/// buffer whose size the file's tags — not its data — declare, so a malformed or hostile header
/// must not be able to name an arbitrary allocation. A file rejected here would fail later anyway;
/// the guard only stops the reservation from happening first, which is why each call site passes
/// its own message. The cap is on *stored* bytes, so a 16-bit image gets half the pixel budget of
/// an 8-bit one at the same channel count — which is the intent: the guard bounds the buffer
/// actually allocated.
fn within_size_limit(bytes: usize, max_bytes: usize, message: &'static str) -> Result<()> {
    if bytes > max_bytes {
        return Err(Error::unsupported(env!("CARGO_PKG_NAME"), message));
    }
    Ok(())
}

/// Decoded samples at the width the file stores them in.
///
/// Sub-byte and 8-bit sources land in [`Samples::U8`] — bilevel expanded to 0/255 — and 16-bit
/// sources in [`Samples::U16`]. Keeping the native width here rather than normalising to one type
/// is what lets the 8-bit path stay a move: converting everything to `u16` would make every
/// ordinary image pay a widen and a narrow it does not need.
///
/// A palette page keeps its indices and its colour table apart, in [`Samples::Indexed`], until the
/// requested layout is known: [`Indexed8`] takes the indices as they are, and every other layout
/// takes the RGB they select.
enum Samples {
    U8(Vec<u8>),
    U16(Vec<u16>),
    /// 8-bit palette indices, and the `ColorMap` they select from. Boxed as in [`Mode::Palette`].
    Indexed(Vec<u8>, Box<Palette8>),
}

/// An image decoded to interleaved samples in `BlackIsZero`/RGB convention.
///
/// `format` is the layout those samples are *already* in — `WhiteIsZero` is inverted and bilevel
/// is expanded to 0/255 [`Bilevel`] samples during decode, so what reaches [`gamut_core::convert`]
/// is a plain bilevel, grayscale, RGB, RGBA, or CMYK buffer, or palette indices. It is the pairing
/// of `format` with `samples` that is load-bearing: a 4-sample page is `Rgba*` or `Cmyk8` by its
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
    /// Interleaved RGBA (RGB + one extra alpha sample); `associated` when the colour samples are
    /// premultiplied by it and must be un-premultiplied for presentation.
    Rgba { associated: bool },
    /// Interleaved `BlackIsZero` grey + one extra alpha sample; `associated` as for
    /// [`Mode::Rgba`].
    GrayAlpha { associated: bool },
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
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// file header or IFD chain is malformed.
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
    /// let dims = Dimensions::new(2, 1)?;
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
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for a
    /// malformed header or IFD chain, or an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for an unrecognised on-disk code or a page whose samples disagree
    /// about their depth or format.
    pub fn info(&self, data: &[u8]) -> Result<TiffInfo> {
        self.info_page(data, 0)
    }

    /// Describes page `page` of a multi-page TIFF (page 0 is the first) without decoding pixels.
    ///
    /// # Errors
    ///
    /// As [`Self::info`], plus an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput)
    /// error for an out-of-range page.
    pub fn info_page(&self, data: &[u8], page: usize) -> Result<TiffInfo> {
        let file = read(data)?;
        let ifd = file.ifds.get(page).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: page index out of range")
        })?;
        info::page_info(ifd, file.order)
    }

    /// Reads the metadata a TIFF carries, without decoding pixels.
    ///
    /// IFD 0 supplies the XMP, IPTC-IIM and ICC payloads, the Exif 0th-IFD fields under
    /// [`IFD0_EXIF_TAGS`](crate::IFD0_EXIF_TAGS), and the `ExifIFD` and `GPSInfo` sub-IFDs; the last IFD
    /// of the main chain supplies the C2PA manifest store (C2PA 2.4 §A.3.6). Every byte-carried
    /// payload comes back **verbatim** — this crate parses none of them — so a block written by
    /// [`TiffEncoder::with_tiff_metadata`](crate::TiffEncoder::with_tiff_metadata) reads back identical.
    /// The Exif directory is a directory model rather than a byte range, so what it promises is
    /// narrower and is stated on [`TiffMetadata::exif`](crate::TiffMetadata::exif); the shapes it
    /// could not promise for are refused by the encode rather than written.
    /// Use [`c2pa_exclusions`](crate::c2pa_exclusions) for *where* the store sits.
    ///
    /// ```
    /// use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef};
    /// use gamut_tiff::{TiffDecoder, TiffEncoder, TiffMetadata};
    ///
    /// let dims = Dimensions::new(2, 1)?;
    /// let tiff = TiffEncoder::new()
    ///     .with_tiff_metadata(TiffMetadata::new().with_xmp(b"<x:xmpmeta/>".to_vec()))
    ///     .encode_to_vec(ImageRef::<Gray8>::new(&[7, 9], dims)?)?;
    ///
    /// let meta = TiffDecoder::new().metadata(&tiff)?;
    /// assert_eq!(meta.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
    /// # Ok::<(), gamut_core::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for a
    /// malformed header or IFD chain, or for a **followed**
    /// pointer that does not resolve into a tree: an out-of-bounds or unparseable target, two
    /// pointers naming one directory, or nesting below the `ExifIFD` → `InteroperabilityIFD` pair
    /// — two levels under IFD 0, the deepest tree those two tags legitimately reach (EXIF 2.3
    /// §4.6.3), and the same bound
    /// [`TiffEncoder::with_tiff_metadata`](crate::TiffEncoder::with_tiff_metadata) writes within. It is also
    /// returned for an IFD-0 `ExifIFD` or `GPSInfo` entry naming **two or more** directories:
    /// EXIF 2.32 §4.6.3 A fixes each count at 1, [`TiffMetadata::exif`]'s model holds one
    /// of each, and keeping only the first would drop the rest
    /// silently. [`deconstruct`](crate::deconstruct) still walks such a file.
    ///
    /// Which pointers are followed depends on the **level**, so which ones can fail this call does
    /// too. At **IFD 0** only `ExifIFD` (34665) and `GPSInfo` (34853) are followed: a `SubIFDs`
    /// (330) or `InteroperabilityIFD` (40965) field on the page itself is left as the integer it
    /// was read as, so however broken it is it cannot fail this call. **Inside the returned Exif
    /// and GPS directories all four** are followed, because those directories are handed back and
    /// an unresolved pointer in them would be a raw offset into the source file — so there, unlike
    /// at IFD 0, an unreadable target under any of the four *does* fail the call. And only IFD 0's subtree is
    /// walked at all: a pointer on any later page of a multi-page document is never resolved, not
    /// even one naming a directory IFD 0's own subtree also names.
    ///
    /// **This can fail on a file [`decode_image`](DecodeImage::decode_image) decodes happily**,
    /// and that is deliberate. Pixel decoding never follows a metadata pointer, so a broken
    /// `ExifIFD` offset cannot stop it; this method does follow one, and the alternative to
    /// failing is reporting `exif: None` for a directory the file plainly declares — silent loss
    /// a caller cannot tell apart from "there is no EXIF here". A caller that wants a partial
    /// answer can walk the [`gamut_ifd::read`] / [`gamut_ifd::IfdReader::read_tree`] spine
    /// itself and decide per pointer. (`gamut-dng` degrades instead of failing, because there the
    /// metadata is incidental to a raw *image* decode that must still succeed.)
    ///
    /// **The refusal is whole, and it is keyed to the directory, not to the page count.** When a
    /// followed pointer fails, no field is returned — not the XMP, IPTC, ICC or C2PA blocks either,
    /// even though none of them came from that pointer and all of them are readable. What can
    /// fail the call is IFD 0's `ExifIFD` or `GPSInfo` (or a pointer inside the directory either
    /// names), in a file
    /// of any length; a pointer on a later page cannot. The two meet unevenly at one shape: a
    /// **single-page** file's only page is both IFD 0 and the last IFD the C2PA store is taken
    /// from, so a dangling `ExifIFD` there costs the caller the store too, while the same dangling
    /// pointer on page 1 of a two-page file costs nothing. That is accepted rather than patched:
    /// returning the blocks beside an `Exif` failure needs a partial-result shape this type does
    /// not have, and collecting the blocks before resolving would make success depend on read
    /// order. A caller that needs the store from such a file has it without this method:
    /// [`c2pa_exclusions`](crate::c2pa_exclusions) follows no pointer, and its
    /// [`store`](gamut_ifd::C2paExclusions::store) range locates the store's bytes in the
    /// file.
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
    /// let decoder = TiffDecoder::new().with_convert_policy(ConvertPolicy::permissive());
    /// let rgb: ImageBuf<Rgb8> = decoder.decode_image(bytes)?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn with_convert_policy(mut self, policy: ConvertPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Sets the image limits: the per-side dimension caps, the pixel-count cap and the opt-in
    /// stored-sample byte budget ([`DecodeLimits::max_image_bytes`]). A page past any of them is
    /// refused with an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error before
    /// anything is allocated for its pixels, and so is a tile whose own dimensions or stored bytes
    /// exceed them — a tile is reserved whole, so it is bounded like a page.
    ///
    /// The default is the workspace's [`DecodeLimits::new`]: each side at most 32768 and at most
    /// 16384 × 16384 pixels, with no byte budget. TIFF's `ImageWidth`/`ImageLength` (and tile
    /// sides) are LONGs that can declare far more; such a page is refused by default and needs
    /// widened limits.
    ///
    /// The budget counts the samples as the file *stores* them — 1-bit rows packed, palette
    /// indices unexpanded — because that is the buffer the tags make this decoder reserve.
    #[must_use]
    pub fn with_limits(mut self, limits: DecodeLimits) -> Self {
        self.limits = limits;
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
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for
    /// malformed input or an out-of-range page, an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a feature not yet
    /// implemented, or an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a
    /// conversion to `P` that would lose information this decoder's [`ConvertPolicy`] does not
    /// permit.
    pub fn decode_page_as<P: Pixel>(&self, data: &[u8], page: usize) -> Result<ImageBuf<P>> {
        let img = decode_page_samples(data, page, self.limits)?;
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
            // Palette indices are not a colour layout, so the conversion engine moves them only
            // into `Indexed8` itself; every other request is served the RGB they select, which the
            // engine then presents like any RGB page.
            Samples::Indexed(ref indices, _) if P::FORMAT == PixelFormat::Indexed8 => {
                convert_from_raw(
                    RawImage::new(indices, PixelFormat::Indexed8, img.dims)?,
                    self.policy,
                )
            }
            Samples::Indexed(ref indices, ref palette) => {
                let rgb: Vec<u8> = indices.iter().flat_map(|&i| palette.entry(i)).collect();
                convert_from_raw(
                    RawImage::new(&rgb, PixelFormat::Rgb8, img.dims)?,
                    self.policy,
                )
            }
        }
    }

    /// Decodes palette-colour page `page` to its bare [`Indexed8`] indices together with the
    /// colour table they select from — the decode-side twin of
    /// [`TiffEncoder::encode_indexed8`](crate::TiffEncoder::encode_indexed8).
    ///
    /// The returned [`Palette`] always holds 256 entries: TIFF 6.0 §5 fixes the `ColorMap` at
    /// `2**BitsPerSample` entries and records no count of the ones in use, so a palette shorter
    /// than 256 that [`encode_indexed8`](crate::TiffEncoder::encode_indexed8) padded with black
    /// comes back with that padding. Each 16-bit `ColorMap` value is reduced to its high byte,
    /// the inverse of the encoder's `×257`.
    ///
    /// ```
    /// use gamut_core::{Dimensions, ImageRef, Indexed8};
    /// use gamut_tiff::{Palette, TiffDecoder, TiffEncoder};
    ///
    /// # fn main() -> gamut_core::Result<()> {
    /// let palette = Palette::new(&[[255, 0, 0], [0, 0, 255]])?;
    /// let indices = [0u8, 1, 1, 0];
    /// let mut tiff = Vec::new();
    /// TiffEncoder::new().encode_indexed8(
    ///     ImageRef::<Indexed8>::new(&indices, Dimensions::new(2, 2)?)?,
    ///     &palette,
    ///     &mut tiff,
    /// )?;
    ///
    /// let page = TiffDecoder::new().decode_page_indexed8(&tiff, 0)?;
    /// assert_eq!(page.image().as_samples(), &indices);
    /// assert_eq!(page.palette().rgb(1), Some([0, 0, 255]));
    /// assert_eq!(page.palette().rgb_entries().len(), 256);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`TiffDecoder::decode_page_as`] at [`Indexed8`]: in particular an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error for a page that is not
    /// palette colour, since no other layout carries indices.
    pub fn decode_page_indexed8(&self, data: &[u8], page: usize) -> Result<IndexedPage> {
        let img = decode_page_samples(data, page, self.limits)?;
        let Samples::Indexed(indices, table) = img.samples else {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                NOT_PALETTE_REFUSAL,
            ));
        };
        let entries: Vec<[u8; 3]> = (0..=u8::MAX).map(|i| table.entry(i)).collect();
        Ok(IndexedPage {
            image: ImageBuf::new(indices, img.dims)?,
            palette: Palette::new(&entries)?,
        })
    }
}

/// A palette-colour page decoded by [`TiffDecoder::decode_page_indexed8`]: its bare [`Indexed8`]
/// indices and the colour table they select from.
///
/// `#[non_exhaustive]` so it can gain fields (as gamut-png's `DecodedPng` carries its palette
/// beside the image) without a breaking change; read it through the accessors, or take both
/// halves with [`IndexedPage::into_parts`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IndexedPage {
    image: ImageBuf<Indexed8>,
    palette: Palette,
}

impl IndexedPage {
    /// The page's palette indices, one byte per pixel.
    #[must_use]
    pub fn image(&self) -> &ImageBuf<Indexed8> {
        &self.image
    }

    /// The page's colour table, always 256 entries (see
    /// [`TiffDecoder::decode_page_indexed8`]).
    #[must_use]
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// Splits the page into its indices and its colour table, without copying either.
    pub fn into_parts(self) -> (ImageBuf<Indexed8>, Palette) {
        (self.image, self.palette)
    }
}

/// The refusal for a palette-index decode of a page that carries no palette.
const NOT_PALETTE_REFUSAL: &str = "TIFF: only a palette-colour page carries Indexed8 indices";

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

impl DecodeImage<GrayAlpha8> for TiffDecoder {
    /// A grey + alpha page passes through (associated alpha un-premultiplied); a grey page gains
    /// opaque alpha. A colour page needs a [`LumaPolicy`](gamut_core::convert::LumaPolicy), and a
    /// 16-bit one a [`DepthPolicy`].
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<GrayAlpha8>> {
        self.decode_page_as(data, 0)
    }
}

impl DecodeImage<GrayAlpha16> for TiffDecoder {
    /// A grey + alpha page passes through (associated alpha un-premultiplied), widening 8-bit
    /// samples by `×257`; a grey page gains opaque alpha. A colour page needs a
    /// [`LumaPolicy`](gamut_core::convert::LumaPolicy).
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<GrayAlpha16>> {
        self.decode_page_as(data, 0)
    }
}

impl DecodeImage<Bilevel> for TiffDecoder {
    /// A 1-bit page passes through as 0/255 samples (`WhiteIsZero` inverted). Any deeper page is a
    /// reduction to two states, so it needs a [`DepthPolicy`] (thresholded at
    /// [`ConvertPolicy::threshold`]), and a colour one a
    /// [`LumaPolicy`](gamut_core::convert::LumaPolicy) as well.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Bilevel>> {
        self.decode_page_as(data, 0)
    }
}

impl DecodeImage<Indexed8> for TiffDecoder {
    /// A palette page's bare indices; any other page is refused with an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error, since no policy turns
    /// colour into indices. The colour table does not fit the single-buffer [`DecodeImage`] shape,
    /// so it comes from [`TiffDecoder::decode_page_indexed8`].
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Indexed8>> {
        self.decode_page_as(data, 0)
    }
}

fn decode_page_samples(data: &[u8], page: usize, limits: DecodeLimits) -> Result<DecodedImage> {
    let file = read(data)?;
    let ifd = file.ifds.get(page).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: page index out of range")
    })?;

    // Everything the page *declares* comes from one shared reader, so the probe and the decoder can
    // never disagree about a default; what follows here is purely which of those declarations this
    // decoder is willing to act on.
    let info = info::page_info(ifd, file.order)?;
    if !limits.admits_dimensions(info.width, info.height) {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "TIFF: image dimensions exceed the decode limits",
        ));
    }
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
        // `Undefined` — the last of §19's four, and the only other one `tiff_sample_format`
        // admits (`SampleFormat` is gamut-ifd's, and `#[non_exhaustive]`).
        _ => {
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

    // Whether an alpha sample is premultiplied (TIFF 6.0 §18 ExtraSamples). Only unassociated
    // alpha is straight: associated alpha is, and so — as libtiff's RGBA reader reads them
    // (`tif_getimage.c`), the crate's oracle — are `Unspecified` and an absent tag.
    let associated = info.extra_sample != Some(ExtraSample::UnassociatedAlpha);

    // How stored samples become the decoded output (TIFF 6.0 §8 PhotometricInterpretation).
    let mode = match (spp, bps, info.photometric) {
        (1, 1 | 8 | 16, PhotometricInterpretation::WhiteIsZero) => Mode::Gray {
            white_is_zero: true,
        },
        (1, 1 | 8 | 16, PhotometricInterpretation::BlackIsZero) => Mode::Gray {
            white_is_zero: false,
        },
        // Grey + alpha is read only when `ExtraSamples` declares the second sample alpha. libtiff
        // extends its `Unspecified`-as-alpha workaround only to pages of more than three samples,
        // so a two-sample grey page that is absent or `Unspecified` has no alpha reading to
        // follow, and stays refused — as does `WhiteIsZero` + alpha.
        (2, 8 | 16, PhotometricInterpretation::BlackIsZero)
            if matches!(
                info.extra_sample,
                Some(ExtraSample::AssociatedAlpha | ExtraSample::UnassociatedAlpha)
            ) =>
        {
            Mode::GrayAlpha { associated }
        }
        (3, 8 | 16, PhotometricInterpretation::Rgb) => Mode::Rgb,
        (4, 8 | 16, PhotometricInterpretation::Rgb) => Mode::Rgba { associated },
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
    within_size_limit(
        stored_total,
        limits.max_image_bytes(),
        "TIFF: image exceeds the size limit",
    )?;

    // Reassemble the stored (packed) row bytes from tiles or strips.
    let layout = Layout {
        width,
        height,
        spp,
        bps,
        stored_row_bytes,
        compression,
        order: info.byte_order,
        limits,
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
    let dims = Dimensions::new(info.width, info.height)?;
    let (format, samples) = match mode {
        Mode::Rgb if bps == 16 => (
            PixelFormat::Rgb16,
            Samples::U16(samples_u16(&packed, layout.order)),
        ),
        Mode::Rgb => (PixelFormat::Rgb8, Samples::U8(packed)),
        Mode::Rgba { associated } if bps == 16 => (
            PixelFormat::Rgba16,
            Samples::U16(straight::<Rgba16>(
                samples_u16(&packed, layout.order),
                dims,
                associated,
            )?),
        ),
        Mode::Rgba { associated } => (
            PixelFormat::Rgba8,
            Samples::U8(straight::<Rgba8>(packed, dims, associated)?),
        ),
        Mode::GrayAlpha { associated } if bps == 16 => (
            PixelFormat::GrayAlpha16,
            Samples::U16(straight::<GrayAlpha16>(
                samples_u16(&packed, layout.order),
                dims,
                associated,
            )?),
        ),
        Mode::GrayAlpha { associated } => (
            PixelFormat::GrayAlpha8,
            Samples::U8(straight::<GrayAlpha8>(packed, dims, associated)?),
        ),
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
            (PixelFormat::Bilevel, Samples::U8(px))
        }
        // The indices stay as stored; `decode_page_as` resolves them once it knows whether the
        // caller wants indices or colour.
        Mode::Palette(palette) => (PixelFormat::Indexed8, Samples::Indexed(packed, palette)),
    };

    Ok(DecodedImage {
        dims,
        format,
        samples,
    })
}

/// Presents alpha-carrying samples as unassociated ("straight") alpha, the only alpha layout a
/// [`PixelFormat`] names: associated samples are un-premultiplied by
/// [`gamut_core::convert::unpremultiply_alpha`], the workspace's one implementation of it,
/// whose rounding inverts libtiff's own premultiply exactly on well-formed input.
fn straight<P: Pixel>(
    samples: Vec<P::Sample>,
    dims: Dimensions,
    associated: bool,
) -> Result<Vec<P::Sample>> {
    if !associated {
        return Ok(samples);
    }
    let mut image = ImageBuf::<P>::new(samples, dims)?;
    unpremultiply_alpha(&mut image);
    Ok(image.into_samples())
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
    /// The page's [`DecodeLimits`]: they bound each tile the tile reader reserves, too.
    limits: DecodeLimits,
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
    // A zero is read as one full-height strip, as libtiff reads it; gamut-dng refuses it.
    let rows_per_strip = rows_per_strip(
        ifd.get_u32(tags::ROWS_PER_STRIP),
        l.height,
        ZeroRowsPerStrip::WholeImage,
        env!("CARGO_PKG_NAME"),
    )?;
    let offsets = ifd.get_u64_vec(tags::STRIP_OFFSETS).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: missing StripOffsets")
    })?;
    let counts = ifd.get_u64_vec(tags::STRIP_BYTE_COUNTS).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: missing StripByteCounts")
    })?;
    let strips = l.height.div_ceil(rows_per_strip);
    if offsets.len() != strips || counts.len() != strips {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: strip count mismatch",
        ));
    }
    let raws = extent_slices(&offsets, &counts, data, env!("CARGO_PKG_NAME"))?;
    let mut packed = Vec::with_capacity(l.stored_row_bytes * l.height);
    for (i, raw) in raws.into_iter().enumerate() {
        let rows = rows_per_strip.min(l.height - i * rows_per_strip);
        let want = rows * l.stored_row_bytes;
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
    let tile_width = ifd
        .get_u32(tags::TILE_WIDTH)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: missing TileWidth"))?;
    let tile_length = ifd
        .get_u32(tags::TILE_LENGTH)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: missing TileLength"))?;
    if tile_width == 0 || tile_length == 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "TIFF: zero tile dimension",
        ));
    }
    if !l.limits.admits_dimensions(tile_width, tile_length) {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "TIFF: tile dimensions exceed the decode limits",
        ));
    }
    let (tw, th) = (tile_width as usize, tile_length as usize);
    let offsets = ifd
        .get_u64_vec(tags::TILE_OFFSETS)
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "TIFF: missing TileOffsets"))?;
    let counts = ifd.get_u64_vec(tags::TILE_BYTE_COUNTS).ok_or_else(|| {
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
    within_size_limit(
        tile_size,
        l.limits.max_image_bytes(),
        "TIFF: tile exceeds the size limit",
    )?;
    let raws = extent_slices(&offsets, &counts, data, env!("CARGO_PKG_NAME"))?;
    let mut packed = vec![0u8; l.stored_row_bytes * l.height];
    for ty in 0..down {
        for tx in 0..across {
            let raw = raws[ty * across + tx];
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
    use gamut_ifd::{ByteOrder, Value};

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
            let dims = Dimensions::new(5, 3).unwrap();
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

    /// A 2-wide, 3-tall, 8-bit single-sample uncompressed layout: 2 stored bytes per row.
    fn gray_layout() -> Layout {
        Layout {
            width: 2,
            height: 3,
            spp: 1,
            bps: 8,
            stored_row_bytes: 2,
            compression: Compression::None,
            order: ByteOrder::LittleEndian,
            limits: DecodeLimits::new(),
        }
    }

    /// Encodes a `width × height` 8-bit grayscale page, tiled when `tile` is given.
    fn gray_tiff(width: u32, height: u32, tile: Option<u32>) -> Vec<u8> {
        let pixels = vec![9u8; (width * height) as usize];
        let image = ImageRef::<Gray8>::new(&pixels, Dimensions::new(width, height).unwrap());
        let encoder = match tile {
            Some(side) => TiffEncoder::new().with_tiling(side, side),
            None => TiffEncoder::new(),
        };
        encoder.encode_to_vec(image.unwrap()).unwrap()
    }

    /// Asserts `got` is an `Unsupported` error carrying exactly `message`.
    fn assert_unsupported<T: std::fmt::Debug>(got: Result<T>, message: &str) {
        let err = got.expect_err("expected an error");
        assert_eq!(err.kind(), gamut_core::ErrorKind::Unsupported, "{err:?}");
        assert_eq!(err.static_message(), Some(message), "{err:?}");
    }

    /// `with_limits` bounds the page by each side and by its stored bytes, each limit inclusive:
    /// a 17×13 page decodes at exactly its own size and is refused one under it on any axis.
    #[test]
    fn with_limits_bounds_each_side_and_the_stored_bytes_inclusively() {
        let tiff = gray_tiff(17, 13, None);
        let decode = |limits: DecodeLimits| {
            TiffDecoder::new()
                .with_limits(limits)
                .decode_image(&tiff)
                .map(|image: ImageBuf<Gray8>| image.dimensions())
        };
        let exact = DecodeLimits::new()
            .with_max_width(17)
            .with_max_height(13)
            .with_max_image_bytes(17 * 13);
        assert_eq!(decode(exact).unwrap(), Dimensions::new(17, 13).unwrap());
        let dims = "TIFF: image dimensions exceed the decode limits";
        assert_unsupported(decode(exact.with_max_width(16)), dims);
        assert_unsupported(decode(exact.with_max_height(12)), dims);
        assert_unsupported(decode(exact.with_max_pixels(17 * 13 - 1)), dims);
        assert_eq!(
            decode(exact.with_max_pixels(17 * 13)).unwrap(),
            Dimensions::new(17, 13).unwrap()
        );
        assert_unsupported(
            decode(exact.with_max_image_bytes(17 * 13 - 1)),
            "TIFF: image exceeds the size limit",
        );
    }

    /// The tile reader reserves one tile at a time against the same byte budget, inclusively: a
    /// 16×1 page in one 16×16 tile stores 16 image bytes but makes the reader reserve 256.
    #[test]
    fn with_limits_bounds_each_tile_by_the_byte_budget() {
        let tiff = gray_tiff(16, 1, Some(16));
        let decode = |bytes: usize| {
            TiffDecoder::new()
                .with_limits(DecodeLimits::new().with_max_image_bytes(bytes))
                .decode_image(&tiff)
                .map(|image: ImageBuf<Gray8>| image.dimensions())
        };
        // The page stores 16 bytes and its one tile 256, so a budget between the two passes the
        // image check and is the tile's to refuse.
        assert_unsupported(decode(255), "TIFF: tile exceeds the size limit");
        assert_eq!(decode(256).unwrap(), Dimensions::new(16, 1).unwrap());
    }

    /// A tile is bounded by the dimension and pixel caps like a page, inclusively: the 16×1 page's
    /// one 16×16 tile passes a 16-pixel-tall cap the page alone would not need.
    #[test]
    fn with_limits_bounds_each_tile_by_the_dimension_caps() {
        let tiff = gray_tiff(16, 1, Some(16));
        let decode = |limits: DecodeLimits| {
            TiffDecoder::new()
                .with_limits(limits)
                .decode_image(&tiff)
                .map(|image: ImageBuf<Gray8>| image.dimensions())
        };
        let tile = "TIFF: tile dimensions exceed the decode limits";
        let exact = DecodeLimits::new().with_max_width(16).with_max_height(16);
        assert_eq!(decode(exact).unwrap(), Dimensions::new(16, 1).unwrap());
        assert_unsupported(decode(exact.with_max_height(15)), tile);
        assert_unsupported(decode(DecodeLimits::new().with_max_pixels(255)), tile);
        assert_eq!(
            decode(DecodeLimits::new().with_max_pixels(256)).unwrap(),
            Dimensions::new(16, 1).unwrap()
        );
    }

    /// The default limits are the workspace's `DecodeLimits::default()`.
    #[test]
    fn the_default_limits_are_the_shared_defaults() {
        assert_eq!(TiffDecoder::new().limits, DecodeLimits::default());
    }

    /// An IFD holding one strip at `offset` of `count` bytes, with `RowsPerStrip` set when given.
    fn one_strip_ifd(rows_per_strip: Option<u32>, offset: u32, count: u32) -> Ifd {
        let mut ifd = Ifd::new();
        if let Some(rows) = rows_per_strip {
            ifd.set(tags::ROWS_PER_STRIP, Value::Long(vec![rows]));
        }
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![offset]));
        ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![count]));
        ifd
    }

    /// Asserts `got` is an `InvalidInput` error carrying exactly `message`.
    fn assert_invalid_input<T: std::fmt::Debug>(got: Result<T>, message: &str) {
        let err = got.expect_err("expected an error");
        assert_eq!(err.kind(), gamut_core::ErrorKind::InvalidInput, "{err:?}");
        assert_eq!(err.static_message(), Some(message), "{err:?}");
    }

    /// `RowsPerStrip` of 0 means the whole image is one strip, the same as an absent tag (TIFF 6.0
    /// §8 defaults it to 2**32 - 1, i.e. "one strip"). This pins the
    /// [`ZeroRowsPerStrip::WholeImage`] policy `decode_strips` passes to
    /// `gamut_ifd::rows_per_strip`, whose own tests pin each alternative of the zero-or-absent
    /// reading. `gamut-dng` refuses the zero instead (`chunk_grid`); the difference is pinned on
    /// both sides.
    #[test]
    fn decode_strips_reads_rows_per_strip_zero_or_absent_as_one_full_height_strip() {
        let data = [1, 2, 3, 4, 5, 6];
        for rows in [Some(0), None] {
            let got = decode_strips(&one_strip_ifd(rows, 0, 6), &data, &gray_layout());
            assert_eq!(got.expect("decode"), data, "RowsPerStrip {rows:?}");
        }
    }

    /// A `RowsPerStrip` beyond the image height decodes as one full-height strip: the strip count
    /// is 1 and the loop clamps that strip's rows to the height. (The separate `.min(l.height)`
    /// clamp this test once stood beside was unobservable for exactly that reason, and was dropped
    /// when the value moved to `gamut_ifd::rows_per_strip`, #1071.)
    #[test]
    fn decode_strips_decodes_an_oversized_rows_per_strip_as_one_strip() {
        let data = [1, 2, 3, 4, 5, 6];
        let got = decode_strips(&one_strip_ifd(Some(100), 0, 6), &data, &gray_layout());
        assert_eq!(got.expect("decode"), data);
    }

    /// A strip whose `offset + count` passes `u32::MAX` is refused as out of bounds by
    /// `gamut_ifd::extent_slices`, on every target width. Before the extents moved into
    /// `gamut-ifd` (#1071) the sum was unchecked `usize` arithmetic, which a 32-bit target could
    /// overflow, so this pin was width-gated; it now holds everywhere, and the error is the shared
    /// helper's message under this crate's origin.
    #[test]
    fn decode_strips_reports_an_extent_past_u32_max_as_out_of_bounds() {
        let got = decode_strips(
            &one_strip_ifd(None, 0xFFFF_FFF0, 0x20),
            &[0; 6],
            &gray_layout(),
        );
        assert_invalid_input(got, "image data out of bounds");
    }

    /// The tile twin of [`decode_strips_reports_an_extent_past_u32_max_as_out_of_bounds`]: the
    /// same checked `offset + count`, the same out-of-bounds refusal.
    #[test]
    fn decode_tiles_reports_an_extent_past_u32_max_as_out_of_bounds() {
        let mut ifd = Ifd::new();
        ifd.set(tags::TILE_WIDTH, Value::Long(vec![2]));
        ifd.set(tags::TILE_LENGTH, Value::Long(vec![3]));
        ifd.set(tags::TILE_OFFSETS, Value::Long(vec![0xFFFF_FFF0]));
        ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(vec![0x20]));
        let got = decode_tiles(&ifd, &[0; 6], &gray_layout(), false);
        assert_invalid_input(got, "image data out of bounds");
    }

    /// `StripOffsets` and `StripByteCounts` must each hold one entry per strip; either one
    /// alone disagreeing is a strip-count mismatch. Each side is asserted separately: when only
    /// one array is off, the arrays also differ from each other, and `extent_slices` would refuse
    /// them under its own message if `decode_strips`' check required both to be wrong.
    #[test]
    fn decode_strips_refuses_either_array_off_the_strip_count() {
        // gray_layout is 3 rows tall; RowsPerStrip absent makes it one strip.
        for (offsets, counts) in [(vec![0, 0], vec![6]), (vec![0], vec![6, 6])] {
            let mut ifd = Ifd::new();
            ifd.set(tags::STRIP_OFFSETS, Value::Long(offsets));
            ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(counts));
            let got = decode_strips(&ifd, &[0; 6], &gray_layout());
            assert_invalid_input(got, "TIFF: strip count mismatch");
        }
    }

    /// The tile twin of [`decode_strips_refuses_either_array_off_the_strip_count`]: a 2×3 page
    /// in 2×3 tiles is one tile, and either array alone holding two entries is a mismatch.
    #[test]
    fn decode_tiles_refuses_either_array_off_the_tile_count() {
        for (offsets, counts) in [(vec![0, 0], vec![6]), (vec![0], vec![6, 6])] {
            let mut ifd = Ifd::new();
            ifd.set(tags::TILE_WIDTH, Value::Long(vec![2]));
            ifd.set(tags::TILE_LENGTH, Value::Long(vec![3]));
            ifd.set(tags::TILE_OFFSETS, Value::Long(offsets));
            ifd.set(tags::TILE_BYTE_COUNTS, Value::Long(counts));
            let got = decode_tiles(&ifd, &[0; 6], &gray_layout(), false);
            assert_invalid_input(got, "TIFF: tile count mismatch");
        }
    }
}
