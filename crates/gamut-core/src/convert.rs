//! Format-agnostic conversion between interleaved pixel layouts, with every lossy decision made
//! explicit by the caller.
//!
//! Decoders present an image in whatever layout the file natively carries. A caller usually wants
//! one specific layout instead, and bridging the two splits cleanly in half:
//!
//! - **Lossless widening** — replicating grey into RGB, adding an opaque alpha channel, widening
//!   8-bit samples to 16 — throws nothing away and needs no permission.
//! - **Lossy narrowing** — dropping an alpha channel, narrowing 16-bit samples to 8, reducing RGB
//!   to a single luma sample — destroys information, and *which* information it destroys is a
//!   policy question with no universally right answer.
//!
//! This module defines both halves once, for every [`Pixel`] layout, so format crates never have to
//! answer the policy questions themselves. A conversion is refused with [`Error::Unsupported`]
//! unless the [`ConvertPolicy`] explicitly permits the loss it would incur — [`ConvertPolicy`]'s
//! default is [`ConvertPolicy::lossless`], which permits none of it.
//!
//! # Two entry points, one engine
//!
//! [`convert`] is the typed door: `ImageRef<'_, P>` in, [`ImageBuf<Q>`] out. Callers use this.
//!
//! [`convert_from_raw`] is the decoder door. A decoder knows its target layout statically (it is
//! implementing [`DecodeImage<Q>`](crate::DecodeImage)) but discovers the file's native layout at
//! runtime, so its source is a [`RawImage`] carrying a [`PixelFormat`] tag rather than a brand.
//! [`convert_from_raw_into`] writes into caller-provided storage, backing
//! [`DecodeImage::decode_image_into`](crate::DecodeImage::decode_image_into).
//!
//! # What is out of scope
//!
//! [`Indexed8`](crate::Indexed8) and [`Cmyk8`](crate::Cmyk8) convert only to themselves. Palette
//! indices are meaningless without the palette table, which the single-buffer model cannot carry
//! (see `gamut-core`'s deferred shared-palette primitive), and CMYK↔RGB is a colour-management
//! transform requiring an ICC rendering intent — not a layout rearrangement. Both refuse with a
//! message naming the missing machinery rather than silently applying a naive approximation.
//!
//! # Example
//!
//! ```
//! use gamut_core::{
//!     convert::{convert, AlphaPolicy, ConvertPolicy},
//!     Dimensions, ImageRef, Rgb8, Rgba8,
//! };
//!
//! let dims = Dimensions::new(2, 1).unwrap();
//! // Two half-transparent red pixels.
//! let rgba = [255u8, 0, 0, 128, 255, 0, 0, 128];
//! let image = ImageRef::<Rgba8>::new(&rgba, dims).unwrap();
//!
//! // Refused by default: dropping alpha loses information.
//! assert!(convert::<Rgba8, Rgb8>(image, ConvertPolicy::lossless()).is_err());
//!
//! // Permitted once the caller says how alpha should be handled.
//! let policy = ConvertPolicy::lossless().with_alpha(AlphaPolicy::Drop);
//! let rgb = convert::<Rgba8, Rgb8>(image, policy).unwrap();
//! assert_eq!(rgb.as_samples(), &[255, 0, 0, 255, 0, 0]);
//! ```

use crate::luminance::{
    BT601_LUMA_WEIGHTS, BT709_LUMA_WEIGHTS, BT2020_LUMA_WEIGHTS, LUMA_FIX, LUMA_ONE,
};
use crate::{
    ColorModel, Dimensions, Error, ImageBuf, ImageRef, Pixel, PixelFormat, Result, Sample,
};

/// What to do when the source carries an alpha channel the target layout cannot hold.
///
/// Only consulted when alpha would actually be lost. Adding an opaque alpha channel to a target
/// that has one is lossless and always permitted, whatever this is set to.
///
/// Discriminants are explicit and permanent — they are C ABI values; new variants append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
#[repr(u32)]
pub enum AlphaPolicy {
    /// Refuse the conversion with [`Error::Unsupported`]. The default.
    #[default]
    Reject = 0,
    /// Discard the alpha channel, keeping the unassociated colour samples untouched.
    ///
    /// The transparent parts of the image keep whatever colour was stored under them, which for an
    /// unassociated-alpha buffer may be arbitrary. Cheap and exactly reversible for opaque images.
    Drop = 1,
    /// Composite the image over [`ConvertPolicy::background`] and keep the result.
    ///
    /// Treats the source as unassociated alpha (which is what [`ColorModel::Rgba`] and
    /// [`ColorModel::GrayAlpha`] specify) and computes `src * a + background * (1 - a)`.
    CompositeOver = 2,
}

/// What to do when the target's samples are narrower than the source's.
///
/// Covers any reduction in sample precision: 16-bit samples into an 8-bit layout, and reducing a
/// grey or colour image to [`Bilevel`](crate::Bilevel). Widening is lossless and always permitted.
///
/// Discriminants are explicit and permanent — they are C ABI values; new variants append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
#[repr(u32)]
pub enum DepthPolicy {
    /// Refuse the conversion with [`Error::Unsupported`]. The default.
    #[default]
    Reject = 0,
    /// Rescale to the narrower range, rounding to nearest.
    ///
    /// Narrowing 16-bit to 8-bit maps the full range onto the full range (`65535 → 255`), the exact
    /// inverse of the widening PNG specifies in §13.12 — not a truncating shift, which would make
    /// white slightly grey. A reduction to bilevel compares luma against
    /// [`ConvertPolicy::threshold`].
    Rescale = 1,
}

