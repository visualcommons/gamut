//! The PNG decoder: a [`PngDecoder`] with hostile-input limits, implementing
//! [`gamut_core::DecodeImage`] for every pixel layout the file can fill losslessly.
//!
//! The pipeline follows the spec stage by stage: signature and chunk framing (§5), IHDR
//! validation (§11.2.1), chunk ordering (§5.6), IDAT concatenation and bounded zlib inflation
//! (§10), per-scanline defiltering (§9), and sub-byte unpacking (§7.2). Every allocation sized
//! from untrusted fields is guarded: dimensions are checked against the decoder's limits before
//! anything is allocated, and the inflater refuses to produce more bytes than the image geometry
//! implies, so a "zlib bomb" fails cleanly.
//!
//! Presenting the decoded image as a caller's pixel layout is delegated to
//! [`gamut_core::convert`], so PNG applies the same rules as every other gamut decoder. What stays
//! here is what only PNG knows: palette lookup, folding a tRNS colour key into a real alpha
//! channel, and §13.12 sub-byte scaling (see [`presentable`]).
//!
//! The typed [`DecodeImage`] implementations therefore perform **lossless widening only** —
//! greyscale replicates into RGB, an opaque alpha channel can be added, sub-byte greys scale
//! exactly to 8 bits, 8-bit samples widen to 16 — and refuse lossy requests (dropping alpha or
//! transparency, narrowing 16-bit samples) with [`Error::Unsupported`] unless
//! [`PngDecoder::convert_policy`] permits them.

use gamut_core::convert::{ConvertPolicy, RawImage, convert_from_raw};
use gamut_core::{
    Bilevel, DecodeImage, Dimensions, Error, Gray8, Gray16, GrayAlpha8, GrayAlpha16, ImageBuf,
    Indexed8, Pixel, PixelFormat, Result, Rgb8, Rgb16, Rgba8, Rgba16,
};

use crate::backend::{IdatInflater, IdatInfo, Registry, run_inflaters};
use crate::chunk::ChunkReader;
use crate::color::ColorType;
use crate::decoded::{self, DecodedPng, PngHeader, PngImage, PngMetadata};
use crate::filter::{self, FilterType};
use crate::ihdr::{self, Ihdr};
use crate::palette::PngPalette;
use crate::{adam7, inflate, pack};

/// Default cap on the decoded sample buffer: 64 MiB, a 4096×4096 RGBA8 image.
///
/// `pub(crate)` because [`crate::deconstruct`] reports against the same budget: a file this
/// decoder decodes is one the report walk will inflate to count filters.
pub(crate) const DEFAULT_MAX_IMAGE_BYTES: usize = 64 << 20;
/// Default cumulative cap on inflated metadata (iCCP/zTXt/iTXt) payloads: 16 MiB.
const DEFAULT_MAX_METADATA_BYTES: usize = 16 << 20;
/// The spec's own dimension bound (§11.2.1): width and height are 1 ..= 2³¹ − 1.
const SPEC_MAX_DIMENSION: u32 = i32::MAX as u32;

/// The one refusal message for typed decodes the layout cannot hold losslessly.
fn lossy() -> Error {
    Error::unsupported(
        env!("CARGO_PKG_NAME"),
        "PNG: this pixel layout cannot hold the image losslessly; use PngDecoder::decode",
    )
}

/// A reusable PNG decoder with hostile-input limits.
///
/// The defaults accept anything the spec allows dimensionally but cap the decoded sample buffer
/// at 64 MiB — the byte budget, not the dimension caps, is the real safety guard (it bounds both
/// the inflated scanline stream and the sample buffer, so peak memory stays within roughly twice
/// the budget plus the input). Tighten the dimension caps when the application knows its domain.
#[derive(Debug, Clone)]
pub struct PngDecoder {
    max_width: u32,
    max_height: u32,
    max_image_bytes: usize,
    max_metadata_bytes: usize,
    backends: Registry<dyn IdatInflater + Send>,
    policy: ConvertPolicy,
}

impl Default for PngDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PngDecoder {
    /// Creates a decoder with the default limits (spec-maximum dimensions, 64 MiB of decoded
    /// samples, 16 MiB of inflated metadata).
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_width: SPEC_MAX_DIMENSION,
            max_height: SPEC_MAX_DIMENSION,
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            max_metadata_bytes: DEFAULT_MAX_METADATA_BYTES,
            backends: Registry::default(),
            policy: ConvertPolicy::lossless(),
        }
    }

    /// Appends a pluggable [`IdatInflater`] backend for the IDAT zlib stream (issue #278).
    ///
    /// Backends are tried in **push order**; the built-in `miniz_oxide` inflater is the implicit
    /// tail, so pushing nothing keeps today's behaviour exactly. A backend that returns `false`
    /// from [`IdatInflater::supports`] (or [`Error::Unsupported`] from [`IdatInflater::inflate`])
    /// is skipped; one that accepts and then fails propagates its error rather than falling back —
    /// see the [`backend`](crate::backend) module docs for the full contract.
    ///
    /// # The output cap stays with the host
    ///
    /// The decoder's own budget (see [`with_max_image_bytes`](Self::with_max_image_bytes)) is
    /// passed to the backend as `max_out` *and* re-checked against the bytes it returns. A backend
    /// that overshoots is rejected with [`Error::InvalidInput`], never truncated, so a hostile or
    /// buggy backend cannot turn a zlib bomb into an out-of-memory.
    ///
    /// Only the **IDAT** stream goes through the seam. Ancillary compressed payloads (iCCP, zTXt,
    /// compressed iTXt) always use the built-in inflater: they are not the codestream, and they
    /// carry their own, much smaller budget.
    ///
    /// # Cloning shares backends
    ///
    /// [`PngDecoder`] is [`Clone`], and cloning copies the registry *handles*: the clone and the
    /// original drive the **same** backend instances (held behind `Arc<Mutex<…>>`, since decoding
    /// takes `&self`). Push a fresh backend instance if you need independent state.
    ///
    /// Takes `&mut self` and returns `&mut Self` — unlike the `with_*` builder setters — because a
    /// registry accumulates rather than replaces.
    pub fn push_backend(&mut self, backend: impl IdatInflater + 'static) -> &mut Self {
        self.backends
            .push(std::sync::Arc::new(std::sync::Mutex::new(backend)));
        self
    }

    /// Caps the accepted image dimensions; a wider or taller image is refused before any
    /// allocation.
    #[must_use]
    pub fn with_max_dimensions(mut self, width: u32, height: u32) -> Self {
        self.max_width = width;
        self.max_height = height;
        self
    }

    /// Caps the decoded sample buffer in bytes (default 64 MiB).
    #[must_use]
    pub fn with_max_image_bytes(mut self, bytes: usize) -> Self {
        self.max_image_bytes = bytes;
        self
    }

    /// Caps the *cumulative* inflated size of compressed metadata payloads — iCCP, zTXt, and
    /// compressed iTXt together (default 16 MiB). Payloads past the budget are skipped, not
    /// errors; the typed [`DecodeImage`] path never inflates metadata at all.
    #[must_use]
    pub fn with_max_metadata_bytes(mut self, bytes: usize) -> Self {
        self.max_metadata_bytes = bytes;
        self
    }

    /// Selects which lossy conversions a typed decode may perform.
    ///
    /// Defaults to [`ConvertPolicy::lossless`], which is what makes a typed decode's result
    /// faithful to the file. Widening is unaffected — it loses nothing and never needed
    /// permission. Pass [`ConvertPolicy::permissive`] to get pixels in a fixed layout regardless
    /// of what the file carries.
    #[must_use]
    pub fn convert_policy(mut self, policy: ConvertPolicy) -> Self {
        self.policy = policy;
        self
    }
}

/// A tRNS colour key for greyscale or truecolour images, in the file's **native** (unscaled)
/// sample range (§11.3.1.1). Pixels equal to the key are fully transparent, all others opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransparencyKey {
    /// The transparent grey level of a greyscale image.
    Gray(u16),
    /// The transparent RGB colour of a truecolour image.
    Rgb(u16, u16, u16),
}

/// The image-bearing chunks collected from one pass over the chunk stream.
struct Parsed<'a> {
    header: Ihdr,
    plte: Option<&'a [u8]>,
    trns: Option<&'a [u8]>,
    /// All IDAT payloads, concatenated (§5.6 requires them consecutive).
    idat: Vec<u8>,
    /// Metadata-bearing ancillary chunks in file order (populated only when requested).
    ancillary: Vec<([u8; 4], &'a [u8])>,
}

