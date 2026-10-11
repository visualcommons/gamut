//! Executable laws for the layout-conversion engine, shared by the property tests and the fuzz
//! tier.
//!
//! Each function takes plain data, checks one law, and returns a [`Violation`] naming what broke
//! it. No test framework is involved and nothing here panics, so the same body runs under a
//! pinned-seed `proptest` in the per-PR gate and under a corpus-guided driver in extended CI
//! (issues #264, #311).
//!
//! `convert` is the largest body of algorithmic content in the workspace with **no oracle
//! anywhere** — no reference implementation converts gamut's pixel matrix, and no specification
//! ships vectors for it — so `docs/testing.md` names `gamut-core` a crate where a property is the
//! primary signal rather than a supplement. The laws here are the module's own documented
//! contract: which conversions are refused, what shape the output has, and the two narrowings the
//! documentation calls exact inverses of their widenings.
//!
//! Note that these laws are stated over [`convert_from_raw`], not [`convert`]. The typed door is a
//! one-line delegation to the raw one, so a law asserting the two agree would be checking the
//! compiler rather than the engine.
//!
//! This module is the test oracle, not the system under test, so `.cargo/mutants.toml` excludes
//! `crates/*/src/invariants.rs` from mutation.

use crate::convert::{
    ConvertPolicy, RawImage, convert_from_raw, convert_from_raw_into, premultiply_alpha,
    unpremultiply_alpha,
};
use crate::{ColorModel, Dimensions, ErrorKind, ImageBuf, Pixel, PixelFormat, Sample};

/// The largest edge a normalised dimension can take.
///
/// Conversion is uniform in the pixel count — it is a per-pixel loop — so a bounded universe costs
/// no generality while keeping every case allocation-bounded for a fuzz driver.
pub const MAX_EDGE: u32 = 32;

/// Every 8-bit-sampled layout in the [`Pixel`] matrix.
///
/// Shared by both tiers that drive these laws — the pinned-seed properties below and the
/// out-of-tree fuzz target — so they search the same matrix rather than two lists that can drift.
pub const NARROW_FORMATS: [PixelFormat; 7] = [
    PixelFormat::Gray8,
    PixelFormat::Bilevel,
    PixelFormat::Indexed8,
    PixelFormat::Rgb8,
    PixelFormat::Rgba8,
    PixelFormat::Cmyk8,
    PixelFormat::GrayAlpha8,
];

/// Every 16-bit-sampled layout in the [`Pixel`] matrix. Companion to [`NARROW_FORMATS`].
pub const WIDE_FORMATS: [PixelFormat; 4] = [
    PixelFormat::Gray16,
    PixelFormat::Rgb16,
    PixelFormat::Rgba16,
    PixelFormat::GrayAlpha16,
];

/// Maps arbitrary bits to a dimension pair inside [`MAX_EDGE`], never zero.
///
/// Zero-sized images are a separate concern with their own refusal path in [`Dimensions`], and a
/// law about per-pixel conversion has nothing to say about a buffer with no pixels.
#[must_use]
pub fn normalise_dims(width: u32, height: u32) -> (u32, u32) {
    (width % MAX_EDGE + 1, height % MAX_EDGE + 1)
}

/// A law that did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    law: &'static str,
    detail: String,
}

impl Violation {
    /// Builds a violation of `law`.
    fn new(law: &'static str, detail: String) -> Self {
        Self { law, detail }
    }

    /// The name of the law that was broken.
    #[must_use]
    pub fn law(&self) -> &'static str {
        self.law
    }

    /// What broke it, in terms the failing case can be read from.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl core::fmt::Display for Violation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.law, self.detail)
    }
}

/// Law: whether a conversion is accepted depends on the layouts and the policy, never on the
/// sample values.
///
/// This is the load-bearing claim of the whole module: the policy is what decides, and it decides
/// up front. `convert_from_raw` is written to honour it — `Plan::derive` runs before any
/// allocation precisely so an unsupported pair costs nothing — and a defect that made acceptance
/// depend on the data would turn a caller's `lossless` guarantee into "lossless for the images we
/// happened to test".
///
/// # Errors
///
/// Returns a violation when the two sample buffers, identical in layout and length, disagree on
/// whether the conversion is accepted.
pub fn acceptance_is_independent_of_the_samples<S: Sample, Q: Pixel>(
    format: PixelFormat,
    dims: Dimensions,
    first: &[S],
    second: &[S],
    policy: ConvertPolicy,
) -> Result<(), Violation> {
    const LAW: &str = "acceptance_is_independent_of_the_samples";

    let (Ok(a), Ok(b)) = (
        RawImage::<S>::new(first, format, dims),
        RawImage::<S>::new(second, format, dims),
    ) else {
        // Both buffers have the same length and layout, so either both build or neither does;
        // a rejected pair says nothing about the conversion engine.
        return Ok(());
    };

    let first_ok = convert_from_raw::<S, Q>(a, policy).is_ok();
    let second_ok = convert_from_raw::<S, Q>(b, policy).is_ok();

    if first_ok == second_ok {
        Ok(())
    } else {
        Err(Violation::new(
            LAW,
            format!(
                "{format:?} -> {:?}: one sample buffer was accepted and another refused",
                Q::FORMAT
            ),
        ))
    }
}

