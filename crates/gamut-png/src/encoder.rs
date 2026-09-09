//! The PNG encoder: a [`PngEncoder`] builder implementing [`gamut_core::EncodeImage`] for each
//! supported pixel layout. This covers the four non-indexed colour types at 8- and 16-bit depth;
//! palette, sub-byte depths, ancillary chunks, and space optimisations layer on in later phases.

use gamut_core::{
    Bilevel, Dimensions, EncodeImage, Error, Gray8, Gray16, GrayAlpha8, GrayAlpha16, ImageRef,
    Indexed8, Pixel, Result, Rgb8, Rgb16, Rgba8, Rgba16,
};
use gamut_deflate::{DeflateEncoder, Level};

use crate::ancillary::{
    Ancillary, PaletteOrigin, PhysicalUnit, SrgbIntent, WrittenHeader, WrittenPalette,
};
use crate::backend::{IdatDeflater, IdatInfo, Registry, run_deflaters};
use crate::chunk::{self, C2paSpan, SIGNATURE};
use crate::color::ColorType;
use crate::filter::{self, FilterStrategy, FilterType};
use crate::palette::PngPalette;
use crate::reduce::{self, Reduced, Reductions};
use crate::{ihdr, pack};

/// IDAT payload cap. A decoder concatenates consecutive IDATs, so the split is transparent; a
/// large-ish cap keeps the 12-byte per-chunk overhead negligible.
const IDAT_MAX: usize = 1 << 16;

/// Whole-image filter strategies tried by [`FilterStrategy::BruteForce`].
///
/// [`FilterStrategy::MinEntropy`] is deliberately **not** here, and that was measured rather than
/// assumed. Across the benchmark corpus it is never the unique winner: it beats `MinSumAbs` on the
/// photographic and palette rows but loses to `MinBigrams` on both, and ties `MinSumAbs` elsewhere.
/// Since this list is resolved by taking the smallest result, a candidate that is dominated
/// everywhere costs a full filter pass and a full DEFLATE for nothing. It stays available as a
/// caller-selectable strategy — the corpus is eight images, not a proof — but it does not earn a
/// slot here. See `STATUS.md`'s heuristic table.
const BRUTE_FORCE_STRATEGIES: [FilterStrategy; 7] = [
    FilterStrategy::None,
    FilterStrategy::Fixed(FilterType::Sub),
    FilterStrategy::Fixed(FilterType::Up),
    FilterStrategy::Fixed(FilterType::Average),
    FilterStrategy::Fixed(FilterType::Paeth),
    FilterStrategy::MinSumAbs,
    FilterStrategy::MinBigrams,
];

/// What [`PngEncoder::encode_with_report`] found in the bytes it wrote.
///
/// Non-exhaustive: a later revision may report a further region without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PngEncodeReport {
    /// Where the C2PA manifest-store chunk landed — the whole `caBX` span a `c2pa.hash.data`
    /// exclusion must cover, and the payload a signer fills — when [`with_c2pa`] or
    /// [`with_c2pa_reserved`] was set; `None` when neither was.
    ///
    /// [`with_c2pa`]: PngEncoder::with_c2pa
    /// [`with_c2pa_reserved`]: PngEncoder::with_c2pa_reserved
    pub c2pa: Option<C2paSpan>,
}

/// A reusable PNG encoder.
#[derive(Debug, Clone)]
pub struct PngEncoder {
    level: Level,
    effort: u8,
    filter: FilterStrategy,
    ancillary: Ancillary,
    auto_reduce: bool,
    clean_transparent: bool,
    backends: Registry<dyn IdatDeflater + Send>,
}

impl Default for PngEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PngEncoder {
    /// Creates an encoder with balanced [`Level::Default`] compression and the
    /// [`FilterStrategy::MinSumAbs`] filter heuristic.
    #[must_use]
    pub fn new() -> Self {
        Self {
            level: Level::Default,
            effort: DeflateEncoder::DEFAULT_EFFORT,
            filter: FilterStrategy::MinSumAbs,
            ancillary: Ancillary::default(),
            auto_reduce: false,
            clean_transparent: false,
            backends: Registry::default(),
        }
    }

    /// Appends a pluggable [`IdatDeflater`] backend for the IDAT zlib stream (issue #278).
    ///
    /// Backends are tried in **push order**; the built-in [`gamut_deflate`] encoder is the implicit
    /// tail, so pushing nothing keeps today's behaviour byte for byte. A backend that returns
    /// `false` from [`IdatDeflater::supports`] (or [`Error::Unsupported`] from
    /// [`IdatDeflater::deflate`]) is skipped; one that accepts and then fails propagates its error
    /// rather than falling back — see the [`backend`](crate::backend) module docs for the full
    /// contract.
    ///
    /// # Interaction with [`with_compression`](Self::with_compression)
    ///
    /// A pushed deflater **bypasses the configured [`Level`]** (and the
    /// [`with_effort`](Self::with_effort) budget) for every stream it accepts: `Level` is a
    /// `gamut-deflate` concept that a foreign backend knows nothing about, and the seam datum is
    /// only the byte stream. The `Level` still governs the built-in tail, i.e. any stream every
    /// pushed backend declines.
    ///
    /// # Cloning shares backends
    ///
    /// [`PngEncoder`] is [`Clone`], and cloning copies the registry *handles*: the clone and the
    /// original drive the **same** backend instances (they are held behind `Arc<Mutex<…>>`, since
    /// encoding takes `&self`). Push a fresh backend instance if you need independent state.
    ///
    /// Takes `&mut self` and returns `&mut Self` — unlike the `with_*` builder setters — because a
    /// registry accumulates rather than replaces.
    pub fn push_backend(&mut self, backend: impl IdatDeflater + 'static) -> &mut Self {
        self.backends
            .push(std::sync::Arc::new(std::sync::Mutex::new(backend)));
        self
    }