/// Decoded samples in the file's native value range: one byte per sample for depths ≤ 8
/// (sub-byte depths unpacked but **unscaled**), native-endian `u16` for depth 16.
enum NativeSamples {
    B8(Vec<u8>),
    B16(Vec<u16>),
}

/// A fully decoded image in its native layout, before presentation as a pixel type.
struct NativeImage {
    header: Ihdr,
    samples: NativeSamples,
    palette: Option<PngPalette>,
    trns_key: Option<TransparencyKey>,
}

impl PngDecoder {
    /// Walks the chunk stream, enforcing criticality, CRC, and ordering rules (§5.4–§5.6).
    /// With `want_metadata`, the metadata-bearing ancillary chunks are collected for the rich
    /// decode path (the typed path skips them, so their payloads are never even copied).
    fn parse_stream<'a>(&self, data: &'a [u8], want_metadata: bool) -> Result<Parsed<'a>> {
        let mut reader = ChunkReader::new(data)?;
        let first = reader
            .next_chunk()?
            .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: missing IHDR"))?;
        if first.chunk_type != *b"IHDR" {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: first chunk must be IHDR",
            ));
        }
        if !first.crc_ok {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: critical chunk CRC mismatch",
            ));
        }
        let header = ihdr::parse(first.data)?;

        let mut plte: Option<&[u8]> = None;
        let mut trns: Option<&[u8]> = None;
        let mut idat = Vec::new();
        let mut ancillary = Vec::new();
        let mut seen_idat = false;
        let mut idat_done = false;
        let mut seen_iend = false;
        while let Some(chunk) = reader.next_chunk()? {
            if seen_idat && chunk.chunk_type != *b"IDAT" {
                idat_done = true;
            }
            match &chunk.chunk_type {
                b"IHDR" => {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "PNG: duplicate IHDR",
                    ));
                }
                b"IEND" => {
                    if !chunk.crc_ok {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "PNG: critical chunk CRC mismatch",
                        ));
                    }
                    if !chunk.data.is_empty() {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "PNG: IEND payload must be empty",
                        ));
                    }
                    seen_iend = true;
                    // Everything after IEND is not part of the PNG datastream; trailing bytes
                    // are ignored, as §13.2 asks decoders to be liberal about.
                    break;
                }
                b"IDAT" => {
                    if !chunk.crc_ok {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "PNG: critical chunk CRC mismatch",
                        ));
                    }
                    if idat_done {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "PNG: IDAT chunks must be consecutive",
                        ));
                    }
                    seen_idat = true;
                    idat.extend_from_slice(chunk.data);
                }
                b"PLTE" => {
                    if !chunk.crc_ok {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "PNG: critical chunk CRC mismatch",
                        ));
                    }
                    if plte.is_some() {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "PNG: duplicate PLTE",
                        ));
                    }
                    if seen_idat {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "PNG: PLTE must precede IDAT",
                        ));
                    }
                    if trns.is_some() {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "PNG: PLTE must precede tRNS",
                        ));
                    }
                    plte = Some(chunk.data);
                }
                b"tRNS" => {
                    if seen_idat {
                        return Err(Error::invalid_input(
                            env!("CARGO_PKG_NAME"),
                            "PNG: tRNS must precede IDAT",
                        ));
                    }
                    // tRNS is ancillary: a CRC mismatch skips the chunk (§13.1); duplicates keep
                    // the first occurrence.
                    if chunk.crc_ok && trns.is_none() {
                        trns = Some(chunk.data);
                    }
                }
                _ if chunk.is_ancillary() => {
                    // Ancillary chunks do not affect the pixels. For the rich decode path they
                    // are handed to `decoded::collect`, which is the single place that decides
                    // which chunk types carry metadata — anything it does not recognise,
                    // including APNG's acTL/fcTL/fdAT, is dropped there, so an animated PNG
                    // decodes as its default image. Only the CRC is enforced here (§13.1).
                    // These are borrowed slices, so an unrecognised chunk costs a fat pointer,
                    // not a copy of its payload.
                    if want_metadata && chunk.crc_ok {
                        ancillary.push((chunk.chunk_type, chunk.data));
                    }
                }
                _ => {
                    // §5.4/§13.2: a chunk that is critical but unknown means the image cannot
                    // be correctly rendered.
                    return Err(Error::unsupported(
                        env!("CARGO_PKG_NAME"),
                        "PNG: unknown critical chunk",
                    ));
                }
            }
        }
        if !seen_iend {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: missing IEND",
            ));
        }
        if !seen_idat {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: missing IDAT",
            ));
        }
        Ok(Parsed {
            header,
            plte,
            trns,
            idat,
            ancillary,
        })
    }

    /// Enforces the dimension and byte-budget limits (all math checked) and returns the exact
    /// byte length of the filtered scanline stream the IDAT data must inflate to.
    fn check_limits(&self, header: &Ihdr) -> Result<usize> {
        if header.width > self.max_width || header.height > self.max_height {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "PNG: image exceeds the dimension limit",
            ));
        }
        // Budget the *decoded* representation, via the one definition of that quantity, so the
        // report walk in `crate::deconstruct` cannot come to budget a different one.
        let native_bytes = ihdr::native_bytes(
            header.width,
            header.height,
            header.color.channels(),
            header.bit_depth,
        )
        .ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: image dimensions overflow")
        })?;
        if native_bytes > self.max_image_bytes {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "PNG: image exceeds the size limit",
            ));
        }
        adam7::expected_stream_len(header).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: image dimensions overflow")
        })
    }

    /// Decodes a PNG into its native layout together with the ancillary metadata — the rich
    /// counterpart of the typed [`DecodeImage`] implementations, and the only way to reach the
    /// palette of an indexed image, the tRNS colour key, and the raw metadata payloads
    /// (eXIf/ICC/XMP/text, plus parsed gAMA/cHRM/sRGB/cICP values).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] for a malformed file and [`Error::Unsupported`] for one
    /// exceeding the decoder's limits or using an unknown critical chunk. Malformed *metadata*
    /// payloads are not errors: the affected chunk is skipped (§13.1) and its field stays empty.
    pub fn decode(&self, data: &[u8]) -> Result<DecodedPng> {
        let parsed = self.parse_stream(data, true)?;
        let meta = decoded::collect(&parsed.ancillary, self.max_metadata_bytes);
        let native = self.decode_parsed(&parsed)?;
        let header = PngHeader {
            width: native.header.width,
            height: native.header.height,
            bit_depth: native.header.bit_depth,
            color_type: native.header.color,
            interlaced: native.header.interlaced,
        };
        let image = native_image(&native.header, native.samples)?;
        Ok(DecodedPng {
            header,
            image,
            palette: native.palette,
            transparency: native.trns_key,
            exif: meta.exif,
            icc_profile: meta.icc_profile,
            xmp: meta.xmp,
            texts: meta.texts,
            gamma: meta.gamma,
            chromaticities: meta.chromaticities,
            srgb: meta.srgb,
            cicp: meta.cicp,
        })
    }

    /// Reads a PNG's ancillary metadata without decoding any pixels, honouring this decoder's
    /// [`with_max_metadata_bytes`](Self::with_max_metadata_bytes) budget.
    ///
    /// Identical to the free [`metadata`] function except for that budget, which is why it exists:
    /// a free function has nowhere to carry the limit. Use this when reading untrusted input with
    /// a tighter cap than the 16 MiB default; use [`metadata`] otherwise.
    ///
    /// # Errors
    ///
    /// As [`metadata`].
    ///
    /// # Example
    ///
    /// ```
    /// use gamut_png::PngDecoder;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let png = {
    /// #     use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
    /// #     let pixels = vec![0u8; 12];
    /// #     let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2)?)?;
    /// #     gamut_png::PngEncoder::new().encode_to_vec(image)?
    /// # };
    /// // Refuse to inflate more than 4 KiB of compressed metadata.
    /// let decoder = PngDecoder::new().with_max_metadata_bytes(4096);
    /// let meta = decoder.metadata(&png)?;
    /// assert!(meta.icc_profile.is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub fn metadata(&self, data: &[u8]) -> Result<PngMetadata> {
        let chunks = walk_metadata_chunks(data)?;
        Ok(decoded::collect(&chunks, self.max_metadata_bytes))
    }

    /// Runs the typed pipeline: parse (without metadata) → decode.
    fn decode_native(&self, data: &[u8]) -> Result<NativeImage> {
        let parsed = self.parse_stream(data, false)?;
        self.decode_parsed(&parsed)
    }

    /// Runs the shared pipeline: validate PLTE/tRNS → inflate → per-pass defilter, unpack, and
    /// (for Adam7) recomposition.
    fn decode_parsed(&self, parsed: &Parsed<'_>) -> Result<NativeImage> {
        let header = parsed.header;
        let (palette, trns_key) = validate_plte_and_trns(&header, parsed.plte, parsed.trns)?;

        let expected = self.check_limits(&header)?;
        let info = IdatInfo::new(
            header.width,
            header.height,
            header.bit_depth,
            header.color,
            expected,
        );
        let stream = run_inflaters(
            &self.backends,
            &info,
            &parsed.idat,
            expected,
            inflate::inflate_zlib,
        )?;
        if stream.len() != expected {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: IDAT is shorter than the image",
            ));
        }

        let samples = match header.bit_depth {
            16 => NativeSamples::B16(decode_canvas(&stream, &header, |packed, _, _| {
                packed
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                    .collect()
            })?),
            8 => NativeSamples::B8(decode_canvas(&stream, &header, |packed, _, _| packed)?),
            depth => NativeSamples::B8(decode_canvas(&stream, &header, |packed, pw, ph| {
                pack::unpack_scanlines(&packed, pw, ph, depth)
            })?),
        };
        // §11.2.2: every pixel of an indexed image must reference an existing palette entry
        // (indexed depths are at most 8, so the samples are always the byte variant).
        if let (Some(palette), NativeSamples::B8(indices)) = (&palette, &samples)
            && indices.iter().any(|&idx| usize::from(idx) >= palette.len())
        {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: palette index out of range",
            ));
        }
        Ok(NativeImage {
            header,
            samples,
            palette,
            trns_key,
        })
    }
}

