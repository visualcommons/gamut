//! Executable laws for this crate's buffer constructors, shared by the property tests and the fuzz
//! tier.
//!
//! Each function takes plain data, checks one law, and returns a [`Violation`] naming what broke
//! it. No test framework is involved and nothing here panics, so the same body runs under a
//! pinned-seed `proptest` in the per-PR gate and can run under a corpus-guided driver in extended
//! CI (`docs/testing.md`). A property is the specification a fuzzer checks.
//!
//! This module is the test oracle, not the system under test, so `.cargo/mutants.toml` excludes
//! `crates/*/src/invariants.rs` from mutation.

use gamut_core::convert::{ConvertPolicy, convert};
use gamut_core::{Gray8, Gray16, ImageRef};

use crate::{BitDepth, Planar16};

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

/// Law: [`Planar16::from_gray8_view`] equals [`Planar16::from_gray16_view`] applied to the image's
/// lossless widening to [`Gray16`] through `gamut_core::convert`, at `bit_depth`.
///
/// That composition is the constructor's definition: the two halves already own the widening and
/// the narrowing, so the per-sample path must agree with them on every sample, the geometry, the
/// subsampling and the depth — not merely on the luma plane.
///
/// # Errors
///
/// Returns a violation if the lossless widening itself fails, or names the first field or sample
/// on which the two buffers disagree.
pub fn gray8_view_is_lossless_widening_then_gray16_view(
    img: ImageRef<'_, Gray8>,
    bit_depth: BitDepth,
) -> Result<(), Violation> {
    const LAW: &str = "gray8_view_is_lossless_widening_then_gray16_view";

    let wide = convert::<Gray8, Gray16>(img, ConvertPolicy::lossless())
        .map_err(|e| Violation::new(LAW, format!("lossless Gray8 -> Gray16 failed: {e}")))?;
    let want = Planar16::from_gray16_view(wide.as_ref(), bit_depth);
    let got = Planar16::from_gray8_view(img, bit_depth);

    match first_difference(&got, &want) {
        None => Ok(()),
        Some(detail) => Err(Violation::new(
            LAW,
            format!("from_gray8_view at {bit_depth:?}: {detail}"),
        )),
    }
}

/// The first field or sample on which `got` and `want` disagree, or `None` when they are equal.
///
/// Compares the geometry, subsampling and depth before the planes, so a buffer that is right in
/// its samples but wrong in its metadata is still reported. Equal shapes imply equal plane
/// lengths — every constructor sizes its planes from exactly these four fields — so the samples
/// are compared pairwise with nothing left over.
fn first_difference(got: &Planar16, want: &Planar16) -> Option<String> {
    let shape = |p: &Planar16| (p.width(), p.height(), p.subsampling(), p.bit_depth());
    if shape(got) != shape(want) {
        return Some(format!("shape {:?} != {:?}", shape(got), shape(want)));
    }
    for index in 0..3 {
        let (g, w) = (got.plane(index), want.plane(index));
        if let Some(i) = g.iter().zip(w).position(|(a, b)| a != b) {
            return Some(format!(
                "plane {index} sample {i}: {:#06x} != {:#06x}",
                g[i], w[i]
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use gamut_core::Dimensions;
    use proptest::collection;
    use proptest::prelude::{Just, Strategy, prop_assert, prop_oneof, proptest};
    use proptest::test_runner::{Config, RngSeed};

    use super::{
        BitDepth, Gray8, ImageRef, Planar16, first_difference,
        gray8_view_is_lossless_widening_then_gray16_view,
    };

    /// The workspace property-test configuration (`docs/testing.md`): pinned seed, finite shrink
    /// bounds, no failure persistence, so the blocking mutation gate is reproducible run to run.
    fn config() -> Config {
        Config {
            cases: 512,
            max_shrink_iters: 2048,
            max_shrink_time: 10_000,
            rng_seed: RngSeed::Fixed(0x6761_6D75_745F_636C),
            failure_persistence: None,
            ..Config::default()
        }
    }

    /// Every modelled depth, each drawn directly so a shrunk failure names it.
    fn bit_depth() -> impl Strategy<Value = BitDepth> {
        prop_oneof![
            Just(BitDepth::Eight),
            Just(BitDepth::Ten),
            Just(BitDepth::Twelve),
            Just(BitDepth::Sixteen),
        ]
    }

    /// A grey image of 1..=8 by 1..=8 pixels with arbitrary samples.
    fn image() -> impl Strategy<Value = (u32, u32, Vec<u8>)> {
        (1u32..=8, 1u32..=8).prop_flat_map(|(w, h)| {
            collection::vec(proptest::num::u8::ANY, (w * h) as usize)
                .prop_map(move |samples| (w, h, samples))
        })
    }

    /// The comparison the law rests on reports what differs, so the property cannot pass
    /// vacuously: a depth and a single sample are each caught on their own.
    #[test]
    fn first_difference_reports_each_kind_of_disagreement() {
        let grey = [0x00u8, 0xFF, 0x80, 0x01];
        let img = ImageRef::<Gray8>::new(&grey, Dimensions::new(2, 2).unwrap()).unwrap();
        let ten = Planar16::from_gray8_view(img, BitDepth::Ten);
        assert_eq!(first_difference(&ten, &ten), None);

        let twelve = Planar16::from_gray8_view(img, BitDepth::Twelve);
        assert!(first_difference(&ten, &twelve).is_some_and(|d| d.starts_with("shape")));

        // `from_planes` builds 4:4:4, so the sample case compares two 4:4:4 buffers that differ
        // only in luma sample 3 (0x01 widened to 10 bits is 0x004).
        let luma = |s3: u16| [vec![0x000, 0x3FF, 0x202, s3], vec![0; 4], vec![0; 4]];
        let want = Planar16::from_planes(2, 2, BitDepth::Ten, luma(0x004)).unwrap();
        let got = Planar16::from_planes(2, 2, BitDepth::Ten, luma(0x005)).unwrap();
        assert_eq!(
            first_difference(&got, &want).as_deref(),
            Some("plane 0 sample 3: 0x0005 != 0x0004")
        );
    }

    proptest! {
        #![proptest_config(config())]

        /// `from_gray8_view` is the lossless widening followed by `from_gray16_view`, for every
        /// grey image and every depth.
        #[test]
        fn gray8_view_equals_lossless_widening_then_gray16_view(
            (w, h, samples) in image(),
            depth in bit_depth(),
        ) {
            let img = ImageRef::<Gray8>::new(&samples, Dimensions::new(w, h).unwrap()).unwrap();
            let result = gray8_view_is_lossless_widening_then_gray16_view(img, depth);
            prop_assert!(result.is_ok(), "{}", result.err().map(|v| v.to_string()).unwrap_or_default());
        }
    }
}
