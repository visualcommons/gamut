//! Executable laws for this crate's sample transforms and decode budget, shared by the property
//! tests and the fuzz tier.
//!
//! Each function takes plain data, checks one law, and returns a [`Violation`] naming what broke
//! it. Nothing here panics on a well-formed input, so the same body runs under a pinned-seed
//! `proptest` in the per-PR gate and under a corpus-guided driver in extended CI
//! (`docs/testing.md`).
//!
//! **Clean alpha** (issue #657): cleaning the colour under transparent pixels must leave the
//! **premultiplied composite** of every pixel unchanged — alpha identical, opaque colour
//! identical, and every colour channel premultiplying to the value it did before.
//! [`premultiply`] is written here, from the definition `round(c·a / MAX)`, rather than borrowed
//! from the cleaner, so a defect in the cleaner's own interval arithmetic cannot cancel against
//! the oracle. For the same reason the law reads samples through its own [`Scale`], not the
//! cleaner's `Sample` trait: a defect in how the cleaner widens a sample would otherwise blind
//! the oracle to it too.
//!
//! **Decode limits** (issue #653): `decoding_stays_within_limits` decodes every item of a file
//! on all three surfaces twice — under the limits being tested and under a looser ceiling — with
//! `StubDecoder` standing in for the AV1 codec. Every image the tighter run returns is within
//! its limits and identical to the looser run's, and every refusal the tighter limits add is a
//! limit refusal. Since each full-image buffer the pipeline allocates is sized like an image it
//! checked, the first clause is the observable face of "decode never allocates past limits ×
//! channels"; the ceiling is what keeps the law's own runs, and the fuzz tier's, from exhausting
//! memory on a hostile file.
//!
//! This module is the test oracle, not the system under test, so `.cargo/mutants.toml` excludes
//! `crates/*/src/invariants.rs` from mutation.

use gamut_core::Result as CoreResult;

use crate::clean_alpha::clean_rgba;
use crate::decode::{DECODED_ITEM_LIMIT, DERIVED_ITEM_LIMIT, DIMENSION_LIMIT, PIXEL_LIMIT};
use crate::{Av1Config, Av1StillDecoder, AvifContainer, DecodeLimits, DecodedFrame};

/// A sample type the law reads, with the alpha value that means fully opaque.
pub(crate) trait Scale: Copy + Into<u32> {
    /// Full opacity, and the top of the colour scale.
    const MAX: u32;
}

impl Scale for u8 {
    const MAX: u32 = u8::MAX as u32;
}

