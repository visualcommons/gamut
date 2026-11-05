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
//! Two concerns live beside this pipeline rather than in it. Reading metadata without the pixels
//! — the chunk walk behind [`metadata`] and [`PngDecoder::metadata`] — is
//! [`crate::walk`]'s. Presenting the decoded image as a caller's pixel layout — the typed
//! [`DecodeImage`] impls, which perform **lossless widening only** —
//! greyscale replicates into RGB, an opaque alpha channel can be added, sub-byte greys scale
//! exactly to 8 bits, 8-bit samples widen to 16 — and refuse lossy requests (dropping alpha or
//! transparency, narrowing 16-bit samples) with an
//! [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error unless
//! [`PngDecoder::with_convert_policy`] permits them — is [`crate::present`]'s.

use gamut_core::convert::ConvertPolicy;
#[cfg(doc)]
use gamut_core::{DecodeImage, Gray8};
use gamut_core::{DecodeLimits, Dimensions, Error, ImageBuf, Palette, Result};

use crate::backend::{IdatInfo, Inflaters, ZlibInflater, run_inflaters};
use crate::chunk::{CABX, ChunkReader};
use crate::color::ColorType;
use crate::decoded::{DecodedPng, PngHeader, PngImage, PngMetadata};
use crate::filter::{self, FilterType};
use crate::ihdr::{self, Ihdr};
use crate::palette::palette_from_chunks;
#[cfg(doc)]
use crate::walk::metadata;
use crate::walk::{collected, walk_metadata_chunks};
use crate::{adam7, inflate, pack};

/// Default cap on the decoded sample buffer: the workspace's
/// [`DecodeLimits::DEFAULT_MAX_IMAGE_BYTES`], 64 MiB, a 4096×4096 RGBA8 image.
///
/// `pub(crate)` because [`crate::deconstruct`] reports against the same budget: a file this
/// decoder decodes is one the report walk will inflate to count filters.
pub(crate) const DEFAULT_MAX_IMAGE_BYTES: usize = DecodeLimits::DEFAULT_MAX_IMAGE_BYTES;
/// Default cumulative cap on metadata payloads — inflated iCCP/zTXt/iTXt plus the raw `caBX`
/// manifest store: 16 MiB.
pub(crate) const DEFAULT_MAX_METADATA_BYTES: usize = 16 << 20;

/// The one refusal message for typed decodes the layout cannot hold losslessly.
pub(crate) fn lossy() -> Error {
    Error::unsupported(
        env!("CARGO_PKG_NAME"),
        "PNG: this pixel layout cannot hold the image losslessly; use PngDecoder::decode",
    )
}

/// A reusable PNG decoder with hostile-input limits.
///
/// The defaults ([`DecodeLimits::default`]) accept anything the spec allows dimensionally — IHDR
/// itself bounds each side to 2³¹ − 1 (§11.2.1) — but cap the decoded sample buffer at 64 MiB.
/// The byte budget, not the dimension caps, is the real safety guard (it bounds both the inflated
/// scanline stream and the sample buffer, so peak memory stays within roughly twice the budget
/// plus the input). Tighten the dimension caps when the application knows its domain.
#[derive(Debug, Clone)]
pub struct PngDecoder {
    limits: DecodeLimits,
    max_metadata_bytes: usize,
    backends: Inflaters,
    /// Crate-visible for the typed presentation (`present.rs`), which applies it.
    pub(crate) policy: ConvertPolicy,
}

impl Default for PngDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PngDecoder {
    /// Creates a decoder with the default limits ([`DecodeLimits::default`]: spec-maximum
    /// dimensions and 64 MiB of decoded samples; and 16 MiB of inflated metadata).
    #[must_use]
    pub fn new() -> Self {
        Self {
            limits: DecodeLimits::new(),
            max_metadata_bytes: DEFAULT_MAX_METADATA_BYTES,
            backends: Inflaters::new(),
            policy: ConvertPolicy::lossless(),
        }
    }

