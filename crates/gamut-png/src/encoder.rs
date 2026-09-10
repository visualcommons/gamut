//! The PNG encoder: a [`PngEncoder`] builder implementing [`gamut_core::EncodeImage`] for each
//! supported pixel layout. This covers the four non-indexed colour types at 8- and 16-bit depth;
//! palette, sub-byte depths, ancillary chunks, and space optimisations layer on in later phases.
//!
//! # How a tie is broken
//!
//! Several candidate encodings are raced and the smallest kept ([`PngEncoder::cleaned_or_plain`],
//! [`PngEncoder::write_reduced_or_native`]). At *equal* size the size contract cannot choose, so
//! one rule decides all three tie-breaks:
//!
//! 1. **Prefer the candidate that discards less of the input's information.** Transparent cleanup
//!    is this crate's one lossy knob — it rewrites samples no decoder renders — and it is opt-in
//!    for a size win; with no win there is nothing to trade the exactness for, so the byte-exact
//!    candidate stands ([`prefers_plain`]).
//! 2. **Where the candidates are information-equivalent, fall back to a fixed order:**
//!    `chunked ≻ chunk-free ≻ native` ([`prefers_chunk_free`], [`prefers_native`]). Every lossless
//!    reduction preserves exactly the same image, so nothing distinguishes them at equal size; the
//!    order exists only so that the output is a function of the input rather than of which
//!    candidate happened to be encoded first. `tests/size_contract.rs`'s
//!    `encoded_size_is_deterministic` is what pins that.

use gamut_core::{
    Bilevel, Dimensions, EncodeImage, Error, Gray8, Gray16, GrayAlpha8, GrayAlpha16, ImageRef,
    Indexed8, Pixel, Result, Rgb8, Rgb16, Rgba8, Rgba16,
};
use gamut_deflate::{DeflateEncoder, Level};

use crate::ancillary::{
    Ancillary, PaletteOrigin, PhysicalUnit, SrgbIntent, WrittenHeader, WrittenPalette,
    background_entry,
};
use crate::backend::{IdatDeflater, IdatInfo, Registry, run_deflaters};
use crate::chunk::{self, C2paSpan, SIGNATURE};
use crate::color::ColorType;
use crate::decoded::{
    Chromaticities, Cicp, DecodedPng, IccProfile, PngMetadata, TextChunk, TextChunkKind, XmpFraming,
};
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

/// The metadata fields [`PngMetadata`] and [`DecodedPng`] both carry, borrowed.
///
/// The two read surfaces agree field for field on purpose (one reads the pixels, one does not),
/// so [`PngEncoder::with_metadata`] and [`PngEncoder::with_metadata_from`] are the same function
/// over two shapes. Borrowing rather than cloning into a `PngMetadata` keeps a large ICC profile
/// or EXIF block from being copied twice on the way into the encoder.
struct MetadataView<'a> {
    exif: Option<&'a [u8]>,
    icc_profile: Option<&'a IccProfile>,
    xmp: Option<&'a [u8]>,
    /// How the source framed its XMP packet (§11.3.3.4): compression flag, language tag,
    /// translated keyword. Carried beside the packet because the packet has its own field.
    xmp_framing: Option<&'a XmpFraming>,
    texts: &'a [TextChunk],
    gamma: Option<u32>,
    chromaticities: Option<Chromaticities>,
    srgb: Option<SrgbIntent>,
    cicp: Option<Cicp>,
    /// Whether the source carried a C2PA manifest store. Only the presence is needed: a store is
    /// never carried, but a caller has to be told it was left behind.
    c2pa: bool,
}