/// Which luma coefficients to use when reducing colour to a single grey sample.
///
/// Only consulted when the source carries RGB and the target is a grey layout. The weights
/// themselves live in [`crate::luminance`].
///
/// Discriminants are explicit and permanent — they are C ABI values; new variants append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
#[repr(u32)]
pub enum LumaPolicy {
    /// Refuse the conversion with [`Error::Unsupported`]. The default.
    #[default]
    Reject = 0,
    /// BT.601 / SMPTE 170M weights — the JFIF convention.
    Bt601 = 1,
    /// BT.709 weights — the sRGB/HD convention, and the right default for gamut's buffers.
    Bt709 = 2,
    /// BT.2020 / BT.2100 weights — for samples carrying wide-gamut UHD primaries.
    Bt2020 = 3,
}

impl LumaPolicy {
    /// The fixed-point weight triple this policy selects, or `None` for [`LumaPolicy::Reject`].
    fn weights(self) -> Option<[u32; 3]> {
        match self {
            LumaPolicy::Reject => None,
            LumaPolicy::Bt601 => Some(BT601_LUMA_WEIGHTS),
            LumaPolicy::Bt709 => Some(BT709_LUMA_WEIGHTS),
            LumaPolicy::Bt2020 => Some(BT2020_LUMA_WEIGHTS),
        }
    }
}

/// The lossy decisions a conversion is permitted to make, plus the parameters those decisions need.
///
/// Plain `Copy` data whose fields are reached through accessors, so it stays mechanically portable
/// to C. Build one by starting from [`ConvertPolicy::lossless`] or [`ConvertPolicy::permissive`]
/// and overriding what you need; every `with_*` setter and every getter is `const`.
///
/// ```
/// use gamut_core::convert::{AlphaPolicy, ConvertPolicy, LumaPolicy};
///
/// let policy = ConvertPolicy::lossless()
///     .with_alpha(AlphaPolicy::CompositeOver)
///     .with_background([u16::MAX; 3]) // composite over white
///     .with_luma(LumaPolicy::Bt709);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub struct ConvertPolicy {
    alpha: AlphaPolicy,
    depth: DepthPolicy,
    luma: LumaPolicy,
    background: [u16; 3],
    threshold: u16,
}

impl Default for ConvertPolicy {
    fn default() -> Self {
        Self::lossless()
    }
}

impl ConvertPolicy {
    /// The default midpoint for a reduction to bilevel: half of full scale.
    const DEFAULT_THRESHOLD: u16 = 32_768;

    /// Permits no loss at all: every narrowing conversion is refused.
    ///
    /// The default, and what every gamut decoder uses unless the caller says otherwise, so a typed
    /// decode never quietly discards part of the file.
    pub const fn lossless() -> Self {
        Self {
            alpha: AlphaPolicy::Reject,
            depth: DepthPolicy::Reject,
            luma: LumaPolicy::Reject,
            background: [0; 3],
            threshold: Self::DEFAULT_THRESHOLD,
        }
    }

    /// Permits every loss this module can express, with conventional defaults.
    ///
    /// Alpha is dropped, samples rescale to the target width, and colour reduces to luma with
    /// [`LumaPolicy::Bt709`]. Suited to an application that just wants pixels in a fixed layout —
    /// `gamut-cli` uses this. Prefer naming the individual policies when the choice matters.
    pub const fn permissive() -> Self {
        Self {
            alpha: AlphaPolicy::Drop,
            depth: DepthPolicy::Rescale,
            luma: LumaPolicy::Bt709,
            background: [0; 3],
            threshold: Self::DEFAULT_THRESHOLD,
        }
    }

    /// Sets how an alpha channel the target cannot hold is handled.
    pub const fn with_alpha(mut self, policy: AlphaPolicy) -> Self {
        self.alpha = policy;
        self
    }

    /// Sets how samples narrower than the source's are produced.
    pub const fn with_depth(mut self, policy: DepthPolicy) -> Self {
        self.depth = policy;
        self
    }

    /// Sets which luma coefficients reduce colour to grey.
    pub const fn with_luma(mut self, policy: LumaPolicy) -> Self {
        self.luma = policy;
        self
    }

    /// Sets the background colour [`AlphaPolicy::CompositeOver`] composites against.
    ///
    /// Full-range RGB regardless of the layouts involved, so one value reads the same for 8- and
    /// 16-bit targets: `[0; 3]` is black (the default) and `[u16::MAX; 3]` is white.
    pub const fn with_background(mut self, rgb: [u16; 3]) -> Self {
        self.background = rgb;
        self
    }

    /// Sets the full-range luma at or above which a bilevel target records white.
    ///
    /// Defaults to half of full scale.
    pub const fn with_threshold(mut self, threshold: u16) -> Self {
        self.threshold = threshold;
        self
    }

    /// How an alpha channel the target cannot hold is handled.
    #[must_use]
    pub const fn alpha(self) -> AlphaPolicy {
        self.alpha
    }

    /// How samples narrower than the source's are produced.
    #[must_use]
    pub const fn depth(self) -> DepthPolicy {
        self.depth
    }

    /// Which luma coefficients reduce colour to grey.
    ///
    /// A decoder that must choose what to ask its backend for — rather than converting after the
    /// fact — reads this to decide whether a colour-to-grey request is worth attempting at all.
    #[must_use]
    pub const fn luma(self) -> LumaPolicy {
        self.luma
    }

    /// The full-range background colour [`AlphaPolicy::CompositeOver`] composites against.
    #[must_use]
    pub const fn background(self) -> [u16; 3] {
        self.background
    }

    /// The full-range luma at or above which a bilevel target records white.
    #[must_use]
    pub const fn threshold(self) -> u16 {
        self.threshold
    }
}