    /// Sets the DEFLATE compression [`Level`] used for the image data. [`Level::Best`] is the
    /// space-efficient (slow) setting.
    #[must_use]
    pub fn with_compression(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// Sets the [`Level::Best`] effort budget: the maximum number of optimal-parse refinement
    /// passes (see [`DeflateEncoder::with_effort`]) applied to every zlib stream this encoder emits
    /// — the IDAT image data and compressed ancillary payloads (`iCCP`, `zTXt`).
    ///
    /// Defaults to [`DeflateEncoder::DEFAULT_EFFORT`]; `0` keeps the lazy seed parse only, and
    /// `zopfli`'s default budget is 15. Ignored at every other [`Level`] and by any pushed
    /// [`IdatDeflater`] backend that accepts a stream.
    #[must_use]
    pub fn with_effort(mut self, effort: u8) -> Self {
        self.effort = effort;
        self
    }

    /// Sets the scanline [`FilterStrategy`].
    #[must_use]
    pub fn with_filter(mut self, filter: FilterStrategy) -> Self {
        self.filter = filter;
        self
    }

    /// Rewrites the colour channels of fully transparent pixels before encoding, so runs of
    /// them compress instead of carrying whatever the source left there.
    ///
    /// Nothing a decoder renders changes -- at `alpha == 0` the colour channels are invisible by
    /// definition -- but the stored samples do, so this is **not** lossless in the strict byte
    /// sense [`with_auto_reduce`](Self::with_auto_reduce) keeps. That is why it is off by
    /// default and separate from it: this crate's other reductions are exactly reversible, and
    /// this one is only reversible in what you can see.
    ///
    /// Worth enabling for sprites, icons and UI assets, where invisible colour noise is common
    /// and can cost real bytes. It applies to every layout that carries an alpha channel, at both
    /// 8 and 16 bits per sample; a 16-bit pixel counts as invisible when its whole alpha sample is
    /// zero, and all sixteen bits of each colour sample are cleared. No effect on an image with no
    /// fully transparent pixel, or on a layout with no alpha channel.
    #[must_use]
    pub fn with_transparent_cleanup(mut self, enabled: bool) -> Self {
        self.clean_transparent = enabled;
        self
    }

    /// Enables automatic lossless reduction of any [`EncodeImage`] input to a smaller encoding
    /// when it does not change any pixel: greyscale (at the smallest exactly-representable bit
    /// depth), palette, alpha-channel drop, and 16→8 demotion when every sample's high and low
    /// bytes agree.
    ///
    /// Off by default so the output colour type and depth match the input. Enable it — ideally
    /// with [`Level::Best`] and [`FilterStrategy::BruteForce`] — for the smallest possible files.
    #[must_use]
    pub fn with_auto_reduce(mut self, enabled: bool) -> Self {
        self.auto_reduce = enabled;
        self
    }

    /// Records an image gamma (gAMA chunk). `gamma` is the encoding gamma, e.g. `1.0 / 2.2`.
    #[must_use]
    pub fn with_gamma(mut self, gamma: f64) -> Self {
        self.ancillary.gamma = Some((gamma * 100_000.0).round().max(0.0) as u32);
        self
    }

    /// Records the standard colour-space rendering intent (sRGB chunk).
    #[must_use]
    pub fn with_srgb(mut self, intent: SrgbIntent) -> Self {
        self.ancillary.set_srgb(intent);
        self
    }

    /// Records the white point and RGB primary chromaticities (cHRM chunk), each as `(x, y)`.
    #[must_use]
    pub fn with_chromaticities(
        mut self,
        white: (f64, f64),
        red: (f64, f64),
        green: (f64, f64),
        blue: (f64, f64),
    ) -> Self {
        let q = |v: f64| (v * 100_000.0).round().max(0.0) as u32;
        self.ancillary.chrm = Some([
            q(white.0),
            q(white.1),
            q(red.0),
            q(red.1),
            q(green.0),
            q(green.1),
            q(blue.0),
            q(blue.1),
        ]);
        self
    }

    /// Records the number of significant bits per channel (sBIT chunk). The length must match the
    /// colour type (1 for grey, 2 for grey+alpha, 3 for RGB/indexed, 4 for RGBA).
    ///
    /// Emitted for the colour type actually **written**, which under
    /// [`with_auto_reduce`](Self::with_auto_reduce) may differ from the input's: the entries are
    /// converted where that is lossless (an alpha entry dropped with its channel, RGB collapsed
    /// to grey where the three agree) and the chunk is **omitted, without error,** where the
    /// written colour type or depth cannot carry them — a reduction is never refused to keep a
    /// metadata chunk. See `STATUS.md`, "Chunks that follow the race".
    #[must_use]
    pub fn with_significant_bits(mut self, bits: &[u8]) -> Self {
        self.ancillary.sbit = Some(bits.to_vec());
        self
    }

    /// Records a greyscale background colour (bKGD chunk) for greyscale images.
    ///
    /// Emitted for the colour type actually **written**, which under
    /// [`with_auto_reduce`](Self::with_auto_reduce) may differ from the input's: converted where
    /// that is lossless (to an RGB triple, or to the palette entry holding the grey) and
    /// **omitted, without error,** where the written colour type or depth cannot carry it. See
    /// `STATUS.md`, "Chunks that follow the race".
    #[must_use]
    pub fn with_background_gray(mut self, gray: u16) -> Self {
        self.ancillary.bkgd = Some(gray.to_be_bytes().to_vec());
        self
    }

    /// Records an RGB background colour (bKGD chunk) for truecolour images.
    ///
    /// Emitted for the colour type actually **written**, which under
    /// [`with_auto_reduce`](Self::with_auto_reduce) may differ from the input's: converted where
    /// that is lossless (to one grey sample where the channels agree, or to the palette entry
    /// holding the colour — an opaque one where a transparent twin exists) and **omitted, without
    /// error,** where the written colour type or depth cannot carry it. See `STATUS.md`, "Chunks
    /// that follow the race".
    #[must_use]
    pub fn with_background_rgb(mut self, red: u16, green: u16, blue: u16) -> Self {
        let mut data = Vec::with_capacity(6);
        data.extend_from_slice(&red.to_be_bytes());
        data.extend_from_slice(&green.to_be_bytes());
        data.extend_from_slice(&blue.to_be_bytes());
        self.ancillary.bkgd = Some(data);
        self
    }

    /// Records a palette-index background colour (bKGD chunk) for indexed images.
    ///
    /// The index names an entry of the palette **you** supply to
    /// [`encode_indexed8`](Self::encode_indexed8), and is emitted only there (and only in range).
    /// Under [`with_auto_reduce`](Self::with_auto_reduce) the palette, if one is written, is the
    /// encoder's own, in an order this index never referred to, so the chunk is **omitted,
    /// without error** — set the background as a colour ([`with_background_rgb`](Self::with_background_rgb))
    /// to have it resolved against whatever is written. See `STATUS.md`, "Chunks that follow the
    /// race".
    #[must_use]
    pub fn with_background_index(mut self, index: u8) -> Self {
        self.ancillary.bkgd = Some(vec![index]);
        self
    }

    /// Records the intended physical pixel dimensions (pHYs chunk).
    #[must_use]
    pub fn with_physical_dimensions(mut self, x_ppu: u32, y_ppu: u32, unit: PhysicalUnit) -> Self {
        self.ancillary.set_physical(x_ppu, y_ppu, unit);
        self
    }

    /// Records the last-modification time (tIME chunk), in UTC.
    #[must_use]
    pub fn with_time(
        mut self,
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Self {
        self.ancillary
            .set_time(year, month, day, hour, minute, second);
        self
    }

    /// Adds an uncompressed Latin-1 text annotation (tEXt chunk).
    #[must_use]
    pub fn with_text(mut self, keyword: &str, text: &str) -> Self {
        self.ancillary.add_text_latin1(keyword, text);
        self
    }

    /// Adds a zlib-compressed Latin-1 text annotation (zTXt chunk).
    #[must_use]
    pub fn with_compressed_text(mut self, keyword: &str, text: &str) -> Self {
        self.ancillary.add_text_compressed(keyword, text);
        self
    }

    /// Adds an uncompressed UTF-8 text annotation (iTXt chunk).
    #[must_use]
    pub fn with_international_text(mut self, keyword: &str, text: &str) -> Self {
        self.ancillary.add_text_international(keyword, text);
        self
    }

    /// Embeds raw EXIF metadata (eXIf chunk). `exif` is the EXIF/TIFF byte stream beginning with the
    /// byte-order marker (`II`/`MM`) — for example the bytes produced by `gamut-exif`.
    #[must_use]
    pub fn with_exif(mut self, exif: &[u8]) -> Self {
        self.ancillary.exif = Some(exif.to_vec());
        self
    }

    /// Embeds an ICC colour profile (iCCP chunk), zlib-compressed. `profile` is the raw ICC profile
    /// — for example the bytes produced by `gamut-icc`. (Mutually exclusive with [`Self::with_srgb`]
    /// per the spec; set only one.)
    #[must_use]
    pub fn with_icc_profile(mut self, name: &str, profile: &[u8]) -> Self {
        self.ancillary.iccp = Some((name.to_string(), profile.to_vec()));
        self
    }

    /// Embeds an XMP packet (an iTXt chunk with the standard `XML:com.adobe.xmp` keyword). `xmp` is
    /// the XMP/RDF document — for example the bytes produced by `gamut-xmp`.
    #[must_use]
    pub fn with_xmp(mut self, xmp: &str) -> Self {
        self.ancillary
            .add_text_international("XML:com.adobe.xmp", xmp);
        self
    }

    /// Embeds a C2PA manifest store (the `caBX` chunk, C2PA 2.4 §A.3.2), verbatim and
    /// uncompressed, as the last chunk before `IDAT`.
    ///
    /// `store` is the JUMBF manifest store computed **for this file** by an external signer such
    /// as `c2pa-rs`. It is written exactly where [`with_c2pa_reserved`](Self::with_c2pa_reserved)
    /// puts a placeholder of the same length, so a store built against a reserved encode drops
    /// into the same bytes — see there for the reserve-then-fill flow. The bytes are not parsed
    /// or validated: gamut carries the store, `c2pa-rs` judges it.
    ///
    /// A store is bound to the bytes it was signed over, which is why no gamut re-encode helper —
    /// and never the `gamut-metadata` facade — hands one to this setter: a store copied forward
    /// into a rewritten file is invalid by construction, and `caBX` is *unsafe to copy* for the
    /// same reason. Set only a store computed for the output this encoder is about to write.
    ///
    /// The last of `with_c2pa` / `with_c2pa_reserved` wins; a file carries exactly one store.
    #[must_use]
    pub fn with_c2pa(mut self, store: &[u8]) -> Self {
        self.ancillary.c2pa = Some(store.to_vec());
        self
    }

    /// Reserves `len` bytes for a C2PA manifest store: a `caBX` chunk whose payload is `len` zero
    /// bytes, as the last chunk before `IDAT`.
    ///
    /// The reserve-then-fill flow an external signer needs (C2PA 2.4 §18.5):
    ///
    /// 1. encode with the reservation, via [`encode_with_report`](Self::encode_with_report), which
    ///    names the chunk's span;
    /// 2. hash the output with that **whole** span excluded — length, type, payload and CRC
    ///    (§18.5.4) — and have the signer build the store against it;
    /// 3. encode again with [`with_c2pa`](Self::with_c2pa) and the finished store of the **same
    ///    length**. The encoder's output is byte-reproducible and the store is the last chunk
    ///    before `IDAT`, so the second file differs from the first only inside that span: the
    ///    payload and the chunk CRC. Every other byte, and every offset, is unchanged.
    ///
    /// The reservation is `len` bytes exactly — no slack is added — so ask for what the signer
    /// says it needs (`c2pa-rs` reports a `reserve_size`).
    ///
    /// The last of `with_c2pa` / `with_c2pa_reserved` wins; a file carries exactly one store.
    #[must_use]
    pub fn with_c2pa_reserved(mut self, len: usize) -> Self {
        self.ancillary.c2pa = Some(vec![0; len]);
        self
    }

    /// Encodes `image` as [`EncodeImage::encode_to_vec`] does and reports where the C2PA
    /// manifest-store chunk landed, for the reserve-then-fill flow described at
    /// [`with_c2pa_reserved`](Self::with_c2pa_reserved).
    ///
    /// The report is read back from the bytes written — the same walk
    /// [`PngReport::c2pa`](crate::PngReport::c2pa) performs — so it cannot disagree with what a
    /// later [`deconstruct`](crate::deconstruct) of the same bytes reports, and an indexed image
    /// encoded through [`encode_indexed8`](Self::encode_indexed8) gets the same answer from
    /// `deconstruct(&png)?.c2pa()`.
    ///
    /// # Errors
    ///
    /// As [`EncodeImage::encode_image`].
    pub fn encode_with_report<P: Pixel>(
        &self,
        image: ImageRef<'_, P>,
    ) -> Result<(Vec<u8>, PngEncodeReport)>
    where
        Self: EncodeImage<P>,
    {
        let png = self.encode_to_vec(image)?;
        let c2pa = chunk::find_c2pa(&png);
        Ok((png, PngEncodeReport { c2pa }))
    }

    /// Encodes an 8-bit indexed (palette) image. Indexed colour does not fit the single-buffer
    /// [`EncodeImage`] shape because it needs a separate palette, so it is an inherent method.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if any index is out of range for `palette`.
    pub fn encode_indexed8(
        &self,
        image: ImageRef<'_, Indexed8>,
        palette: &PngPalette,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let indices = image.as_samples();
        let max_index = indices.iter().copied().max().unwrap_or(0);
        if usize::from(max_index) >= palette.len() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: palette index out of range",
            ));
        }
        let dims = image.dimensions();
        // Use the smallest bit depth that holds every index — a free, lossless space win.
        let depth = reduce::index_bit_depth(palette.len());
        let packed;
        let sample_bytes = if depth < 8 {
            packed =
                pack::pack_scanlines(indices, dims.width as usize, dims.height as usize, depth);
            packed.as_slice()
        } else {
            indices
        };
        let plte = palette.plte();
        let trns = palette.trns();
        self.write_png(
            (dims.width, dims.height),
            sample_bytes,
            WrittenHeader {
                color: ColorType::Indexed,
                bit_depth: depth,
                palette: Some(WrittenPalette {
                    plte: &plte,
                    trns,
                    origin: PaletteOrigin::Caller,
                }),
            },
            |out| {
                chunk::write_chunk(out, *b"PLTE", &plte);
                if let Some(alpha) = trns {
                    chunk::write_chunk(out, *b"tRNS", alpha);
                }
            },
            out,
        )
    }

    /// Encodes an 8-bit-per-sample image (samples are already PNG's storage bytes).
    fn encode_8bit<P: Pixel<Sample = u8>>(
        &self,
        image: ImageRef<'_, P>,
        color: ColorType,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let dims = image.dimensions();
        self.write_png(
            (dims.width, dims.height),
            image.as_samples(),
            WrittenHeader::new(color, 8),
            |_| {},
            out,
        )
    }

    /// Encodes one 8-bit alpha-carrying sample buffer: the auto-reduce race if it applies, the
    /// plain layout otherwise.
    ///
    /// Split out of the `EncodeImage` impls so [`cleaned_or_plain`](Self::cleaned_or_plain) can
    /// run it twice over two different sample buffers.
    fn encode_alpha8(
        &self,
        dims: Dimensions,
        samples: &[u8],
        channels: usize,
        color: ColorType,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        if self.auto_reduce {
            return self.write_reduced_or_native(
                dims,
                reduce::analyze8(samples, channels),
                |o| {
                    self.write_png(
                        (dims.width, dims.height),
                        samples,
                        WrittenHeader::new(color, 8),
                        |_| {},
                        o,
                    )
                },
                out,
            );
        }
        self.write_png(
            (dims.width, dims.height),
            samples,
            WrittenHeader::new(color, 8),
            |_| {},
            out,
        )
    }

    /// The 16-bit twin of [`encode_alpha8`](Self::encode_alpha8).
    fn encode_alpha16(
        &self,
        dims: Dimensions,
        samples: &[u16],
        channels: usize,
        color: ColorType,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        if self.auto_reduce {
            return self.write_reduced_or_native(
                dims,
                reduce::analyze16(samples, channels),
                |o| self.encode_16bit(dims, samples, color, o),
                out,
            );
        }
        self.encode_16bit(dims, samples, color, out)
    }

    /// Encodes the image both ways when cleaning changed something, and keeps the smaller file.
    ///
    /// Cleaning collapses every invisible pixel to one colour, which is what makes a palette or a
    /// colour key reachable at all — worth ~31% on a sprite whose invisible pixels carry noise.
    /// But it is a *transform*, not a reduction: it rewrites bytes DEFLATE was already
    /// compressing. Where the invisible pixels carry structure — a gradient that continues under
    /// the transparent region — zeroing them inserts a discontinuity that costs more than the
    /// collapsed palette saves. Measured on `palette64_rgba8`, cleaning is worth −2.3% at 32x32,
    /// **+10.7% at 128x128** and −5.2% at 256x256, with both candidates landing on the same
    /// colour type throughout: the sign genuinely depends on the image.
    ///
    /// So the choice is raced rather than assumed, exactly as
    /// [`write_reduced_or_native`](Self::write_reduced_or_native) races a palette against the
    /// unreduced encoding, and for the same reason: no tuned constant can predict a compressed
    /// size. [`with_transparent_cleanup`](Self::with_transparent_cleanup) therefore means "clean
    /// where it pays", and enabling it can never cost bytes.
    ///
    /// A tie keeps the *plain* encoding: cleaning is only worth its rewritten samples for a
    /// size win, so where there is none the byte-exact candidate stands. See [`prefers_plain`].
    fn cleaned_or_plain(
        &self,
        cleaned: impl FnOnce(&mut Vec<u8>) -> Result<usize>,
        plain: impl FnOnce(&mut Vec<u8>) -> Result<usize>,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let mut cleaned_encoding = Vec::new();
        cleaned(&mut cleaned_encoding)?;
        let mut plain_encoding = Vec::new();
        plain(&mut plain_encoding)?;

        let winner = if prefers_plain(plain_encoding.len(), cleaned_encoding.len()) {
            plain_encoding
        } else {
            cleaned_encoding
        };
        out.extend_from_slice(&winner);
        Ok(winner.len())
    }

    /// The cleaned samples, or `None` to use the caller's buffer unchanged — either because the
    /// knob is off or because the image has no fully transparent pixel.
    fn cleaned_samples(&self, samples: &[u8], channels: usize) -> Option<Vec<u8>> {
        self.clean_transparent
            .then(|| reduce::clean_transparent(samples, channels))
            .flatten()
    }

    /// The 16-bit twin of [`cleaned_samples`](Self::cleaned_samples): the cleaned samples, or
    /// `None` to use the caller's buffer unchanged.
    fn cleaned_samples16(&self, samples: &[u16], channels: usize) -> Option<Vec<u16>> {
        self.clean_transparent
            .then(|| clean_transparent16(samples, channels))
            .flatten()
    }

    /// Encodes a 16-bit-per-sample image, serialising samples big-endian (PNG's network byte order).
    ///
    /// Takes the samples rather than the [`ImageRef`] so the alpha layouts can hand over a cleaned
    /// buffer (see [`cleaned_samples16`](Self::cleaned_samples16)).
    fn encode_16bit(
        &self,
        dims: Dimensions,
        samples: &[u16],
        color: ColorType,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for &sample in samples {
            bytes.extend_from_slice(&sample.to_be_bytes());
        }
        self.write_png(
            (dims.width, dims.height),
            &bytes,
            WrittenHeader::new(color, 16),
            |_| {},
            out,
        )
    }

    /// Shared back end: signature → IHDR → `pre_idat` chunks (e.g. PLTE/tRNS) → the remaining
    /// ancillary chunks, the C2PA store last → filtered +
    /// DEFLATE-compressed scanlines as IDAT(s) → IEND. `sample_bytes` is the image in PNG storage
    /// order; the stride is derived from `written`'s colour type and bit depth. `written` also
    /// carries the palette `pre_idat` writes for an indexed image, which `bKGD` is resolved
    /// against: the ancillary chunks whose shape is the colour type are emitted for the header
    /// written here, not the one the caller set them for (see [`crate::ancillary`]).
    fn write_png<F: FnOnce(&mut Vec<u8>)>(
        &self,
        (width, height): (u32, u32),
        sample_bytes: &[u8],
        written: WrittenHeader<'_>,
        pre_idat: F,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let (color, bit_depth) = (written.color, written.bit_depth);
        // Stride in bytes per pixel (≥1, even for sub-byte depths) and the padded row length.
        let bits_per_pixel = color.channels() * bit_depth as usize;
        let bpp = bits_per_pixel.div_ceil(8).max(1);
        let row_bytes = (width as usize * bits_per_pixel).div_ceil(8);

        let start = out.len();
        out.extend_from_slice(&SIGNATURE);
        ihdr::write(out, width, height, bit_depth, color);
        // Colour-space chunks precede PLTE.
        self.ancillary.write_pre_plte(out, self.effort, written);
        pre_idat(out); // PLTE + tRNS (indexed only)
        // Background / physical / timing / text, and the C2PA store last: immediately before IDAT.
        self.ancillary.write_post_plte(out, self.effort, written);

        let idat = self.compress_scanlines(
            sample_bytes,
            row_bytes,
            bpp,
            IdatInfo::new(
                width,
                height,
                bit_depth,
                color,
                // The filtered stream is one filter-type byte plus `row_bytes` per scanline; the
                // encoder never interlaces, so this is exact.
                (height as usize) * (row_bytes + 1),
            ),
        )?;
        write_idat(out, &idat);

        chunk::write_chunk(out, *b"IEND", &[]);
        Ok(out.len() - start)
    }

    /// Filters and DEFLATE-compresses the scanlines into a zlib stream. For
    /// [`FilterStrategy::BruteForce`] it compresses under every whole-image strategy and keeps the
    /// smallest; otherwise it uses the single configured strategy.
    fn compress_scanlines(
        &self,
        sample_bytes: &[u8],
        row_bytes: usize,
        bpp: usize,
        info: IdatInfo,
    ) -> Result<Vec<u8>> {
        let deflate = DeflateEncoder::new()
            .with_level(self.level)
            .with_effort(self.effort);
        // Every candidate stream goes through the same seam: a pushed backend that accepts sees
        // each brute-force candidate, and the smallest result still wins.
        let compress = |strategy| {
            let filtered = filter::filter_image(strategy, sample_bytes, row_bytes, bpp);
            run_deflaters(&self.backends, &info, &filtered, |raw| {
                let mut idat = Vec::new();
                deflate.zlib_compress(raw, &mut idat);
                idat
            })
        };
        if matches!(self.filter, FilterStrategy::BruteForce) {
            // `min_by_key` keeps the *first* minimum, so a tie resolves to the earlier (more
            // preferred) strategy — the behaviour the golden outputs are pinned to.
            let candidates = BRUTE_FORCE_STRATEGIES
                .into_iter()
                .map(compress)
                .collect::<Result<Vec<_>>>()?;
            Ok(candidates
                .into_iter()
                .min_by_key(Vec::len)
                .unwrap_or_default())
        } else {
            compress(self.filter)
        }
    }

    /// Writes the smallest of the encodings [`reduce::analyze8`] / [`reduce::analyze16`] made
    /// reachable: the reduction they ranked first, the best reduction that adds no chunk, and the
    /// image encoded untouched.
    ///
    /// The analysis chooses by comparing **raw** sizes, and raw size does not predict compressed
    /// size when one candidate's bytes are incompressible and the other's are not. A palette
    /// carries a `PLTE` (and often `tRNS`) chunk that DEFLATE cannot touch, while the pixels it
    /// replaces may compress by two orders of magnitude. On a 128x128 image with 64 colours the
    /// estimate sees 16 664 bytes against 65 536 and picks the palette by 4x — and the finished
    /// file is 451 bytes against 405. The crossover sits near 160x160, so the estimate is right on
    /// large images and wrong on small ones.
    ///
    /// Rather than guess a correction factor, the candidates are encoded and the smallest kept.
    /// That is exactly what [`FilterStrategy::BruteForce`] already does for filters, and it needs
    /// no tuned constant.
    ///
    /// **Three candidates, not two.** The raw estimate collapses five reductions to one winner,
    /// and when that winner is a palette the runner-up it eliminated is often a chunk-free
    /// reduction — an alpha drop, a greyscale collapse, a 16→8 demotion — that *would* have won
    /// the finished file. Racing only the palette against the unreduced image threw those away
    /// and fell all the way back to no reduction at all: a 128x128 opaque RGBA image with 256
    /// colours kept an alpha channel that was 255 everywhere (349 bytes against 317), and a 64x64
    /// 16-bit image whose samples are all `k·257` kept all sixteen bits (220 against 172). So
    /// [`Reductions`] hands over the best chunk-free candidate beside the chunk-carrying one, and
    /// all three are measured — `tests/size_contract.rs`'s `opaque256_rgba8` and
    /// `demotable_rgb16` rows are those two cases.
    ///
    /// **The total order.** Ties resolve toward the earlier of `chunked ≻ chunk-free ≻ native` —
    /// the more reduced encoding, and, among equal-length files, the one the encoder already
    /// emitted before the runner-up joined the race, so a tie changes no output. See
    /// [`prefers_chunk_free`] and [`prefers_native`], where each step is stated on its own.
    ///
    /// Only a reduction that *carries a chunk* pays for the extra encodes — a palette's `PLTE`
    /// (+ `tRNS`), or a colour key's `tRNS`. A chunk-free winner adds nothing DEFLATE cannot
    /// compress, so the raw comparison that chose it is sound and it is written immediately;
    /// that case is [`Reductions::ChunkFree`], and the analysis, not this function, decides it.
    fn write_reduced_or_native(
        &self,
        dims: Dimensions,
        reductions: Reductions,
        native: impl FnOnce(&mut Vec<u8>) -> Result<usize>,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let (chunked, chunk_free) = match reductions {
            Reductions::None => return native(out),
            Reductions::ChunkFree(reduced) => return self.write_reduced(dims, reduced, out),
            Reductions::Chunked {
                chunked,
                chunk_free,
            } => (chunked, chunk_free),
        };
        let mut reduced_encoding = Vec::new();
        self.write_reduced(dims, chunked, &mut reduced_encoding)?;
        if let Some(free) = chunk_free {
            let mut free_encoding = Vec::new();
            self.write_reduced(dims, free, &mut free_encoding)?;
            if prefers_chunk_free(free_encoding.len(), reduced_encoding.len()) {
                reduced_encoding = free_encoding;
            }
        }
        let mut native_encoding = Vec::new();
        native(&mut native_encoding)?;

        let winner = if prefers_native(native_encoding.len(), reduced_encoding.len()) {
            native_encoding
        } else {
            reduced_encoding
        };
        out.extend_from_slice(&winner);
        Ok(winner.len())
    }

    /// Writes a reduced encoding chosen by [`reduce::analyze8`] / [`reduce::analyze16`].
    fn write_reduced(
        &self,
        dims: Dimensions,
        reduced: Reduced,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let wh = (dims.width, dims.height);
        match reduced {
            Reduced::Gray { depth, samples } => {
                let packed;
                let sample_bytes = if depth < 8 {
                    packed = pack::pack_scanlines(
                        &samples,
                        dims.width as usize,
                        dims.height as usize,
                        depth,
                    );
                    packed.as_slice()
                } else {
                    &samples
                };
                self.write_png(
                    wh,
                    sample_bytes,
                    WrittenHeader::new(ColorType::Grayscale, depth),
                    |_| {},
                    out,
                )
            }
            Reduced::GrayAlpha8(samples) => self.write_png(
                wh,
                &samples,
                WrittenHeader::new(ColorType::GrayscaleAlpha, 8),
                |_| {},
                out,
            ),
            Reduced::Rgb8(samples) => self.write_png(
                wh,
                &samples,
                WrittenHeader::new(ColorType::Truecolor, 8),
                |_| {},
                out,
            ),
            // §11.3.2.1: for truecolour, tRNS is three 16-bit big-endian samples naming the one
            // colour a decoder renders as fully transparent. At depth 8 the high byte is zero.
            Reduced::Rgb8Keyed { samples, key } => self.write_png(
                wh,
                &samples,
                WrittenHeader::new(ColorType::Truecolor, 8),
                |out| {
                    let trns = [0, key[0], 0, key[1], 0, key[2]];
                    chunk::write_chunk(out, *b"tRNS", &trns);
                },
                out,
            ),
            // ...and for greyscale, one 16-bit big-endian sample.
            Reduced::GrayKeyed { samples, key } => self.write_png(
                wh,
                &samples,
                WrittenHeader::new(ColorType::Grayscale, 8),
                |out| chunk::write_chunk(out, *b"tRNS", &[0, key]),
                out,
            ),
            Reduced::Rgba8(samples) => self.write_png(
                wh,
                &samples,
                WrittenHeader::new(ColorType::TruecolorAlpha, 8),
                |_| {},
                out,
            ),
            Reduced::Gray16Be(bytes) => self.write_png(
                wh,
                &bytes,
                WrittenHeader::new(ColorType::Grayscale, 16),
                |_| {},
                out,
            ),
            Reduced::GrayAlpha16Be(bytes) => self.write_png(
                wh,
                &bytes,
                WrittenHeader::new(ColorType::GrayscaleAlpha, 16),
                |_| {},
                out,
            ),
            Reduced::Rgb16Be(bytes) => self.write_png(
                wh,
                &bytes,
                WrittenHeader::new(ColorType::Truecolor, 16),
                |_| {},
                out,
            ),
            Reduced::Indexed {
                depth,
                indices,
                plte,
                trns,
            } => {
                let packed;
                let sample_bytes = if depth < 8 {
                    packed = pack::pack_scanlines(
                        &indices,
                        dims.width as usize,
                        dims.height as usize,
                        depth,
                    );
                    packed.as_slice()
                } else {
                    &indices
                };
                self.write_png(
                    wh,
                    sample_bytes,
                    WrittenHeader {
                        color: ColorType::Indexed,
                        bit_depth: depth,
                        palette: Some(WrittenPalette {
                            plte: &plte,
                            trns: trns.as_deref(),
                            origin: PaletteOrigin::Derived,
                        }),
                    },
                    |out| {
                        chunk::write_chunk(out, *b"PLTE", &plte);
                        if let Some(alpha) = &trns {
                            chunk::write_chunk(out, *b"tRNS", alpha);
                        }
                    },
                    out,
                )
            }
        }
    }
}