    /// Appends a pluggable [`ZlibInflater`] backend for the IDAT zlib stream (issue #278).
    ///
    /// Backends are tried in **push order**; the built-in `miniz_oxide` inflater is the implicit
    /// tail, so pushing nothing keeps today's behaviour exactly. A backend that returns `false`
    /// from [`ZlibInflater::supports`] is skipped — the only fall-through; one that accepts and
    /// then fails, with any error, propagates it rather than falling back.
    ///
    /// # The output cap stays with the host
    ///
    /// The decoder's own budget (the image-byte cap of [`with_limits`](Self::with_limits)) is
    /// passed to the backend as `max_out` *and* re-checked against the bytes it returns. A backend
    /// that overshoots is rejected with an
    /// [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error, never truncated, so
    /// a hostile or
    /// buggy backend cannot turn a zlib bomb into an out-of-memory.
    ///
    /// Only the **IDAT** stream goes through the seam. Ancillary compressed payloads (iCCP, zTXt,
    /// compressed iTXt) always use the built-in inflater: they are not the codestream, and they
    /// carry their own, much smaller budget.
    ///
    /// # Cloning shares backends
    ///
    /// [`PngDecoder`] is [`Clone`], and cloning copies the registry's list of *handles*: the clone
    /// and the original drive the **same** backend instances (held behind `Arc<Mutex<…>>`, since
    /// decoding takes `&self`), while a backend pushed to one afterwards is not seen by the other.
    /// Push a fresh backend instance if you need independent state.
    ///
    /// Takes `&mut self` and returns `&mut Self` — unlike the `with_*` builder setters — because a
    /// registry accumulates rather than replaces.
    pub fn push_backend(&mut self, backend: impl ZlibInflater + Send + 'static) -> &mut Self {
        self.backends
            .push(std::sync::Arc::new(std::sync::Mutex::new(backend)));
        self
    }

    /// Sets the image limits: the per-side dimension caps and the decoded-sample byte budget
    /// ([`DecodeLimits::max_image_bytes`], default 64 MiB). An image past any of them is refused
    /// with an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error before
    /// anything is allocated for its pixels.
    #[must_use]
    pub fn with_limits(mut self, limits: DecodeLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Caps the *cumulative* size of metadata payloads — the inflated iCCP, zTXt and compressed
    /// iTXt, plus the raw C2PA manifest store (`caBX`), which is uncompressed but sized by the
    /// file, together (default 16 MiB). Payloads past the budget are skipped, not errors; the
    /// typed [`DecodeImage`] path never inflates or copies metadata at all.
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
    pub fn with_convert_policy(mut self, policy: ConvertPolicy) -> Self {
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
    /// Metadata-bearing ancillary chunks in file order (populated only when requested), holding
    /// only chunks in a position where a manifest store may appear — see `c2pa_after_idat`.
    ancillary: Vec<([u8; 4], &'a [u8])>,
    /// CRC-valid `caBX` chunks found *after* `IDAT`. C2PA §A.3.2 places the store before `IDAT`
    /// and calls data after it bad-form, so these are never the store; they are counted rather
    /// than dropped silently, because a `caBX` appended to a finished file is what an injection
    /// attempt looks like.
    c2pa_after_idat: usize,
}

/// Decoded samples in the file's native value range: one byte per sample for depths ≤ 8
/// (sub-byte depths unpacked but **unscaled**), native-endian `u16` for depth 16.
pub(crate) enum NativeSamples {
    B8(Vec<u8>),
    B16(Vec<u16>),
}

/// A fully decoded image in its native layout, before presentation as a pixel type.
pub(crate) struct NativeImage {
    pub(crate) header: Ihdr,
    pub(crate) samples: NativeSamples,
    pub(crate) palette: Option<Palette>,
    pub(crate) trns_key: Option<TransparencyKey>,
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
        let mut c2pa_after_idat = 0usize;
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
                        // The one chunk type whose *position* decides whether it is metadata at
                        // all: a manifest store precedes IDAT (C2PA §A.3.2), so one after IDAT is
                        // counted as ignored rather than offered as the store.
                        if chunk.chunk_type == CABX && seen_idat {
                            c2pa_after_idat += 1;
                        } else {
                            ancillary.push((chunk.chunk_type, chunk.data));
                        }
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
            c2pa_after_idat,
        })
    }