/// Walks the chunk stream collecting the CRC-valid ancillary chunks, for [`decoded::collect`] to
/// classify — the same handoff [`PngDecoder::parse_stream`] makes, so the two entry points cannot
/// disagree about which chunks carry metadata.
///
/// Deliberately not `parse_stream` itself: that accumulates every IDAT payload into an owned
/// `Vec` and then requires at least one, neither of which a metadata read should do. Here IDAT
/// (and PLTE) is skipped by length, so the pixel data is never touched or copied.
fn walk_metadata_chunks(data: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    let mut reader = ChunkReader::new(data)?;
    let first = reader
        .next_chunk()?
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: missing IHDR"))?;
    if first.chunk_type != *b"IHDR" {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "PNG: first chunk must be IHDR",
        ));
    }
    if !first.crc_ok {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "PNG: critical chunk CRC mismatch",
        ));
    }
    // Parsed but discarded: this validates the header the same way `decode` does, so a stream
    // with a corrupt IHDR is rejected here too rather than yielding metadata from a broken file.
    ihdr::parse(first.data)?;

    let mut chunks = Vec::new();
    let mut seen_iend = false;
    while let Some(chunk) = reader.next_chunk()? {
        match &chunk.chunk_type {
            b"IHDR" => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "PNG: duplicate IHDR",
                ));
            }
            b"IEND" => {
                if !chunk.crc_ok {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "PNG: critical chunk CRC mismatch",
                    ));
                }
                if !chunk.data.is_empty() {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "PNG: IEND payload must be empty",
                    ));
                }
                seen_iend = true;
                break;
            }
            // The pixel-bearing critical chunks. Skipped by length — never read, never copied.
            b"IDAT" | b"PLTE" => {}
            _ if chunk.is_ancillary() => {
                // §13.1: a CRC mismatch skips the chunk rather than failing the image.
                if chunk.crc_ok {
                    chunks.push((chunk.chunk_type, chunk.data));
                }
            }
            _ => {
                // §5.4/§13.2: an unknown critical chunk means this is not a stream this decoder
                // understands. Rejected here as in `decode`, so `metadata` never reports on a
                // file the rest of the crate would refuse.
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "PNG: unknown critical chunk",
                ));
            }
        }
    }
    if !seen_iend {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "PNG: missing IEND",
        ));
    }
    Ok(chunks)
}

/// Reads a PNG's ancillary metadata without decoding any pixels.
///
/// Walks the chunk stream, collecting the metadata-bearing ancillary chunks and inflating only
/// the compressed ones (iCCP, zTXt, and compressed iTXt) under one cumulative 16 MiB budget.
/// IDAT is skipped by length, so no pixel data is read, copied, or inflated — which makes this
/// cheap enough for a probe on a large file. `cICP`, `sRGB`, `gAMA` and `cHRM` are uncompressed
/// and cost nothing beyond the walk.
///
/// The result matches [`PngDecoder::decode`] field for field on the same file. Use
/// [`PngDecoder::metadata`] instead if you need a metadata budget other than the default.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the signature, IHDR, or chunk framing is malformed, IHDR is
/// duplicated, or IEND is missing; [`Error::Unsupported`] for an unknown critical chunk. Unlike
/// [`PngDecoder::decode`] it does **not** require IDAT — a stream carrying only metadata is read
/// successfully. Malformed *metadata* payloads are never errors: the affected chunk is skipped
/// (§13.1) and its field stays empty, as are chunks whose CRC does not match.
///
/// # Example
///
/// The extracted payload is byte-for-byte identical to the one given to the encoder:
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_png::PngEncoder;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let exif = b"II\x2a\x00opaque exif bytes";
/// let pixels = vec![0u8; 3 * 4];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2)?)?;
/// let png = PngEncoder::new().with_exif(exif).encode_to_vec(image)?;
///
/// let meta = gamut_png::metadata(&png)?;
/// assert_eq!(meta.exif.as_deref(), Some(exif.as_slice()));
/// # Ok(())
/// # }
/// ```
///
/// A PNG with no ancillary chunks reads back as the default:
///
/// ```
/// use gamut_core::{Dimensions, EncodeImage, ImageRef, Rgb8};
/// use gamut_png::{PngEncoder, PngMetadata};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let pixels = vec![0u8; 3 * 4];
/// let image = ImageRef::<Rgb8>::new(&pixels, Dimensions::new(2, 2)?)?;
/// let png = PngEncoder::new().encode_to_vec(image)?;
/// assert_eq!(gamut_png::metadata(&png)?, PngMetadata::default());
/// # Ok(())
/// # }
/// ```
pub fn metadata(data: &[u8]) -> Result<PngMetadata> {
    let chunks = walk_metadata_chunks(data)?;
    Ok(decoded::collect(&chunks, DEFAULT_MAX_METADATA_BYTES))
}

/// Validates PLTE presence/shape and tRNS shape against the colour type (§11.2.2, §11.3.1.1),
/// returning the palette for indexed images and the parsed colour key for greyscale/truecolour
/// images.
fn validate_plte_and_trns(
    header: &Ihdr,
    plte: Option<&[u8]>,
    trns: Option<&[u8]>,
) -> Result<(Option<PngPalette>, Option<TransparencyKey>)> {
    if let Some(plte) = plte {
        match header.color {
            ColorType::Grayscale | ColorType::GrayscaleAlpha => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "PNG: PLTE is forbidden for greyscale colour types",
                ));
            }
            // For truecolour it is a suggested quantisation palette (§11.2.2): shape-checked,
            // then ignored.
            ColorType::Truecolor | ColorType::TruecolorAlpha => {
                let entries = plte.len() / 3;
                if !plte.len().is_multiple_of(3) || !(1..=256).contains(&entries) {
                    return Err(Error::invalid_input(
                        env!("CARGO_PKG_NAME"),
                        "PNG: malformed PLTE payload",
                    ));
                }
            }
            ColorType::Indexed => {}
        }
    }
    if header.color == ColorType::Indexed {
        let plte = plte.ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: an indexed image requires a PLTE chunk",
            )
        })?;
        let palette = PngPalette::from_chunks(plte, trns)?;
        // §11.2.2: the palette must not have more entries than the bit depth can reference.
        if palette.len() > 1 << header.bit_depth {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: palette larger than the bit depth can reference",
            ));
        }
        return Ok((Some(palette), None));
    }
    let Some(trns) = trns else {
        return Ok((None, None));
    };
    let key = match header.color {
        ColorType::Grayscale => {
            let bytes: &[u8; 2] = trns.try_into().map_err(|_| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: malformed tRNS payload")
            })?;
            TransparencyKey::Gray(u16::from_be_bytes(*bytes) & depth_mask(header.bit_depth))
        }
        ColorType::Truecolor => {
            let bytes: &[u8; 6] = trns.try_into().map_err(|_| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: malformed tRNS payload")
            })?;
            let mask = depth_mask(header.bit_depth);
            TransparencyKey::Rgb(
                u16::from_be_bytes([bytes[0], bytes[1]]) & mask,
                u16::from_be_bytes([bytes[2], bytes[3]]) & mask,
                u16::from_be_bytes([bytes[4], bytes[5]]) & mask,
            )
        }
        // Indexed images returned above (their tRNS folds into the palette).
        ColorType::Indexed => return Ok((None, None)),
        ColorType::GrayscaleAlpha | ColorType::TruecolorAlpha => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: tRNS is forbidden for colour types with alpha",
            ));
        }
    };
    Ok((None, Some(key)))
}