/// Law: an accepted conversion produces exactly `width * height * Q::CHANNELS` samples and keeps
/// the source's dimensions.
///
/// Conversion rearranges channels; it never resamples. A defect in the per-pixel stride arithmetic
/// shows up here as a buffer of the wrong length before it shows up as wrong colour.
///
/// # Errors
///
/// Returns a violation when the output's dimensions or sample count do not match the source's
/// dimensions under the target layout.
pub fn output_shape_matches_the_target_layout<S: Sample, Q: Pixel>(
    format: PixelFormat,
    dims: Dimensions,
    samples: &[S],
    policy: ConvertPolicy,
) -> Result<(), Violation> {
    const LAW: &str = "output_shape_matches_the_target_layout";

    let Ok(src) = RawImage::<S>::new(samples, format, dims) else {
        return Ok(());
    };
    let Ok(out) = convert_from_raw::<S, Q>(src, policy) else {
        // A refused conversion has no output to check; refusal is another law's business.
        return Ok(());
    };

    if out.dimensions() != dims {
        return Err(Violation::new(
            LAW,
            format!(
                "{format:?} -> {:?}: dimensions {:?} became {:?}",
                Q::FORMAT,
                dims,
                out.dimensions()
            ),
        ));
    }

    let want = dims
        .sample_count(Q::CHANNELS)
        .expect("dimensions are bounded by MAX_EDGE, so the product cannot overflow");
    if out.as_samples().len() != want {
        return Err(Violation::new(
            LAW,
            format!(
                "{format:?} -> {:?}: expected {want} samples, got {}",
                Q::FORMAT,
                out.as_samples().len()
            ),
        ));
    }
    Ok(())
}

/// Law: converting a layout to itself is accepted under any policy and changes nothing.
///
/// A same-layout conversion incurs no loss, so no policy has anything to permit — including
/// [`ConvertPolicy::lossless`], which permits nothing. This covers the whole matrix, `Indexed8`
/// and `Cmyk8` included: those two convert *only* to themselves, and this is the case that must
/// still work.
///
/// # Errors
///
/// Returns a violation when a same-layout conversion is refused, or returns samples that differ
/// from the input.
pub fn converting_a_layout_to_itself_changes_nothing<P: Pixel>(
    dims: Dimensions,
    samples: &[P::Sample],
    policy: ConvertPolicy,
) -> Result<(), Violation> {
    const LAW: &str = "converting_a_layout_to_itself_changes_nothing";

    let Ok(src) = RawImage::<P::Sample>::new(samples, P::FORMAT, dims) else {
        return Ok(());
    };

    let out = match convert_from_raw::<P::Sample, P>(src, policy) {
        Ok(out) => out,
        Err(error) => {
            return Err(Violation::new(
                LAW,
                format!("{:?} -> itself was refused: {error}", P::FORMAT),
            ));
        }
    };

    if out.as_samples() == samples {
        Ok(())
    } else {
        Err(Violation::new(
            LAW,
            format!("{:?} -> itself altered the samples", P::FORMAT),
        ))
    }
}

/// Law: a conversion into or out of `Indexed8` or `Cmyk8` is refused, whatever the policy permits.
///
/// The module documents this as absolute rather than policy-dependent: palette indices are
/// meaningless without the table, and CMYK is a colour-management transform, not a layout
/// rearrangement. No `ConvertPolicy` can authorise either, so a permissive policy must not open a
/// door a lossless one closes. The refusal must also be [`ErrorKind::Unsupported`] specifically —
/// that is the code a caller matches on to fall back.
///
/// # Errors
///
/// Returns a violation when such a pair is accepted, or refused with the wrong error kind.
pub fn palette_and_cmyk_convert_only_to_themselves<S: Sample, Q: Pixel>(
    format: PixelFormat,
    dims: Dimensions,
    samples: &[S],
    policy: ConvertPolicy,
) -> Result<(), Violation> {
    const LAW: &str = "palette_and_cmyk_convert_only_to_themselves";

    let closed = |f: PixelFormat| matches!(f, PixelFormat::Indexed8 | PixelFormat::Cmyk8);
    if !(closed(format) || closed(Q::FORMAT)) || format == Q::FORMAT {
        return Ok(());
    }

    let Ok(src) = RawImage::<S>::new(samples, format, dims) else {
        return Ok(());
    };

    match convert_from_raw::<S, Q>(src, policy) {
        Ok(_) => Err(Violation::new(
            LAW,
            format!("{format:?} -> {:?} was accepted", Q::FORMAT),
        )),
        Err(error) if error.kind() == ErrorKind::Unsupported => Ok(()),
        Err(error) => Err(Violation::new(
            LAW,
            format!(
                "{format:?} -> {:?} was refused as {:?}, not Unsupported",
                Q::FORMAT,
                error.kind()
            ),
        )),
    }
}

