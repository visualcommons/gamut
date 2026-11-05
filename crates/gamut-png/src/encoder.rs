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
    Indexed8, Palette, Pixel, Result, Rgb8, Rgb16, Rgba8, Rgba16,
};
use gamut_deflate::{DeflateEncoder, Level};

use crate::ancillary::{
    Ancillary, C2paStore, PaletteOrigin, SrgbIntent, WrittenHeader, WrittenPalette,
    background_entry,
};
use crate::backend::{Deflaters, IdatInfo, ZlibDeflater, run_deflaters};
use crate::carry::MetadataNotice;
use crate::chunk::{self, C2paSpan, SIGNATURE};
use crate::color::ColorType;
#[cfg(doc)]
use crate::decoded::PngMetadata;
use crate::decoded::{Background, Chromaticities, Cicp, ModificationTime, PhysicalDimensions};
use crate::effort::{Effort, EffortKnobs};
use crate::filter::{self, FilterStrategy, FilterType};
use crate::palette::{PaletteOrder, Plte};
use crate::reduce::{self, Family, Reduced, Reductions};
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
///
/// [`FilterStrategy::MinTrialDeflate`] is last, so it changes a file only where it is strictly
/// smaller than all seven before it (`experiments/480/`).
///
/// `pub` only so `stages` can re-export it to `benches/encode.rs`, whose serial arm must race
/// exactly this list (issue #940); the module itself stays private.
pub const BRUTE_FORCE_STRATEGIES: [FilterStrategy; 8] = [
    FilterStrategy::None,
    FilterStrategy::Fixed(FilterType::Sub),
    FilterStrategy::Fixed(FilterType::Up),
    FilterStrategy::Fixed(FilterType::Average),
    FilterStrategy::Fixed(FilterType::Paeth),
    FilterStrategy::MinSumAbs,
    FilterStrategy::MinBigrams,
    FilterStrategy::MinTrialDeflate,
];

/// Runs `trial` once per [`BRUTE_FORCE_STRATEGIES`] candidate, concurrently, and returns the
/// results **in candidate order** (issue #624).
///
/// The eight trials share nothing but borrowed input, so each of the last seven runs on a scoped
/// thread while the calling thread takes the first. Results are joined in the order the
/// candidates are listed, never in completion order, so the caller's `min_by_key` sees exactly
/// the sequence a serial loop would produce and a tie still resolves to the earlier strategy:
/// output stays byte-reproducible however the threads are scheduled. Every trial is joined before
/// any result is examined (issue #941). Where an error is returned, it is the first one in
/// candidate order — the same error the serial loop stopped at — though the later trials have run
/// by then rather than being skipped.
///
/// A thread the platform cannot create is not an error: that candidate runs on the calling
/// thread instead, at its turn in candidate order, even after an earlier candidate has failed.
/// This is also how `wasm32` runs serially without a `cfg` — its `std` reports thread creation as
/// unsupported, so every trial falls back in order.
///
/// A panic in a trial is re-raised on the caller with its own payload, and where several trials
/// panic it is the first in candidate order. A panic outranks an error from an earlier candidate:
/// the serial loop would have stopped at that error and never run the panicking trial, but here
/// the trial has already run, and resuming its payload is what keeps it from being replaced by
/// `std::thread::scope`'s generic "a scoped thread panicked".
///
/// **The speed is paid for in memory.** Run one after another, the trials held one candidate's
/// filtered image and optimal-parse buffers at a time; run together, all of them are alive at
/// once, and nothing here caps how many. Each trial's filtered copy is the whole image, so peak
/// memory grows with image size times the number of concurrent candidates; the optimal-parse span
/// bounds only the parse buffers. Measured on a square RGB photograph at [`Level::Best`] with the
/// [`Effort::Smallest`] span, a `BruteForce` encode's peak resident set with all eight candidates
/// was **2.7x to 5.0x** what the serial loop's was — typical rather than a bound, since the
/// concurrent peak depends on scheduling; `STATUS.md` carries the table. That is the trade the
/// maintainer's decision on #624 accepted; a caller encoding near its memory limit should budget
/// for it.
///
/// No dependency is added for this (a maintainer decision on #624): `std::thread::scope` borrows
/// the image without copying it, and one encode's eight trials are too few and too coarse for a
/// work-stealing pool to schedule any better.
fn run_trials(trial: &(impl Fn(FilterStrategy) -> Result<Vec<u8>> + Sync)) -> Result<Vec<Vec<u8>>> {
    let [first, rest @ ..] = BRUTE_FORCE_STRATEGIES;
    std::thread::scope(|scope| {
        let spawned = rest.map(|strategy| {
            let handle = std::thread::Builder::new().spawn_scoped(scope, move || trial(strategy));
            (strategy, handle)
        });
        let first = trial(first);
        // `[_; N]::map` is eager, so every handle is joined here, before the `Result`s are
        // reduced: a lazy chain into `collect` would stop at the first `Err` and leave a later
        // panicked thread unjoined for the scope to report without its payload (issue #941).
        let joined = spawned.map(|(strategy, handle)| match handle {
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            Err(_) => trial(strategy),
        });
        core::iter::once(first).chain(joined).collect()
    })
}