/// The native-range mask for a colour key: §11.3.1.1 stores keys as 2-byte values but only the
/// low `bit_depth` bits are significant below depth 16.
fn depth_mask(bit_depth: u8) -> u16 {
    if bit_depth >= 16 {
        u16::MAX
    } else {
        (1 << bit_depth) - 1
    }
}

/// Bytes of one packed scanline: `ceil(width × bits_per_pixel / 8)` (§7.2), checked.
fn packed_row_bytes(width: u32, bits_per_pixel: usize) -> Option<usize> {
    (width as usize)
        .checked_mul(bits_per_pixel)
        .map(|bits| bits.div_ceil(8))
}

/// The filter's byte stride (§9.2): whole bytes per pixel, rounded up to at least one.
fn filter_stride(header: &Ihdr) -> usize {
    (header.bits_per_pixel() / 8).max(1)
}

/// Defilters, unpacks (via `to_samples`), and scatters every (reduced) image of the filtered
/// scanline stream onto a full-size canvas. Non-interlaced images run the same loop with a
/// single full-frame pass. `stream` is exactly [`adam7::expected_stream_len`] bytes.
fn decode_canvas<S: Copy + Default>(
    stream: &[u8],
    header: &Ihdr,
    to_samples: impl Fn(Vec<u8>, usize, usize) -> Vec<S>,
) -> Result<Vec<S>> {
    let overflow =
        || Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: image dimensions overflow");
    let (width, height) = (header.width as usize, header.height as usize);
    let channels = header.color.channels();
    let canvas_len = width
        .checked_mul(height)
        .and_then(|px| px.checked_mul(channels))
        .ok_or_else(overflow)?;
    let mut canvas = vec![S::default(); canvas_len];
    let stride = filter_stride(header);
    let mut rest = stream;
    for pass in adam7::passes_for(header.interlaced) {
        let (pass_width, pass_height) = adam7::pass_dimensions(pass, header.width, header.height);
        if pass_width == 0 || pass_height == 0 {
            continue; // absent from the stream, filter bytes included (§7.3)
        }
        let (pass_width, pass_height) = (pass_width as usize, pass_height as usize);
        let row_bytes =
            packed_row_bytes(pass_width as u32, header.bits_per_pixel()).ok_or_else(overflow)?;
        let pass_len = pass_height
            .checked_mul(row_bytes + 1)
            .ok_or_else(overflow)?;
        let (segment, remaining) = rest.split_at_checked(pass_len).ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: IDAT is shorter than the image",
            )
        })?;
        rest = remaining;
        let packed = unfilter_stream(segment, row_bytes, pass_height, stride)?;
        let samples = to_samples(packed, pass_width, pass_height);
        adam7::scatter(&mut canvas, width, pass, &samples, pass_width, channels);
    }
    Ok(canvas)
}

/// Defilters a scanline stream (`height` rows of `1 + row_bytes` bytes) into packed raw rows
/// (§9.2–§9.4).
fn unfilter_stream(stream: &[u8], row_bytes: usize, height: usize, bpp: usize) -> Result<Vec<u8>> {
    debug_assert_eq!(stream.len(), height * (row_bytes + 1));
    let mut out = vec![0u8; row_bytes * height];
    let zero_row = vec![0u8; row_bytes];
    for y in 0..height {
        let src = &stream[y * (row_bytes + 1)..(y + 1) * (row_bytes + 1)];
        let filter = FilterType::from_code(src[0]).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: undefined filter type")
        })?;
        let (done, rest) = out.split_at_mut(y * row_bytes);
        let prev = if y == 0 {
            zero_row.as_slice()
        } else {
            &done[(y - 1) * row_bytes..]
        };
        let cur = &mut rest[..row_bytes];
        cur.copy_from_slice(&src[1..]);
        filter::unfilter_row(filter, cur, prev, bpp);
    }
    Ok(out)
}

/// Wraps decoded native samples as the matching [`PngImage`] variant: sub-byte greyscale is
/// scaled to [`Gray8`] (§13.12), sub-byte indices are widened unscaled, 16-bit is native `u16`.
fn native_image(header: &Ihdr, samples: NativeSamples) -> Result<PngImage> {
    let dims = dims(header)?;
    Ok(match (header.color, samples) {
        (ColorType::Grayscale, NativeSamples::B8(mut gray)) => {
            let scale = pack::gray8_scale(header.bit_depth);
            for value in &mut gray {
                *value *= scale;
            }
            PngImage::Gray8(ImageBuf::new(gray, dims)?)
        }
        (ColorType::Grayscale, NativeSamples::B16(gray)) => {
            PngImage::Gray16(ImageBuf::new(gray, dims)?)
        }
        (ColorType::GrayscaleAlpha, NativeSamples::B8(v)) => {
            PngImage::GrayAlpha8(ImageBuf::new(v, dims)?)
        }
        (ColorType::GrayscaleAlpha, NativeSamples::B16(v)) => {
            PngImage::GrayAlpha16(ImageBuf::new(v, dims)?)
        }
        (ColorType::Truecolor, NativeSamples::B8(v)) => PngImage::Rgb8(ImageBuf::new(v, dims)?),
        (ColorType::Truecolor, NativeSamples::B16(v)) => PngImage::Rgb16(ImageBuf::new(v, dims)?),
        (ColorType::TruecolorAlpha, NativeSamples::B8(v)) => {
            PngImage::Rgba8(ImageBuf::new(v, dims)?)
        }
        (ColorType::TruecolorAlpha, NativeSamples::B16(v)) => {
            PngImage::Rgba16(ImageBuf::new(v, dims)?)
        }
        (ColorType::Indexed, NativeSamples::B8(indices)) => {
            PngImage::Indexed8(ImageBuf::new(indices, dims)?)
        }
        // Indexed depths are at most 8 (Table 12), so 16-bit indexed samples cannot exist.
        (ColorType::Indexed, NativeSamples::B16(_)) => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: bit depth not allowed for the colour type",
            ));
        }
    })
}

/// `Dimensions` for a validated header (post-IHDR this cannot fail, but stays checked).
fn dims(header: &Ihdr) -> Result<Dimensions> {
    Dimensions::new(header.width, header.height)
}

/// The palette's grey values (every entry has `r == g == b`), or `None` if any entry is coloured —
/// the gate for presenting an indexed file as a greyscale layout losslessly.
fn gray_palette(palette: &PngPalette) -> Option<Vec<u8>> {
    (0..palette.len())
        .map(|i| {
            let [r, g, b] = palette.rgb(i as u8)?;
            (r == g && g == b).then_some(r)
        })
        .collect()
}

/// A decoded image reduced to one plain interleaved buffer plus the layout tag describing it.
///
/// The boundary between what is PNG's business and what is not. Everything format-specific —
/// palette lookup, folding a tRNS key into a real alpha channel, §13.12 sub-byte scaling — is
/// resolved on this side, so what crosses into [`gamut_core::convert`] is an ordinary grey,
/// grey+alpha, RGB, or RGBA buffer that any format could have produced.
enum Presentable {
    /// 8-bit samples.
    B8(PixelFormat, Vec<u8>),
    /// 16-bit samples.
    B16(PixelFormat, Vec<u16>),
}