/// Whether the uncleaned encoding beats the cleaned one, for [`PngEncoder::cleaned_or_plain`].
///
/// **A tie keeps the plain encoding.** Every other reduction in this crate is byte-exact;
/// [`with_transparent_cleanup`](PngEncoder::with_transparent_cleanup) is the one knob that alters
/// stored samples, and it is opt-in *for a size win*. Where there is no size win there is nothing
/// to trade the exactness for, so the candidate that changed no sample is kept. Split out for the
/// same reason as [`prefers_native`]: engineering two encodings of the same image to land on
/// exactly equal lengths is not something a fixture can do reliably, so the tie is only assertable
/// here.
fn prefers_plain(plain_len: usize, cleaned_len: usize) -> bool {
    plain_len <= cleaned_len
}

/// Whether the chunk-free reduction beats the chunk-carrying one, the first step of
/// [`PngEncoder::write_reduced_or_native`]'s three-way race.
///
/// **A tie keeps the chunk-carrying encoding**: it is the candidate the raw estimate ranked first
/// and the one the encoder emitted before the runner-up joined the race, so an equal-length
/// runner-up changes no output. Split out for the same reason as [`prefers_native`].
fn prefers_chunk_free(chunk_free_len: usize, chunked_len: usize) -> bool {
    chunk_free_len < chunked_len
}