/// A borrowed interleaved image whose layout is known only at runtime.
///
/// The counterpart of [`ImageRef`] for a decoder that has just produced native samples and does not
/// yet have them in a branded buffer: the layout travels as a [`PixelFormat`] tag instead of a
/// [`Pixel`] brand. Validated on construction exactly as [`ImageRef::new`] is, so the conversion
/// engine can trust the length and the sample width.
#[derive(Debug, Clone, Copy)]
#[must_use]
pub struct RawImage<'a, S: Sample> {
    samples: &'a [S],
    format: PixelFormat,
    dims: Dimensions,
}

impl<'a, S: Sample> RawImage<'a, S> {
    /// Describes `samples` as an image of `dims` in layout `format`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `S` is not `format`'s sample width, if `dims` is
    /// zero-sized or overflows `usize`, or if `samples.len()` is not
    /// `width * height * format.channels()`.
    pub fn new(samples: &'a [S], format: PixelFormat, dims: Dimensions) -> Result<Self> {
        if format.bytes_per_sample() != core::mem::size_of::<S>() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "convert: sample type does not match the pixel format's sample width",
            ));
        }
        if dims.is_empty() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "convert: zero-sized image",
            ));
        }
        let want = dims.sample_count(format.channels()).ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "convert: image dimensions overflow usize",
            )
        })?;
        if samples.len() != want {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "convert: sample count does not match dimensions",
            ));
        }
        Ok(Self {
            samples,
            format,
            dims,
        })
    }

    /// Builds a view whose invariants a [`Pixel`] brand has already established.
    ///
    /// Backs [`ImageRef::as_raw`]; re-running [`RawImage::new`]'s checks there could only fail on
    /// a buffer that was already validated at construction.
    pub(crate) fn from_branded(samples: &'a [S], format: PixelFormat, dims: Dimensions) -> Self {
        Self {
            samples,
            format,
            dims,
        }
    }

    /// The raw interleaved samples, row-major.
    #[must_use]
    pub fn as_samples(self) -> &'a [S] {
        self.samples
    }

    /// The runtime layout tag.
    #[must_use]
    pub fn format(self) -> PixelFormat {
        self.format
    }

    /// The image dimensions.
    #[must_use]
    pub fn dimensions(self) -> Dimensions {
        self.dims
    }
}

/// Colour (non-alpha) channels a model carries, or `None` when the model has no colour-model-free
/// interpretation this module can convert through.
fn colour_channels(model: ColorModel) -> Option<usize> {
    match model {
        // Bilevel is one channel of luma with only two legal states; the read/write helpers below
        // handle the state mapping, so it travels the same path as Gray.
        ColorModel::Gray | ColorModel::GrayAlpha | ColorModel::Bilevel => Some(1),
        ColorModel::Rgb | ColorModel::Rgba => Some(3),
        // Palette indices need the table; ink separations need a colour-management transform.
        ColorModel::Cmyk | ColorModel::Indexed => None,
    }
}

/// Whether a model carries an alpha channel after its colour channels.
fn has_alpha(model: ColorModel) -> bool {
    match model {
        ColorModel::GrayAlpha | ColorModel::Rgba => true,
        ColorModel::Gray
        | ColorModel::Bilevel
        | ColorModel::Rgb
        | ColorModel::Cmyk
        | ColorModel::Indexed => false,
    }
}

/// The refusal for a layout this module deliberately does not convert.
fn opaque_model(model: ColorModel) -> Error {
    let message = match model {
        ColorModel::Indexed => {
            "convert: palette indices cannot be converted without the palette table"
        }
        _ => "convert: CMYK conversion needs a colour-management transform, not a layout change",
    };
    Error::unsupported(env!("CARGO_PKG_NAME"), message)
}

/// A validated, per-image conversion recipe: the policy questions are all answered here, once,
/// before a single pixel is touched, so a refused conversion never writes a partial result.
#[derive(Debug, Clone, Copy)]
struct Plan {
    src_channels: usize,
    dst_channels: usize,
    src_colour: usize,
    dst_colour: usize,
    src_alpha: bool,
    dst_alpha: bool,
    src_bilevel: bool,
    dst_bilevel: bool,
    /// Source and target layouts are the same: copy the samples through untouched. The only path
    /// open to layouts with no colour-model-free interpretation ([`ColorModel::Cmyk`],
    /// [`ColorModel::Indexed`]), for which a copy is the one conversion that is unambiguously
    /// correct.
    identity: bool,
    /// Set only when alpha is genuinely lost; `Drop` and `CompositeOver` differ only then.
    composite: bool,
    weights: [u32; 3],
    background: [u16; 3],
    threshold: u16,
}

impl Plan {
    /// A straight sample-for-sample copy between identical layouts.
    fn identity(format: PixelFormat) -> Self {
        Self {
            src_channels: format.channels(),
            dst_channels: format.channels(),
            src_colour: 0,
            dst_colour: 0,
            src_alpha: false,
            dst_alpha: false,
            src_bilevel: false,
            dst_bilevel: false,
            identity: true,
            composite: false,
            weights: BT709_LUMA_WEIGHTS,
            background: [0; 3],
            threshold: 0,
        }
    }

