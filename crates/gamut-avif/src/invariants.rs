//! Executable laws for this crate's sample transforms, shared by the property tests and the fuzz
//! tier.
//!
//! Each function takes plain data, checks one law, and returns a [`Violation`] naming what broke
//! it. Nothing here panics on a well-formed input, so the same body runs under a pinned-seed
//! `proptest` in the per-PR gate and under a corpus-guided driver in extended CI
//! (`docs/testing.md`).
//!
//! The law here is the clean-alpha contract (issue #657): cleaning the colour under transparent
//! pixels must leave the **premultiplied composite** of every pixel unchanged — alpha identical,
//! opaque colour identical, and every colour channel premultiplying to the value it did before.
//! [`premultiply`] is written here, from the definition `round(c·a / MAX)`, rather than borrowed
//! from the cleaner, so a defect in the cleaner's own interval arithmetic cannot cancel against
//! the oracle. For the same reason the law reads samples through its own [`Scale`], not the
//! cleaner's `Sample` trait: a defect in how the cleaner widens a sample would otherwise blind
//! the oracle to it too.
//!
//! This module is the test oracle, not the system under test, so `.cargo/mutants.toml` excludes
//! `crates/*/src/invariants.rs` from mutation.

use crate::clean_alpha::clean_rgba;

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

#[cfg(test)]
mod tests {
    // ---- the law's own failure paths -------------------------------------------------------
    //
    // `invariants.rs` is excluded from mutation, so these are not mutation-driven. They exist so
    // that a law which can no longer *fail* is caught: a law stuck at `Ok(())` would make every
    // property over it vacuous while still passing.

    use super::{composite_is_unchanged, premultiply};

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