impl Scale for u16 {
    const MAX: u32 = u16::MAX as u32;
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

/// `round(c·a / MAX)`: the colour `c` of a pixel with alpha `a`, premultiplied and rounded half
/// up on the sample's own scale.
///
/// Written as `(2·c·a + MAX) / (2·MAX)` — the rounding definition itself, not the `+ (MAX−1)/2`
/// shortcut the cleaner's interval derivation uses — so the two agree only if both are right.
pub(crate) fn premultiply<S: Scale>(c: u32, a: u32) -> u32 {
    let max = u64::from(S::MAX);
    ((2 * u64::from(c) * u64::from(a) + max) / (2 * max)) as u32
}

/// The law: `cleaned` composites exactly as `original` does. Both are packed RGBA.
pub(crate) fn composite_is_unchanged<S: Scale>(
    original: &[S],
    cleaned: &[S],
) -> Result<(), Violation> {
    const LAW: &str = "composite_is_unchanged";
    if original.len() != cleaned.len() {
        return Err(Violation::new(
            LAW,
            format!("{} samples in, {} out", original.len(), cleaned.len()),
        ));
    }
    let (before_px, after_px) = (original.as_chunks::<4>().0, cleaned.as_chunks::<4>().0);
    for (i, (before, after)) in before_px.iter().zip(after_px).enumerate() {
        let (a, a2): (u32, u32) = (before[3].into(), after[3].into());
        if a2 != a {
            return Err(Violation::new(
                LAW,
                format!("pixel {i}: alpha {a} became {a2}"),
            ));
        }
        for ch in 0..3 {
            let (c, c2): (u32, u32) = (before[ch].into(), after[ch].into());
            if a == S::MAX && c2 != c {
                return Err(Violation::new(
                    LAW,
                    format!("pixel {i}: opaque channel {ch} changed {c} -> {c2}"),
                ));
            }
            let (p, p2) = (premultiply::<S>(c, a), premultiply::<S>(c2, a));
            if p2 != p {
                return Err(Violation::new(
                    LAW,
                    format!(
                        "pixel {i}: channel {ch} at alpha {a} premultiplied {c} -> {p}, {c2} -> {p2}"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Cleans the packed 8-bit RGBA image `rgba`, `width` pixels per row, and checks
/// [the composite law](self) against the result.
///
/// Precondition (normalising is the driver's job): `width > 0` and `rgba.len()` is a whole number
/// of `4 * width`-sample rows.
pub fn cleaning_8_bit_rgba_keeps_the_composite(width: usize, rgba: &[u8]) -> Result<(), Violation> {
    composite_is_unchanged(rgba, &clean_rgba(rgba, width))
}

/// [`cleaning_8_bit_rgba_keeps_the_composite`] for 16-bit samples on the full 16-bit scale.
pub fn cleaning_16_bit_rgba_keeps_the_composite(
    width: usize,
    rgba: &[u16],
) -> Result<(), Violation> {
    composite_is_unchanged(rgba, &clean_rgba(rgba, width))
}

/// The largest frame side [`StubDecoder`] returns, in pixels.
pub const STUB_MAX_SIDE: u8 = 16;

/// A stand-in for an AV1 codec: it ignores the coded picture and returns a frame whose size the
/// payload's last two bytes choose (`1..=STUB_MAX_SIDE` each, width then height), at the `av1C`
/// chroma format and bit depth, with samples drawn from the payload.
///
/// Small by construction, so a file can make the pipeline ask for a large canvas while every
/// frame the "codec" returns stays cheap. It serves every call it is given: how many a file can
/// demand is the pipeline's own bound ([`DecodeLimits::max_decoded_items`] and
/// [`DecodeLimits::max_derived_items`]), which the law exercises rather than a budget of the
/// stub's.
#[derive(Debug, Clone, Default)]
pub struct StubDecoder;

impl Av1StillDecoder for StubDecoder {
    fn decode_still(&mut self, config: &Av1Config, payload: &[u8]) -> CoreResult<DecodedFrame> {
        let from_end = |back: usize| {
            payload
                .len()
                .checked_sub(back)
                .map_or(0, |i| u32::from(payload[i]))
        };
        let side = u32::from(STUB_MAX_SIDE);
        let (width, height) = (1 + from_end(2) % side, 1 + from_end(1) % side);
        let chroma = config.chroma_format();
        let sample = |i: usize| {
            payload
                .get(i % payload.len().max(1))
                .map_or(0, |&b| u16::from(b))
        };
        let luma = (width * height) as usize;
        let (cw, ch) = chroma.chroma_dimensions(width, height);
        let chroma_len = if chroma == crate::ChromaFormat::Monochrome {
            0
        } else {
            (cw * ch) as usize
        };
        DecodedFrame::new(
            width,
            height,
            config.bit_depth(),
            chroma,
            (0..luma).map(sample).collect(),
            (0..chroma_len).map(|i| sample(i + 1)).collect(),
            (0..chroma_len).map(|i| sample(i + 2)).collect(),
        )
    }
}

/// One surface's output reduced to what the law compares: dimensions and samples.
type Decoded = (u32, u32, Vec<u16>);

/// Checks one surface's two runs of item `id` against the law; `limits` are the tighter run's.
pub(crate) fn runs_agree_within_limits(
    surface: &str,
    id: u32,
    limits: DecodeLimits,
    tight: CoreResult<Decoded>,
    loose: CoreResult<Decoded>,
) -> Result<(), Violation> {
    const LAW: &str = "decoding_stays_within_limits";
    match (tight, loose) {
        (Ok(tight), Ok(loose)) => {
            let (w, h, _) = tight;
            if w.max(h) > limits.max_dimension || u64::from(w) * u64::from(h) > limits.max_pixels {
                return Err(Violation::new(
                    LAW,
                    format!("{surface} item {id}: {w}x{h} returned under {limits:?}"),
                ));
            }
            if tight != loose {
                return Err(Violation::new(
                    LAW,
                    format!("{surface} item {id}: the limits changed the decoded image"),
                ));
            }
            Ok(())
        }
        (Ok(_), Err(error)) => Err(Violation::new(
            LAW,
            format!("{surface} item {id}: decoded under {limits:?} but refused looser: {error}"),
        )),
        (Err(error), Ok(_))
            if !matches!(
                error.static_message(),
                Some(DIMENSION_LIMIT | PIXEL_LIMIT | DECODED_ITEM_LIMIT | DERIVED_ITEM_LIMIT)
            ) =>
        {
            Err(Violation::new(
                LAW,
                format!("{surface} item {id}: tighter limits raised a non-limit error: {error}"),
            ))
        }
        _ => Ok(()),
    }
}

/// The law: decoding any item of `file` on any surface under `limits` either refuses or returns
/// an image within `limits` — the very image a decode under `ceiling` returns — and any refusal
/// `limits` add over `ceiling` is a limit refusal. See [the module docs](self).
///
/// `limits` are clamped into `ceiling` field by field, so a driver may pass any pair. A file that
/// does not parse satisfies the law vacuously; parsing is the robustness targets' subject.
pub fn decoding_stays_within_limits(
    file: &[u8],
    limits: DecodeLimits,
    ceiling: DecodeLimits,
) -> Result<(), Violation> {
    let Ok(container) = AvifContainer::parse(file) else {
        return Ok(());
    };
    let image = container.image();
    let limits = DecodeLimits::new(
        limits.max_dimension.min(ceiling.max_dimension),
        limits.max_pixels.min(ceiling.max_pixels),
    )
    .with_item_limits(
        limits.max_decoded_items.min(ceiling.max_decoded_items),
        limits.max_derived_items.min(ceiling.max_derived_items),
    );
    for item in image.items() {
        let id = item.id();
        let planar = |l| {
            image
                .decode_item_planar_with_limits(id, &mut StubDecoder, l)
                .map(|f| (f.width(), f.height(), [f.y(), f.cb(), f.cr()].concat()))
        };
        runs_agree_within_limits("planar", id, limits, planar(limits), planar(ceiling))?;
        let rgba8 = |l| {
            image
                .decode_item_rgba8_with_limits(id, &mut StubDecoder, l)
                .map(|b| {
                    let samples = b.as_samples().iter().map(|&s| u16::from(s)).collect();
                    (b.width(), b.height(), samples)
                })
        };
        runs_agree_within_limits("rgba8", id, limits, rgba8(limits), rgba8(ceiling))?;
        let rgba16 = |l| {
            image
                .decode_item_rgba16_with_limits(id, &mut StubDecoder, l)
                .map(|b| (b.width(), b.height(), b.as_samples().to_vec()))
        };
        runs_agree_within_limits("rgba16", id, limits, rgba16(limits), rgba16(ceiling))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // ---- the law's own failure paths -------------------------------------------------------
    //
    // `invariants.rs` is excluded from mutation, so these are not mutation-driven. They exist so
    // that a law which can no longer *fail* is caught: a law stuck at `Ok(())` would make every
    // property over it vacuous while still passing.

    use gamut_core::Error;

    use super::{composite_is_unchanged, premultiply, runs_agree_within_limits};
    use crate::DecodeLimits;
    use crate::decode::{DECODED_ITEM_LIMIT, DERIVED_ITEM_LIMIT, DIMENSION_LIMIT, PIXEL_LIMIT};

    const LIMITS: DecodeLimits = DecodeLimits::new(4, 12);

    #[test]
    fn an_image_past_the_limits_violates_the_decode_law() {
        let wide = || Ok((5, 1, vec![0; 5]));
        assert!(runs_agree_within_limits("t", 1, LIMITS, wide(), wide()).is_err());
        let large = || Ok((4, 4, vec![0; 16]));
        assert!(runs_agree_within_limits("t", 1, LIMITS, large(), large()).is_err());
    }

    #[test]
    fn a_changed_image_violates_the_decode_law() {
        let v = runs_agree_within_limits("t", 1, LIMITS, Ok((1, 1, vec![0])), Ok((1, 1, vec![1])))
            .unwrap_err();
        assert!(v.detail().contains("changed"), "{v}");
    }

    #[test]
    fn success_only_under_the_tighter_limits_violates_the_decode_law() {
        let refused = Err(Error::InvalidInput("x"));
        assert!(runs_agree_within_limits("t", 1, LIMITS, Ok((1, 1, vec![0])), refused).is_err());
    }

    #[test]
    fn only_a_limit_refusal_may_be_added_by_the_tighter_limits() {
        let ok = || Ok((1, 1, vec![0]));
        let other = Err(Error::InvalidInput("x"));
        assert!(runs_agree_within_limits("t", 1, LIMITS, other, ok()).is_err());
        for message in [
            DIMENSION_LIMIT,
            PIXEL_LIMIT,
            DECODED_ITEM_LIMIT,
            DERIVED_ITEM_LIMIT,
        ] {
            let limit = Err(Error::unsupported("gamut-avif", message));
            assert_eq!(
                runs_agree_within_limits("t", 1, LIMITS, limit, ok()),
                Ok(())
            );
        }
        let both = || Err(Error::InvalidInput("x"));
        assert_eq!(
            runs_agree_within_limits("t", 1, LIMITS, both(), both()),
            Ok(())
        );
    }

    #[test]
    fn premultiply_rounds_half_up_on_the_samples_scale() {
        assert_eq!(premultiply::<u8>(255, 255), 255);
        assert_eq!(premultiply::<u8>(1, 128), 1); // 128/255 = 0.502
        assert_eq!(premultiply::<u8>(1, 127), 0); // 127/255 = 0.498
        assert_eq!(premultiply::<u16>(65535, 1), 1);
        assert_eq!(premultiply::<u16>(32767, 1), 0);
    }

    #[test]
    fn a_changed_alpha_violates_the_law() {
        let v = composite_is_unchanged::<u8>(&[1, 2, 3, 4], &[1, 2, 3, 5]).unwrap_err();
        assert_eq!(v.law(), "composite_is_unchanged");
        assert!(v.detail().contains("alpha"), "{v}");
    }

    #[test]
    fn a_changed_opaque_colour_violates_the_law() {
        let v = composite_is_unchanged::<u8>(&[1, 2, 3, 255], &[1, 2, 4, 255]).unwrap_err();
        assert!(v.detail().contains("opaque"), "{v}");
    }

    #[test]
    fn a_changed_premultiplied_value_violates_the_law() {
        // At alpha 128, colour 200 premultiplies to 100 and colour 0 to 0.
        let v = composite_is_unchanged::<u8>(&[200, 0, 0, 128], &[0, 0, 0, 128]).unwrap_err();
        assert!(v.detail().contains("premultiplied"), "{v}");
    }

    #[test]
    fn a_length_change_violates_the_law() {
        assert!(composite_is_unchanged::<u16>(&[0; 4], &[0; 8]).is_err());
    }

    #[test]
    fn any_colour_under_zero_alpha_and_a_same_premultiplied_colour_satisfy_the_law() {
        // At alpha 1 of 255, colours 0..=127 all premultiply to 0.
        assert_eq!(
            composite_is_unchanged::<u8>(
                &[9, 9, 9, 0, 0, 5, 127, 1],
                &[200, 1, 2, 0, 127, 0, 64, 1]
            ),
            Ok(())
        );
    }
}