/// Law: [`convert_from_raw_into`] writes exactly what [`convert_from_raw`] allocates.
///
/// The two doors share `Plan::derive` and `run`, but the in-place one additionally validates the
/// destination length and writes through a borrow the caller owns. They must not diverge: a
/// decoder calling `decode_image_into` to avoid an allocation must get the same image as one that
/// let the allocation happen.
///
/// # Errors
///
/// Returns a violation when the two doors disagree on acceptance, or produce different samples.
pub fn the_in_place_door_matches_the_allocating_door<S: Sample, Q: Pixel>(
    format: PixelFormat,
    dims: Dimensions,
    samples: &[S],
    policy: ConvertPolicy,
) -> Result<(), Violation> {
    const LAW: &str = "the_in_place_door_matches_the_allocating_door";

    let (Ok(a), Ok(b)) = (
        RawImage::<S>::new(samples, format, dims),
        RawImage::<S>::new(samples, format, dims),
    ) else {
        return Ok(());
    };

    let allocated = convert_from_raw::<S, Q>(a, policy);
    let Some(want) = dims.sample_count(Q::CHANNELS) else {
        return Ok(());
    };
    let mut storage: Vec<Q::Sample> = vec![Q::Sample::default(); want];
    let in_place = convert_from_raw_into::<S, Q>(b, policy, &mut storage);

    match (allocated, in_place) {
        (Ok(out), Ok(())) => {
            if out.as_samples() == storage.as_slice() {
                Ok(())
            } else {
                Err(Violation::new(
                    LAW,
                    format!(
                        "{format:?} -> {:?}: the two doors wrote different samples",
                        Q::FORMAT
                    ),
                ))
            }
        }
        (Err(_), Err(_)) => Ok(()),
        (Ok(_), Err(error)) => Err(Violation::new(
            LAW,
            format!(
                "{format:?} -> {:?}: the allocating door accepted, the in-place one refused: {error}",
                Q::FORMAT
            ),
        )),
        (Err(error), Ok(())) => Err(Violation::new(
            LAW,
            format!(
                "{format:?} -> {:?}: the in-place door accepted, the allocating one refused: {error}",
                Q::FORMAT
            ),
        )),
    }
}

/// Law: widening 8-bit samples to 16 and rescaling them back recovers the original exactly.
///
/// The module documents the narrowing as "the exact inverse of the widening PNG specifies in
/// §13.12 — not a truncating shift", so this is a claim the code makes about itself and the one
/// place a round-trip is the right shape here: the claim *is* mutual inversion. A truncating
/// shift, or a rounding rule that is off by one anywhere, breaks it for some byte.
///
/// # Errors
///
/// Returns the first sample that did not survive the round trip.
pub fn widening_to_16_bit_and_back_is_exact<
    Narrow: Pixel<Sample = u8>,
    Wide: Pixel<Sample = u16>,
>(
    dims: Dimensions,
    samples: &[u8],
    policy: ConvertPolicy,
) -> Result<(), Violation> {
    const LAW: &str = "widening_to_16_bit_and_back_is_exact";

    let Ok(src) = RawImage::<u8>::new(samples, Narrow::FORMAT, dims) else {
        return Ok(());
    };
    // Widening needs no permission, so this half must be accepted under any policy.
    let Ok(wide) = convert_from_raw::<u8, Wide>(src, policy) else {
        return Ok(());
    };
    let Ok(back) = RawImage::<u16>::new(wide.as_samples(), Wide::FORMAT, dims) else {
        return Ok(());
    };
    let Ok(narrow) = convert_from_raw::<u16, Narrow>(back, policy) else {
        // Narrowing is lossy and this policy may forbid it; that is the policy's business.
        return Ok(());
    };

    for (index, (&got, &want)) in narrow.as_samples().iter().zip(samples.iter()).enumerate() {
        if got != want {
            return Err(Violation::new(
                LAW,
                format!(
                    "{:?} -> {:?} -> {:?}: sample {index} was {want}, came back {got}",
                    Narrow::FORMAT,
                    Wide::FORMAT,
                    Narrow::FORMAT
                ),
            ));
        }
    }
    Ok(())
}

// ---- associated alpha ----------------------------------------------------------------------
//
// `premultiply_alpha` / `unpremultiply_alpha` have no oracle either: the competitor that prompted
// them (ravif's `convert_alpha_8bit`) is defective, so it is a counter-example, not a reference.
// The laws below state the arithmetic over the *rationals* — "the nearest integer to c·a/max" —
// rather than restating the integer formula the implementation evaluates, so a wrong rounding
// addend, a truncation, or a missing clamp is visible as a broken inequality.

/// The full scale of `S` as a code value, decided from its width rather than through `convert`'s
/// own helper so a defect there cannot cancel out here.
fn alpha_code_max<S: Sample>() -> u64 {
    match core::mem::size_of::<S>() {
        1 => 255,
        _ => 65_535,
    }
}

/// A sample's code value: its full-range widening divided by the widening factor (257 or 1).
fn alpha_code<S: Sample>(sample: S) -> u64 {
    u64::from(sample.to_full_range_u16()) / (65_535 / alpha_code_max::<S>())
}

/// One pixel's colour and alpha code values: `(colours, alpha)`, alpha being the last channel.
fn split_pixel<S: Sample>(pixel: &[S]) -> (Vec<u64>, u64) {
    let (colour, alpha) = pixel.split_at(pixel.len() - 1);
    (
        colour.iter().map(|&s| alpha_code(s)).collect(),
        alpha_code(alpha[0]),
    )
}

/// Builds the owned image these laws operate on, or `None` when the description is not an image of
/// an alpha-carrying layout (which no alpha law has anything to say about).
fn alpha_image<P: Pixel>(dims: Dimensions, samples: &[P::Sample]) -> Option<ImageBuf<P>> {
    if !matches!(
        P::FORMAT.color_model(),
        ColorModel::Rgba | ColorModel::GrayAlpha
    ) {
        return None;
    }
    ImageBuf::<P>::new(samples.to_vec(), dims).ok()
}