    /// Derives the recipe for `src` → `dst`, or the refusal explaining which policy is missing.
    fn derive(src: PixelFormat, dst: PixelFormat, policy: ConvertPolicy) -> Result<Self> {
        // Identical layouts short-circuit every policy question: nothing is lost, so nothing needs
        // permission. This is what lets CMYK and palette buffers pass through unchanged.
        if src == dst {
            return Ok(Self::identity(src));
        }

        let (src_model, dst_model) = (src.color_model(), dst.color_model());
        let src_colour = colour_channels(src_model).ok_or_else(|| opaque_model(src_model))?;
        let dst_colour = colour_channels(dst_model).ok_or_else(|| opaque_model(dst_model))?;

        let src_alpha = has_alpha(src_model);
        let dst_alpha = has_alpha(dst_model);
        let src_bilevel = src_model == ColorModel::Bilevel;
        let dst_bilevel = dst_model == ColorModel::Bilevel;

        // Losing alpha: the target has no slot for a channel the source actually carries.
        let drops_alpha = src_alpha && !dst_alpha;
        if drops_alpha && policy.alpha == AlphaPolicy::Reject {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "convert: target layout cannot hold the source's alpha channel; set an AlphaPolicy",
            ));
        }

        // Losing colour: three channels collapsing into one.
        if src_colour == 3 && dst_colour == 1 && policy.luma == LumaPolicy::Reject {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "convert: target layout cannot hold colour; set a LumaPolicy",
            ));
        }

        // Losing precision: narrower samples, or a collapse to two states. A bilevel *source* is
        // already two-state, so bilevel -> bilevel narrows nothing.
        let narrows_samples = src.bytes_per_sample() > dst.bytes_per_sample();
        let narrows_to_bilevel = dst_bilevel && !src_bilevel;
        if (narrows_samples || narrows_to_bilevel) && policy.depth == DepthPolicy::Reject {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "convert: target layout is narrower than the source; set a DepthPolicy",
            ));
        }

        Ok(Self {
            src_channels: src.channels(),
            dst_channels: dst.channels(),
            src_colour,
            dst_colour,
            src_alpha,
            dst_alpha,
            src_bilevel,
            dst_bilevel,
            identity: false,
            composite: drops_alpha && policy.alpha == AlphaPolicy::CompositeOver,
            // Unconditional for a grey target: the weights sum to LUMA_ONE, so applying them to an
            // already-grey pixel reproduces it exactly and needs no special case. Reject cannot
            // reach here for a colour source, so black weights would be unused; BT.709 keeps the
            // grey-source path exact.
            weights: policy.luma.weights().unwrap_or(BT709_LUMA_WEIGHTS),
            background: policy.background,
            threshold: policy.threshold,
        })
    }
}

/// Reads one source sample onto the canonical full-range scale.
fn read<S: Sample>(sample: S, bilevel: bool) -> u16 {
    if bilevel {
        // ColorModel::Bilevel: 0 is black, any non-zero value is white. Accepts both the 0/1 form a
        // 1-bit decoder produces and the 0/MAX form `write` emits.
        if sample == S::default() { 0 } else { u16::MAX }
    } else {
        sample.to_full_range_u16()
    }
}

/// Writes one canonical full-range value into a target sample.
fn write<T: Sample>(value: u16, bilevel: bool, threshold: u16) -> T {
    if bilevel {
        if value >= threshold {
            T::MAX_VALUE
        } else {
            T::default()
        }
    } else {
        T::from_full_range_u16(value)
    }
}

/// Composites one unassociated colour sample over `background`.
fn composite(value: u16, alpha: u16, background: u16) -> u16 {
    let inverse = u32::from(u16::MAX - alpha);
    let blended = u64::from(value) * u64::from(alpha) + u64::from(background) * u64::from(inverse);
    // Round to nearest so a fully opaque or fully transparent pixel is reproduced exactly.
    ((blended + 32_767) / 65_535) as u16
}

/// Reduces a full-range RGB triple to one luma sample with the plan's weights.
fn luma(rgb: [u16; 3], weights: [u32; 3]) -> u16 {
    let sum: u64 = weights
        .iter()
        .zip(rgb)
        .map(|(&weight, channel)| u64::from(weight) * u64::from(channel))
        .sum();
    ((sum + u64::from(LUMA_ONE / 2)) >> LUMA_FIX) as u16
}

/// Runs a derived plan over every pixel. Infallible: `derive` has already rejected the impossible
/// and both slices are known to hold whole pixels of the right count.
fn run<S: Sample, T: Sample>(src: &[S], dst: &mut [T], plan: &Plan) {
    if plan.identity {
        // Same layout, so same sample width: the widen/narrow pair round-trips exactly and is the
        // only way to move between two distinct `Sample` types that happen to be the same width.
        for (target, source) in dst.iter_mut().zip(src) {
            *target = T::from_full_range_u16(source.to_full_range_u16());
        }
        return;
    }
    let pixels = src.chunks_exact(plan.src_channels);
    let targets = dst.chunks_exact_mut(plan.dst_channels);
    for (source, target) in pixels.zip(targets) {
        let mut rgb = [0u16; 3];
        if plan.src_colour == 1 {
            let grey = read(source[0], plan.src_bilevel);
            rgb = [grey; 3];
        } else {
            for (channel, sample) in rgb.iter_mut().zip(source) {
                *channel = read(*sample, plan.src_bilevel);
            }
        }
        // A source without alpha is fully opaque; that keeps the compositing arithmetic below
        // uniform instead of branching on presence.
        let alpha = if plan.src_alpha {
            read(source[plan.src_colour], false)
        } else {
            u16::MAX
        };

        if plan.composite {
            for (channel, background) in rgb.iter_mut().zip(plan.background) {
                *channel = composite(*channel, alpha, background);
            }
        }

        if plan.dst_colour == 1 {
            target[0] = write(luma(rgb, plan.weights), plan.dst_bilevel, plan.threshold);
        } else {
            for (sample, channel) in target.iter_mut().zip(rgb) {
                *sample = write(channel, plan.dst_bilevel, plan.threshold);
            }
        }
        if plan.dst_alpha {
            // Alpha itself is never bilevel-coded, and survives a drop only into a target that has
            // a slot for it — so it passes through on the plain sample scale.
            target[plan.dst_colour] = write(alpha, false, plan.threshold);
        }
    }
}