/// Whether the unreduced encoding beats the winning reduction, for
/// [`PngEncoder::write_reduced_or_native`].
///
/// **A tie keeps the reduction**, which decodes with less work for the same bytes — and where the
/// palette won the first step, a tie here keeps the palette. Split out because engineering two
/// encodings of the same image to land on exactly equal lengths is not something a fixture can do
/// reliably, so the tie is only assertable here.
fn prefers_native(native_len: usize, palette_len: usize) -> bool {
    native_len < palette_len
}

/// Zeroes the colour samples of every fully transparent pixel in a 16-bit interleaved buffer,
/// returning `None` when there is nothing to do (no alpha channel, or no fully transparent pixel)
/// so the caller can keep borrowing its own samples.
///
/// The 8-bit twin is `reduce::clean_transparent`, which cannot serve here: it reads one-byte
/// samples with a one-byte stride, whereas a 16-bit pixel is invisible only when its *whole* alpha
/// sample is zero (both bytes of the stored big-endian pair), and clearing a colour sample must
/// clear all sixteen bits. Working on the `u16` samples rather than on the big-endian bytes
/// `PngEncoder::encode_16bit` emits keeps the ordering identical to the 8-bit paths — cleanup runs
/// first, so `reduce::analyze16` gets to see the collapsed invisible pixels.
fn clean_transparent16(samples: &[u16], channels: usize) -> Option<Vec<u16>> {
    debug_assert!((1..=4).contains(&channels));
    if !channels.is_multiple_of(2) {
        return None; // no alpha channel
    }
    let colour = channels - 1; // colour samples are everything before alpha
    if !samples.chunks_exact(channels).any(|px| px[colour] == 0) {
        return None;
    }

    let mut out = samples.to_vec();
    for px in out.chunks_exact_mut(channels) {
        if px[colour] == 0 {
            px[..colour].fill(0);
        }
    }
    Some(out)
}