/// Something [`PngEncoder::with_metadata`] could not do faithfully with a payload it was given.
///
/// Preservation exists to stop metadata disappearing quietly, so anything a carry cannot take —
/// and anything it takes only by writing bytes the specification does not endorse — is named
/// rather than passed over. Read them back with [`PngEncoder::metadata_notices`] and tell the
/// user; `gamut convert` does. [`carried`](Self::carried) separates the two cases: a payload
/// left behind from one that reached the output with a caveat on it.
///
/// This is deliberately **not** an error channel. The only thing that stops an encode is a null
/// byte in a text field, which makes the chunk re-parse as a different annotation; everything
/// here is something a caller has to *know*, not something that should fail a conversion whose
/// pixels are fine.
///
/// `#[repr(u8)]` with explicit discriminants, which are permanent and append-only: the value
/// crosses the C ABI as a plain integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum MetadataNotice {
    /// A `cICP` whose matrix coefficients are not 0, left behind. §11.3.2.6 requires 0 for PNG —
    /// "RGB is currently the only supported color model in PNG, and as such Matrix Coefficients
    /// shall be set to 0" — so the source chunk is not conforming and copying it forward would
    /// reproduce the defect in a file this encoder signed off on.
    NonRgbCicp = 0,
    /// The C2PA manifest store (`caBX`), left behind. A store is signed over the exact bytes of
    /// the file it was made for, which is why C2PA 2.4 §A.3.2 marks the chunk unsafe to copy:
    /// carried into a re-encode it is invalid by construction, and a validator reports a
    /// *tampered* file rather than an unsigned one. Re-sign the output and set it with
    /// [`with_c2pa`](PngEncoder::with_c2pa).
    C2paManifestStore = 1,
    /// A text annotation left behind because its keyword holds a character Latin-1 cannot
    /// encode. §11.3.3.1 binds the keyword to Latin-1 in *all three* text chunks, so unlike the
    /// text — which §11.3.3.2 routes to `iTXt` — there is no chunk that could carry it.
    TextKeywordNotLatin1 = 2,
    /// A text annotation left behind because its keyword is empty or longer than the 79 bytes
    /// §11.3.3.1 allows. All three chunks fix that field at 1–79 bytes, so a reader — this
    /// crate's own included — drops the whole chunk rather than reading a longer one.
    TextKeywordLength = 3,
    /// A text annotation **written**, whose keyword leaves the repertoire §11.3.3.1 recommends
    /// ("only code points 0x20-7E and 0xA1-FF are allowed", and expressly "nor is U+00A0
    /// NON-BREAKING SPACE"). The keyword is written exactly as it arrived — this crate reads it
    /// back unchanged — but another reader need not be so forgiving.
    TextKeywordRepertoire = 4,
    /// A text annotation **written**, whose keyword has a leading, trailing or consecutive
    /// space, which §11.3.3.1 says are "not permitted in keywords" so that one keyword cannot be
    /// misread as another. Written as it arrived, for the same reason as
    /// [`TextKeywordRepertoire`](Self::TextKeywordRepertoire).
    TextKeywordSpacing = 5,
    /// A text annotation **written without its `iTXt` language tag**, because the tag was not
    /// the ASCII shape §11.3.3.4 requires ("a well-formed language tag defined by [BCP47]").
    /// Written as UTF-8 into a field a reader takes as Latin-1 the tag would not survive the
    /// trip; an empty tag is §11.3.3.4's own way of saying the language is unspecified.
    ItxtLanguageTag = 6,
    /// An XMP packet left behind because it is not UTF-8. §11.3.3.4 gives the `iTXt` text field
    /// UTF-8 and no alternative, so there is no chunk to frame it in.
    XmpNotUtf8 = 7,
}

impl MetadataNotice {
    /// One line naming the payload and what happened to it, fit to show a user.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::NonRgbCicp => {
                "cICP: its matrix coefficients are not 0, which PNG requires (§11.3.2.6)"
            }
            Self::C2paManifestStore => {
                "C2PA manifest store: signed over the source bytes, so a copy would be invalid \
                 (C2PA 2.4 §A.3.2) — re-sign the output"
            }
            Self::TextKeywordNotLatin1 => {
                "text annotation: its keyword is not Latin-1, which every text chunk requires \
                 (§11.3.3.1)"
            }
            Self::TextKeywordLength => {
                "text annotation: its keyword is not 1 to 79 bytes, the length every text chunk \
                 fixes (§11.3.3.1)"
            }
            Self::TextKeywordRepertoire => {
                "text annotation: written, but its keyword leaves the code points 0x20-0x7E and \
                 0xA1-0xFF §11.3.3.1 recommends — another reader may reject it"
            }
            Self::TextKeywordSpacing => {
                "text annotation: written, but its keyword has a leading, trailing or \
                 consecutive space, which §11.3.3.1 does not permit"
            }
            Self::ItxtLanguageTag => {
                "text annotation: written without its language tag, which was not the BCP 47 \
                 shape §11.3.3.4 requires"
            }
            Self::XmpNotUtf8 => {
                "XMP packet: not UTF-8, and an iTXt text string must be (§11.3.3.4)"
            }
        }
    }

    /// Whether the payload still reached the output.
    ///
    /// `false` means it was left behind entirely; `true` means it was written, with the caveat
    /// [`reason`](Self::reason) gives. A caller showing these to a user needs the difference —
    /// "this did not come along" and "this came along in a form some readers dislike" call for
    /// different action.
    #[must_use]
    pub fn carried(self) -> bool {
        matches!(
            self,
            Self::TextKeywordRepertoire | Self::TextKeywordSpacing | Self::ItxtLanguageTag
        )
    }
}