/// What [`PngEncoder::encode_with_report`] found in the bytes it wrote.
///
/// Non-exhaustive: a later revision may report a further region without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PngEncodeReport {
    /// The number of bytes appended to the output — the whole PNG.
    pub len: usize,
    /// Where the C2PA manifest-store chunk landed, as offsets from the first byte of this PNG
    /// (not of the output buffer it was appended to) — the whole `caBX` span a `c2pa.hash.data`
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
    deflate_passes: u8,
    optimal_parse_limit: usize,
    filter: FilterStrategy,
    /// The accumulated ancillary chunks. Crate-visible for the carry (`carry.rs`), which opens
    /// and closes its carry bracket on it.
    pub(crate) ancillary: Ancillary,
    auto_reduce: bool,
    clean_transparent: bool,
    palette_order: PaletteOrder,
    backends: Deflaters,
    /// What the last metadata carry could not take *as a whole payload*, in the order it was
    /// found. Reset by each [`Self::with_png_metadata`] call, so it
    /// describes that call. Per-annotation notices live with their annotation instead, so that a
    /// second carry replaces them exactly as it replaces the annotations themselves.
    pub(crate) carry_notices: Vec<MetadataNotice>,
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
            deflate_passes: DeflateEncoder::DEFAULT_DEFLATE_PASSES,
            optimal_parse_limit: DeflateEncoder::DEFAULT_OPTIMAL_PARSE_LIMIT,
            filter: FilterStrategy::MinSumAbs,
            ancillary: Ancillary::default(),
            auto_reduce: false,
            clean_transparent: false,
            palette_order: PaletteOrder::AsSupplied,
            backends: Deflaters::new(),
            carry_notices: Vec::new(),
        }
    }

    /// Appends a pluggable [`ZlibDeflater`] backend for the IDAT zlib stream (issue #278).
    ///
    /// Backends are tried in **push order**; the built-in [`gamut_deflate`] encoder is the implicit
    /// tail, so pushing nothing keeps today's behaviour byte for byte. A backend that returns
    /// `false` from [`ZlibDeflater::supports`] is skipped — the only fall-through; one that accepts
    /// and then fails, with any error, propagates it rather than falling back.
    ///
    /// # Interaction with [`with_compression`](Self::with_compression)
    ///
    /// A pushed deflater **bypasses the configured [`Level`]** (and the
    /// [`with_deflate_passes`](Self::with_deflate_passes) budget) for every stream it accepts: `Level` is a
    /// `gamut-deflate` concept that a foreign backend knows nothing about, and the seam datum is
    /// only the byte stream. The `Level` still governs the built-in tail, i.e. any stream every
    /// pushed backend declines.
    ///
    /// # Cloning shares backends
    ///
    /// [`PngEncoder`] is [`Clone`], and cloning copies the registry's list of *handles*: the clone
    /// and the original drive the **same** backend instances (they are held behind
    /// `Arc<Mutex<…>>`, since encoding takes `&self`), while a backend pushed to one afterwards is
    /// not seen by the other. Push a fresh backend instance if you need independent state.
    ///
    /// # Concurrent calls under [`FilterStrategy::BruteForce`]
    ///
    /// The brute-force search runs its eight candidate streams on scoped threads (issue #624),
    /// so a backend is offered them from several threads, one at a time through its mutex, in no
    /// fixed order *across* candidates. Push order still decides who sees each stream first. If
    /// one candidate fails, the encode returns the first failure in candidate order, but the other
    /// candidates have already been offered to the backend by then.
    ///
    /// Takes `&mut self` and returns `&mut Self` — unlike the `with_*` builder setters — because a
    /// registry accumulates rather than replaces.
    pub fn push_backend(&mut self, backend: impl ZlibDeflater + Send + 'static) -> &mut Self {
        self.backends
            .push(std::sync::Arc::new(std::sync::Mutex::new(backend)));
        self
    }

    /// Sets all five size/time knobs at once from a [`Effort`] rung — the composed effort dial
    /// (issue #484).
    ///
    /// A plain setter, not a mode: it assigns the same fields the individual `with_*` knobs do, so
    /// a later call overrides the rung and an earlier one is overridden by it. `with_effort` last
    /// is "this rung"; a knob after it is "this rung, except". It leaves everything the rung does
    /// not name — ancillary chunks, metadata, pushed backends and
    /// [`with_transparent_cleanup`](Self::with_transparent_cleanup) — exactly as it found them.
    ///
    /// ```
    /// use gamut_png::{PngEncoder, Effort};
    ///
    /// // The smallest file this crate can produce, with the one lossy knob added on top.
    /// let encoder = PngEncoder::new()
    ///     .with_effort(Effort::Smallest)
    ///     .with_transparent_cleanup(true);
    /// ```
    #[must_use]
    pub fn with_effort(mut self, effort: Effort) -> Self {
        let EffortKnobs {
            level,
            deflate_passes,
            filter,
            optimal_parse_limit,
            auto_reduce,
        } = effort.knobs();
        self.level = level;
        self.deflate_passes = deflate_passes;
        self.filter = filter;
        self.optimal_parse_limit = optimal_parse_limit;
        self.auto_reduce = auto_reduce;
        self
    }

    /// Sets the DEFLATE compression [`Level`] used for the image data. [`Level::Best`] is the
    /// space-efficient (slow) setting.
    #[must_use]
    pub fn with_compression(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// Sets the [`Level::Best`] refinement budget: the maximum number of optimal-parse refinement
    /// passes (see [`DeflateEncoder::with_deflate_passes`]) applied to every zlib stream this
    /// encoder emits — the IDAT image data and compressed ancillary payloads (`iCCP`, `zTXt`).
    ///
    /// Defaults to [`DeflateEncoder::DEFAULT_DEFLATE_PASSES`]; `0` keeps the lazy seed parse only,
    /// and `zopfli`'s default budget is 15. Ignored at every other [`Level`] and by any pushed
    /// [`ZlibDeflater`] backend that accepts a stream.
    #[must_use]
    pub fn with_deflate_passes(mut self, passes: u8) -> Self {
        self.deflate_passes = passes;
        self
    }

    /// Sets the [`Level::Best`] optimal-parse limit: the largest span the shortest-path parse
    /// handles in one piece (see [`DeflateEncoder::with_optimal_parse_limit`]), defaulting to
    /// [`DeflateEncoder::DEFAULT_OPTIMAL_PARSE_LIMIT`] (1 MiB).
    ///
    /// A filtered PNG scanline stream longer than the limit is parsed as consecutive spans of this
    /// size, so raising it lets one cost model span more of the image. A limit below the 32 KiB
    /// LZ77 window is raised to it. Ignored at every other [`Level`], and by any pushed
    /// [`ZlibDeflater`] backend that accepts a stream.
    ///
    /// # What raising it costs: memory, linearly, with no bound of its own
    ///
    /// The shortest-path parse allocates three span-length vectors per refinement pass — the cost
    /// row, the chosen length and the chosen distance — which is 12 bytes for every byte of span,
    /// plus the span's match cache, capped at 10 bytes per byte of span (#959): **up to 22 bytes
    /// for every byte of span**. The limit is therefore the only thing bounding this encoder's
    /// working set, and a caller who removes it makes one encode's peak memory grow without bound
    /// in the image. Measured peak resident set for a single encode of a square RGB photograph at
    /// [`Level::Best`], default span against no bound: **32.3 → 73.0 MiB** at 1024x1024,
    /// **56.8 → 284.4 MiB** at 2048x2048, **178.6 → 1130.8 MiB** at 4096x4096. Time is not what
    /// scales here — the same three pairs measured, before #959's cache, 8.41 → 8.47 s,
    /// 34.65 → 33.95 s and 133.9 → 132.7 s, because the parse's work is linear in the input
    /// whatever the span. (A
    /// wider span can still take more refinement passes to converge on some material, so time is
    /// data-dependent rather than flat.)
    ///
    /// # What raising it buys: bytes, sometimes, depending on the picture
    ///
    /// A wider span is a different cost model, not a better one, and the sign is a property of the
    /// data. Measured at 1024x1024 with no bound against the default: **−0.75%** on a greyscale
    /// ramp, **−0.18%** on a gradient, **−0.12%** on a sprite sheet, **−0.08%** on a palette
    /// image, nothing at all on noise or on a picture already inside one span — and **+0.10% on a
    /// photograph**. Raise it if your material
    /// is homogeneous and you have measured that it helps; the crate's own top rung takes a
    /// finite, bounded 8 MiB for exactly this reason.
    ///
    /// Unlike [`with_deflate_passes`](Self::with_deflate_passes), which governs every zlib stream this encoder
    /// emits, this limit governs the **`IDAT` image data only**. It is the one stream whose length
    /// grows with the image, so it is the one a caller can need to re-span; a compressed ancillary
    /// payload (`iCCP`, `zTXt`) is whatever the caller handed over and keeps the default.
    #[must_use]
    pub fn with_optimal_parse_limit(mut self, limit: usize) -> Self {
        self.optimal_parse_limit = limit;
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
    ///
    /// With an ICC profile set ([`with_icc_profile`](Self::with_icc_profile)), the reduction stays
    /// in the colour family the profile's header names, as §11.3.2.3 requires ("an RGB color
    /// space for color images (color types 2, 3, and 6), or a greyscale color space for greyscale
    /// images (color types 0 and 4)"): grey content under an RGB profile is not reduced to
    /// greyscale, and grey content under a greyscale profile is not written as a palette. A
    /// reduction never leaves the family; the layout you hand over is still raced against the
    /// reductions like any other candidate, so pixels whose own layout contradicts the profile are
    /// written as they are unless an in-family reduction is no larger — auto-reduce never grows a
    /// file, and the pairing you made is yours, as with auto-reduce off.
    #[must_use]
    pub fn with_auto_reduce(mut self, enabled: bool) -> Self {
        self.auto_reduce = enabled;
        self
    }

    /// Sets the order [`encode_indexed8`](Self::encode_indexed8) writes a caller-supplied
    /// palette in: [`PaletteOrder::AsSupplied`] (the default) or [`PaletteOrder::ModifiedZeng`].
    ///
    /// Lossless in what a decoder renders, and applied after the palette is cleaned: the entries
    /// that survive are renumbered, and every pixel index and the `bKGD` index are rewritten to
    /// follow them. The palette's length, and so the index bit depth, does not change.
    ///
    /// Not part of any [`Effort`] rung, and not applied to the palette
    /// [`with_auto_reduce`](Self::with_auto_reduce) derives, which has its own fixed order. A rung
    /// is a dial for effort, and this changes which number names which colour in a palette you
    /// chose — see [`PaletteOrder`] for when that matters.
    #[must_use]
    pub fn with_palette_order(mut self, order: PaletteOrder) -> Self {
        self.palette_order = order;
        self
    }

    /// Records an image gamma (gAMA chunk, §11.3.2.2) in the unit the chunk stores: the encoding
    /// gamma × 100 000, so `45_455` for `1 / 2.2` — the value [`PngMetadata::gamma`] reads back,
    /// written byte for byte.
    #[must_use]
    pub fn with_gamma(mut self, gamma: u32) -> Self {
        self.ancillary.gamma = Some(gamma);
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
    /// full value range — the value [`PngMetadata::cicp`] reads back.
    ///
    /// §11.3.2.6 fixes the matrix coefficients: "RGB is currently the only supported color model
    /// in PNG, and as such Matrix Coefficients shall be set to 0." [`Cicp::new`] builds code
    /// points with that 0. Code points with any other matrix — as a non-conforming file may carry
    /// them — are **not written**: the chunk is omitted and [`MetadataNotice::NonRgbCicp`] is
    /// reported through [`metadata_notices`](Self::metadata_notices). Calling this again replaces
    /// both the code points and their notice.
    ///
    /// cICP is the **highest-precedence** colour chunk (§4.3 Table 1, priority 1), so a reader
    /// that understands it ignores any `iCCP`, `sRGB`, `gAMA` and `cHRM` in the same file. Those
    /// are worth keeping alongside it as a fallback for readers that do not.
    ///
    /// [`Cicp::new`]: crate::Cicp::new
    #[must_use]
    pub fn with_cicp(mut self, cicp: Cicp) -> Self {
        self.ancillary.set_cicp(cicp);
        self
    }

    /// Records the white point and RGB primary chromaticities (cHRM chunk, §11.3.2.1), each
    /// coordinate × 100 000 as the chunk stores it — the value [`PngMetadata::chromaticities`]
    /// reads back, written byte for byte. Build one with [`Chromaticities::new`].
    ///
    /// [`Chromaticities::new`]: crate::Chromaticities::new
    #[must_use]
    pub fn with_chromaticities(mut self, chromaticities: Chromaticities) -> Self {
        let Chromaticities {
            white,
            red,
            green,
            blue,
        } = chromaticities;
        self.ancillary.chrm = Some([
            white.0, white.1, red.0, red.1, green.0, green.1, blue.0, blue.1,
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

    /// Records a default background colour (bKGD chunk, §11.3.4.1) in the form its
    /// [`Background`] variant names — the value [`PngMetadata::background`] reads back. Samples
    /// are at the input's bit depth.
    ///
    /// A [`Background::Gray`] or [`Background::Rgb`] colour is emitted for the colour type
    /// actually **written**, which under [`with_auto_reduce`](Self::with_auto_reduce) may differ
    /// from the input's: converted where that is lossless (grey to an RGB triple, RGB to one grey
    /// sample where the channels agree, either to the palette entry holding the colour — an opaque
    /// one where a transparent twin exists), rescaled exactly as the pixels were where auto-reduce
    /// lowers the depth (16→8, sub-byte grey), and **omitted, without error,** where the written
    /// colour type or depth cannot carry it. See `STATUS.md`, "Chunks that follow the race".
    ///
    /// A [`Background::Index`] names an entry of the palette **you** supply to
    /// [`encode_indexed8`](Self::encode_indexed8), and is emitted only there (and only in range).
    /// Under [`with_auto_reduce`](Self::with_auto_reduce) the palette, if one is written, is the
    /// encoder's own, in an order the index never referred to, so the chunk is **omitted, without
    /// error** — set the background as a colour to have it resolved against whatever is written.
    ///
    /// Under [`encode_indexed8`](Self::encode_indexed8), whichever form it takes, the entry the
    /// background names is kept by the palette cleaning even when no pixel names it, and the chunk
    /// becomes that entry's index in the cleaned palette — so the opaque entry a transparent twin
    /// would otherwise outlive is the one you keep.
    #[must_use]
    pub fn with_background(mut self, background: Background) -> Self {
        self.ancillary.set_background(background);
        self
    }

    /// Records the intended physical pixel dimensions (pHYs chunk, §11.3.4.3) — the value
    /// [`PngMetadata::physical_dimensions`] reads back.
    #[must_use]
    pub fn with_physical_dimensions(mut self, dimensions: PhysicalDimensions) -> Self {
        self.ancillary.set_physical(dimensions);
        self
    }

    /// Records the last-modification time (tIME chunk), in UTC — the value
    /// [`PngMetadata::modification_time`] reads back.
    ///
    /// Each field must lie within the range §11.3.5.1's Table 26 gives it — month 1–12, day 1–31,
    /// hour 0–23, minute 0–59, second 0–60 (a leap second); the year is any value. A time outside
    /// those ranges is one this crate's reader skips as malformed, so it is **not written**: the
    /// chunk is omitted and [`MetadataNotice::TimeOutOfRange`] is reported through
    /// [`metadata_notices`](Self::metadata_notices). The day is not checked against the month.
    /// Calling this again replaces both the time and its notice.
    #[must_use]
    pub fn with_time(mut self, time: ModificationTime) -> Self {
        self.ancillary.set_time(time);
        self
    }

    /// Adds an uncompressed Latin-1 text annotation (tEXt chunk).
    ///
    /// Almost nothing here fails: what §11.3.3 does not endorse — a keyword outside §11.3.3.1's
    /// repertoire, length or spacing, a text string holding a null — is reported through
    /// [`metadata_notices`](Self::metadata_notices), which also says whether the annotation was
    /// written. The one exception is a null in the *keyword*, which fails the encode, because a
    /// keyword ends at its first null and the chunk would re-parse as a different annotation.
    #[must_use]
    pub fn with_text(mut self, keyword: &str, text: &str) -> Self {
        self.ancillary.add_text_latin1(keyword, text);
        self
    }

    /// Adds a zlib-compressed Latin-1 text annotation (zTXt chunk).
    ///
    /// §11.3.3's rules reach this annotation exactly as they reach
    /// [`with_text`](Self::with_text): everything but a null in the keyword is reported through
    /// [`metadata_notices`](Self::metadata_notices) rather than failing the encode.
    #[must_use]
    pub fn with_compressed_text(mut self, keyword: &str, text: &str) -> Self {
        self.ancillary.add_text_compressed(keyword, text);
        self
    }

    /// Adds an uncompressed UTF-8 text annotation (iTXt chunk).
    ///
    /// §11.3.3's rules reach this annotation exactly as they reach
    /// [`with_text`](Self::with_text): everything but a null in the keyword is reported through
    /// [`metadata_notices`](Self::metadata_notices) rather than failing the encode.
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
    ///
    /// §11.3.2.3 requires an RGB profile for colour types 2, 3 and 6 and a greyscale one for 0
    /// and 4. Under [`with_auto_reduce`](Self::with_auto_reduce) the reduction keeps to the
    /// family the profile's header names; otherwise the colour type is the pixels' own, and
    /// matching the profile to them is the caller's.
    ///
    /// The profile is written under the name
    /// [`with_icc_profile_name`](Self::with_icc_profile_name) sets, or `"ICC profile"` when none
    /// is set. Setting a profile again replaces it.
    #[must_use]
    pub fn with_icc_profile(mut self, profile: &[u8]) -> Self {
        self.ancillary.set_icc_profile(profile);
        self
    }

    /// Sets the name the [`with_icc_profile`](Self::with_icc_profile) profile is written under
    /// (iCCP chunk, §11.3.2.3) — the [`IccProfile::name`](crate::IccProfile::name) a reader gets
    /// back. Defaults to `"ICC profile"`; on its own, with no profile set, it writes nothing.
    ///
    /// `name` is held to §11.3.2.3's rule, which is the text keyword's: 1–79 printable Latin-1
    /// characters, no leading, trailing or consecutive space. A name that cannot be stored so
    /// this crate's reader accepts it — outside Latin-1, empty or over 79 bytes, or holding a null
    /// — drops the profile (it does not constrain auto-reduce either); one that only breaks the
    /// repertoire or spacing rule is written verbatim. Either way
    /// [`metadata_notices`](Self::metadata_notices) names it, and the encode proceeds. Setting a
    /// name again replaces both the name and its notice.
    #[must_use]
    pub fn with_icc_profile_name(mut self, name: &str) -> Self {
        self.ancillary.set_icc_profile_name(name);
        self
    }

    /// Embeds an XMP packet (an iTXt chunk with the standard `XML:com.adobe.xmp` keyword).
    /// `packet` is the XMP/RDF document — for example the bytes produced by `gamut-xmp`.
    ///
    /// A PNG carries one packet, so this replaces any packet already set — by an earlier call or
    /// carried in by [`with_png_metadata`](Self::with_png_metadata) — rather than adding a
    /// second; it reads back through [`PngMetadata::xmp`].
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when `packet` is
    /// not UTF-8, which the `iTXt` text field must be (§11.3.3.4). The check is made here rather
    /// than at encode time so a packet is never accepted that no chunk could hold.
    pub fn with_xmp(mut self, packet: &[u8]) -> Result<Self> {
        let packet = core::str::from_utf8(packet).map_err(|_| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "PNG: XMP packet is not UTF-8")
        })?;
        // §11.3.3.1 Table 21: "The use of iTXt, with Compression Flag set to 0, and both Language
        // Tag and Translated Keyword set to the null string, are recommended for XMP compliance."
        // A packet read out of a file that framed it otherwise keeps its framing; this entry
        // point has no framing to keep, so it takes the recommended one.
        self.ancillary.add_xmp(packet);
        Ok(self)
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
    /// A chunk's payload is limited to 2^31 − 1 bytes (PNG §5.3); an encode with a longer `store`
    /// fails with an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error rather
    /// than writing a chunk no reader accepts.
    ///
    /// The last of `with_c2pa` / `with_c2pa_reserved` wins; a file carries exactly one store.
    #[must_use]
    pub fn with_c2pa(mut self, store: &[u8]) -> Self {
        self.ancillary.c2pa = Some(C2paStore::Store(store.to_vec()));
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
    ///    (C2PA §18.5.4) — and have the signer build the store against it;
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
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when `len`
    /// exceeds the 2^31 − 1 bytes PNG §5.3 bounds any chunk payload to, refused here rather than
    /// at encode time so no reservation is accepted that no chunk could hold. Nothing is
    /// allocated for the reservation until the encode writes it.
    pub fn with_c2pa_reserved(mut self, len: usize) -> Result<Self> {
        chunk::check_data_len(len)?;
        self.ancillary.c2pa = Some(C2paStore::Reserved(len));
        Ok(self)
    }

    /// Encodes `image`, appending it to `out` as [`EncodeImage::encode_image`] does, and reports
    /// how many bytes the PNG took and where the C2PA manifest-store chunk landed, for the
    /// reserve-then-fill flow described at [`with_c2pa_reserved`](Self::with_c2pa_reserved).
    ///
    /// The span is read back from the bytes written — the same walk
    /// [`PngReport::c2pa`](crate::PngReport::c2pa) performs, over the same rule (the first
    /// CRC-valid `caBX` before the first `IDAT`) — so it cannot disagree with what a later
    /// [`deconstruct`](crate::deconstruct) of the same bytes reports. Its offsets count from the
    /// first byte of this PNG, not of `out`. There is deliberately no indexed twin of this method:
    /// [`encode_indexed8`](Self::encode_indexed8) needs a palette and does not fit this shape, and
    /// `deconstruct(&png)?.c2pa()` gives an indexed caller the same span, which
    /// [`fill_c2pa`](crate::fill_c2pa) then fills.
    ///
    /// # Errors
    ///
    /// As [`EncodeImage::encode_image`], which includes rejecting a C2PA store longer than the
    /// 2^31 − 1 bytes a chunk can carry (PNG §5.3) — so a store that was set always yields
    /// `Some` span on success.
    pub fn encode_with_report<P: Pixel>(
        &self,
        image: ImageRef<'_, P>,
        out: &mut Vec<u8>,
    ) -> Result<PngEncodeReport>
    where
        Self: EncodeImage<P>,
    {
        let start = out.len();
        let len = self.encode_image(image, out)?;
        let c2pa = chunk::find_c2pa(&out[start..]);
        Ok(PngEncodeReport { len, c2pa })
    }

    /// Encodes an 8-bit indexed (palette) image. Indexed colour does not fit the single-buffer
    /// [`EncodeImage`] shape because it needs a separate palette, so it is an inherent method.
    ///
    /// `palette` is **cleaned** before it is written, silently and losslessly: an entry nothing in
    /// the file names is dropped, a second entry holding the same RGB *and* alpha as an earlier one
    /// is merged into it, the trailing opaque `tRNS` bytes §11.3.1.1 lets a chunk omit are omitted,
    /// the index bit depth is derived from what survives, and the image's indices are renumbered to
    /// match (the crate-private `Plte::cleaned`). The colour every pixel resolves to is unchanged,
    /// which is why this reports nothing: a merged entry did not fail to come along, it arrived under
    /// another index. Surviving entries keep the order you gave them unless
    /// [`with_palette_order`](Self::with_palette_order) asks for another, in which case they are
    /// renumbered after cleaning and the indices follow.
    ///
    /// "Nothing in the file names it" includes the `bKGD` background, in **whichever** of the three
    /// [`Background`] forms you set it in with [`with_background`](Self::with_background) — an
    /// index, a grey level or an RGB triple. Each names an entry of the palette you
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
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if any
    /// index is out of range for `palette`.
    pub fn encode_indexed8(
        &self,
        image: ImageRef<'_, Indexed8>,
        palette: &Palette,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let indices = image.as_samples();
        let max_index = indices.iter().copied().max().unwrap_or(0);
        if usize::from(max_index) >= palette.rgb_entries().len() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "PNG: palette index out of range",
            ));
        }
        let palette = Plte::from(palette);
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
                    // The caller's background is a colour in its 8-bit palette's terms.
                    8,
                )
            }
            None => None,
        };
        if let Some(index) = background {
            used[usize::from(index)] = true;
        }
        let (palette, mut remap) = palette.cleaned(&used);
        let mut indices: Vec<u8> = indices.iter().map(|&i| remap[usize::from(i)]).collect();
        // A reorder is a second renumbering on top of the cleaning one, composed into `remap`
        // so the background below follows both.
        let palette = match self.palette_order {
            PaletteOrder::AsSupplied => palette,
            PaletteOrder::ModifiedZeng => {
                let (reordered, order) = palette.modified_zeng(&indices);
                for index in indices.iter_mut().chain(remap.iter_mut()) {
                    *index = order[usize::from(*index)];
                }
                reordered
            }
        };
        // The background still names the colour the caller chose, so the chunk is rewritten as
        // that entry's index in the *cleaned* palette, whichever form it arrived in. Pinning the
        // index is also what stops a colour-form background being resolved a second time against
        // the cleaned palette and landing on a different entry — an opaque triple whose
        // transparent twin outlived it would resolve to the twin. Unconditionally, so that the
        // answer never depends on the order cleaning leaves the survivors in: guarding the copy on
        // "the bytes would change" saves one clone of the encoder's chunk state on the one path
        // that has a background at all, and buys it with a branch whose two sides write the same
        // file.
        let renumbered;
        let this = match background {
            Some(index) => {
                renumbered = self
                    .clone()
                    .with_background(Background::Index(remap[usize::from(index)]));
                &renumbered
            }
            None => self,
        };

        let dims = image.dimensions();
        // Use the smallest bit depth that holds every index — a free, lossless space win.
        let depth = reduce::index_bit_depth(palette.len());
        let packed;
        let sample_bytes = if depth < 8 {
            packed = pack::pack_scanlines(
                &indices,
                dims.width() as usize,
                dims.height() as usize,
                depth,
            );
            packed.as_slice()
        } else {
            indices.as_slice()
        };
        let plte = palette.plte();
        let trns = palette.trns();
        this.write_png(
            (dims.width(), dims.height()),
            sample_bytes,
            WrittenHeader {
                color: ColorType::Indexed,
                bit_depth: depth,
                palette: Some(WrittenPalette {
                    plte: &plte,
                    trns,
                    origin: PaletteOrigin::Caller,
                }),
                // The caller's background is a colour in its 8-bit palette's terms.
                source_depth: 8,
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
            (dims.width(), dims.height()),
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
                8,
                reduce::analyze8_for(samples, channels, self.profile_family()),
                |o| {
                    self.write_png(
                        (dims.width(), dims.height()),
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
            (dims.width(), dims.height()),
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
                16,
                reduce::analyze16_for(samples, channels, self.profile_family()),
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

    /// The colour family the embedded ICC profile, if any, pins a reduction to (§11.3.2.3).
    fn profile_family(&self) -> Family {
        self.ancillary
            .iccp()
            .map_or(Family::Any, |(_, profile)| Family::of_profile(profile))
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
            (dims.width(), dims.height()),
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
        // Reject a C2PA store the §5.3 length field cannot carry before any byte is written,
        // rather than emit a caBX no reader accepts (and a report of `c2pa: None` for a file that
        // was told to carry one).
        if let Some(store) = &self.ancillary.c2pa {
            chunk::check_data_len(store.len())?;
        }

        let start = out.len();
        out.extend_from_slice(&SIGNATURE);
        ihdr::write(out, width, height, bit_depth, color);
        // Colour-space chunks precede PLTE.
        self.ancillary
            .write_pre_plte(out, self.deflate_passes, written);
        pre_idat(out); // PLTE + tRNS (indexed only)
        // Background / physical / timing / text, and the C2PA store last: immediately before IDAT.
        self.ancillary
            .write_post_plte(out, self.deflate_passes, written);

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
            .with_deflate_passes(self.deflate_passes)
            .with_optimal_parse_limit(self.optimal_parse_limit);
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
            // preferred) strategy — the behaviour the golden outputs are pinned to. The trials run
            // concurrently, but `run_trials` hands them back in candidate order, so which thread
            // finished first never reaches the reduction.
            let candidates = run_trials(&compress)?;
            Ok(candidates
                .into_iter()
                .min_by_key(Vec::len)
                .unwrap_or_default())
        } else {
            compress(self.filter)
        }
    }

    /// Writes the smallest of the encodings [`reduce::analyze8_for`] / [`reduce::analyze16_for`]
    /// made reachable: the reduction they ranked first, the best reduction that adds no chunk, and
    /// the image encoded untouched.
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
    ///
    /// **An embedded ICC profile narrows the reductions, not the race.** §11.3.2.3 pins the colour
    /// type to the profile's family, and the analysis already withheld the reductions outside it
    /// ([`Family`]), so no candidate this function adds leaves the family. The unreduced encoding
    /// is the caller's own layout and stays a candidate even where that layout is outside the
    /// family — grey content handed over as RGBA under a greyscale profile: an in-family reduction
    /// replaces it only where it is no larger, exactly as without a profile, because auto-reduce
    /// never grows a file. Where the input wins, the file keeps the pairing the caller made, as it
    /// would with auto-reduce off.
    fn write_reduced_or_native(
        &self,
        dims: Dimensions,
        source_depth: u8,
        reductions: Reductions,
        native: impl FnOnce(&mut Vec<u8>) -> Result<usize>,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let (chunked, chunk_free) = match reductions {
            Reductions::None => return native(out),
            Reductions::ChunkFree(reduced) => {
                return self.write_reduced(dims, source_depth, reduced, out);
            }
            Reductions::Chunked {
                chunked,
                chunk_free,
            } => (chunked, chunk_free),
        };
        let mut reduced_encoding = Vec::new();
        self.write_reduced(dims, source_depth, chunked, &mut reduced_encoding)?;
        if let Some(free) = chunk_free {
            let mut free_encoding = Vec::new();
            self.write_reduced(dims, source_depth, free, &mut free_encoding)?;
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

    /// Writes a reduced encoding chosen by [`reduce::analyze8_for`] / [`reduce::analyze16_for`],
    /// whose input samples were `source_depth` (8 or 16) bits — the depth the caller's `bKGD` is
    /// at.
    fn write_reduced(
        &self,
        dims: Dimensions,
        source_depth: u8,
        reduced: Reduced,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        let wh = (dims.width(), dims.height());
        match reduced {
            Reduced::Gray { depth, samples } => {
                let packed;
                let sample_bytes = if depth < 8 {
                    packed = pack::pack_scanlines(
                        &samples,
                        dims.width() as usize,
                        dims.height() as usize,
                        depth,
                    );
                    packed.as_slice()
                } else {
                    &samples
                };
                self.write_png(
                    wh,
                    sample_bytes,
                    WrittenHeader::new(ColorType::Grayscale, depth).reduced_from(source_depth),
                    |_| {},
                    out,
                )
            }
            Reduced::GrayAlpha8(samples) => self.write_png(
                wh,
                &samples,
                WrittenHeader::new(ColorType::GrayscaleAlpha, 8).reduced_from(source_depth),
                |_| {},
                out,
            ),
            Reduced::Rgb8(samples) => self.write_png(
                wh,
                &samples,
                WrittenHeader::new(ColorType::Truecolor, 8).reduced_from(source_depth),
                |_| {},
                out,
            ),
            // §11.3.1.1: for truecolour, tRNS is three 16-bit big-endian samples naming the one
            // colour a decoder renders as fully transparent. At depth 8 the high byte is zero.
            Reduced::Rgb8Keyed { samples, key } => self.write_png(
                wh,
                &samples,
                WrittenHeader::new(ColorType::Truecolor, 8).reduced_from(source_depth),
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
                WrittenHeader::new(ColorType::Grayscale, 8).reduced_from(source_depth),
                |out| chunk::write_chunk(out, *b"tRNS", &[0, key]),
                out,
            ),
            Reduced::Rgba8(samples) => self.write_png(
                wh,
                &samples,
                WrittenHeader::new(ColorType::TruecolorAlpha, 8).reduced_from(source_depth),
                |_| {},
                out,
            ),
            Reduced::Gray16Be(bytes) => self.write_png(
                wh,
                &bytes,
                WrittenHeader::new(ColorType::Grayscale, 16).reduced_from(source_depth),
                |_| {},
                out,
            ),
            Reduced::GrayAlpha16Be(bytes) => self.write_png(
                wh,
                &bytes,
                WrittenHeader::new(ColorType::GrayscaleAlpha, 16).reduced_from(source_depth),
                |_| {},
                out,
            ),
            Reduced::Rgb16Be(bytes) => self.write_png(
                wh,
                &bytes,
                WrittenHeader::new(ColorType::Truecolor, 16).reduced_from(source_depth),
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
                        dims.width() as usize,
                        dims.height() as usize,
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
                        source_depth,
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
                8,
                reduce::analyze8_for(image.as_samples(), 1, self.profile_family()),
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
        let packed = pack::pack_scanlines(&bits, dims.width() as usize, dims.height() as usize, 1);
        self.write_png(
            (dims.width(), dims.height()),
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
                8,
                reduce::analyze8_for(image.as_samples(), 3, self.profile_family()),
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
                16,
                reduce::analyze16_for(samples, 1, self.profile_family()),
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
                16,
                reduce::analyze16_for(samples, 3, self.profile_family()),
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

    /// The background builder reaches the bKGD chunk, in the width the colour type requires.
    ///
    /// The grey and index forms once had setters of their own that could return
    /// `Self::default()` -- discarding the caller's colour *and* every other setting made before
    /// them -- and no test noticed (#110); one `with_background` now carries all three forms, and
    /// the RGB form is pinned by `tests/ancillary_colour_type.rs`.
    ///
    /// bKGD's payload width is colour-type-specific (PNG 3rd ed. §11.3.4.1): two bytes for
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
            .with_background(Background::Gray(0x34))
            .encode_image(img, &mut png)
            .unwrap();
        assert_eq!(
            find_chunk(&png, b"bKGD"),
            Some(vec![0x00, 0x34]),
            "greyscale bKGD is the 16-bit level, big-endian"
        );

        // Indexed: one byte, the palette index.
        let entries: Vec<[u8; 3]> = (0..8u8).map(|i| [i, i.wrapping_add(70), 90]).collect();
        let palette = Palette::new(&entries).unwrap();
        let indices: Vec<u8> = (0..200u8).map(|i| i % 8).collect();
        let img = ImageRef::<Indexed8>::new(&indices, Dimensions::new(200, 1).unwrap()).unwrap();
        let mut png = Vec::new();
        PngEncoder::new()
            .with_background(Background::Index(7))
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
            let palette = Palette::new(&entries).unwrap();
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
        let padded = Palette::with_alpha(&padded, &padded_alpha).unwrap();
        let tight = Palette::with_alpha(&colours, &alphas).unwrap();

        let indices: Vec<u8> = (0..(w * h) as usize)
            .map(|i| (((i as u32 % w) / 7 + (i as u32 / w) / 5) % 4) as u8)
            .collect();
        let encode = |palette: &Palette| {
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
            Palette::new(&[[0, 0, 0], [1, 1, 1], [2, 2, 2], [3, 3, 3], [4, 4, 4]]).unwrap();
        // Only entry 1 is painted; entry 3 is named by the background alone.
        let indices = vec![1u8; 8];
        let img = ImageRef::<Indexed8>::new(&indices, Dimensions::new(8, 1).unwrap()).unwrap();
        let mut png = Vec::new();
        PngEncoder::new()
            .with_background(Background::Index(3))
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

    /// [`PaletteOrder::ModifiedZeng`] writes the cleaned palette in its reordered sequence, and
    /// the pixels and the `bKGD` index follow both renumberings — cleaning's, then the reorder's.
    ///
    /// Entry 5 is dropped (nothing names it) and entry 0 is named by the background alone. Zeng
    /// over the pixels seeds with 1–3, appends 4 and then 2, and the unattached 0 goes last, so
    /// the background moves from slot 0 to slot 4. libpng resolves every pixel to the colour the
    /// caller gave it — libpng rather than our own decoder, which would read the file through the
    /// palette the encoder wrote, so a wrong palette and a matching wrong index map would agree.
    #[test]
    fn a_reordered_palette_carries_its_pixels_and_background_along() {
        let entries: Vec<[u8; 3]> = (0..6).map(|i| [i * 10, i * 10, i * 10]).collect();
        let palette = Palette::new(&entries).unwrap();
        let indices = [1u8, 3, 1, 3, 1, 3, 4, 3, 4, 2];
        let img = ImageRef::<Indexed8>::new(&indices, Dimensions::new(10, 1).unwrap()).unwrap();
        let mut reordered = Vec::new();
        PngEncoder::new()
            .with_background(Background::Index(0))
            .with_palette_order(PaletteOrder::ModifiedZeng)
            .encode_indexed8(img, &palette, &mut reordered)
            .unwrap();

        assert_eq!(
            find_chunk(&reordered, b"PLTE"),
            Some(vec![
                10, 10, 10, 30, 30, 30, 40, 40, 40, 20, 20, 20, 0, 0, 0
            ])
        );
        assert_eq!(find_chunk(&reordered, b"bKGD"), Some(vec![4]));
        let (_, _, rgba) = libpng_oracle::decode_rgba8(&reordered);
        let expected: Vec<u8> = indices
            .iter()
            .flat_map(|&i| {
                let [r, g, b] = entries[usize::from(i)];
                [r, g, b, 255]
            })
            .collect();
        assert_eq!(
            rgba, expected,
            "every pixel still resolves to the colour the caller gave it"
        );
    }

    /// A background set as a *colour* names, after cleaning, an entry holding that colour with
    /// that alpha.
    ///
    /// `bKGD` names a palette entry in three forms (§11.3.4.1) -- an index, a grey sample, an RGB
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
    /// The colour is read back the way §11.3.4.1 says a reader reads it: the index into `PLTE`,
    /// and the same index into `tRNS` (opaque past its end, §11.3.1.1).
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
            let palette = Palette::with_alpha(entries, alphas).unwrap();
            let dims = Dimensions::new(painted.len() as u32, 1).unwrap();
            let img = ImageRef::<Indexed8>::new(painted, dims).unwrap();
            let mut png = Vec::new();
            PngEncoder::new()
                .with_background(Background::Rgb(r.into(), g.into(), b.into()))
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
        let palette = Palette::new(&[[9, 8, 7], [6, 5, 4], [3, 2, 1]]).unwrap();
        for index in [3u8, 4, 200, 255] {
            let indices = vec![0u8, 1, 2, 1];
            let img = ImageRef::<Indexed8>::new(&indices, Dimensions::new(4, 1).unwrap()).unwrap();
            let mut png = Vec::new();
            PngEncoder::new()
                .with_background(Background::Index(index))
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

    /// `run_trials` re-raises a later trial's own panic payload even when an earlier candidate
    /// returned `Err` (issue #941). Joining lazily stopped at that `Err` and left the panicked
    /// thread unjoined, so `std::thread::scope` replaced the payload with its generic one.
    #[test]
    fn run_trials_resumes_a_later_panic_past_an_earlier_err() {
        let trial = |strategy: FilterStrategy| -> Result<Vec<u8>> {
            match strategy {
                FilterStrategy::Fixed(FilterType::Sub) => Err(crate::backend::poisoned()),
                FilterStrategy::MinTrialDeflate => panic!("the backend's own payload"),
                _ => Ok(Vec::new()),
            }
        };
        let payload = std::panic::catch_unwind(|| run_trials(&trial))
            .expect_err("the later trial's panic must reach the caller");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"the backend's own payload"),
            "the scope's generic payload replaced the trial's"
        );
    }

    /// One long greyscale row whose bytes are neither flat nor periodic, so the span the optimal
    /// parse works over is what decides the parse. It filters to 40 001 bytes, which is one span
    /// when the limit is unbounded and two when the limit sits at the 32 KiB LZ77 window floor.
    fn long_unrepeating_row() -> Vec<u8> {
        (0..40_000u32)
            .map(|i| {
                let mut h = i.wrapping_mul(2_654_435_761);
                h ^= h >> 15;
                // A slow ramp plus a small jitter: compressible enough that the parse has matches
                // to choose between, unrepeating enough that the choice is not forced.
                ((i / 97) as u8).wrapping_add((h % 7) as u8)
            })
            .collect()
    }

    /// Encodes [`long_unrepeating_row`] at `Level::Best` under `configure`.
    fn best_encoded_row(configure: impl FnOnce(PngEncoder) -> PngEncoder) -> Vec<u8> {
        let src = long_unrepeating_row();
        let img = ImageRef::<Gray8>::new(&src, Dimensions::new(40_000, 1).unwrap()).unwrap();
        let mut png = Vec::new();
        configure(PngEncoder::new().with_compression(Level::Best))
            // The optimal parse is the only consumer of the limit, and refinement passes are the
            // expensive part of `Level::Best`; the seed parse alone already spans.
            .with_deflate_passes(0)
            .encode_image(img, &mut png)
            .unwrap();
        png
    }

    #[test]
    fn the_optimal_parse_limit_reaches_the_idat_stream() {
        // `with_optimal_parse_limit` is only observable through the bytes it changes: a limit at
        // the LZ77 window splits this row's filtered stream into two spans, each with its own cost
        // model, where the default parses it as one.
        let one_span = best_encoded_row(|e| e.with_optimal_parse_limit(usize::MAX));
        let two_spans = best_encoded_row(|e| e.with_optimal_parse_limit(1));
        assert!(
            one_span != two_spans,
            "the limit did not reach the IDAT deflate: both spans encoded to {} bytes",
            one_span.len()
        );
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

    /// A profile header whose data colour space (ICC.1:2022 §7.2.6, bytes 16–19) is `space`.
    fn icc_profile(space: &[u8; 4]) -> Vec<u8> {
        let mut icc = vec![0u8; 128];
        icc[16..20].copy_from_slice(space);
        icc
    }

    /// Auto-reduces `samples` as `P` under an optional profile, returning the IHDR colour type.
    fn reduced_color_type<P: gamut_core::Pixel>(samples: &[P::Sample], icc: Option<&[u8]>) -> u8
    where
        PngEncoder: EncodeImage<P>,
    {
        let dims = Dimensions::new(8, 8).unwrap();
        let mut encoder = PngEncoder::new().with_auto_reduce(true);
        if let Some(icc) = icc {
            encoder = encoder.with_icc_profile_name("p").with_icc_profile(icc);
        }
        let png = encoder
            .encode_to_vec(ImageRef::<P>::new(samples, dims).unwrap())
            .unwrap();
        png[25]
    }

    /// §11.3.2.3 puts an RGB profile under colour types 2, 3 and 6 only. Grey RGBA content with
    /// 64 distinct levels reduces to greyscale on its own; under an RGB profile it keeps the
    /// colour family and drops only the opaque alpha. Kills `profile_family` and its use in
    /// `encode_alpha8`, which the CLI's default convert path reaches.
    #[test]
    fn auto_reduce_keeps_grey_content_rgb_under_an_rgb_profile() {
        let greys: Vec<u8> = (0..64u8)
            .flat_map(|i| [i * 3; 3].into_iter().chain([255]))
            .collect();
        assert_eq!(
            reduced_color_type::<Rgba8>(&greys, None),
            ColorType::Grayscale.code()
        );
        assert_eq!(
            reduced_color_type::<Rgba8>(&greys, Some(&icc_profile(b"RGB "))),
            ColorType::Truecolor.code()
        );
    }

    /// The input layout stays a race candidate when it is outside the profile's family: an
    /// in-family reduction replaces it only where it is no larger. Four grey levels as `Gray8`
    /// win the race as themselves with no profile, and still do under an RGB profile, where the
    /// in-family palette — colour type 3 — is the larger file. Pins that `write_reduced_or_native`
    /// measures the native encoding whatever the profile.
    #[test]
    fn auto_reduce_races_a_native_layout_outside_the_profiles_family() {
        let greys: Vec<u8> = (0..64u8).map(|i| i % 4 + 1).collect();
        assert_eq!(
            reduced_color_type::<Gray8>(&greys, None),
            ColorType::Grayscale.code()
        );
        assert_eq!(
            reduced_color_type::<Gray8>(&greys, Some(&icc_profile(b"RGB "))),
            ColorType::Grayscale.code()
        );
    }

    /// A greyscale profile keeps grey RGBA content in colour types 0 and 4: neither a palette
    /// nor the RGBA layout it arrived in.
    #[test]
    fn auto_reduce_keeps_grey_content_grey_under_a_greyscale_profile() {
        let greys: Vec<u8> = (0..64u8)
            .flat_map(|i| [i % 4 + 1; 3].into_iter().chain([255]))
            .collect();
        assert_eq!(
            reduced_color_type::<Rgba8>(&greys, Some(&icc_profile(b"GRAY"))),
            ColorType::Grayscale.code()
        );
    }
}