/// Converts a runtime-tagged raw image into the branded layout `Q`.
///
/// The entry point a decoder uses: it knows `Q` statically from the
/// [`DecodeImage`](crate::DecodeImage) impl it is fulfilling, but learns the file's native layout
/// only at runtime.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] if the conversion would lose information `policy` does not
/// permit, or if either layout is [`Indexed8`](crate::Indexed8) or [`Cmyk8`](crate::Cmyk8) and the
/// two differ. Returns [`Error::InvalidInput`] if the dimensions overflow `usize`.
pub fn convert_from_raw<S: Sample, Q: Pixel>(
    src: RawImage<'_, S>,
    policy: ConvertPolicy,
) -> Result<ImageBuf<Q>> {
    // `derive` runs first so an unsupported pair costs no allocation.
    let plan = Plan::derive(src.format, Q::FORMAT, policy)?;
    let mut out = ImageBuf::<Q>::zeroed(src.dims)?;
    run(src.samples, out.as_mut_samples(), &plan);
    Ok(out)
}

/// Converts a runtime-tagged raw image into caller-provided storage for layout `Q`.
///
/// Backs [`DecodeImage::decode_image_into`](crate::DecodeImage::decode_image_into), letting a
/// decoder refill an existing buffer instead of allocating a new one. `dst` is left untouched
/// unless the conversion succeeds.
///
/// # Errors
///
/// As [`convert_from_raw`], plus [`Error::InvalidInput`] if `dst` is not exactly
/// `width * height * Q::CHANNELS` samples.
pub fn convert_from_raw_into<S: Sample, Q: Pixel>(
    src: RawImage<'_, S>,
    policy: ConvertPolicy,
    dst: &mut [Q::Sample],
) -> Result<()> {
    let plan = Plan::derive(src.format, Q::FORMAT, policy)?;
    let want = src.dims.sample_count(Q::CHANNELS).ok_or_else(|| {
        Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "convert: image dimensions overflow usize",
        )
    })?;
    if dst.len() != want {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "convert: destination length does not match dimensions",
        ));
    }
    run(src.samples, dst, &plan);
    Ok(())
}