/// Writes the zlib datastream as one or more consecutive IDAT chunks.
fn write_idat(out: &mut Vec<u8>, zlib_stream: &[u8]) {
    if zlib_stream.is_empty() {
        chunk::write_chunk(out, *b"IDAT", &[]);
        return;
    }
    for piece in zlib_stream.chunks(IDAT_MAX) {
        chunk::write_chunk(out, *b"IDAT", piece);
    }
}

// One impl per supported pixel layout. Indexed colour is handled separately (it needs a palette);
// CMYK has no PNG colour type.
impl EncodeImage<Gray8> for PngEncoder {
    fn encode_image(&self, image: ImageRef<'_, Gray8>, out: &mut Vec<u8>) -> Result<usize> {
        if self.auto_reduce {
            return self.write_reduced_or_native(
                image.dimensions(),
                reduce::analyze8(image.as_samples(), 1),
                |o| self.encode_8bit(image, ColorType::Grayscale, o),
                out,
            );
        }
        self.encode_8bit(image, ColorType::Grayscale, out)
    }
}
impl EncodeImage<Bilevel> for PngEncoder {
    /// Bilevel pixels (0 = black, non-zero = white) are packed to a 1-bit greyscale image.
    fn encode_image(&self, image: ImageRef<'_, Bilevel>, out: &mut Vec<u8>) -> Result<usize> {
        let dims = image.dimensions();
        let bits: Vec<u8> = image
            .as_samples()
            .iter()
            .map(|&v| u8::from(v != 0))
            .collect();
        let packed = pack::pack_scanlines(&bits, dims.width as usize, dims.height as usize, 1);
        self.write_png(
            (dims.width, dims.height),
            &packed,
            WrittenHeader::new(ColorType::Grayscale, 1),
            |_| {},
            out,
        )
    }
}
impl EncodeImage<Rgb8> for PngEncoder {
    fn encode_image(&self, image: ImageRef<'_, Rgb8>, out: &mut Vec<u8>) -> Result<usize> {
        if self.auto_reduce {
            return self.write_reduced_or_native(
                image.dimensions(),
                reduce::analyze8(image.as_samples(), 3),
                |o| self.encode_8bit(image, ColorType::Truecolor, o),
                out,
            );
        }
        self.encode_8bit(image, ColorType::Truecolor, out)
    }
}
impl EncodeImage<Rgba8> for PngEncoder {
    fn encode_image(&self, image: ImageRef<'_, Rgba8>, out: &mut Vec<u8>) -> Result<usize> {
        let dims = image.dimensions();
        let plain = image.as_samples();
        match self.cleaned_samples(plain, 4) {
            Some(cleaned) => self.cleaned_or_plain(
                |o| self.encode_alpha8(dims, &cleaned, 4, ColorType::TruecolorAlpha, o),
                |o| self.encode_alpha8(dims, plain, 4, ColorType::TruecolorAlpha, o),
                out,
            ),
            None => self.encode_alpha8(dims, plain, 4, ColorType::TruecolorAlpha, out),
        }
    }
}
impl EncodeImage<GrayAlpha8> for PngEncoder {
    fn encode_image(&self, image: ImageRef<'_, GrayAlpha8>, out: &mut Vec<u8>) -> Result<usize> {
        let dims = image.dimensions();
        let plain = image.as_samples();
        match self.cleaned_samples(plain, 2) {
            Some(cleaned) => self.cleaned_or_plain(
                |o| self.encode_alpha8(dims, &cleaned, 2, ColorType::GrayscaleAlpha, o),
                |o| self.encode_alpha8(dims, plain, 2, ColorType::GrayscaleAlpha, o),
                out,
            ),
            None => self.encode_alpha8(dims, plain, 2, ColorType::GrayscaleAlpha, out),
        }
    }
}
impl EncodeImage<Gray16> for PngEncoder {
    fn encode_image(&self, image: ImageRef<'_, Gray16>, out: &mut Vec<u8>) -> Result<usize> {
        let (dims, samples) = (image.dimensions(), image.as_samples());
        if self.auto_reduce {
            return self.write_reduced_or_native(
                dims,
                reduce::analyze16(samples, 1),
                |o| self.encode_16bit(dims, samples, ColorType::Grayscale, o),
                out,
            );
        }
        self.encode_16bit(dims, samples, ColorType::Grayscale, out)
    }
}
impl EncodeImage<Rgb16> for PngEncoder {
    fn encode_image(&self, image: ImageRef<'_, Rgb16>, out: &mut Vec<u8>) -> Result<usize> {
        let (dims, samples) = (image.dimensions(), image.as_samples());
        if self.auto_reduce {
            return self.write_reduced_or_native(
                dims,
                reduce::analyze16(samples, 3),
                |o| self.encode_16bit(dims, samples, ColorType::Truecolor, o),
                out,
            );
        }
        self.encode_16bit(dims, samples, ColorType::Truecolor, out)
    }
}
impl EncodeImage<Rgba16> for PngEncoder {
    fn encode_image(&self, image: ImageRef<'_, Rgba16>, out: &mut Vec<u8>) -> Result<usize> {
        let dims = image.dimensions();
        let plain = image.as_samples();
        match self.cleaned_samples16(plain, 4) {
            Some(cleaned) => self.cleaned_or_plain(
                |o| self.encode_alpha16(dims, &cleaned, 4, ColorType::TruecolorAlpha, o),
                |o| self.encode_alpha16(dims, plain, 4, ColorType::TruecolorAlpha, o),
                out,
            ),
            None => self.encode_alpha16(dims, plain, 4, ColorType::TruecolorAlpha, out),
        }
    }
}
impl EncodeImage<GrayAlpha16> for PngEncoder {
    fn encode_image(&self, image: ImageRef<'_, GrayAlpha16>, out: &mut Vec<u8>) -> Result<usize> {
        let dims = image.dimensions();
        let plain = image.as_samples();
        match self.cleaned_samples16(plain, 2) {
            Some(cleaned) => self.cleaned_or_plain(
                |o| self.encode_alpha16(dims, &cleaned, 2, ColorType::GrayscaleAlpha, o),
                |o| self.encode_alpha16(dims, plain, 2, ColorType::GrayscaleAlpha, o),
                out,
            ),
            None => self.encode_alpha16(dims, plain, 2, ColorType::GrayscaleAlpha, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::Dimensions;

    use super::*;

    /// Walks the chunk stream after the signature and returns a chunk's payload.
    fn find_chunk(png: &[u8], ty: &[u8; 4]) -> Option<Vec<u8>> {
        let mut i = 8;
        while i + 12 <= png.len() {
            let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
            if &png[i + 4..i + 8] == ty {
                return Some(png[i + 8..i + 8 + len].to_vec());
            }
            i += 12 + len;
        }
        None
    }

    /// The background builders reach the bKGD chunk, in the width the colour type requires.
    ///
    /// Both `with_background_gray` and `with_background_index` could return `Self::default()` --
    /// discarding the caller's colour *and* every other setting made before them -- and no test
    /// noticed (#110). `with_background_rgb` was covered; these two were not.
    ///
    /// bKGD's payload width is colour-type-specific (PNG 3rd ed. §11.3.5.1): two bytes for
    /// greyscale, one for indexed. Asserting the bytes rather than mere presence is what
    /// distinguishes the right builder from any of them.
    ///
    /// Each colour is one the written file can carry — a grey level inside the 8-bit depth, an
    /// index inside the palette `encode_indexed8` writes — because a background the written
    /// header cannot express is omitted rather than emitted for a reader to reject
    /// (`ancillary::bkgd_for`), and that omission is pinned by its own tests.
    #[test]
    fn background_builders_reach_the_bkgd_chunk() {
        let gray = vec![0u8; 4 * 4];
        let img = ImageRef::<Gray8>::new(&gray, Dimensions::new(4, 4).unwrap()).unwrap();
        let mut png = Vec::new();
        PngEncoder::new()
            .with_background_gray(0x34)
            .encode_image(img, &mut png)
            .unwrap();
        assert_eq!(
            find_chunk(&png, b"bKGD"),
            Some(vec![0x00, 0x34]),
            "greyscale bKGD is the 16-bit level, big-endian"
        );

        // Indexed: one byte, the palette index.
        let entries: Vec<[u8; 3]> = (0..8u8).map(|i| [i, i.wrapping_add(70), 90]).collect();
        let palette = PngPalette::new(&entries).unwrap();
        let indices: Vec<u8> = (0..200u8).map(|i| i % 8).collect();
        let img = ImageRef::<Indexed8>::new(&indices, Dimensions::new(200, 1).unwrap()).unwrap();
        let mut png = Vec::new();
        PngEncoder::new()
            .with_background_index(7)
            .encode_indexed8(img, &palette, &mut png)
            .unwrap();
        assert_eq!(find_chunk(&png, b"bKGD"), Some(vec![7]));
    }

    /// An indexed image needing 8-bit indices is written a byte per pixel, not bit-packed.
    ///
    /// The packing branch is gated on `depth < 8`. Every indexed fixture had at most 16 colours,
    /// so depth was always 1, 2 or 4 and the boundary was never reached -- `<=` survived (#110).
    /// It is not a cosmetic difference: `pack_scanlines` asserts `1 | 2 | 4` and computes
    /// `1u8 << depth`, which overflows at 8.
    #[test]
    fn indexed_at_depth_eight_is_not_bit_packed() {
        // 32 distinct opaque colours over 1024 pixels: more than 16, so the index depth is 8.
        //
        // Pseudo-random rather than cycling, and 1024 pixels rather than 200, because
        // `write_reduced_or_native` races the palette against the unreduced encoding and keeps
        // whichever is smaller. A period-32 cycle over 200 pixels compresses to an 82-byte RGB
        // file, which a 96-byte `PLTE` cannot beat before a single index is written -- the race
        // correctly declines the palette, and pinning `Indexed` there would assert the defect the
        // race exists to fix. Shuffling denies DEFLATE the period and 1024 pixels amortise the
        // palette: 479 bytes indexed against 525 unreduced.
        let mut rgb = Vec::new();
        for i in 0..1024u32 {
            let mut h = i.wrapping_mul(2654435761);
            h ^= h >> 15;
            let c = (h % 32) as u8;
            rgb.extend_from_slice(&[c, c.wrapping_add(70), 90]);
        }
        let img = ImageRef::<Rgb8>::new(&rgb, Dimensions::new(1024, 1).unwrap()).unwrap();
        let mut png = Vec::new();
        // Reduction is opt-in; without it the encoder writes the input layout unchanged and the
        // indexed path -- the one this test is about -- is never reached.
        PngEncoder::new()
            .with_auto_reduce(true)
            .encode_image(img, &mut png)
            .unwrap();

        assert_eq!(png[24], 8, "bit depth");
        assert_eq!(png[25], ColorType::Indexed.code(), "colour type");
        assert_eq!(
            find_chunk(&png, b"PLTE").map(|p| p.len()),
            Some(32 * 3),
            "32 palette entries"
        );
    }

    #[test]
    fn emits_signature_ihdr_idat_iend() {
        let src = vec![0u8; 2 * 2 * 3];
        let img = ImageRef::<Rgb8>::new(&src, Dimensions::new(2, 2).unwrap()).unwrap();
        let mut png = Vec::new();
        PngEncoder::new().encode_image(img, &mut png).unwrap();
        assert_eq!(&png[..8], &SIGNATURE);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn ihdr_reports_color_type_and_depth() {
        // A 16-bit grayscale-alpha image should declare colour type 4, bit depth 16.
        let src = vec![0u16; 3 * 3 * 2];
        let img = ImageRef::<GrayAlpha16>::new(&src, Dimensions::new(3, 3).unwrap()).unwrap();
        let mut png = Vec::new();
        PngEncoder::new().encode_image(img, &mut png).unwrap();
        // IHDR data starts at byte 16: width(4) height(4) depth(1) colortype(1).
        assert_eq!(png[24], 16, "bit depth");
        assert_eq!(png[25], ColorType::GrayscaleAlpha.code(), "colour type");
    }

    #[test]
    fn returns_the_number_of_bytes_appended_not_the_total_length() {
        // `write_png` reports `out.len() - start`; encoding into a non-empty buffer is what tells
        // that apart from the buffer's total length.
        let src = vec![0u8; 2 * 2 * 3];
        let img = ImageRef::<Rgb8>::new(&src, Dimensions::new(2, 2).unwrap()).unwrap();
        let mut fresh = Vec::new();
        let alone = PngEncoder::new().encode_image(img, &mut fresh).unwrap();
        assert_eq!(alone, fresh.len());

        let mut appended = vec![0xAAu8; 17];
        let written = PngEncoder::new().encode_image(img, &mut appended).unwrap();
        assert_eq!(written, alone, "only the PNG's own bytes are counted");
        assert_eq!(appended.len(), 17 + alone);
        assert_eq!(&appended[17..], &fresh[..], "the prefix is left untouched");
    }

    #[test]
    fn a_tie_between_palette_and_native_keeps_the_palette() {
        assert!(prefers_native(10, 11), "smaller native wins");
        assert!(!prefers_native(11, 10), "smaller palette wins");
        assert!(!prefers_native(10, 10), "a tie keeps the palette");
    }

    #[test]
    fn a_tie_between_the_chunk_free_runner_up_and_the_palette_keeps_the_palette() {
        assert!(
            prefers_chunk_free(10, 11),
            "a smaller chunk-free reduction wins"
        );
        assert!(!prefers_chunk_free(11, 10), "a smaller palette wins");
        assert!(
            !prefers_chunk_free(10, 10),
            "a tie keeps the chunk-carrying encoding the estimate ranked first"
        );
    }

    #[test]
    fn a_tie_between_cleaned_and_plain_keeps_the_plain_encoding() {
        assert!(prefers_plain(10, 11), "smaller plain wins");
        assert!(!prefers_plain(11, 10), "smaller cleaned wins");
        assert!(
            prefers_plain(10, 10),
            "a tie keeps the plain encoding, which altered no stored sample"
        );
    }

    #[test]
    fn brute_force_keeps_the_first_strategy_on_a_tie() {
        // A 1x1 image compresses to the same length under every strategy, so the tie-break is what
        // picks the output. `BRUTE_FORCE_STRATEGIES` is in preference order and the first minimum
        // wins, so the filter byte must be `None` (0) — not the last strategy's choice.
        let src = vec![200u8];
        let img = ImageRef::<Gray8>::new(&src, Dimensions::new(1, 1).unwrap()).unwrap();
        let mut brute = Vec::new();
        PngEncoder::new()
            .with_filter(FilterStrategy::BruteForce)
            .encode_image(img, &mut brute)
            .unwrap();
        let mut none = Vec::new();
        PngEncoder::new()
            .with_filter(FilterStrategy::None)
            .encode_image(img, &mut none)
            .unwrap();
        assert_eq!(
            brute, none,
            "the tie must resolve to the first (None) candidate"
        );
        // And it is genuinely a tie the later strategies could have won.
        let mut paeth = Vec::new();
        PngEncoder::new()
            .with_filter(FilterStrategy::Fixed(FilterType::Paeth))
            .encode_image(img, &mut paeth)
            .unwrap();
        assert_eq!(brute.len(), paeth.len(), "same length, different bytes");
        assert_ne!(brute, paeth);
    }

    #[test]
    fn large_stream_splits_into_multiple_idats() {
        // Incompressible data larger than IDAT_MAX must yield more than one IDAT chunk.
        let mut out = Vec::new();
        let big = vec![0xABu8; IDAT_MAX * 2 + 100];
        write_idat(&mut out, &big);
        let idats = out.windows(4).filter(|w| *w == b"IDAT").count();
        assert!(idats >= 3, "expected multiple IDAT chunks, found {idats}");
    }

    #[test]
    fn cleaning_16_bit_pixels_needs_the_whole_alpha_sample_to_be_zero() {
        // The byte-wise twin would read the big-endian pair `0x0001` as a zero high byte and
        // wrongly call this pixel invisible; at `u16` width it is visible and must be untouched.
        // The third pixel is the genuinely invisible one, and all three of its colour samples —
        // both bytes of each — must be cleared.
        let src: [u16; 12] = [
            0x1234, 0x5678, 0x9ABC, 0xFFFF, // visible
            0x1111, 0x2222, 0x3333, 0x0001, // alpha 1: barely visible, must stay
            0x4444, 0x5555, 0x6666, 0x0000, // invisible: colour must go
        ];
        let cleaned = clean_transparent16(&src, 4).expect("there is a transparent pixel");
        assert_eq!(
            cleaned,
            vec![
                0x1234, 0x5678, 0x9ABC, 0xFFFF, //
                0x1111, 0x2222, 0x3333, 0x0001, //
                0, 0, 0, 0,
            ]
        );
    }

    #[test]
    fn cleaning_16_bit_grey_alpha_zeroes_only_the_grey_sample() {
        let src: [u16; 6] = [0xC800, 0xFFFF, 0x6F00, 0x0000, 0x5A00, 0x0001];
        let cleaned = clean_transparent16(&src, 2).expect("there is a transparent pixel");
        assert_eq!(cleaned, vec![0xC800, 0xFFFF, 0, 0, 0x5A00, 0x0001]);
    }

    #[test]
    fn cleaning_16_bit_declines_when_there_is_nothing_to_clean() {
        let opaque: [u16; 8] = [1, 2, 3, 0xFFFF, 4, 5, 6, 0xFFFF];
        assert!(
            clean_transparent16(&opaque, 4).is_none(),
            "no fully transparent pixel"
        );

        // Odd channel counts have no alpha sample, so a zero there is a colour, not transparency.
        let grey: [u16; 3] = [0, 7, 9];
        assert!(clean_transparent16(&grey, 1).is_none(), "no alpha channel");
        let rgb: [u16; 6] = [1, 2, 0, 4, 5, 6];
        assert!(clean_transparent16(&rgb, 3).is_none(), "no alpha channel");
    }
}