/// Law: a fully opaque pixel is a fixed point of both [`premultiply_alpha`] and
/// [`unpremultiply_alpha`].
///
/// The issue that introduced the operations makes this the first acceptance criterion, and it is
/// where ravif's premultiplication fails outright (it maps `a == 255` to transparent black).
///
/// # Errors
///
/// Returns the first opaque pixel either operation altered.
pub fn opaque_pixels_are_fixed_points<P: Pixel>(
    dims: Dimensions,
    samples: &[P::Sample],
) -> Result<(), Violation> {
    const LAW: &str = "opaque_pixels_are_fixed_points";

    let Some(original) = alpha_image::<P>(dims, samples) else {
        return Ok(());
    };
    let max = alpha_code_max::<P::Sample>();
    let mut premultiplied = original.clone();
    premultiply_alpha(&mut premultiplied);
    let mut unpremultiplied = original.clone();
    unpremultiply_alpha(&mut unpremultiplied);

    let pixels = original.as_samples().chunks_exact(P::CHANNELS);
    let forward = premultiplied.as_samples().chunks_exact(P::CHANNELS);
    let backward = unpremultiplied.as_samples().chunks_exact(P::CHANNELS);
    for (index, ((source, there), back)) in pixels.zip(forward).zip(backward).enumerate() {
        if split_pixel(source).1 != max {
            continue;
        }
        if there != source || back != source {
            return Err(Violation::new(
                LAW,
                format!(
                    "{:?} pixel {index} {source:?}: premultiplied to {there:?}, unpremultiplied to {back:?}",
                    P::FORMAT
                ),
            ));
        }
    }
    Ok(())
}

/// Law: [`premultiply_alpha`] keeps alpha and replaces each colour code `c` with the integer
/// nearest `c · a / max`.
///
/// `max` is odd at both widths, so `c · a / max` is never half-way and "nearest" is unique:
/// `|p · max − c · a| ≤ max / 2`. That also bounds associated colour by its alpha (`p ≤ a`), the
/// well-formedness condition of associated alpha, and sends a transparent pixel to black.
///
/// # Errors
///
/// Returns the first sample that is not the nearest integer, or whose alpha changed.
pub fn premultiply_rounds_to_nearest<P: Pixel>(
    dims: Dimensions,
    samples: &[P::Sample],
) -> Result<(), Violation> {
    const LAW: &str = "premultiply_rounds_to_nearest";

    let Some(original) = alpha_image::<P>(dims, samples) else {
        return Ok(());
    };
    let max = alpha_code_max::<P::Sample>();
    let mut premultiplied = original.clone();
    premultiply_alpha(&mut premultiplied);

    let pixels = original.as_samples().chunks_exact(P::CHANNELS);
    let results = premultiplied.as_samples().chunks_exact(P::CHANNELS);
    for (index, (source, result)) in pixels.zip(results).enumerate() {
        let (colours, alpha) = split_pixel(source);
        let (associated, kept_alpha) = split_pixel(result);
        let nearest = colours
            .iter()
            .zip(&associated)
            .all(|(&c, &p)| 2 * (p * max).abs_diff(c * alpha) <= max);
        if kept_alpha != alpha || !nearest {
            return Err(Violation::new(
                LAW,
                format!(
                    "{:?} pixel {index} {source:?} premultiplied to {result:?}",
                    P::FORMAT
                ),
            ));
        }
    }
    Ok(())
}

/// Law: [`unpremultiply_alpha`] keeps alpha and replaces each colour code `p` with the integer
/// nearest `p · max / a`, clamped to `max`; a transparent pixel becomes black.
///
/// Stated for **arbitrary** associated input, not only what [`premultiply_alpha`] produces: a
/// reader meets colour above its alpha in real files, and the clamp is what keeps it in range.
///
/// # Errors
///
/// Returns the first sample that is neither the nearest integer nor the clamp, or whose alpha
/// changed.
pub fn unpremultiply_rounds_to_nearest_or_clamps<P: Pixel>(
    dims: Dimensions,
    samples: &[P::Sample],
) -> Result<(), Violation> {
    const LAW: &str = "unpremultiply_rounds_to_nearest_or_clamps";

    let Some(original) = alpha_image::<P>(dims, samples) else {
        return Ok(());
    };
    let max = alpha_code_max::<P::Sample>();
    let mut unpremultiplied = original.clone();
    unpremultiply_alpha(&mut unpremultiplied);

    let pixels = original.as_samples().chunks_exact(P::CHANNELS);
    let results = unpremultiplied.as_samples().chunks_exact(P::CHANNELS);
    for (index, (source, result)) in pixels.zip(results).enumerate() {
        let (associated, alpha) = split_pixel(source);
        let (colours, kept_alpha) = split_pixel(result);
        let lawful = associated.iter().zip(&colours).all(|(&p, &c)| {
            if alpha == 0 {
                c == 0
            } else if c == max {
                // Clamped, or genuinely nearest to full scale: p·max/a ≥ max − ½.
                2 * p * max >= (2 * max - 1) * alpha
            } else {
                2 * (c * alpha).abs_diff(p * max) <= alpha
            }
        });
        if kept_alpha != alpha || !lawful {
            return Err(Violation::new(
                LAW,
                format!(
                    "{:?} pixel {index} {source:?} unpremultiplied to {result:?}",
                    P::FORMAT
                ),
            ));
        }
    }
    Ok(())
}