    /// Enforces the dimension and byte-budget limits (all math checked) and returns the exact
    /// byte length of the filtered scanline stream the IDAT data must inflate to.
    fn check_limits(&self, header: &Ihdr) -> Result<usize> {
        if header.width > self.limits.max_width() || header.height > self.limits.max_height() {
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
        if native_bytes > self.limits.max_image_bytes() {
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
    /// (eXIf/ICC/XMP/text and the C2PA manifest store, plus parsed gAMA/cHRM/sRGB/cICP and
    /// sBIT/bKGD/pHYs/tIME values).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error for a
    /// malformed file and an [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error
    /// for one exceeding the decoder's limits or using an unknown critical chunk. Malformed
    /// *metadata* payloads are not errors: the affected chunk is skipped (§13.1) and its field
    /// stays empty.
    pub fn decode(&self, data: &[u8]) -> Result<DecodedPng> {
        let parsed = self.parse_stream(data, true)?;
        let meta = collected(
            &parsed.ancillary,
            self.max_metadata_bytes,
            parsed.c2pa_after_idat,
            &parsed.header,
        );
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
            metadata: meta,
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
        let (chunks, c2pa_after_idat, header) = walk_metadata_chunks(data)?;
        Ok(collected(
            &chunks,
            self.max_metadata_bytes,
            c2pa_after_idat,
            &header,
        ))
    }

    /// Runs the typed pipeline: parse (without metadata) → decode.
    pub(crate) fn decode_native(&self, data: &[u8]) -> Result<NativeImage> {
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
            && indices
                .iter()
                .any(|&idx| usize::from(idx) >= palette.rgb_entries().len())
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

/// Validates PLTE presence/shape and tRNS shape against the colour type (§11.2.2, §11.3.1.1),
/// returning the palette for indexed images and the parsed colour key for greyscale/truecolour
/// images.
fn validate_plte_and_trns(
    header: &Ihdr,
    plte: Option<&[u8]>,
    trns: Option<&[u8]>,
) -> Result<(Option<Palette>, Option<TransparencyKey>)> {
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
        let palette = palette_from_chunks(plte, trns)?;
        // §11.2.2: the palette must not have more entries than the bit depth can reference.
        if palette.rgb_entries().len() > 1 << header.bit_depth {
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

/// The native-range mask for a two-byte sample: §11.3.1.1 stores a colour key, and §11.3.4.1 a
/// background, as 2-byte values but only the low `bit_depth` bits are significant below depth 16.
pub(crate) fn depth_mask(bit_depth: u8) -> u16 {
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
pub(crate) fn dims(header: &Ihdr) -> Result<Dimensions> {
    Dimensions::new(header.width, header.height)
}

#[cfg(test)]
pub(crate) mod tests {
    use gamut_core::{DecodeImage, EncodeImage, Gray8, ImageRef, Rgb8, Rgb16, Rgba8};

    use super::*;
    use crate::PngEncoder;

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
    fn indexed_round_trips_at_every_depth() {
        use gamut_core::Indexed8 as Idx;
        for entries in [2usize, 4, 16, 200] {
            let rgb: Vec<[u8; 3]> = (0..entries)
                .map(|i| [i as u8, (i * 3) as u8, 255 - i as u8])
                .collect();
            let palette = Palette::with_alpha(&rgb, &[10, 250]).unwrap();
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

    /// Hand-assembles a greyscale PNG from raw parts (the encoder cannot write interlaced files,
    /// and choosing the colour key by hand keeps the decoder's claim independent of
    /// `reduce`'s).
    pub(crate) fn build_gray_png(
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

    pub(crate) fn build_png(
        width: u32,
        height: u32,
        bit_depth: u8,
        interlace: u8,
        stream: &[u8],
    ) -> Vec<u8> {
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
            .with_gamma(45_455)
            .with_srgb(SrgbIntent::Perceptual)
            .with_cicp(crate::Cicp::new(9, 16, true))
            .with_chromaticities(crate::Chromaticities::new(
                (31_270, 32_900),
                (64_000, 33_000),
                (30_000, 60_000),
                (15_000, 6_000),
            ))
            .with_exif(&exif)
            .with_icc_profile_name("prof")
            .with_icc_profile(b"not-a-real-profile-but-bytes")
            .with_xmp(xmp.as_bytes())
            .unwrap()
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
        assert_eq!(decoded.metadata.gamma, Some(45455));
        assert_eq!(decoded.metadata.srgb, Some(SrgbIntent::Perceptual));
        let chrm = decoded.metadata.chromaticities.unwrap();
        assert_eq!(chrm.white, (31270, 32900));
        assert_eq!(chrm.blue, (15000, 6000));
        assert_eq!(decoded.metadata.exif.as_deref(), Some(&exif[..]));
        let icc = decoded.metadata.icc_profile.as_ref().unwrap();
        assert_eq!(icc.name, "prof");
        assert_eq!(icc.profile, b"not-a-real-profile-but-bytes");
        assert_eq!(decoded.metadata.xmp(), Some(xmp.as_bytes()));
        // The packet is the first annotation, in the order the setters ran.
        assert_eq!(decoded.metadata.texts.len(), 3);
        assert_eq!(
            (
                decoded.metadata.texts[1].keyword.as_str(),
                decoded.metadata.texts[1].text.as_str()
            ),
            ("Title", "gamut")
        );
        assert_eq!(decoded.metadata.texts[2].text, comment);
        assert!(decoded.palette.is_none());
        assert!(decoded.transparency.is_none());
        let cicp = decoded.metadata.cicp.expect("cICP present");
        assert_eq!(
            (
                cicp.color_primaries,
                cicp.transfer_function,
                cicp.matrix_coefficients,
                cicp.full_range
            ),
            (9, 16, 0, true)
        );
    }

    #[test]
    fn rich_decode_carries_palette_and_indices() {
        use gamut_core::Indexed8 as Idx;

        use crate::decoded::PngImage;

        let rgb = [[9u8, 8, 7], [1, 2, 3]];
        let palette = Palette::with_alpha(&rgb, &[128]).unwrap();
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

    /// Default limits with the per-side caps set to `width` × `height`.
    fn limits(width: u32, height: u32) -> DecodeLimits {
        DecodeLimits::new()
            .with_max_width(width)
            .with_max_height(height)
    }

    #[test]
    fn dimension_limit_is_exact() {
        let (png, _) = rgb_png(17, 13);
        let decoder = PngDecoder::new().with_limits(limits(17, 13));
        assert!(DecodeImage::<Rgb8>::decode_image(&decoder, &png).is_ok());
        let narrow = PngDecoder::new().with_limits(limits(16, 13));
        assert!(DecodeImage::<Rgb8>::decode_image(&narrow, &png).is_err());
        let short = PngDecoder::new().with_limits(limits(17, 12));
        assert!(DecodeImage::<Rgb8>::decode_image(&short, &png).is_err());
    }

    #[test]
    fn byte_budget_is_exact() {
        let (png, _) = rgb_png(8, 8);
        let exact =
            PngDecoder::new().with_limits(DecodeLimits::new().with_max_image_bytes(8 * 8 * 3));
        assert!(DecodeImage::<Rgb8>::decode_image(&exact, &png).is_ok());
        let tight =
            PngDecoder::new().with_limits(DecodeLimits::new().with_max_image_bytes(8 * 8 * 3 - 1));
        assert!(matches!(
            DecodeImage::<Rgb8>::decode_image(&tight, &png),
            Err(error) if error.kind() == gamut_core::ErrorKind::Unsupported
        ));
    }
}