impl core::fmt::Display for MetadataNotice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.reason())
    }
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
    /// What the last metadata carry could not take *as a whole payload*, in the order it was
    /// found. Reset by each [`Self::with_metadata`] / [`Self::with_metadata_from`] call, so it
    /// describes that call. Per-annotation notices live with their annotation instead, so that a
    /// second carry replaces them exactly as it replaces the annotations themselves.
    carry_notices: Vec<MetadataNotice>,
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
            carry_notices: Vec::new(),
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

    /// Records the standard colour-space rendering intent (sRGB chunk, §11.3.2.5).
    ///
    /// May be combined with [`with_icc_profile`](Self::with_icc_profile). §5.6 Table 5 and
    /// §11.3.2.5 say only that the two "should not" appear together — lowercase, and §15 gives
    /// the BCP 14 keywords force "when, and only when, they appear in all capitals" — while §4.3
    /// Table 1 presupposes the pair and settles it, ranking `iCCP` (priority 2) above `sRGB`
    /// (3). Both are written; a reader honours the profile and treats the intent as the fallback
    /// for readers that cannot apply one.
    #[must_use]
    pub fn with_srgb(mut self, intent: SrgbIntent) -> Self {
        self.ancillary.set_srgb(intent);
        self
    }

    /// Records the video-signal colour space by its ITU-T H.273 code points (cICP chunk,
    /// §11.3.2.6): the colour primaries, the transfer function, and whether the samples use the
    /// full value range.
    ///
    /// There is no matrix-coefficients parameter because §11.3.2.6 fixes it: "RGB is currently
    /// the only supported color model in PNG, and as such Matrix Coefficients shall be set to 0."
    ///
    /// cICP is the **highest-precedence** colour chunk (§4.3 Table 1, priority 1), so a reader
    /// that understands it ignores any `iCCP`, `sRGB`, `gAMA` and `cHRM` in the same file. Those
    /// stay legal alongside it — unlike the `sRGB`/`iCCP` pair — and are worth keeping as a
    /// fallback for readers that do not.
    #[must_use]
    pub fn with_cicp(
        mut self,
        color_primaries: u8,
        transfer_function: u8,
        full_range: bool,
    ) -> Self {
        self.ancillary.cicp = Some((color_primaries, transfer_function, full_range));
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
    ///
    /// Under [`encode_indexed8`](Self::encode_indexed8) the entry holding the grey is kept by the
    /// palette cleaning even when no pixel names it, and the chunk becomes that entry's index.
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
    ///
    /// Under [`encode_indexed8`](Self::encode_indexed8) the entry holding the colour is kept by the
    /// palette cleaning even when no pixel names it, and the chunk becomes that entry's index — so
    /// the opaque entry a transparent twin would otherwise outlive is the one you keep.
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
    /// It keeps naming that entry: cleaning the palette keeps the entry and may renumber it, and
    /// the chunk is renumbered with it, so the background written is the colour you pointed at.
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

    /// Embeds an ICC colour profile (iCCP chunk, §11.3.2.3), zlib-compressed. `profile` is the
    /// raw ICC profile — for example the bytes produced by `gamut-icc`.
    ///
    /// May be combined with [`with_srgb`](Self::with_srgb); see there for why the pair is
    /// written rather than refused, and which chunk a reader honours.
    #[must_use]
    pub fn with_icc_profile(mut self, name: &str, profile: &[u8]) -> Self {
        self.ancillary.iccp = Some((name.to_string(), profile.to_vec()));
        self
    }

    /// Embeds an XMP packet (an iTXt chunk with the standard `XML:com.adobe.xmp` keyword). `xmp` is
    /// the XMP/RDF document — for example the bytes produced by `gamut-xmp`.
    #[must_use]
    pub fn with_xmp(mut self, xmp: &str) -> Self {
        // §11.3.3.1 Table 21: "The use of iTXt, with Compression Flag set to 0, and both Language
        // Tag and Translated Keyword set to the null string, are recommended for XMP compliance."
        // A packet read out of a file that framed it otherwise keeps its framing; this entry
        // point has no framing to keep, so it takes the recommended one.
        self.ancillary.add_xmp(xmp.as_bytes(), "", "", false);
        self
    }

    /// Carries every metadata chunk a [`PngMetadata`] holds into this encoder, so that
    /// re-encoding a file keeps its EXIF, ICC profile, XMP packet, text annotations and colour
    /// chunks instead of dropping them.
    ///
    /// This is the write-side counterpart of [`metadata`](crate::metadata): read a file's
    /// metadata without touching its pixels, then hand it to the encoder that rewrites them.
    /// [`with_metadata_from`](Self::with_metadata_from) is the same thing for a full
    /// [`DecodedPng`].
    ///
    /// Calling it twice with the same metadata is the same as calling it once: a later carry
    /// replaces what an earlier one contributed rather than appending a second copy of every
    /// annotation.
    ///
    /// # What it carries, and what it deliberately does not
    ///
    /// Everything the read side surfaces is set, including a `cICP`, an `sRGB` and an `iCCP`
    /// together — §4.3 Table 1 ranks the colour chunks precisely so a file may carry more than
    /// one, and a reader honours the lowest priority number. That is a claim about *other*
    /// readers: this crate's own reader surfaces all of them and ranks none, because which chunk
    /// to honour depends on whether the reader has a colour-management module, which an encoder
    /// cannot know. Resolving a profile against an intent belongs to `gamut-cmm`. Each text annotation goes back into
    /// the chunk it came out of, compressed if it was compressed
    /// ([`TextChunkKind`](crate::TextChunkKind)); so does the XMP packet, whose own framing —
    /// compression flag, language tag, translated keyword — rides in
    /// [`XmpFraming`](crate::XmpFraming).
    ///
    /// Two payloads cannot be carried at all, and neither is dropped in silence — read them back
    /// with [`metadata_notices`](Self::metadata_notices):
    ///
    /// - a **`cICP` whose matrix coefficients are not 0**, which §11.3.2.6 does not allow in PNG;
    /// - the **C2PA manifest store**, signed over the bytes of the file it was made for.
    ///
    /// A text annotation whose keyword or XMP packet §11.3.3 does not endorse is reported through
    /// the same channel rather than failing the carry: a keyword outside §11.3.3.1's repertoire
    /// or spacing rules is written as it arrived, a keyword no chunk can hold and an XMP packet
    /// that is not UTF-8 are left behind, and
    /// [`MetadataNotice::carried`](MetadataNotice::carried) says which happened. **Only a null**
    /// in a keyword or text string fails the encode with [`Error::InvalidInput`] naming the
    /// annotation — the null is the field separator, so the chunk would be read back as a
    /// *different* annotation, which no notice can undo.
    ///
    /// One further limit is the read side's, not this method's: `pHYs`, `tIME`, `sBIT` and `bKGD`
    /// are not part of [`PngMetadata`], so they cannot be carried here (set them with their own
    /// builder methods).
    #[must_use]
    pub fn with_metadata(self, metadata: &PngMetadata) -> Self {
        self.with_metadata_view(MetadataView {
            exif: metadata.exif.as_deref(),
            icc_profile: metadata.icc_profile.as_ref(),
            xmp: metadata.xmp.as_deref(),
            xmp_framing: metadata.xmp_framing.as_ref(),
            texts: &metadata.texts,
            gamma: metadata.gamma,
            chromaticities: metadata.chromaticities,
            srgb: metadata.srgb,
            cicp: metadata.cicp,
            c2pa: metadata.c2pa.is_some(),
        })
    }

    /// Carries the metadata of a decoded file into this encoder: the [`DecodedPng`] twin of
    /// [`with_metadata`](Self::with_metadata), which documents exactly what is and is not carried.
    ///
    /// Use this when you already decoded the pixels; use `with_metadata` when
    /// [`metadata`](crate::metadata) read the file without them.
    #[must_use]
    pub fn with_metadata_from(self, decoded: &DecodedPng) -> Self {
        self.with_metadata_view(MetadataView {
            exif: decoded.exif.as_deref(),
            icc_profile: decoded.icc_profile.as_ref(),
            xmp: decoded.xmp.as_deref(),
            xmp_framing: decoded.xmp_framing.as_ref(),
            texts: &decoded.texts,
            gamma: decoded.gamma,
            chromaticities: decoded.chromaticities,
            srgb: decoded.srgb,
            cicp: decoded.cicp,
            c2pa: decoded.c2pa.is_some(),
        })
    }

    /// What this encoder could not carry faithfully: whole payloads left behind, then the
    /// per-annotation notices, in the order they were found — empty when everything came along
    /// intact.
    ///
    /// Surface this to whoever asked for the re-encode. Losing metadata without saying so is the
    /// defect the preservation path exists to remove; losing it — or bending it — *with* an
    /// explanation is a choice the spec forces. Use
    /// [`MetadataNotice::carried`](MetadataNotice::carried) to tell the two apart.
    ///
    /// The payload-level notices describe the last [`with_metadata`](Self::with_metadata) /
    /// [`with_metadata_from`](Self::with_metadata_from) call and are reset by each; the
    /// per-annotation notices belong to the annotations still accumulated, so they follow the
    /// same replace-not-append rule a carry gives the text list.
    #[must_use]
    pub fn metadata_notices(&self) -> Vec<MetadataNotice> {
        let mut notices = self.carry_notices.clone();
        notices.extend(self.ancillary.text_notices());
        notices
    }

    /// The one implementation behind [`with_metadata`](Self::with_metadata) and
    /// [`with_metadata_from`](Self::with_metadata_from).
    fn with_metadata_view(mut self, meta: MetadataView<'_>) -> Self {
        self.carry_notices.clear();
        self.ancillary.begin_carry();
        if let Some(exif) = meta.exif {
            self = self.with_exif(exif);
        }
        // Both colour statements are carried. §5.6 Table 5 and §11.3.2.5 only *recommend* against
        // the pair, and §4.3 Table 1 exists to resolve it: `iCCP` outranks `sRGB`, so the profile
        // is what a reader applies and the intent is what a reader without a CMM falls back on.
        // Dropping either would throw away colour information the source carried.
        if let Some(icc) = meta.icc_profile {
            self = self.with_icc_profile(&icc.name, &icc.profile);
        }
        if let Some(intent) = meta.srgb {
            self = self.with_srgb(intent);
        }
        match meta.cicp {
            // §11.3.2.6: "Matrix Coefficients shall be set to 0". A source chunk that says
            // otherwise is not a conforming cICP; carrying it forward would put the same defect
            // in the output.
            Some(cicp) if cicp.matrix_coefficients != 0 => {
                self.carry_notices.push(MetadataNotice::NonRgbCicp);
            }
            Some(cicp) => {
                self = self.with_cicp(
                    cicp.color_primaries,
                    cicp.transfer_function,
                    cicp.full_range,
                );
            }
            None => {}
        }
        if meta.c2pa {
            self.carry_notices.push(MetadataNotice::C2paManifestStore);
        }
        // Set in the stored ×100 000 fixed-point units rather than through `with_gamma` /
        // `with_chromaticities`, whose `f64` arguments would round-trip the value through a
        // division and a `round()`: preservation must be byte-exact.
        if let Some(gamma) = meta.gamma {
            self.ancillary.gamma = Some(gamma);
        }
        if let Some(chrm) = meta.chromaticities {
            self.ancillary.chrm = Some([
                chrm.white.0,
                chrm.white.1,
                chrm.red.0,
                chrm.red.1,
                chrm.green.0,
                chrm.green.1,
                chrm.blue.0,
                chrm.blue.1,
            ]);
        }
        // Handed over as bytes, because that is what the chunk held, and with the framing its
        // chunk gave it — above all §11.3.3.4's compression flag, without which a packet stored
        // as 71 compressed bytes is rewritten as the 4 045 it inflates to. §11.3.3.4 requires
        // UTF-8, so a packet that is not is reported by `metadata_notices` — never a silent drop.
        if let Some(xmp) = meta.xmp {
            let (language, translated, compressed) =
                meta.xmp_framing.map_or(("", "", false), |f| {
                    (
                        f.language.as_deref().unwrap_or_default(),
                        f.translated_keyword.as_deref().unwrap_or_default(),
                        f.compressed,
                    )
                });
            self.ancillary
                .add_xmp(xmp, language, translated, compressed);
        }
        for text in meta.texts {
            let (language, translated) = (
                text.language.as_deref().unwrap_or_default(),
                text.translated_keyword.as_deref().unwrap_or_default(),
            );
            match text.kind {
                TextChunkKind::Text => self.ancillary.add_text_latin1(&text.keyword, &text.text),
                TextChunkKind::CompressedText => {
                    self.ancillary
                        .add_text_compressed(&text.keyword, &text.text);
                }
                TextChunkKind::International => self.ancillary.add_text_international_tagged(
                    &text.keyword,
                    language,
                    translated,
                    &text.text,
                    false,
                ),
                TextChunkKind::CompressedInternational => {
                    self.ancillary.add_text_international_tagged(
                        &text.keyword,
                        language,
                        translated,
                        &text.text,
                        true,
                    );
                }
            }
        }
        self.ancillary.end_carry();
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
    /// To put a finished store into a file that has **already** been encoded, prefer
    /// [`fill_c2pa`](crate::fill_c2pa): it rewrites the reserved chunk in place, where this
    /// setter re-runs the whole encode.
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
    ///    names the chunk's span (for an indexed image, `deconstruct(&png)?.c2pa()` names the same
    ///    span — see [`encode_indexed8`](Self::encode_indexed8));
    /// 2. hash the output with that **whole** span excluded — length, type, payload and CRC
    ///    (§18.5.4) — and have the signer build the store against it;
    /// 3. write the finished store into the span with [`fill_c2pa`](crate::fill_c2pa). Only the
    ///    payload and the chunk CRC change, so every other byte — and every offset — is the one
    ///    the signer hashed.
    ///
    /// Re-encoding with [`with_c2pa`](Self::with_c2pa) and a store of the same length reaches the
    /// same bytes, because the output is byte-reproducible and the store is the last chunk before
    /// `IDAT`; it costs a second full encode and ties the signature to that reproducibility, which
    /// is why the in-place fill is the documented step 3.
    ///
    /// The reservation is `len` bytes exactly — no slack is added — so ask for what the signer
    /// says it needs (`c2pa-rs` reports a `reserve_size`).
    ///
    /// The offsets hold for the file as this encoder wrote it. A PNG editor may lawfully insert
    /// another ancillary chunk after the store (PNG §14.3.2), so reserve, hash and fill without
    /// passing the file through one.
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
    /// [`PngReport::c2pa`](crate::PngReport::c2pa) performs, over the same rule (the first
    /// CRC-valid `caBX` before the first `IDAT`) — so it cannot disagree with what a later
    /// [`deconstruct`](crate::deconstruct) of the same bytes reports. There is deliberately no
    /// indexed twin of this method: [`encode_indexed8`](Self::encode_indexed8) needs a palette and
    /// does not fit this shape, and `deconstruct(&png)?.c2pa()` gives an indexed caller the same
    /// span, which [`fill_c2pa`](crate::fill_c2pa) then fills.
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
    /// `palette` is **cleaned** before it is written, silently and losslessly: an entry nothing in
    /// the file names is dropped, a second entry holding the same RGB *and* alpha as an earlier one
    /// is merged into it, the trailing opaque `tRNS` bytes §11.3.2.1 lets a chunk omit are omitted,
    /// the index bit depth is derived from what survives, and the image's indices are renumbered to
    /// match ([`PngPalette::cleaned`]). The colour every pixel resolves to is unchanged, which is
    /// why this reports nothing: a merged entry did not fail to come along, it arrived under
    /// another index. Surviving entries keep the order you gave them.
    ///
    /// "Nothing in the file names it" includes the `bKGD` background, in **whichever** of its three
    /// forms you set it — [`with_background_index`](Self::with_background_index),
    /// [`with_background_gray`](Self::with_background_gray) or
    /// [`with_background_rgb`](Self::with_background_rgb). Each names an entry of the palette you
    /// supply, so that entry survives even when no pixel names it, and the chunk is written as its
    /// index in the cleaned palette. Your background keeps its colour *and* its alpha: a triple
    /// that appears both opaque and transparent resolves to the opaque entry, and it is that entry
    /// the chunk keeps naming.
    ///
    /// What it costs is bounded by the palette, not by the picture: at most 256 entries, each
    /// compared against the survivors before it. Only the index remap walks the image, one pass
    /// and one byte per pixel, beside the filter candidates the encoder already deflates.
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
        // Every entry the finished file still has to name. A pixel names one; so does the `bKGD`
        // background, in whichever of its three forms it was set — an index into this palette, a
        // grey sample, an RGB triple. Which entry each form names is `ancillary::background_entry`
        // to answer, and it is asked rather than restated here, so no form can be missed.
        let mut used = [false; 256];
        for &index in indices {
            used[usize::from(index)] = true;
        }
        let background = match self.ancillary.bkgd.as_deref() {
            Some(bkgd) => {
                let supplied = palette.plte();
                background_entry(
                    bkgd,
                    WrittenPalette {
                        plte: &supplied,
                        trns: palette.trns(),
                        origin: PaletteOrigin::Caller,
                    },
                )
            }
            None => None,
        };
        if let Some(index) = background {
            used[usize::from(index)] = true;
        }
        let (palette, remap) = palette.cleaned(&used);
        let indices: Vec<u8> = indices.iter().map(|&i| remap[usize::from(i)]).collect();
        // The background still names the colour the caller chose, so the chunk is rewritten as
        // that entry's index in the *cleaned* palette, whichever form it arrived in. Pinning the
        // index here is also what stops a colour-form background from being resolved a second time
        // against the cleaned palette and landing on a different entry — an opaque triple whose
        // transparent twin outlived it resolves to the twin. Only a background whose bytes
        // actually change copies the encoder's chunk state.
        let renumbered;
        let this = match background.map(|index| remap[usize::from(index)]) {
            Some(index) if self.ancillary.bkgd.as_deref() != Some([index].as_slice()) => {
                renumbered = self.clone().with_background_index(index);
                &renumbered
            }
            _ => self,
        };

        let dims = image.dimensions();
        // Use the smallest bit depth that holds every index — a free, lossless space win.
        let depth = reduce::index_bit_depth(palette.len());
        let packed;
        let sample_bytes = if depth < 8 {
            packed =
                pack::pack_scanlines(&indices, dims.width as usize, dims.height as usize, depth);
            packed.as_slice()
        } else {
            indices.as_slice()
        };
        let plte = palette.plte();
        let trns = palette.trns();
        this.write_png(
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
        // Refuse an accumulation the spec says must not be written before emitting a byte, so a
        // caller never receives a half-written buffer for a chunk set it chose (see
        // [`Ancillary::validate`]). Every encode path funnels through here.
        self.ancillary.validate()?;
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
    /// **The total order.** All three candidates here are lossless, so at equal size none is
    /// better by any property the size contract can see; ties resolve toward the earlier of
    /// `chunked ≻ chunk-free ≻ native` purely so that the output is a function of the input. See
    /// [the module's tie-break rule](self#how-a-tie-is-broken), and [`prefers_chunk_free`] /
    /// [`prefers_native`], where each step is stated on its own.
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
/// **A tie keeps the plain encoding.** This is the first half of
/// [the module's tie-break rule](self#how-a-tie-is-broken): the two candidates are *not*
/// information-equivalent, and the one that discards less wins. Every other reduction in this
/// crate is byte-exact; [`with_transparent_cleanup`](PngEncoder::with_transparent_cleanup) is the
/// one knob that alters stored samples, and it is opt-in *for a size win*. Where there is no size
/// win there is nothing to trade the exactness for. Split out for the
/// same reason as [`prefers_native`]: engineering two encodings of the same image to land on
/// exactly equal lengths is not something a fixture can do reliably, so the tie is only assertable
/// here.
fn prefers_plain(plain_len: usize, cleaned_len: usize) -> bool {
    plain_len <= cleaned_len
}

/// Whether the chunk-free reduction beats the chunk-carrying one, the first step of
/// [`PngEncoder::write_reduced_or_native`]'s three-way race.
///
/// **A tie keeps the chunk-carrying encoding.** Both candidates are lossless and encode the same
/// image, so at equal size neither is better; the fixed order is what makes the choice
/// deterministic — see [the module's tie-break rule](self#how-a-tie-is-broken). Split out for the
/// same reason as [`prefers_native`].
fn prefers_chunk_free(chunk_free_len: usize, chunked_len: usize) -> bool {
    chunk_free_len < chunked_len
}

/// Whether the unreduced encoding beats the winning reduction, for
/// [`PngEncoder::write_reduced_or_native`].
///
/// **A tie keeps the reduction** — and where the palette won the first step, a tie here keeps the
/// palette. Both candidates are lossless, so the fixed order is what makes the choice
/// deterministic rather than a property of the winner; see
/// [the module's tie-break rule](self#how-a-tie-is-broken). Split out because engineering two
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

    /// The written index depth is derived from the palette *after* cleaning, so an oversized
    /// caller palette does not pin the file to 8-bit indices.
    ///
    /// This is where the saving actually is. Dropping 253 unnamed entries is 759 `PLTE` bytes;
    /// dropping the depth they forced is three quarters of every pixel. The one reason it fails is
    /// that `encode_indexed8` measured the caller's entry count instead of the cleaned one.
    ///
    /// Which count maps to which depth is [`reduce::index_bit_depth`]'s own fact, pinned by
    /// `tests/oracle.rs::indexed_uses_minimal_bit_depth` against libpng; this asserts only that
    /// the cleaned count is what reaches it. Every case is a depth below 8, so a caller palette
    /// that stayed uncleaned could not produce it.
    #[test]
    fn the_index_depth_follows_the_cleaned_entry_count() {
        for distinct in [1usize, 2, 3, 4, 5, 16] {
            // 256 entries, of which the image names the first `distinct`.
            let entries: Vec<[u8; 3]> = (0..256u32).map(|i| [i as u8, 0, 0]).collect();
            let palette = PngPalette::new(&entries).unwrap();
            let indices: Vec<u8> = (0..64usize).map(|i| (i % distinct) as u8).collect();
            let img = ImageRef::<Indexed8>::new(&indices, Dimensions::new(64, 1).unwrap()).unwrap();
            let mut png = Vec::new();
            PngEncoder::new()
                .encode_indexed8(img, &palette, &mut png)
                .unwrap();

            let expected = reduce::index_bit_depth(distinct);
            assert!(expected < 8, "{distinct}: the fixture must be able to tell");
            assert_eq!(png[24], expected, "{distinct} entries named");
            assert_eq!(
                find_chunk(&png, b"PLTE").map(|plte| plte.len()),
                Some(distinct * 3),
                "{distinct} entries named"
            );
        }
    }

    /// A caller palette padded with redundancy produces the *same file*, byte for byte, as the
    /// tight palette holding the same colours.
    ///
    /// The whole feature in one equality, and it fails for one reason: what was written was not
    /// the tight palette. It is stronger than counting `PLTE` bytes because it also fixes the
    /// order the survivors are written in and the length of the `tRNS` chunk beside them — a clean
    /// that dropped and merged correctly but reordered, or left a trailing opaque alpha, produces
    /// a different file and is caught here.
    ///
    /// The sizes are the ones `STATUS.md` publishes: 1 194 bytes before this clean existed, 162
    /// after, against 164/162 for the tight palette.
    #[test]
    fn a_redundant_palette_costs_what_the_tight_one_costs() {
        let (w, h) = (64u32, 64u32);
        let colours: [[u8; 3]; 4] = [[240, 90, 40], [30, 30, 60], [255, 255, 255], [10, 200, 120]];
        let alphas: [u8; 4] = [255, 0, 255, 255];
        // 256 entries holding those four colours, 64 times over.
        let padded: Vec<[u8; 3]> = (0..256).map(|i| colours[i % 4]).collect();
        let padded_alpha: Vec<u8> = (0..256).map(|i| alphas[i % 4]).collect();
        let padded = PngPalette::with_transparency(&padded, &padded_alpha).unwrap();
        let tight = PngPalette::with_transparency(&colours, &alphas).unwrap();

        let indices: Vec<u8> = (0..(w * h) as usize)
            .map(|i| (((i as u32 % w) / 7 + (i as u32 / w) / 5) % 4) as u8)
            .collect();
        let encode = |palette: &PngPalette| {
            let img = ImageRef::<Indexed8>::new(&indices, Dimensions::new(w, h).unwrap()).unwrap();
            let mut png = Vec::new();
            PngEncoder::new()
                .encode_indexed8(img, palette, &mut png)
                .unwrap();
            png
        };

        assert_eq!(encode(&padded), encode(&tight));
        assert_eq!(encode(&tight).len(), 162, "the size STATUS.md publishes");
    }

    /// A `bKGD` palette index still names the colour the caller chose after cleaning renumbers the
    /// palette — and the entry it names survives even when no pixel names it.
    ///
    /// Both halves are the same claim, and it fails for one reason: the background stopped meaning
    /// what the caller said. The chunk is kept verbatim on this path
    /// (`ancillary::bkgd_for`, [`PaletteOrigin::Caller`]), so an index left pointing into the
    /// caller's numbering would silently repaint the background — or, if its entry were dropped,
    /// name a colour the file no longer holds.
    #[test]
    fn a_background_index_names_a_surviving_entry_after_cleaning() {
        let palette =
            PngPalette::new(&[[0, 0, 0], [1, 1, 1], [2, 2, 2], [3, 3, 3], [4, 4, 4]]).unwrap();
        // Only entry 1 is painted; entry 3 is named by the background alone.
        let indices = vec![1u8; 8];
        let img = ImageRef::<Indexed8>::new(&indices, Dimensions::new(8, 1).unwrap()).unwrap();
        let mut png = Vec::new();
        PngEncoder::new()
            .with_background_index(3)
            .encode_indexed8(img, &palette, &mut png)
            .unwrap();

        assert_eq!(
            find_chunk(&png, b"PLTE"),
            Some(vec![1, 1, 1, 3, 3, 3]),
            "the background's entry is kept, in the caller's order"
        );
        assert_eq!(
            find_chunk(&png, b"bKGD"),
            Some(vec![1]),
            "and the index moved with it"
        );
    }

    /// A background set as a *colour* names, after cleaning, an entry holding that colour with
    /// that alpha.
    ///
    /// `bKGD` names a palette entry in three forms (§11.3.5.1) -- an index, a grey sample, an RGB
    /// triple -- and `ancillary::background_entry` resolves all three against the caller's palette.
    /// Marking only the index form's entry as used left the other two naming an entry cleaning was
    /// free to drop or to renumber under them, which is the repainting the index form's mark
    /// exists to prevent, one field over. It fails for one reason: the background stopped naming
    /// the colour the caller set.
    ///
    /// Both rows are that one defect; they differ only in how it shows. In the first the entry is
    /// named by nothing else, so it was dropped and the chunk vanished with it. In the second an
    /// opaque entry and a transparent twin hold the same triple: the resolver prefers the opaque
    /// one (`WrittenPalette::index_of`), no pixel names it, and dropping it left the chunk written
    /// and
    /// pointing at the transparent twin -- an opaque background silently turned see-through, with
    /// nothing missing from the file to show for it.
    ///
    /// The colour is read back the way §11.3.5.1 says a reader reads it: the index into `PLTE`,
    /// and the same index into `tRNS` (opaque past its end, §11.3.2.1).
    #[test]
    fn a_colour_background_names_an_entry_holding_its_colour() {
        struct Case {
            /// The palette the caller supplies, and its tRNS bytes.
            entries: &'static [[u8; 3]],
            alphas: &'static [u8],
            /// The indices the image paints — never the background's entry.
            painted: &'static [u8],
            /// The background colour, and the alpha the entry it names must still have.
            background: [u8; 3],
            alpha: u8,
        }
        let cases = [
            Case {
                entries: &[[10, 20, 30], [200, 100, 50]],
                alphas: &[],
                painted: &[0, 0, 0, 0],
                background: [200, 100, 50],
                alpha: 255,
            },
            Case {
                entries: &[[7, 7, 7], [7, 7, 7], [1, 2, 3]],
                alphas: &[255, 0],
                painted: &[1, 2, 1, 2],
                background: [7, 7, 7],
                alpha: 255,
            },
        ];
        for case in cases {
            let Case {
                entries,
                alphas,
                painted,
                background: [r, g, b],
                alpha,
            } = case;
            let palette = PngPalette::with_transparency(entries, alphas).unwrap();
            let dims = Dimensions::new(painted.len() as u32, 1).unwrap();
            let img = ImageRef::<Indexed8>::new(painted, dims).unwrap();
            let mut png = Vec::new();
            PngEncoder::new()
                .with_background_rgb(r.into(), g.into(), b.into())
                .encode_indexed8(img, &palette, &mut png)
                .unwrap();

            let bkgd = find_chunk(&png, b"bKGD")
                .unwrap_or_else(|| panic!("{r},{g},{b}: the background's entry was dropped"));
            let index = usize::from(bkgd[0]);
            let plte = find_chunk(&png, b"PLTE").expect("an indexed file has a palette");
            assert_eq!(&plte[index * 3..index * 3 + 3], &[r, g, b], "{r},{g},{b}");
            let written = find_chunk(&png, b"tRNS")
                .and_then(|trns| trns.get(index).copied())
                .unwrap_or(255);
            assert_eq!(written, alpha, "{r},{g},{b}: the background's alpha");
        }
    }

    /// A `bKGD` index past the end of the caller's palette is still omitted, not renumbered into
    /// range.
    ///
    /// Cleaning maps an index it never marked to 0, which is a *valid* entry, so an out-of-range
    /// index that reached the renumbering would come back in range and be written — turning a
    /// chunk `ancillary::bkgd_for` deliberately drops into a background the caller never asked
    /// for.
    ///
    /// The first case is `palette.len()` itself, the smallest index that is out of range: the
    /// range test is `<`, and the off-by-one that makes it `<=` is invisible to any index further
    /// out.
    #[test]
    fn a_background_index_past_the_palette_stays_omitted() {
        let palette = PngPalette::new(&[[9, 8, 7], [6, 5, 4], [3, 2, 1]]).unwrap();
        for index in [3u8, 4, 200, 255] {
            let indices = vec![0u8, 1, 2, 1];
            let img = ImageRef::<Indexed8>::new(&indices, Dimensions::new(4, 1).unwrap()).unwrap();
            let mut png = Vec::new();
            PngEncoder::new()
                .with_background_index(index)
                .encode_indexed8(img, &palette, &mut png)
                .unwrap();

            assert_eq!(find_chunk(&png, b"bKGD"), None, "background index {index}");
        }
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