/// Law: unpremultiplying a premultiplied image recovers each colour code `c` of a pixel with alpha
/// `a > 0` to within `⌊(a + max) / 2a⌋`, and re-premultiplying that result reproduces the
/// premultiplied image exactly.
///
/// The bound is the quantisation premultiplying incurs: the associated value is off by at most ½,
/// which dividing by `a / max` magnifies to `max / 2a`, plus ½ for the final rounding. It is one
/// code value only where `a` is at least about half of full scale; at `a = 1` an 8-bit colour keeps
/// one bit, and nothing can recover the rest. The second half is exact because unpremultiplying
/// errs by at most ½ on the unassociated scale, which `a / max ≤ 1` shrinks below ½ again.
///
/// # Errors
///
/// Returns the first pixel that breaks either half.
pub fn premultiply_round_trip_is_bounded_and_idempotent<P: Pixel>(
    dims: Dimensions,
    samples: &[P::Sample],
) -> Result<(), Violation> {
    const LAW: &str = "premultiply_round_trip_is_bounded_and_idempotent";

    let Some(original) = alpha_image::<P>(dims, samples) else {
        return Ok(());
    };
    let max = alpha_code_max::<P::Sample>();
    let mut associated = original.clone();
    premultiply_alpha(&mut associated);
    let mut recovered = associated.clone();
    unpremultiply_alpha(&mut recovered);
    let mut again = recovered.clone();
    premultiply_alpha(&mut again);

    let pixels = original.as_samples().chunks_exact(P::CHANNELS);
    let backs = recovered.as_samples().chunks_exact(P::CHANNELS);
    for (index, (source, back)) in pixels.zip(backs).enumerate() {
        let (colours, alpha) = split_pixel(source);
        let (recovered_colours, _) = split_pixel(back);
        let bounded = alpha == 0
            || colours
                .iter()
                .zip(&recovered_colours)
                .all(|(&c, &r)| c.abs_diff(r) <= (alpha + max) / (2 * alpha));
        if !bounded {
            return Err(Violation::new(
                LAW,
                format!(
                    "{:?} pixel {index} {source:?} came back as {back:?}",
                    P::FORMAT
                ),
            ));
        }
    }
    if again != associated {
        return Err(Violation::new(
            LAW,
            format!(
                "{:?}: re-premultiplying the recovered image did not reproduce the associated one",
                P::FORMAT
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{Just, Strategy, prop_assert, prop_oneof, proptest};
    use proptest::test_runner::{Config, RngSeed};

    use super::{
        MAX_EDGE, NARROW_FORMATS, Violation, WIDE_FORMATS,
        acceptance_is_independent_of_the_samples, alpha_image,
        converting_a_layout_to_itself_changes_nothing, normalise_dims,
        opaque_pixels_are_fixed_points, output_shape_matches_the_target_layout,
        palette_and_cmyk_convert_only_to_themselves,
        premultiply_round_trip_is_bounded_and_idempotent, premultiply_rounds_to_nearest,
        split_pixel, the_in_place_door_matches_the_allocating_door,
        unpremultiply_rounds_to_nearest_or_clamps, widening_to_16_bit_and_back_is_exact,
    };
    use crate::convert::{AlphaPolicy, ConvertPolicy, DepthPolicy, LumaPolicy};
    use crate::{
        Bilevel, Cmyk8, Dimensions, Gray8, Gray16, GrayAlpha8, GrayAlpha16, Indexed8, Pixel,
        PixelFormat, Rgb8, Rgb16, Rgba8, Rgba16,
    };

    /// The workspace property-test configuration (`docs/testing.md`).
    ///
    /// The seed is **pinned**: the `--in-diff` mutation gate is blocking, so a test whose
    /// pass/fail depends on OS entropy would make cargo-mutants report CAUGHT or MISSED for the
    /// same mutant on different runs. Shrink bounds are finite because proptest's defaults are
    /// `u32::MAX` iterations and no wall-clock cap, and failure persistence is off because
    /// cargo-mutants reuses one tree copy across mutants, so a `proptest-regressions` file would
    /// leak into the next mutant's run and be replayed first.
    ///
    /// `cases` is 256 rather than the 512 `gamut-ifd` uses because each case here sweeps all
    /// seven 8-bit target layouts, so one case is already seven conversions.
    fn config() -> Config {
        Config {
            cases: 256,
            max_shrink_iters: 2048,
            max_shrink_time: 10_000,
            rng_seed: RngSeed::Fixed(0x6761_6D75_745F_636F),
            failure_persistence: None,
            ..Config::default()
        }
    }

    /// Runs a two-parameter law for every 8-bit target layout, stopping at the first violation.
    ///
    /// One property then covers the target axis of the matrix instead of needing one property per
    /// layout pair, which is the difference between 7 properties and 49.
    macro_rules! for_each_narrow_target {
        ($law:ident, $($arg:expr),* $(,)?) => {{
            let mut result: Result<(), Violation> = Ok(());
            if result.is_ok() { result = $law::<u8, Gray8>($($arg),*); }
            if result.is_ok() { result = $law::<u8, Bilevel>($($arg),*); }
            if result.is_ok() { result = $law::<u8, Indexed8>($($arg),*); }
            if result.is_ok() { result = $law::<u8, Rgb8>($($arg),*); }
            if result.is_ok() { result = $law::<u8, Rgba8>($($arg),*); }
            if result.is_ok() { result = $law::<u8, Cmyk8>($($arg),*); }
            if result.is_ok() { result = $law::<u8, GrayAlpha8>($($arg),*); }
            result
        }};
    }

    /// Dimensions inside [`MAX_EDGE`].
    ///
    /// Normalising here rather than inside the laws is deliberate: a law that normalised its own
    /// inputs would feed the same value to the model and to the code under test, so a defect in
    /// the normaliser would cancel on both sides.
    fn dimensions() -> impl Strategy<Value = Dimensions> {
        (0..MAX_EDGE, 0..MAX_EDGE).prop_map(|(w, h)| {
            let (width, height) = normalise_dims(w, h);
            Dimensions::new(width, height).expect("normalise_dims yields a non-zero edge")
        })
    }

    /// A policy drawn across the whole lattice, so acceptance is exercised in both directions.
    ///
    /// The rejecting variants are drawn as often as the permitting ones on purpose: refusal is
    /// half the contract, and a policy engine that accepted everything would satisfy any law
    /// phrased only over accepted conversions.
    fn policy() -> impl Strategy<Value = ConvertPolicy> {
        (
            prop_oneof![
                Just(AlphaPolicy::Reject),
                Just(AlphaPolicy::Drop),
                Just(AlphaPolicy::CompositeOver),
            ],
            prop_oneof![Just(DepthPolicy::Reject), Just(DepthPolicy::Rescale)],
            prop_oneof![
                Just(LumaPolicy::Reject),
                Just(LumaPolicy::Bt601),
                Just(LumaPolicy::Bt709),
                Just(LumaPolicy::Bt2020),
            ],
            proptest::array::uniform3(proptest::num::u16::ANY),
            proptest::num::u16::ANY,
        )
            .prop_map(|(alpha, depth, luma, background, threshold)| {
                ConvertPolicy::lossless()
                    .with_alpha(alpha)
                    .with_depth(depth)
                    .with_luma(luma)
                    .with_background(background)
                    .with_threshold(threshold)
            })
    }

    /// Renders a law's result for a proptest failure message.
    fn describe(result: &Result<(), Violation>) -> String {
        match result {
            Ok(()) => String::new(),
            Err(violation) => violation.to_string(),
        }
    }

    /// A deterministic filler for a buffer of `len` 8-bit samples.
    fn filler(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| seed.wrapping_add((i % 251) as u8))
            .collect()
    }

    proptest! {
        #![proptest_config(config())]

        /// Acceptance is decided by the layouts and the policy, never by the pixel values.
        #[test]
        fn acceptance_never_depends_on_the_pixel_values(
            source in proptest::sample::select(NARROW_FORMATS.as_slice()),
            dims in dimensions(),
            policy in policy(),
            seed in proptest::num::u8::ANY,
        ) {
            let len = dims.sample_count(source.channels()).expect("bounded dims");
            let zeros = vec![0u8; len];
            let varied = filler(len, seed);

            let outcome = for_each_narrow_target!(
                acceptance_is_independent_of_the_samples,
                source, dims, &zeros, &varied, policy
            );
            prop_assert!(outcome.is_ok(), "{}", describe(&outcome));
        }

        /// An accepted conversion keeps the dimensions and produces exactly the target layout's
        /// sample count.
        #[test]
        fn an_accepted_conversion_has_the_target_layouts_shape(
            source in proptest::sample::select(NARROW_FORMATS.as_slice()),
            dims in dimensions(),
            policy in policy(),
            seed in proptest::num::u8::ANY,
        ) {
            let len = dims.sample_count(source.channels()).expect("bounded dims");
            let samples = filler(len, seed);

            let outcome = for_each_narrow_target!(
                output_shape_matches_the_target_layout,
                source, dims, &samples, policy
            );
            prop_assert!(outcome.is_ok(), "{}", describe(&outcome));
        }

        /// `Indexed8` and `Cmyk8` convert only to themselves, whatever the policy permits.
        #[test]
        fn palette_and_cmyk_are_closed_under_every_policy(
            source in proptest::sample::select(NARROW_FORMATS.as_slice()),
            dims in dimensions(),
            policy in policy(),
            seed in proptest::num::u8::ANY,
        ) {
            let len = dims.sample_count(source.channels()).expect("bounded dims");
            let samples = filler(len, seed);

            let outcome = for_each_narrow_target!(
                palette_and_cmyk_convert_only_to_themselves,
                source, dims, &samples, policy
            );
            prop_assert!(outcome.is_ok(), "{}", describe(&outcome));
        }

        /// The in-place door writes exactly what the allocating door returns.
        #[test]
        fn both_doors_onto_the_engine_produce_the_same_image(
            source in proptest::sample::select(NARROW_FORMATS.as_slice()),
            dims in dimensions(),
            policy in policy(),
            seed in proptest::num::u8::ANY,
        ) {
            let len = dims.sample_count(source.channels()).expect("bounded dims");
            let samples = filler(len, seed);

            let outcome = for_each_narrow_target!(
                the_in_place_door_matches_the_allocating_door,
                source, dims, &samples, policy
            );
            prop_assert!(outcome.is_ok(), "{}", describe(&outcome));
        }

        /// Converting a layout to itself is accepted under any policy and changes nothing.
        #[test]
        fn every_layout_converts_to_itself_unchanged(
            dims in dimensions(),
            policy in policy(),
            seed in proptest::num::u8::ANY,
        ) {
            macro_rules! identity {
                ($p:ty, $sample:ty) => {{
                    let len = dims
                        .sample_count(<$p as Pixel>::CHANNELS)
                        .expect("bounded dims");
                    let samples: Vec<$sample> = (0..len)
                        .map(|i| <$sample>::from(seed).wrapping_add(i as $sample))
                        .collect();
                    let result =
                        converting_a_layout_to_itself_changes_nothing::<$p>(dims, &samples, policy);
                    prop_assert!(result.is_ok(), "{}", describe(&result));
                }};
            }

            identity!(Gray8, u8);
            identity!(Bilevel, u8);
            identity!(Indexed8, u8);
            identity!(Rgb8, u8);
            identity!(Rgba8, u8);
            identity!(Cmyk8, u8);
            identity!(GrayAlpha8, u8);
            identity!(Gray16, u16);
            identity!(Rgb16, u16);
            identity!(Rgba16, u16);
            identity!(GrayAlpha16, u16);
        }

        /// Widening 8-bit samples to 16 and rescaling back is exact, for each layout with a
        /// 16-bit twin.
        ///
        /// The module documents the narrowing as the exact inverse of PNG §13.12's widening — not
        /// a truncating shift — so this is a claim the code makes about itself, and a rounding
        /// rule off by one anywhere breaks it for some byte.
        #[test]
        fn widening_to_16_bit_and_rescaling_back_recovers_every_sample(
            dims in dimensions(),
            seed in proptest::num::u8::ANY,
        ) {
            // Narrowing needs permission; widening never does.
            let policy = ConvertPolicy::lossless().with_depth(DepthPolicy::Rescale);

            macro_rules! exact {
                ($narrow:ty, $wide:ty) => {{
                    let len = dims
                        .sample_count(<$narrow as Pixel>::CHANNELS)
                        .expect("bounded dims");
                    let samples = filler(len, seed);
                    let result = widening_to_16_bit_and_back_is_exact::<$narrow, $wide>(
                        dims, &samples, policy,
                    );
                    prop_assert!(result.is_ok(), "{}", describe(&result));
                }};
            }

            exact!(Gray8, Gray16);
            exact!(Rgb8, Rgb16);
            exact!(Rgba8, Rgba16);
            exact!(GrayAlpha8, GrayAlpha16);
        }

        /// Every associated-alpha law holds for every alpha-carrying layout, at both widths.
        ///
        /// Alpha is drawn from {0, full scale, anything} in equal parts: uniform 16-bit samples
        /// would almost never be opaque or transparent, which are exactly the pixels the
        /// fixed-point and transparent-black clauses are about.
        #[test]
        fn associated_alpha_laws_hold_for_every_alpha_layout(
            dims in dimensions(),
            colours in proptest::collection::vec(proptest::num::u16::ANY, (MAX_EDGE * MAX_EDGE * 3) as usize),
            alphas in proptest::collection::vec(
                prop_oneof![Just(0u16), Just(u16::MAX), proptest::num::u16::ANY],
                (MAX_EDGE * MAX_EDGE) as usize,
            ),
        ) {
            macro_rules! laws {
                ($p:ty, $sample:ty, $narrow:expr) => {{
                    let pixels = dims.sample_count(1).expect("bounded dims");
                    let colour_channels = <$p as Pixel>::CHANNELS - 1;
                    let mut samples: Vec<$sample> = Vec::new();
                    for pixel in 0..pixels {
                        for channel in 0..colour_channels {
                            samples.push($narrow(colours[pixel * 3 + channel]));
                        }
                        samples.push($narrow(alphas[pixel]));
                    }
                    for result in [
                        opaque_pixels_are_fixed_points::<$p>(dims, &samples),
                        premultiply_rounds_to_nearest::<$p>(dims, &samples),
                        unpremultiply_rounds_to_nearest_or_clamps::<$p>(dims, &samples),
                        premultiply_round_trip_is_bounded_and_idempotent::<$p>(dims, &samples),
                    ] {
                        prop_assert!(result.is_ok(), "{}", describe(&result));
                    }
                }};
            }
            // The high byte keeps 0 and full scale at 0 and 255.
            let narrow = |v: u16| (v >> 8) as u8;
            let wide = |v: u16| v;
            laws!(Rgba8, u8, narrow);
            laws!(GrayAlpha8, u8, narrow);
            laws!(Rgba16, u16, wide);
            laws!(GrayAlpha16, u16, wide);
        }
    }

    /// Every (colour, alpha) pair an 8-bit pixel can hold, once each: 65536 pixels.
    ///
    /// The 8-bit domain is small enough to settle exhaustively rather than sample, so the laws are
    /// proven over it instead of probed. Each colour channel walks the full range in a different
    /// order, so a defect confined to one channel position still meets every pair.
    #[test]
    fn associated_alpha_laws_hold_for_every_8_bit_pair() {
        let dims = Dimensions::new(256, 256).expect("non-zero");
        let mut rgba = Vec::with_capacity(256 * 256 * 4);
        let mut grey_alpha = Vec::with_capacity(256 * 256 * 2);
        for alpha in 0..=u8::MAX {
            for colour in 0..=u8::MAX {
                rgba.extend_from_slice(&[colour, u8::MAX - colour, colour.rotate_left(4), alpha]);
                grey_alpha.extend_from_slice(&[colour, alpha]);
            }
        }

        for result in [
            opaque_pixels_are_fixed_points::<Rgba8>(dims, &rgba),
            premultiply_rounds_to_nearest::<Rgba8>(dims, &rgba),
            unpremultiply_rounds_to_nearest_or_clamps::<Rgba8>(dims, &rgba),
            premultiply_round_trip_is_bounded_and_idempotent::<Rgba8>(dims, &rgba),
            opaque_pixels_are_fixed_points::<GrayAlpha8>(dims, &grey_alpha),
            premultiply_rounds_to_nearest::<GrayAlpha8>(dims, &grey_alpha),
            unpremultiply_rounds_to_nearest_or_clamps::<GrayAlpha8>(dims, &grey_alpha),
            premultiply_round_trip_is_bounded_and_idempotent::<GrayAlpha8>(dims, &grey_alpha),
        ] {
            assert!(result.is_ok(), "{}", describe(&result));
        }
    }

    #[test]
    fn the_alpha_laws_govern_only_alpha_layouts() {
        // A layout without alpha is outside every alpha law, so it passes vacuously...
        let dims = Dimensions::new(1, 1).expect("non-zero");
        assert!(alpha_image::<Rgb8>(dims, &[1, 2, 3]).is_none());
        // ...while an alpha layout is governed: a mis-sized buffer is the only other way out.
        assert!(alpha_image::<Rgba8>(dims, &[1, 2, 3, 4]).is_some());
        assert!(alpha_image::<Rgba8>(dims, &[1, 2, 3]).is_none());

        // The code-value helpers the laws read pixels through must agree with the samples.
        assert_eq!(split_pixel(&[7u8, 9, 255]), (vec![7, 9], 255));
        assert_eq!(split_pixel(&[7u16, 0xFFFF]), (vec![7], 65_535));
    }

    // ---- the laws' own guards --------------------------------------------------------------
    //
    // `invariants.rs` is excluded from mutation, so these are not mutation-driven. They exist so
    // that a law which silently governs nothing is caught: a law stuck at `Ok(())` would make
    // every property above vacuous while still passing.

    #[test]
    fn the_closure_law_does_not_govern_an_open_pair() {
        // Gray8 -> Rgb8 is a lossless widening, not a closed pair. The law must pass it through
        // untouched rather than reporting a violation for a conversion it has nothing to say
        // about — if it governed everything, the property would be asserting the wrong thing.
        let dims = Dimensions::new(2, 2).expect("non-zero");
        let samples = [1u8, 2, 3, 4];

        assert!(
            palette_and_cmyk_convert_only_to_themselves::<u8, Rgb8>(
                PixelFormat::Gray8,
                dims,
                &samples,
                ConvertPolicy::permissive(),
            )
            .is_ok()
        );
    }

    #[test]
    fn the_closure_law_governs_a_closed_pair_and_the_engine_refuses_it() {
        // The pair the law does govern. The assertion is that the engine refuses it — if it ever
        // started accepting, the law would report the violation rather than pass.
        let dims = Dimensions::new(2, 2).expect("non-zero");
        let samples = [1u8, 2, 3, 4];
        let src = crate::convert::RawImage::<u8>::new(&samples, PixelFormat::Indexed8, dims)
            .expect("4 samples is one channel at 2x2");

        assert!(
            crate::convert::convert_from_raw::<u8, Rgb8>(src, ConvertPolicy::permissive()).is_err(),
            "Indexed8 -> Rgb8 must be refused whatever the policy permits"
        );
    }

    #[test]
    fn sample_count_multiplies_the_pixel_count_by_the_channel_count() {
        // The arithmetic every law above uses to size its buffers. A defect here would make the
        // shape law compare an output against a wrong expectation.
        let dims = Dimensions::new(2, 2).expect("non-zero");

        assert_eq!(dims.sample_count(1), Some(4));
        assert_eq!(dims.sample_count(3), Some(12));
        assert_eq!(dims.sample_count(4), Some(16));
    }

    #[test]
    fn normalise_dims_always_yields_a_usable_edge() {
        for (w, h) in [
            (0u32, 0u32),
            (1, 1),
            (u32::MAX, u32::MAX),
            (MAX_EDGE, MAX_EDGE),
        ] {
            let (width, height) = normalise_dims(w, h);

            assert!((1..=MAX_EDGE).contains(&width), "{w} gave {width}");
            assert!((1..=MAX_EDGE).contains(&height), "{h} gave {height}");
            assert!(Dimensions::new(width, height).is_ok());
        }
    }

    #[test]
    fn the_format_tables_cover_the_whole_pixel_matrix() {
        // A drifting table would silently shrink the matrix the properties run over, which no
        // property could report because it would simply stop being asked.
        let eight_bit: Vec<PixelFormat> = PixelFormat::ALL
            .into_iter()
            .filter(|f| f.bytes_per_sample() == 1)
            .collect();
        let sixteen_bit: Vec<PixelFormat> = PixelFormat::ALL
            .into_iter()
            .filter(|f| f.bytes_per_sample() == 2)
            .collect();

        assert_eq!(eight_bit, NARROW_FORMATS);
        assert_eq!(sixteen_bit, WIDE_FORMATS);
        assert_eq!(eight_bit.len() + sixteen_bit.len(), PixelFormat::ALL.len());
    }
}