/// Converts a branded image from layout `P` to layout `Q`.
///
/// The typed door onto the same engine [`convert_from_raw`] drives; use it when both layouts are
/// known at compile time.
///
/// # Errors
///
/// As [`convert_from_raw`].
pub fn convert<P: Pixel, Q: Pixel>(
    src: ImageRef<'_, P>,
    policy: ConvertPolicy,
) -> Result<ImageBuf<Q>> {
    convert_from_raw(src.as_raw(), policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Bilevel, Cmyk8, ErrorKind, Gray8, Gray16, GrayAlpha8, GrayAlpha16, Indexed8, Rgb8, Rgb16,
        Rgba8, Rgba16,
    };

    fn dims(w: u32, h: u32) -> Dimensions {
        Dimensions {
            width: w,
            height: h,
        }
    }

    /// Builds a one-pixel raw image; the shortest way to exercise a specific layout pair.
    fn raw<S: Sample>(samples: &[S], format: PixelFormat) -> RawImage<'_, S> {
        RawImage::new(samples, format, dims(1, 1)).unwrap()
    }

    #[test]
    fn lossless_widening_needs_no_policy() {
        let strict = ConvertPolicy::lossless();

        // Grey replicates into every colour channel, and gains opaque alpha.
        let grey = raw(&[7u8], PixelFormat::Gray8);
        let rgb = convert_from_raw::<_, Rgb8>(grey, strict).unwrap();
        assert_eq!(rgb.as_samples(), &[7, 7, 7]);
        let rgba = convert_from_raw::<_, Rgba8>(grey, strict).unwrap();
        assert_eq!(rgba.as_samples(), &[7, 7, 7, 255]);

        // 8 -> 16 widening replicates the byte (PNG 13.12), so 0xFF reaches full scale.
        let white = raw(&[255u8, 128, 0], PixelFormat::Rgb8);
        let wide = convert_from_raw::<_, Rgb16>(white, strict).unwrap();
        assert_eq!(wide.as_samples(), &[0xFFFF, 0x8080, 0]);
    }

    #[test]
    fn dropping_alpha_needs_an_alpha_policy() {
        let src = raw(&[10u8, 20, 30, 128], PixelFormat::Rgba8);

        let refused = convert_from_raw::<_, Rgb8>(src, ConvertPolicy::lossless()).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Unsupported);
        assert_eq!(refused.origin(), Some("gamut-core"));

        // Drop keeps the stored colour untouched.
        let dropped = convert_from_raw::<_, Rgb8>(
            src,
            ConvertPolicy::lossless().with_alpha(AlphaPolicy::Drop),
        )
        .unwrap();
        assert_eq!(dropped.as_samples(), &[10, 20, 30]);
    }

    #[test]
    fn compositing_blends_against_the_background() {
        // Half-transparent mid-grey over black and over white must land either side of the source.
        let src = raw(&[128u8, 128, 128, 128], PixelFormat::Rgba8);
        let over_black = ConvertPolicy::lossless()
            .with_alpha(AlphaPolicy::CompositeOver)
            .with_background([0; 3]);
        let over_white = ConvertPolicy::lossless()
            .with_alpha(AlphaPolicy::CompositeOver)
            .with_background([u16::MAX; 3]);

        let dark = convert_from_raw::<_, Rgb8>(src, over_black).unwrap();
        let light = convert_from_raw::<_, Rgb8>(src, over_white).unwrap();
        assert!(dark.as_samples()[0] < 128, "{:?}", dark.as_samples());
        assert!(light.as_samples()[0] > 128, "{:?}", light.as_samples());

        // The endpoints are exact: opaque reproduces the source, transparent reproduces the
        // background. This is what distinguishes correct rounding from an off-by-one blend.
        let opaque = raw(&[200u8, 100, 50, 255], PixelFormat::Rgba8);
        assert_eq!(
            convert_from_raw::<_, Rgb8>(opaque, over_white)
                .unwrap()
                .as_samples(),
            &[200, 100, 50]
        );
        let clear = raw(&[200u8, 100, 50, 0], PixelFormat::Rgba8);
        assert_eq!(
            convert_from_raw::<_, Rgb8>(clear, over_white)
                .unwrap()
                .as_samples(),
            &[255, 255, 255]
        );
    }

    #[test]
    fn compositing_rounds_to_nearest_at_full_precision() {
        // An 8-bit target re-quantises the blend, which hides a one-unit error in the rounding
        // addend. A 16-bit target does not: an opaque sample of 1 survives only if the addend is
        // added, since (1*65535 - 32767) / 65535 truncates to 0.
        let src = raw(&[1u16, 1, 1, u16::MAX], PixelFormat::Rgba16);
        let policy = ConvertPolicy::lossless()
            .with_alpha(AlphaPolicy::CompositeOver)
            .with_background([0; 3]);
        assert_eq!(
            convert_from_raw::<_, Rgb16>(src, policy)
                .unwrap()
                .as_samples(),
            &[1, 1, 1]
        );
    }

    #[test]
    fn reducing_colour_needs_a_luma_policy_and_honours_the_standard() {
        let src = raw(&[255u8, 0, 0], PixelFormat::Rgb8);
        assert_eq!(
            convert_from_raw::<_, Gray8>(src, ConvertPolicy::lossless())
                .unwrap_err()
                .kind(),
            ErrorKind::Unsupported
        );

        // Pure red weighs 0.299 under BT.601 and 0.2126 under BT.709 -- the two must not agree,
        // otherwise the policy is being ignored.
        let bt601 = convert_from_raw::<_, Gray8>(
            src,
            ConvertPolicy::lossless().with_luma(LumaPolicy::Bt601),
        )
        .unwrap();
        let bt709 = convert_from_raw::<_, Gray8>(
            src,
            ConvertPolicy::lossless().with_luma(LumaPolicy::Bt709),
        )
        .unwrap();
        assert_eq!(bt601.as_samples(), &[76]); // round(0.299 * 255)
        assert_eq!(bt709.as_samples(), &[54]); // round(0.2126 * 255)
        assert_ne!(bt601.as_samples(), bt709.as_samples());
    }

    #[test]
    fn luma_rounds_to_nearest_rather_than_truncating() {
        // BT.709 red weight is 13933, so an input of 3 sums to 41799 -- less than one full unit
        // (65536) but more than half of it. Rounding gives 1, truncation gives 0. A 16-bit target
        // keeps the difference visible instead of re-quantising it away.
        let src = raw(&[3u16, 0, 0], PixelFormat::Rgb16);
        let policy = ConvertPolicy::lossless().with_luma(LumaPolicy::Bt709);
        assert_eq!(
            convert_from_raw::<_, Gray16>(src, policy)
                .unwrap()
                .as_samples(),
            &[1]
        );
    }

    #[test]
    fn a_grey_source_reaches_a_grey_target_unchanged() {
        // The luma reduction runs unconditionally for a grey target, so every value must survive it
        // bit-exactly -- otherwise GrayAlpha8 -> Gray8 would drift.
        let policy = ConvertPolicy::lossless().with_alpha(AlphaPolicy::Drop);
        for value in 0..=u8::MAX {
            let samples = [value, 255];
            let src = raw(&samples, PixelFormat::GrayAlpha8);
            let out = convert_from_raw::<_, Gray8>(src, policy).unwrap();
            assert_eq!(out.as_samples(), &[value]);
        }
    }

    #[test]
    fn narrowing_samples_needs_a_depth_policy() {
        let src = raw(&[0xFFFFu16, 0x8080, 0], PixelFormat::Rgb16);
        assert_eq!(
            convert_from_raw::<_, Rgb8>(src, ConvertPolicy::lossless())
                .unwrap_err()
                .kind(),
            ErrorKind::Unsupported
        );
        let narrowed = convert_from_raw::<_, Rgb8>(
            src,
            ConvertPolicy::lossless().with_depth(DepthPolicy::Rescale),
        )
        .unwrap();
        // Full scale must reach full scale: a truncating `>> 8` would give 254 for 0xFFFF.
        assert_eq!(narrowed.as_samples(), &[255, 128, 0]);
    }

    #[test]
    fn bilevel_expands_losslessly_and_thresholds_on_the_way_back() {
        // Both conventions for "white" (1 from a 1-bit decoder, 255 from `write`) must expand.
        for white in [1u8, 255] {
            let samples = [white];
            let src = raw(&samples, PixelFormat::Bilevel);
            let grey = convert_from_raw::<_, Gray8>(src, ConvertPolicy::lossless()).unwrap();
            assert_eq!(grey.as_samples(), &[255]);
        }
        let black = raw(&[0u8], PixelFormat::Bilevel);
        assert_eq!(
            convert_from_raw::<_, Gray8>(black, ConvertPolicy::lossless())
                .unwrap()
                .as_samples(),
            &[0]
        );

        // Going the other way is a precision loss, and the threshold is respected on both sides.
        let mid = raw(&[128u8], PixelFormat::Gray8);
        assert_eq!(
            convert_from_raw::<_, Bilevel>(mid, ConvertPolicy::lossless())
                .unwrap_err()
                .kind(),
            ErrorKind::Unsupported
        );
        let rescale = ConvertPolicy::lossless().with_depth(DepthPolicy::Rescale);
        assert_eq!(
            convert_from_raw::<_, Bilevel>(mid, rescale)
                .unwrap()
                .as_samples(),
            &[255]
        );
        // 128 widens to 0x8080, just above the default midpoint; raising the threshold flips it.
        assert_eq!(
            convert_from_raw::<_, Bilevel>(mid, rescale.with_threshold(0x8081))
                .unwrap()
                .as_samples(),
            &[0]
        );
    }

    #[test]
    fn palette_and_cmyk_convert_only_to_themselves() {
        let strict = ConvertPolicy::lossless();
        let permissive = ConvertPolicy::permissive();

        let indexed = raw(&[5u8], PixelFormat::Indexed8);
        let cmyk = raw(&[1u8, 2, 3, 4], PixelFormat::Cmyk8);

        // Refused in both directions, and not rescued by a permissive policy -- the machinery is
        // missing, so no policy can authorise it.
        for policy in [strict, permissive] {
            assert_eq!(
                convert_from_raw::<_, Rgb8>(indexed, policy)
                    .unwrap_err()
                    .kind(),
                ErrorKind::Unsupported
            );
            assert_eq!(
                convert_from_raw::<_, Rgb8>(cmyk, policy)
                    .unwrap_err()
                    .kind(),
                ErrorKind::Unsupported
            );
            let rgb = raw(&[1u8, 2, 3], PixelFormat::Rgb8);
            assert_eq!(
                convert_from_raw::<_, Cmyk8>(rgb, policy)
                    .unwrap_err()
                    .kind(),
                ErrorKind::Unsupported
            );
            assert_eq!(
                convert_from_raw::<_, Indexed8>(rgb, policy)
                    .unwrap_err()
                    .kind(),
                ErrorKind::Unsupported
            );
        }

        // Each refusal names the machinery it is missing; they are not interchangeable.
        let indexed_err = convert_from_raw::<_, Rgb8>(indexed, strict).unwrap_err();
        let cmyk_err = convert_from_raw::<_, Rgb8>(cmyk, strict).unwrap_err();
        let indexed_msg = indexed_err.static_message().unwrap();
        let cmyk_msg = cmyk_err.static_message().unwrap();
        assert_ne!(indexed_msg, cmyk_msg);
        assert!(indexed_msg.contains("palette"), "{indexed_msg}");
        assert!(cmyk_msg.contains("colour-management"), "{cmyk_msg}");

        // Identity still works: the samples pass straight through.
        assert_eq!(
            convert_from_raw::<_, Indexed8>(indexed, strict)
                .unwrap()
                .as_samples(),
            &[5]
        );
        assert_eq!(
            convert_from_raw::<_, Cmyk8>(cmyk, strict)
                .unwrap()
                .as_samples(),
            &[1, 2, 3, 4]
        );
    }

    #[test]
    fn typed_convert_matches_the_raw_engine() {
        let dimensions = dims(2, 1);
        let rgba = [10u8, 20, 30, 255, 40, 50, 60, 255];
        let image = ImageRef::<Rgba8>::new(&rgba, dimensions).unwrap();
        let policy = ConvertPolicy::lossless().with_alpha(AlphaPolicy::Drop);

        let typed = convert::<Rgba8, Rgb8>(image, policy).unwrap();
        let via_raw = convert_from_raw::<_, Rgb8>(
            RawImage::new(&rgba, PixelFormat::Rgba8, dimensions).unwrap(),
            policy,
        )
        .unwrap();
        assert_eq!(typed, via_raw);
        assert_eq!(typed.as_samples(), &[10, 20, 30, 40, 50, 60]);
        assert_eq!(typed.dimensions(), dimensions);
    }

    #[test]
    fn convert_into_reuses_storage_and_validates_length() {
        let dimensions = dims(2, 1);
        let grey = [1u8, 2];
        let src = RawImage::new(&grey, PixelFormat::Gray8, dimensions).unwrap();

        let mut dst = ImageBuf::<Rgb8>::zeroed(dimensions).unwrap();
        convert_from_raw_into::<_, Rgb8>(src, ConvertPolicy::lossless(), dst.as_mut_samples())
            .unwrap();
        assert_eq!(dst.as_samples(), &[1, 1, 1, 2, 2, 2]);

        // A wrongly sized destination is rejected before anything is written.
        let mut short = [0u8; 5];
        let err = convert_from_raw_into::<_, Rgb8>(src, ConvertPolicy::lossless(), &mut short)
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(short, [0; 5]);

        // A refused conversion leaves the destination untouched too.
        let alpha_src = RawImage::new(&[1u8, 2, 3, 4], PixelFormat::Rgba8, dims(1, 1)).unwrap();
        let mut target = [9u8; 3];
        assert!(
            convert_from_raw_into::<_, Rgb8>(alpha_src, ConvertPolicy::lossless(), &mut target)
                .is_err()
        );
        assert_eq!(target, [9; 3]);
    }

    #[test]
    fn raw_image_validates_its_description() {
        let samples = [0u8; 6];
        assert!(RawImage::new(&samples, PixelFormat::Rgb8, dims(2, 1)).is_ok());
        // Wrong sample count.
        assert!(RawImage::new(&samples, PixelFormat::Rgb8, dims(3, 1)).is_err());
        // Zero-sized.
        assert!(RawImage::new(&[] as &[u8], PixelFormat::Rgb8, dims(0, 1)).is_err());
        // Sample width disagrees with the format: u8 samples described as a 16-bit layout.
        assert!(RawImage::new(&samples, PixelFormat::Rgb16, dims(2, 1)).is_err());
        assert!(RawImage::new(&[0u16; 6], PixelFormat::Rgb8, dims(2, 1)).is_err());
        // Accessors report what was validated.
        let image = RawImage::new(&samples, PixelFormat::Rgb8, dims(2, 1)).unwrap();
        assert_eq!(image.format(), PixelFormat::Rgb8);
        assert_eq!(image.dimensions(), dims(2, 1));
        assert_eq!(image.as_samples().len(), 6);
    }

    /// One `RawImage` per format, sized for a single pixel, so the matrix test below can drive
    /// every source layout without eleven hand-written buffers.
    fn sample_pixel(format: PixelFormat) -> Vec<u8> {
        (0..format.channels()).map(|i| (i as u8 + 1) * 17).collect()
    }

    /// Whether `src -> dst` should succeed under `permissive`, derived independently of `Plan` so
    /// the test states the contract rather than restating the implementation.
    fn expected_supported(src: PixelFormat, dst: PixelFormat) -> bool {
        let opaque =
            |f: PixelFormat| matches!(f.color_model(), ColorModel::Cmyk | ColorModel::Indexed);
        if opaque(src) || opaque(dst) {
            return src == dst;
        }
        true
    }

    #[test]
    fn every_layout_pair_is_either_supported_or_refused() {
        // Drives all 11 x 11 = 121 pairs. Under `permissive` every pair must succeed except those
        // involving the two layouts this module deliberately excludes; under `lossless` a pair may
        // additionally refuse, but must never panic or produce a wrongly sized buffer.
        for &src in PixelFormat::ALL {
            for &dst in PixelFormat::ALL {
                let bytes = sample_pixel(src);
                let result = if src.bytes_per_sample() == 1 {
                    run_pair(RawImage::new(&bytes, src, dims(1, 1)).unwrap(), dst)
                } else {
                    let widened: Vec<u16> = bytes.iter().map(|&b| u16::from(b) * 257).collect();
                    run_pair(RawImage::new(&widened, src, dims(1, 1)).unwrap(), dst)
                };
                assert_eq!(
                    result.is_ok(),
                    expected_supported(src, dst),
                    "{src:?} -> {dst:?} disagreed with the documented contract"
                );
                if let Ok(len) = result {
                    assert_eq!(
                        len,
                        dst.channels(),
                        "{src:?} -> {dst:?} wrote a short pixel"
                    );
                }
            }
        }
    }

    /// Converts `src` to the runtime-selected `dst` under `permissive`, reporting the produced
    /// sample count. The `match` is the one place the runtime tag is turned back into a brand.
    fn run_pair<S: Sample>(src: RawImage<'_, S>, dst: PixelFormat) -> Result<usize> {
        let policy = ConvertPolicy::permissive();
        macro_rules! go {
            ($marker:ty) => {
                convert_from_raw::<S, $marker>(src, policy).map(|b| b.as_samples().len())
            };
        }
        match dst {
            PixelFormat::Gray8 => go!(Gray8),
            PixelFormat::Bilevel => go!(Bilevel),
            PixelFormat::Indexed8 => go!(Indexed8),
            PixelFormat::Rgb8 => go!(Rgb8),
            PixelFormat::Rgba8 => go!(Rgba8),
            PixelFormat::Cmyk8 => go!(Cmyk8),
            PixelFormat::GrayAlpha8 => go!(GrayAlpha8),
            PixelFormat::Gray16 => go!(Gray16),
            PixelFormat::Rgb16 => go!(Rgb16),
            PixelFormat::Rgba16 => go!(Rgba16),
            PixelFormat::GrayAlpha16 => go!(GrayAlpha16),
        }
    }

    #[test]
    fn policy_defaults_and_setters_are_independent() {
        assert_eq!(ConvertPolicy::default(), ConvertPolicy::lossless());
        assert_ne!(ConvertPolicy::lossless(), ConvertPolicy::permissive());

        // Each setter must change only its own field; a shared assignment would be invisible to a
        // test that only checked one of them.
        let base = ConvertPolicy::lossless();
        assert_eq!(
            base.with_alpha(AlphaPolicy::Drop).depth,
            DepthPolicy::Reject
        );
        assert_eq!(
            base.with_depth(DepthPolicy::Rescale).alpha,
            AlphaPolicy::Reject
        );
        assert_eq!(base.with_luma(LumaPolicy::Bt601).luma, LumaPolicy::Bt601);
        assert_eq!(base.with_background([1, 2, 3]).background, [1, 2, 3]);
        assert_eq!(base.with_threshold(7).threshold, 7);
        assert_eq!(base.with_threshold(7).background, [0; 3]);
    }

    #[test]
    fn luma_policies_select_distinct_weights() {
        assert_eq!(LumaPolicy::Reject.weights(), None);
        assert_eq!(LumaPolicy::Bt601.weights(), Some(BT601_LUMA_WEIGHTS));
        assert_eq!(LumaPolicy::Bt709.weights(), Some(BT709_LUMA_WEIGHTS));
        assert_eq!(LumaPolicy::Bt2020.weights(), Some(BT2020_LUMA_WEIGHTS));
    }
}