/// Reduces a natively decoded image to a [`Presentable`] buffer.
///
/// A 1-bit greyscale image without transparency stays [`PixelFormat::Bilevel`] rather than being
/// scaled to grey, so an [`ImageBuf<Bilevel>`] request is a copy rather than a re-thresholding.
fn presentable(native: NativeImage) -> Result<Presentable> {
    let depth = native.header.bit_depth;
    let key = native.trns_key;
    Ok(match (native.header.color, native.samples) {
        (ColorType::Grayscale, NativeSamples::B8(gray)) => {
            let scale = pack::gray8_scale(depth);
            match gray_key(key) {
                // A colour key turns the image into genuine grey+alpha.
                Some(transparent) => {
                    let mut out = Vec::with_capacity(gray.len() * 2);
                    for &value in &gray {
                        out.push(value * scale);
                        out.push(alpha8(u16::from(value) != transparent));
                    }
                    Presentable::B8(PixelFormat::GrayAlpha8, out)
                }
                // Unscaled 0/1 samples: exactly what Bilevel means.
                None if depth == 1 => Presentable::B8(PixelFormat::Bilevel, gray),
                None => {
                    let mut out = gray;
                    for value in &mut out {
                        *value *= scale;
                    }
                    Presentable::B8(PixelFormat::Gray8, out)
                }
            }
        }
        (ColorType::Grayscale, NativeSamples::B16(gray)) => match gray_key(key) {
            Some(transparent) => {
                let mut out = Vec::with_capacity(gray.len() * 2);
                for &value in &gray {
                    out.push(value);
                    out.push(alpha16(value != transparent));
                }
                Presentable::B16(PixelFormat::GrayAlpha16, out)
            }
            None => Presentable::B16(PixelFormat::Gray16, gray),
        },
        (ColorType::GrayscaleAlpha, NativeSamples::B8(v)) => {
            Presentable::B8(PixelFormat::GrayAlpha8, v)
        }
        (ColorType::GrayscaleAlpha, NativeSamples::B16(v)) => {
            Presentable::B16(PixelFormat::GrayAlpha16, v)
        }
        (ColorType::Truecolor, NativeSamples::B8(rgb)) => match key {
            Some(TransparencyKey::Rgb(kr, kg, kb)) => {
                let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
                for px in rgb.as_chunks::<3>().0 {
                    let opaque =
                        (u16::from(px[0]), u16::from(px[1]), u16::from(px[2])) != (kr, kg, kb);
                    out.extend_from_slice(&[px[0], px[1], px[2], alpha8(opaque)]);
                }
                Presentable::B8(PixelFormat::Rgba8, out)
            }
            _ => Presentable::B8(PixelFormat::Rgb8, rgb),
        },
        (ColorType::Truecolor, NativeSamples::B16(rgb)) => match key {
            Some(TransparencyKey::Rgb(kr, kg, kb)) => {
                let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
                for px in rgb.as_chunks::<3>().0 {
                    let opaque = (px[0], px[1], px[2]) != (kr, kg, kb);
                    out.extend_from_slice(&[px[0], px[1], px[2], alpha16(opaque)]);
                }
                Presentable::B16(PixelFormat::Rgba16, out)
            }
            _ => Presentable::B16(PixelFormat::Rgb16, rgb),
        },
        (ColorType::TruecolorAlpha, NativeSamples::B8(v)) => Presentable::B8(PixelFormat::Rgba8, v),
        (ColorType::TruecolorAlpha, NativeSamples::B16(v)) => {
            Presentable::B16(PixelFormat::Rgba16, v)
        }
        (ColorType::Indexed, NativeSamples::B8(indices)) => {
            // The palette is the image's real colour; indices alone carry none of it.
            let palette = native.palette.ok_or_else(lossy)?;
            let with_alpha = palette.has_transparency();
            // An all-grey palette is genuinely a greyscale image, so it is presented as one: the
            // conversion engine widens grey into colour losslessly, but never the reverse.
            match gray_palette(&palette) {
                Some(grays) if with_alpha => {
                    let mut out = Vec::with_capacity(indices.len() * 2);
                    for &index in &indices {
                        out.push(grays.get(usize::from(index)).copied().unwrap_or_default());
                        out.push(palette.alpha(index).unwrap_or(255));
                    }
                    Presentable::B8(PixelFormat::GrayAlpha8, out)
                }
                Some(grays) => Presentable::B8(
                    PixelFormat::Gray8,
                    indices
                        .iter()
                        .map(|&index| grays.get(usize::from(index)).copied().unwrap_or_default())
                        .collect(),
                ),
                None => {
                    let format = if with_alpha {
                        PixelFormat::Rgba8
                    } else {
                        PixelFormat::Rgb8
                    };
                    Presentable::B8(format, expand_palette(&indices, &palette, with_alpha))
                }
            }
        }
        // Indexed depths are at most 8 (Table 12), so 16-bit indexed samples cannot exist.
        (ColorType::Indexed, NativeSamples::B16(_)) => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: bit depth not allowed for the colour type",
            ));
        }
    })
}

// --- Typed presentation ------------------------------------------------------------------------
//
// Every impl funnels through `PngDecoder::present`, which reduces the file to a plain interleaved
// buffer (see `presentable`) and hands the layout change to `gamut_core::convert`. The typed path
// never inflates metadata chunks.

/// Expands validated palette indices to RGB or RGBA bytes. Indices were range-checked during
/// decode, so lookups cannot fail; a defensive default keeps the path panic-free regardless.
fn expand_palette(indices: &[u8], palette: &PngPalette, with_alpha: bool) -> Vec<u8> {
    let channels = if with_alpha { 4 } else { 3 };
    let mut out = Vec::with_capacity(indices.len() * channels);
    for &index in indices {
        let [red, green, blue] = palette.rgb(index).unwrap_or_default();
        out.push(red);
        out.push(green);
        out.push(blue);
        if with_alpha {
            out.push(palette.alpha(index).unwrap_or(255));
        }
    }
    out
}

impl PngDecoder {
    /// Decodes `data` and presents it as pixel layout `P`.
    ///
    /// [`Indexed8`] is served straight from the decoded indices: palette indices are not a colour
    /// layout, so there is nothing for the conversion engine to do with them (and it refuses them
    /// for exactly that reason). Every other layout goes through the shared engine.
    fn present<P: Pixel>(&self, data: &[u8]) -> Result<ImageBuf<P>> {
        let native = self.decode_native(data)?;
        let dimensions = dims(&native.header)?;
        if P::FORMAT == PixelFormat::Indexed8 {
            let NativeSamples::B8(indices) = native.samples else {
                return Err(lossy());
            };
            if native.header.color != ColorType::Indexed {
                return Err(lossy());
            }
            // `P` is Indexed8, so its sample type is u8 -- but only the value knows that, so the
            // buffer is rebuilt through the engine's identity path rather than transmuted.
            let raw = RawImage::new(&indices, PixelFormat::Indexed8, dimensions)?;
            return convert_from_raw(raw, self.policy);
        }
        match presentable(native)? {
            Presentable::B8(format, samples) => {
                convert_from_raw(RawImage::new(&samples, format, dimensions)?, self.policy)
            }
            Presentable::B16(format, samples) => {
                convert_from_raw(RawImage::new(&samples, format, dimensions)?, self.policy)
            }
        }
    }
}

impl DecodeImage<Bilevel> for PngDecoder {
    /// Accepts 1-bit greyscale without transparency; samples are presented as 0/1.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Bilevel>> {
        self.present(data)
    }
}

impl DecodeImage<Gray8> for PngDecoder {
    /// Accepts 1/2/4/8-bit greyscale without transparency; sub-byte samples scale exactly to
    /// 8 bits (§13.12).
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Gray8>> {
        self.present(data)
    }
}

impl DecodeImage<GrayAlpha8> for PngDecoder {
    /// Accepts 8-bit grey+alpha, and 1/2/4/8-bit greyscale (opaque, or keyed by tRNS).
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<GrayAlpha8>> {
        self.present(data)
    }
}

impl DecodeImage<Rgb8> for PngDecoder {
    /// Accepts 8-bit RGB, 1/2/4/8-bit greyscale, and an opaque palette, none with transparency.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgb8>> {
        self.present(data)
    }
}

impl DecodeImage<Rgba8> for PngDecoder {
    /// Accepts 8-bit RGBA and, by lossless widening, 8-bit RGB / grey+alpha, 1/2/4/8-bit
    /// greyscale, and palette images, honouring a tRNS colour key.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgba8>> {
        self.present(data)
    }
}

impl DecodeImage<Indexed8> for PngDecoder {
    /// Accepts indexed images at any bit depth, returning the bare palette indices (sub-byte
    /// indices are widened to one byte but never scaled — §13.12 rescaling does not apply to
    /// indices). The palette itself is carried by [`PngDecoder::decode`].
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Indexed8>> {
        self.present(data)
    }
}

impl DecodeImage<Gray16> for PngDecoder {
    /// Accepts 16-bit greyscale without transparency, and widens 1/2/4/8-bit greyscale into it.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Gray16>> {
        self.present(data)
    }
}

impl DecodeImage<GrayAlpha16> for PngDecoder {
    /// Accepts 16-bit grey+alpha, and widens the narrower greyscale layouts into it (honouring a
    /// tRNS key).
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<GrayAlpha16>> {
        self.present(data)
    }
}

impl DecodeImage<Rgb16> for PngDecoder {
    /// Accepts 16-bit RGB, and widens greyscale and 8-bit colour into it, without transparency.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgb16>> {
        self.present(data)
    }
}

impl DecodeImage<Rgba16> for PngDecoder {
    /// The widest target: accepts every non-indexed layout, honouring a tRNS colour key.
    fn decode_image(&self, data: &[u8]) -> Result<ImageBuf<Rgba16>> {
        self.present(data)
    }
}

/// The grey colour key, if one applies.
fn gray_key(key: Option<TransparencyKey>) -> Option<u16> {
    match key {
        Some(TransparencyKey::Gray(gray)) => Some(gray),
        _ => None,
    }
}

/// 8-bit alpha: fully opaque or fully transparent.
fn alpha8(opaque: bool) -> u8 {
    if opaque { 255 } else { 0 }
}

/// 16-bit alpha: fully opaque or fully transparent.
fn alpha16(opaque: bool) -> u16 {
    if opaque { u16::MAX } else { 0 }
}

#[cfg(test)]
mod tests {
    use gamut_core::{EncodeImage, ImageRef};

    use super::*;
    use crate::PngEncoder;

    /// The policy set by `convert_policy` must reach the typed decode: a 16-bit file cannot fill
    /// an 8-bit layout under the default, and can under a permissive one. A decoder that dropped
    /// the setter on the floor would refuse both times.
    #[test]
    fn convert_policy_reaches_the_typed_decode() {
        let dims = Dimensions::new(2, 1).unwrap();
        let gray16 = [0u16, 0xFFFF];
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_image(ImageRef::<Gray16>::new(&gray16, dims).unwrap(), &mut png)
            .expect("encode");

        let strict = PngDecoder::new();
        assert_eq!(
            DecodeImage::<Gray8>::decode_image(&strict, &png)
                .unwrap_err()
                .kind(),
            gamut_core::ErrorKind::Unsupported
        );

        let lax = PngDecoder::new().convert_policy(ConvertPolicy::permissive());
        let narrowed: ImageBuf<Gray8> = lax.decode_image(&png).expect("permissive decode");
        assert_eq!(narrowed.as_samples(), &[0, 255]);
    }

    fn rgb_png(w: u32, h: u32) -> (Vec<u8>, Vec<u8>) {
        let src: Vec<u8> = (0..(w * h * 3) as usize)
            .map(|i| (i.wrapping_mul(41) ^ (i >> 3)) as u8)
            .collect();
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_image(
                ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut png,
            )
            .unwrap();
        (png, src)
    }

    #[test]
    fn decodes_own_encoder_output() {
        for (w, h) in [(1, 1), (3, 2), (17, 13), (40, 30)] {
            let (png, src) = rgb_png(w, h);
            let img: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).unwrap();
            assert_eq!(img.dimensions(), Dimensions::new(w, h).unwrap());
            assert_eq!(img.as_samples(), src, "{w}x{h}");
        }
    }

    #[test]
    fn widens_gray_to_rgb_and_rgba() {
        let src: Vec<u8> = (0..20u8).collect();
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_image(
                ImageRef::<Gray8>::new(&src, Dimensions::new(5, 4).unwrap()).unwrap(),
                &mut png,
            )
            .unwrap();
        let rgb: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).unwrap();
        let expected_rgb: Vec<u8> = src.iter().flat_map(|&g| [g; 3]).collect();
        assert_eq!(rgb.as_samples(), expected_rgb);
        let rgba: ImageBuf<Rgba8> = PngDecoder::new().decode_image(&png).unwrap();
        let expected_rgba: Vec<u8> = src.iter().flat_map(|&g| [g, g, g, 255]).collect();
        assert_eq!(rgba.as_samples(), expected_rgba);
    }

    #[test]
    fn refuses_lossy_requests() {
        let src = vec![0u16; 4];
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_image(
                ImageRef::<Gray16>::new(&src, Dimensions::new(2, 2).unwrap()).unwrap(),
                &mut png,
            )
            .unwrap();
        // A 16-bit file cannot decode into 8-bit layouts.
        assert!(DecodeImage::<Gray8>::decode_image(&PngDecoder::new(), &png).is_err());
        assert!(DecodeImage::<Rgba8>::decode_image(&PngDecoder::new(), &png).is_err());
        // But it widens losslessly to 16-bit RGB.
        let rgb: ImageBuf<Rgb16> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(rgb.as_samples(), vec![0u16; 12]);
    }

    #[test]
    fn indexed_round_trips_at_every_depth() {
        use gamut_core::Indexed8 as Idx;
        for entries in [2usize, 4, 16, 200] {
            let rgb: Vec<[u8; 3]> = (0..entries)
                .map(|i| [i as u8, (i * 3) as u8, 255 - i as u8])
                .collect();
            let palette = PngPalette::with_transparency(&rgb, &[10, 250]).unwrap();
            let (w, h) = (13u32, 9u32);
            let indices: Vec<u8> = (0..(w * h) as usize).map(|i| (i % entries) as u8).collect();
            let mut png = Vec::new();
            PngEncoder::new()
                .encode_indexed8(
                    ImageRef::<Idx>::new(&indices, Dimensions::new(w, h).unwrap()).unwrap(),
                    &palette,
                    &mut png,
                )
                .unwrap();

            // Indices come back exactly, whatever sub-byte depth the encoder picked.
            let decoded: ImageBuf<Idx> = PngDecoder::new().decode_image(&png).unwrap();
            assert_eq!(decoded.as_samples(), indices, "{entries} entries");

            // RGBA expansion resolves palette colours and tRNS alpha.
            let rgba: ImageBuf<Rgba8> = PngDecoder::new().decode_image(&png).unwrap();
            let expected: Vec<u8> = indices
                .iter()
                .flat_map(|&idx| {
                    let [r, g, b] = rgb[usize::from(idx)];
                    let a = [10u8, 250].get(usize::from(idx)).copied().unwrap_or(255);
                    [r, g, b, a]
                })
                .collect();
            assert_eq!(rgba.as_samples(), expected, "{entries} entries");

            // RGB refuses the transparent palette (lossy), Rgb16 refuses outright.
            assert!(DecodeImage::<Rgb8>::decode_image(&PngDecoder::new(), &png).is_err());
            assert!(DecodeImage::<Rgb16>::decode_image(&PngDecoder::new(), &png).is_err());
        }
    }

    #[test]
    fn opaque_indexed_expands_to_rgb() {
        use gamut_core::Indexed8 as Idx;
        let rgb = [[1u8, 2, 3], [4, 5, 6], [7, 8, 9]];
        let palette = PngPalette::new(&rgb).unwrap();
        let indices = [0u8, 1, 2, 1, 0, 2];
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_indexed8(
                ImageRef::<Idx>::new(&indices, Dimensions::new(3, 2).unwrap()).unwrap(),
                &palette,
                &mut png,
            )
            .unwrap();
        let decoded: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).unwrap();
        let expected: Vec<u8> = indices
            .iter()
            .flat_map(|&idx| rgb[usize::from(idx)])
            .collect();
        assert_eq!(decoded.as_samples(), expected);
    }

    /// Hand-assembles a greyscale PNG from raw parts (the encoder cannot write interlaced files,
    /// and choosing the colour key by hand keeps the decoder's claim independent of
    /// `reduce`'s).
    fn build_gray_png(
        width: u32,
        height: u32,
        bit_depth: u8,
        interlace: u8,
        trns_key: Option<u16>,
        stream: &[u8],
    ) -> Vec<u8> {
        let mut png = crate::chunk::SIGNATURE.to_vec();
        let mut ihdr_data = [0u8; 13];
        ihdr_data[0..4].copy_from_slice(&width.to_be_bytes());
        ihdr_data[4..8].copy_from_slice(&height.to_be_bytes());
        ihdr_data[8] = bit_depth;
        ihdr_data[9] = 0; // greyscale
        ihdr_data[12] = interlace;
        crate::chunk::write_chunk(&mut png, *b"IHDR", &ihdr_data);
        if let Some(key) = trns_key {
            crate::chunk::write_chunk(&mut png, *b"tRNS", &key.to_be_bytes());
        }
        let mut idat = Vec::new();
        gamut_deflate::DeflateEncoder::new().zlib_compress(stream, &mut idat);
        crate::chunk::write_chunk(&mut png, *b"IDAT", &idat);
        crate::chunk::write_chunk(&mut png, *b"IEND", &[]);
        png
    }

    fn build_png(width: u32, height: u32, bit_depth: u8, interlace: u8, stream: &[u8]) -> Vec<u8> {
        build_gray_png(width, height, bit_depth, interlace, None, stream)
    }

    #[test]
    fn adam7_golden_3x3() {
        // A 3×3 grey8 image with values 1..=9 row-major transmits as five non-empty passes:
        // pass 1 → (0,0); pass 4 → (2,0); pass 5 → (0,2),(2,2); pass 6 → (1,0),(1,2);
        // pass 7 → (0,1),(1,1),(2,1). Every scanline carries a filter-type byte (None here).
        let stream = [
            0, 1, // pass 1
            0, 3, // pass 4
            0, 7, 9, // pass 5
            0, 2, 0, 8, // pass 6 (two 1-pixel rows)
            0, 4, 5, 6, // pass 7
        ];
        let png = build_png(3, 3, 8, 1, &stream);
        let img: ImageBuf<Gray8> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(img.as_samples(), [1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn adam7_passes_defilter_independently() {
        // An Up filter in the first row of a *later* pass must reference that pass's zero row,
        // not the previous pass's last row: pass 7 row filtered Up from zero reconstructs as-is.
        let stream = [
            0, 1, // pass 1
            0, 3, // pass 4
            1, 7, 2, // pass 5: Sub -> 7, 9
            2, 2, 2, 6, // pass 6: two rows, Up chains 2 then 8
            2, 4, 5, 6, // pass 7: Up over the pass's own zero row
        ];
        let png = build_png(3, 3, 8, 1, &stream);
        let img: ImageBuf<Gray8> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(img.as_samples(), [1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn adam7_1x1_has_a_single_pass_pixel() {
        let png = build_png(1, 1, 8, 1, &[0, 42]);
        let img: ImageBuf<Gray8> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(img.as_samples(), [42]);
    }

    #[test]
    fn sub_byte_gray_scales_exactly_in_every_typed_widening() {
        // 4-bit grey 3×2 with samples 1,2,3 / 4,5,15 (scale ×17); packed rows 0x12 0x30 / 0x45 0xF0.
        let png = build_png(3, 2, 4, 0, &[0, 0x12, 0x30, 0, 0x45, 0xF0]);
        let values = [1u8, 2, 3, 4, 5, 15];
        let gray: ImageBuf<Gray8> = PngDecoder::new().decode_image(&png).unwrap();
        let scaled: Vec<u8> = values.iter().map(|&v| v * 17).collect();
        assert_eq!(gray.as_samples(), scaled);
        let rgb: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&png).unwrap();
        let replicated: Vec<u8> = values.iter().flat_map(|&v| [v * 17; 3]).collect();
        assert_eq!(rgb.as_samples(), replicated);
        let ga: ImageBuf<GrayAlpha8> = PngDecoder::new().decode_image(&png).unwrap();
        let opaque: Vec<u8> = values.iter().flat_map(|&v| [v * 17, 255]).collect();
        assert_eq!(ga.as_samples(), opaque);
    }

    #[test]
    fn sub_16_files_widen_by_257_into_every_16bit_layout() {
        // The same 4-bit grey file, presented as each 16-bit layout: exact §13.12 scaling is
        // ×17 to 8 bits then ×257 to 16 (= ×4369 total).
        let png = build_png(3, 2, 4, 0, &[0, 0x12, 0x30, 0, 0x45, 0xF0]);
        let values = [1u16, 2, 3, 4, 5, 15];
        let gray: ImageBuf<Gray16> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(
            gray.as_samples(),
            values.map(|v| v * 4369),
            "Gray16 widening"
        );
        let ga: ImageBuf<GrayAlpha16> = PngDecoder::new().decode_image(&png).unwrap();
        let opaque: Vec<u16> = values.iter().flat_map(|&v| [v * 4369, u16::MAX]).collect();
        assert_eq!(ga.as_samples(), opaque, "GrayAlpha16 widening");
        let rgb: ImageBuf<Rgb16> = PngDecoder::new().decode_image(&png).unwrap();
        let replicated: Vec<u16> = values.iter().flat_map(|&v| [v * 4369; 3]).collect();
        assert_eq!(rgb.as_samples(), replicated, "Rgb16 widening");
        let rgba: ImageBuf<Rgba16> = PngDecoder::new().decode_image(&png).unwrap();
        let widened: Vec<u16> = values
            .iter()
            .flat_map(|&v| [v * 4369, v * 4369, v * 4369, u16::MAX])
            .collect();
        assert_eq!(rgba.as_samples(), widened, "Rgba16 widening");
    }

    #[test]
    fn grey_layouts_accept_only_all_grey_palettes() {
        use gamut_core::Indexed8 as Idx;
        let encode = |palette: &PngPalette| {
            let indices = [0u8, 1, 0, 1, 1, 0];
            let mut png = Vec::new();
            PngEncoder::new()
                .encode_indexed8(
                    ImageRef::<Idx>::new(&indices, Dimensions::new(3, 2).unwrap()).unwrap(),
                    palette,
                    &mut png,
                )
                .unwrap();
            png
        };

        // An entry with r == g but a different b is coloured: both grey layouts must refuse it.
        let colored = encode(&PngPalette::new(&[[7, 7, 200], [3, 3, 3]]).unwrap());
        assert!(DecodeImage::<Gray8>::decode_image(&PngDecoder::new(), &colored).is_err());
        assert!(DecodeImage::<GrayAlpha8>::decode_image(&PngDecoder::new(), &colored).is_err());

        // An all-grey opaque palette resolves to its grey values (plus opaque alpha).
        let grey = encode(&PngPalette::new(&[[7, 7, 7], [200, 200, 200]]).unwrap());
        let gray: ImageBuf<Gray8> = PngDecoder::new().decode_image(&grey).unwrap();
        assert_eq!(gray.as_samples(), [7, 200, 7, 200, 200, 7]);
        let ga: ImageBuf<GrayAlpha8> = PngDecoder::new().decode_image(&grey).unwrap();
        assert_eq!(
            ga.as_samples(),
            [7, 255, 200, 255, 7, 255, 200, 255, 200, 255, 7, 255]
        );

        // A grey palette with transparency: Gray8 refuses (alpha is lossy), GrayAlpha8 keeps it.
        let keyed =
            encode(&PngPalette::with_transparency(&[[7, 7, 7], [200, 200, 200]], &[128]).unwrap());
        assert!(DecodeImage::<Gray8>::decode_image(&PngDecoder::new(), &keyed).is_err());
        let ga: ImageBuf<GrayAlpha8> = PngDecoder::new().decode_image(&keyed).unwrap();
        assert_eq!(
            ga.as_samples(),
            [7, 128, 200, 255, 7, 128, 200, 255, 200, 255, 7, 128]
        );
    }

    #[test]
    fn gray8_colour_key_resolves_in_grayalpha8_and_refusals_hold() {
        // 8-bit grey 3×1 with samples 7, 8, 7 and colour key 7.
        let png = build_gray_png(3, 1, 8, 0, Some(7), &[0, 7, 8, 7]);
        let ga: ImageBuf<GrayAlpha8> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(ga.as_samples(), [7, 0, 8, 255, 7, 0]);
        // Alpha-less layouts refuse the keyed file (dropping transparency is lossy).
        assert!(DecodeImage::<Gray8>::decode_image(&PngDecoder::new(), &png).is_err());
        assert!(DecodeImage::<Rgb8>::decode_image(&PngDecoder::new(), &png).is_err());
        assert!(DecodeImage::<Bilevel>::decode_image(&PngDecoder::new(), &png).is_err());
    }

    #[test]
    fn bilevel_requires_exactly_1bit_keyless_gray() {
        // Depth is the only violation: 8-bit grey must NOT present as Bilevel.
        let gray8 = build_png(2, 1, 8, 0, &[0, 1, 0]);
        assert!(DecodeImage::<Bilevel>::decode_image(&PngDecoder::new(), &gray8).is_err());
        // Transparency is the only violation: keyed 1-bit grey must NOT present as Bilevel...
        let keyed = build_gray_png(2, 1, 1, 0, Some(0), &[0, 0x80]);
        assert!(DecodeImage::<Bilevel>::decode_image(&PngDecoder::new(), &keyed).is_err());
        // ...while the keyless twin decodes to 0/1 samples.
        let plain = build_png(2, 1, 1, 0, &[0, 0x80]);
        let img: ImageBuf<Bilevel> = PngDecoder::new().decode_image(&plain).unwrap();
        assert_eq!(img.as_samples(), [1, 0]);
    }

    #[test]
    fn gray16_colour_key_resolves_in_16bit_widenings() {
        // 16-bit grey 3×1 with samples 7, 8, 7 and colour key 7.
        let png = build_gray_png(3, 1, 16, 0, Some(7), &[0, 0, 7, 0, 8, 0, 7]);
        // The keyed file refuses alpha-less Gray16/Rgb16.
        assert!(DecodeImage::<Gray16>::decode_image(&PngDecoder::new(), &png).is_err());
        assert!(DecodeImage::<Rgb16>::decode_image(&PngDecoder::new(), &png).is_err());
        // GrayAlpha16 and Rgba16 resolve the key to alpha zero, exactly on matching pixels.
        let ga: ImageBuf<GrayAlpha16> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(ga.as_samples(), [7, 0, 8, u16::MAX, 7, 0]);
        let rgba: ImageBuf<Rgba16> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(
            rgba.as_samples(),
            [7, 7, 7, 0, 8, 8, 8, u16::MAX, 7, 7, 7, 0]
        );
        // The keyless twin widens opaque.
        let plain = build_png(2, 1, 16, 0, &[0, 0, 9, 1, 4]);
        let rgba: ImageBuf<Rgba16> = PngDecoder::new().decode_image(&plain).unwrap();
        assert_eq!(
            rgba.as_samples(),
            [9, 9, 9, u16::MAX, 260, 260, 260, u16::MAX]
        );
    }

    #[test]
    fn grayalpha16_widens_to_rgba16() {
        let src: Vec<u16> = vec![100, 200, 300, 400];
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_image(
                ImageRef::<GrayAlpha16>::new(&src, Dimensions::new(2, 1).unwrap()).unwrap(),
                &mut png,
            )
            .unwrap();
        let rgba: ImageBuf<Rgba16> = PngDecoder::new().decode_image(&png).unwrap();
        assert_eq!(rgba.as_samples(), [100, 100, 100, 200, 300, 300, 300, 400]);
    }

    #[test]
    fn suggested_palette_on_truecolor_is_shape_checked_then_ignored() {
        let src = [1u8, 2, 3, 4, 5, 6];
        let stream = [0u8, 1, 2, 3, 4, 5, 6];
        let build = |plte: &[u8]| {
            let mut png = crate::chunk::SIGNATURE.to_vec();
            let mut ihdr_data = [0u8; 13];
            ihdr_data[0..4].copy_from_slice(&2u32.to_be_bytes());
            ihdr_data[4..8].copy_from_slice(&1u32.to_be_bytes());
            ihdr_data[8] = 8;
            ihdr_data[9] = 2; // truecolour
            crate::chunk::write_chunk(&mut png, *b"IHDR", &ihdr_data);
            crate::chunk::write_chunk(&mut png, *b"PLTE", plte);
            let mut idat = Vec::new();
            gamut_deflate::DeflateEncoder::new().zlib_compress(&stream, &mut idat);
            crate::chunk::write_chunk(&mut png, *b"IDAT", &idat);
            crate::chunk::write_chunk(&mut png, *b"IEND", &[]);
            png
        };
        // A valid 86-entry suggested palette is accepted and ignored (86 entries also pins the
        // `len / 3` arithmetic: neither `%` nor `*` lands in 1..=256).
        let suggested: Vec<u8> = (0..258).map(|i| i as u8).collect();
        let img: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&build(&suggested)).unwrap();
        assert_eq!(img.as_samples(), src);
        // Shape violations are still rejected.
        assert!(
            DecodeImage::<Rgb8>::decode_image(&PngDecoder::new(), &build(&[1, 2, 3, 4])).is_err()
        );
        assert!(DecodeImage::<Rgb8>::decode_image(&PngDecoder::new(), &build(&[0; 771])).is_err());
    }

    #[test]
    fn rich_decode_surfaces_metadata_and_native_image() {
        use crate::SrgbIntent;
        use crate::decoded::PngImage;

        let (w, h) = (6u32, 4u32);
        let src: Vec<u8> = (0..(w * h * 3) as usize).map(|i| i as u8).collect();
        let exif = [0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00];
        let xmp = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>";
        let comment = "zlib-compressed comment body";
        let mut png = Vec::new();
        PngEncoder::new()
            .with_gamma(1.0 / 2.2)
            .with_srgb(SrgbIntent::Perceptual)
            .with_chromaticities((0.3127, 0.3290), (0.64, 0.33), (0.30, 0.60), (0.15, 0.06))
            .with_exif(&exif)
            .with_icc_profile("prof", b"not-a-real-profile-but-bytes")
            .with_xmp(xmp)
            .with_text("Title", "gamut")
            .with_compressed_text("Comment", comment)
            .encode_image(
                ImageRef::<Rgb8>::new(&src, Dimensions::new(w, h).unwrap()).unwrap(),
                &mut png,
            )
            .unwrap();

        let decoded = PngDecoder::new().decode(&png).unwrap();
        assert_eq!((decoded.header.width, decoded.header.height), (w, h));
        assert_eq!(decoded.header.color_type, ColorType::Truecolor);
        assert_eq!(decoded.header.bit_depth, 8);
        assert!(!decoded.header.interlaced);
        match &decoded.image {
            PngImage::Rgb8(img) => assert_eq!(img.as_samples(), src),
            other => panic!("expected Rgb8, got {other:?}"),
        }
        assert_eq!(decoded.gamma, Some(45455));
        assert_eq!(decoded.srgb, Some(SrgbIntent::Perceptual));
        let chrm = decoded.chromaticities.unwrap();
        assert_eq!(chrm.white, (31270, 32900));
        assert_eq!(chrm.blue, (15000, 6000));
        assert_eq!(decoded.exif.as_deref(), Some(&exif[..]));
        let icc = decoded.icc_profile.unwrap();
        assert_eq!(icc.name, "prof");
        assert_eq!(icc.profile, b"not-a-real-profile-but-bytes");
        assert_eq!(decoded.xmp.as_deref(), Some(xmp.as_bytes()));
        assert_eq!(decoded.texts.len(), 2);
        assert_eq!(
            (
                decoded.texts[0].keyword.as_str(),
                decoded.texts[0].text.as_str()
            ),
            ("Title", "gamut")
        );
        assert_eq!(decoded.texts[1].text, comment);
        assert!(decoded.palette.is_none());
        assert!(decoded.transparency.is_none());
        assert!(decoded.cicp.is_none());
    }

    #[test]
    fn rich_decode_carries_palette_and_indices() {
        use gamut_core::Indexed8 as Idx;

        use crate::decoded::PngImage;

        let rgb = [[9u8, 8, 7], [1, 2, 3]];
        let palette = PngPalette::with_transparency(&rgb, &[128]).unwrap();
        let indices = [0u8, 1, 1, 0];
        let mut png = Vec::new();
        PngEncoder::new()
            .encode_indexed8(
                ImageRef::<Idx>::new(&indices, Dimensions::new(2, 2).unwrap()).unwrap(),
                &palette,
                &mut png,
            )
            .unwrap();
        let decoded = PngDecoder::new().decode(&png).unwrap();
        assert_eq!(decoded.header.color_type, ColorType::Indexed);
        match &decoded.image {
            PngImage::Indexed8(img) => assert_eq!(img.as_samples(), indices),
            other => panic!("expected Indexed8, got {other:?}"),
        }
        let carried = decoded.palette.unwrap();
        assert_eq!(carried.rgb(0), Some([9, 8, 7]));
        assert_eq!(carried.alpha(0), Some(128));
        assert_eq!(carried.alpha(1), Some(255));
    }

    #[test]
    fn dimension_limit_is_exact() {
        let (png, _) = rgb_png(17, 13);
        let decoder = PngDecoder::new().with_max_dimensions(17, 13);
        assert!(DecodeImage::<Rgb8>::decode_image(&decoder, &png).is_ok());
        let narrow = PngDecoder::new().with_max_dimensions(16, 13);
        assert!(DecodeImage::<Rgb8>::decode_image(&narrow, &png).is_err());
        let short = PngDecoder::new().with_max_dimensions(17, 12);
        assert!(DecodeImage::<Rgb8>::decode_image(&short, &png).is_err());
    }

    #[test]
    fn byte_budget_is_exact() {
        let (png, _) = rgb_png(8, 8);
        let exact = PngDecoder::new().with_max_image_bytes(8 * 8 * 3);
        assert!(DecodeImage::<Rgb8>::decode_image(&exact, &png).is_ok());
        let tight = PngDecoder::new().with_max_image_bytes(8 * 8 * 3 - 1);
        assert!(matches!(
            DecodeImage::<Rgb8>::decode_image(&tight, &png),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));
    }
}
