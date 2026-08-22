//! Gamut checking: does a colour survive the trip onto a proof device? — lcms2's
//! `GamutSampler` (`cmsgmt.c:213-276`) as a runnable [`Transform`].
//!
//! # The lcms2 double-round-trip heuristic, transcribed
//!
//! A colour counts as in-gamut on the proof device when rendering it onto that device
//! (media-relative) and reading it back colorimetrically lands close to where it started.
//! One round trip is not decisive — LUT profiles round-trip imperfectly even inside their
//! gamut — so lcms2 measures **two**: `dE1` for the colour itself and `dE2` for the point
//! its first round trip landed on (a point that, having come *from* the device, should be
//! reproducible). The verdict table (`GamutSampler`), against a threshold `T` (5.0, or 1.0
//! when the proof profile is a matrix shaper — `cmsgmt.c:210/326-331`):
//!
//! | `dE1` | `dE2` | verdict |
//! |-------|-------|---------|
//! | `< T` | `< T` | in gamut (`0`) |
//! | `< T` | `> T` | "undefined, assume in gamut" (`0`) |
//! | `> T` | `< T` | out of gamut; excess `dE1 − T` |
//! | else  | | ratio test: `dE1/dE2` (or `dE1` when `dE2 = 0`) `> T` ⇒ excess `ratio − T`, else `0` |
//!
//! ΔE here is the plain CIE76 Euclidean distance (`cmsDeltaE`).
//!
//! # Divergence: the excess is a plain `f64`
//!
//! lcms2 bakes the sampler into a 16-bit CLUT: the excess is quantized to a `u16` word
//! (`_cmsQuickFloor(x + .5)`) at grid nodes and *interpolated* between them, and its
//! transform machinery then compares `>= 1` (16-bit path) / `> 0` (float path) to decide
//! whether to substitute the alarm colour. [`GamutCheck`] instead evaluates the sampler
//! exactly, per pixel, and emits the raw `f64` excess — `0.0` means in-gamut, anything
//! positive is the ΔE₇₆-derived excess above the threshold. Classification agrees with
//! lcms2 away from the decision boundary; magnitudes are deliberately not quantized
//! (documented in STATUS.md).

use gamut_icc::{
    ColorSpace, Curve, CurveOrParametric, DeviceClass, IccProfile, LutAToB, ProfileHeader,
    RenderingIntent, Signature, TagData,
};

use crate::bpc;
use crate::chain::link_chain;
use crate::error::{CmmError, Result};
use crate::pipeline::{MAX_CHANNELS, Pipeline};
use crate::transform::Transform;

/// The default gamut-check threshold, lcms2's `ERR_THRESHOLD` (`cmsgmt.c:210`).
const ERR_THRESHOLD: f64 = 5.0;

/// The tightened threshold for matrix-shaper proof profiles (`cmsgmt.c:326-328`): shapers
/// round-trip analytically, so even 1 ΔE of loss marks the gamut boundary.
const SHAPER_THRESHOLD: f64 = 1.0;

/// CIE76 ΔE — the Euclidean Lab distance (lcms2's `cmsDeltaE`).
fn delta_e76(a: [f64; 3], b: [f64; 3]) -> f64 {
    let dl = a[0] - b[0];
    let da = a[1] - b[1];
    let db = a[2] - b[2];
    (dl * dl + da * da + db * db).sqrt()
}

/// The Lab-identity abstract profile lcms2's gamut/proofing machinery uses as its Lab
/// endpoint (`cmsCreateLab4Profile`: v4, abstract class, Lab↔Lab, `A2B0` = identity
/// curves). Hand-built here so the sub-transforms can run through the one chain engine.
fn lab_connection_profile() -> IccProfile {
    let mut header = ProfileHeader::new(DeviceClass::Abstract, ColorSpace::Lab);
    header.pcs = ColorSpace::Lab;
    IccProfile {
        header,
        tags: vec![(
            Signature(*b"A2B0"),
            TagData::LutAToB(LutAToB {
                input_channels: 3,
                output_channels: 3,
                a_curves: None,
                clut: None,
                m_curves: None,
                matrix: None,
                b_curves: vec![CurveOrParametric::Curve(Curve::Identity); 3],
            }),
        )],
    }
}

/// A gamut check for one source/proof profile pair: a [`Transform`] taking `src` device
/// pixels and emitting **one channel per pixel** — `0.0` for in-gamut colours, else the
/// ΔE₇₆ excess above the threshold (module docs). Built by [`GamutCheck::new`].
#[derive(Debug, Clone)]
#[must_use]
pub struct GamutCheck {
    /// `src` device → decoded Lab at the check's intent (lcms2's `hInput`).
    input: Pipeline,
    /// Decoded Lab → proof device at media-relative (lcms2's `hForward`).
    forward: Pipeline,
    /// Proof device → decoded Lab at media-relative (lcms2's `hReverse`).
    reverse: Pipeline,
    /// The verdict threshold: [`ERR_THRESHOLD`], or [`SHAPER_THRESHOLD`] for a
    /// matrix-shaper proof.
    threshold: f64,
}

impl GamutCheck {
    /// Builds the gamut check: `src`'s colours are converted to Lab at `intent` (through
    /// the same chain machinery a `[src, Lab-identity]` transform would use, forced-BPC
    /// rule included — lcms2's `hInput`), then round-tripped twice through `proof` at
    /// media-relative (`hForward`/`hReverse`) and judged by the ΔE table in the module
    /// docs. The threshold is 5.0, tightened to 1.0 when `proof` is a matrix-shaper
    /// profile (lcms2's `cmsIsMatrixShaper` — tag presence, not the path actually used).
    ///
    /// # Errors
    ///
    /// Whatever the underlying profile links raise: `src` needs a device→PCS rendition at
    /// `intent`, `proof` needs **both** directions at media-relative
    /// ([`CmmError::MissingTag`] and friends otherwise).
    pub fn new(src: &IccProfile, proof: &IccProfile, intent: RenderingIntent) -> Result<Self> {
        let lab = lab_connection_profile();
        let relative = RenderingIntent::MediaRelativeColorimetric;
        let input = link_chain(&[src, &lab], &[intent, intent], &[false, false])?;
        let forward = link_chain(&[&lab, proof], &[relative, relative], &[false, false])?;
        let reverse = link_chain(&[proof, &lab], &[relative, relative], &[false, false])?;
        let threshold = if bpc::is_matrix_shaper(proof) {
            SHAPER_THRESHOLD
        } else {
            ERR_THRESHOLD
        };
        Ok(Self {
            input,
            forward,
            reverse,
            threshold,
        })
    }

    /// One Lab colour onto the proof device and back: `(ΔE₇₆(in, out), out)`.
    fn round_trip(&self, lab: [f64; 3]) -> Result<(f64, [f64; 3])> {
        let mut device = [0.0_f64; MAX_CHANNELS as usize];
        let device = &mut device[..usize::from(self.forward.output_channels())];
        self.forward.eval(&lab, device)?;
        let mut out = [0.0; 3];
        self.reverse.eval(device, &mut out)?;
        Ok((delta_e76(lab, out), out))
    }

    /// The gamut verdict for one source device pixel — lcms2's `GamutSampler` decision
    /// table (`cmsgmt.c:255-273`), with its first two arms **folded into one**: every
    /// `dE1 < T` input returns 0 in lcms2 — arm 1 (`dE2 < T`) directly, arm 2 (`dE2 > T`,
    /// "undefined, assume in gamut") directly, and the `dE2 == T` residue through the ratio
    /// branch with `ratio = dE1/T < 1 < T` — so the fold is behaviour-identical while
    /// keeping the table free of redundant (untestable) comparisons. The remaining
    /// comparisons keep lcms2's exact `<`/`>` asymmetry: threshold hits fall through to the
    /// ratio test.
    fn excess(&self, device: &[f64]) -> Result<f64> {
        let mut lab_in = [0.0; 3];
        self.input.eval(device, &mut lab_in)?;
        let (de1, lab_mid) = self.round_trip(lab_in)?;
        let (de2, _) = self.round_trip(lab_mid)?;
        let t = self.threshold;
        Ok(if de1 < t {
            0.0
        } else if de1 > t && de2 < t {
            de1 - t
        } else {
            ratio_excess(de1, de2, t)
        })
    }
}

/// The ratio arm of `GamutSampler`'s decision table: both round trips (or an exact
/// threshold hit) failed outright, so the *ratio* `dE1/dE2` decides — a first trip much
/// worse than the second means the colour itself (not the profile's round-trip quality) is
/// the problem. `dE2 == 0` degrades to `dE1` alone (lcms2's guard, reachable only at
/// `dE1 == T` exactly). Isolated so its one boundary-equivalent mutant (`>` vs `>=`: at
/// `ratio == T` both branches return exactly 0) can be excluded narrowly.
fn ratio_excess(de1: f64, de2: f64, threshold: f64) -> f64 {
    let ratio = if de2 == 0.0 { de1 } else { de1 / de2 };
    if ratio > threshold {
        ratio - threshold
    } else {
        0.0
    }
}

impl Transform for GamutCheck {
    fn transform(&self, src: &[f64], dst: &mut [f64]) -> Result<()> {
        let n_in = usize::from(self.input.input_channels());
        if !src.len().is_multiple_of(n_in) {
            return Err(CmmError::BufferLength {
                channels: self.input.input_channels(),
                found: src.len(),
            });
        }
        if dst.len() != src.len() / n_in {
            return Err(CmmError::BufferLength {
                channels: 1,
                found: dst.len(),
            });
        }
        for (pixel, out) in src.chunks_exact(n_in).zip(dst.iter_mut()) {
            *out = self.excess(pixel)?;
        }
        Ok(())
    }

    fn input_channels(&self) -> u8 {
        self.input.input_channels()
    }

    fn output_channels(&self) -> u8 {
        1
    }
}

#[cfg(test)]
mod tests {
    use gamut_icc::{U8Fixed8, XyzNumber};

    use super::*;

    /// An RGB→XYZ shaper whose colorant matrix is scaled by `gain` — `gain < 1` shrinks the
    /// reproducible gamut, so saturated colours of the unit-gain space fall outside it.
    fn scaled_shaper(gain: f64) -> IccProfile {
        let xyz_tag = |v: [f64; 3]| TagData::Xyz(vec![XyzNumber::from_f64([v[0], v[1], v[2]])]);
        let gamma = || TagData::Curve(Curve::Gamma(U8Fixed8(0x0100))); // γ = 1: linear
        IccProfile {
            header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
            tags: vec![
                (
                    Signature(*b"rXYZ"),
                    xyz_tag([0.5 * gain, 0.25 * gain, 0.0625 * gain]),
                ),
                (
                    Signature(*b"gXYZ"),
                    xyz_tag([0.375 * gain, 0.625 * gain, 0.125 * gain]),
                ),
                (
                    Signature(*b"bXYZ"),
                    xyz_tag([0.125 * gain, 0.125 * gain, 0.625 * gain]),
                ),
                (Signature(*b"rTRC"), gamma()),
                (Signature(*b"gTRC"), gamma()),
                (Signature(*b"bTRC"), gamma()),
            ],
        }
    }

    #[test]
    fn delta_e76_is_the_euclidean_distance() {
        assert_eq!(delta_e76([50.0, 0.0, 0.0], [50.0, 0.0, 0.0]), 0.0);
        assert_eq!(delta_e76([50.0, 3.0, 0.0], [50.0, 0.0, 4.0]), 5.0);
        // Fully asymmetric operands (every b component non-zero, no delta of 0 or 2), so
        // each per-channel subtraction and squaring is independently pinned:
        // √(3² + 3² + (−5)²) = √43 — and e.g. a[1] + b[1] = 5 ≠ 3 breaks it.
        assert_eq!(
            delta_e76([53.0, 4.0, -6.0], [50.0, 1.0, -1.0]),
            43.0_f64.sqrt()
        );
    }

    /// A hand-assembled [`GamutCheck`] whose "round trip" is the affine map
    /// `L ↦ scale·L + offset` (a/b untouched): with the identity `input` pipeline, feeding
    /// Lab values directly steers `dE1`/`dE2` into every arm of the `GamutSampler` decision
    /// table.
    fn synthetic_check(scale: f64, offset: f64) -> GamutCheck {
        use crate::pipeline::Stage;
        let identity3 = || Pipeline::new(3, 3, vec![Stage::Identity { channels: 3 }]).unwrap();
        let reverse = Pipeline::new(
            3,
            3,
            vec![Stage::Matrix {
                m: [[scale, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                offset: [offset, 0.0, 0.0],
            }],
        )
        .unwrap();
        GamutCheck {
            input: identity3(),
            forward: identity3(),
            reverse,
            threshold: ERR_THRESHOLD,
        }
    }

    #[test]
    fn decision_table_arms_are_transcribed_verbatim() {
        let excess = |check: &GamutCheck, l: f64| {
            let mut out = [f64::NAN];
            check.transform(&[l, 0.0, 0.0], &mut out).unwrap();
            out[0]
        };
        // Arm 1 — dE1 < T, dE2 < T: the identity round trip is in gamut, excess exactly 0.
        assert_eq!(excess(&synthetic_check(1.0, 0.0), 50.0), 0.0);
        // Arm 2 — dE1 < T, dE2 > T ("undefined, assume in gamut"): L ↦ 3L at L = 2 gives
        // dE1 = 4 < 5 but dE2 = |18 − 6| = 12 > 5 → still 0.
        assert_eq!(excess(&synthetic_check(3.0, 0.0), 2.0), 0.0);
        // Arm 3 — dE1 > T, dE2 < T: L ↦ L/4 at L = 8 gives dE1 = 6, dE2 = 1.5 → the excess
        // is dE1 − T = 1 exactly.
        assert!((excess(&synthetic_check(0.25, 0.0), 8.0) - 1.0).abs() < 1e-12);
        // Arm 4 — both above: L ↦ L/10 at L = 80 gives dE1 = 72, dE2 = 7.2, ratio 10 > 5 →
        // excess = ratio − T = 5 exactly.
        assert!((excess(&synthetic_check(0.1, 0.0), 80.0) - 5.0).abs() < 1e-12);
        // Arm 4, ratio under the threshold: L ↦ L − 6 gives dE1 = dE2 = 6 (both > 5),
        // ratio 1 → 0.
        assert_eq!(excess(&synthetic_check(1.0, -6.0), 50.0), 0.0);
        // The dE2 == 0 guard (reachable only at dE1 == T exactly): a constant round trip
        // L ↦ 5 at L = 10 gives dE1 = 5 exactly (neither < nor > T) and dE2 = 0, so the
        // ratio IS dE1 = 5, not > 5 → 0. Without the guard the ratio would be ∞ → ∞ excess.
        assert_eq!(excess(&synthetic_check(0.0, 5.0), 10.0), 0.0);
        // dE1 == T exactly with a small non-zero dE2 (L ↦ L/8 + 2 at L = 8: dE1 = 5 exact,
        // dE2 = 0.625): NOT in gamut via the first arm (its `<` is strict) — the ratio
        // branch fires with ratio = 8 → excess 3 exactly. An `<=` in the first arm (or a
        // `>=` in the second's dE1) would call this in-gamut/zero instead.
        assert_eq!(excess(&synthetic_check(0.125, 2.0), 8.0), 3.0);
        // dE2 == T exactly with dE1 > T (L ↦ L/2 + 20 at L = 20: dE1 = 10, dE2 = 5 exact):
        // the second arm's strict `dE2 < T` does NOT fire — the ratio branch does, with
        // ratio = 2 → 0, where an `<=` would emit dE1 − T = 5.
        assert_eq!(excess(&synthetic_check(0.5, 20.0), 20.0), 0.0);
    }

    #[test]
    fn shaper_proof_tightens_the_threshold() {
        let src = scaled_shaper(1.0);
        let proof = scaled_shaper(0.5);
        let check =
            GamutCheck::new(&src, &proof, RenderingIntent::MediaRelativeColorimetric).unwrap();
        assert_eq!(check.threshold, SHAPER_THRESHOLD);
        assert_eq!(check.input_channels(), 3);
        assert_eq!(check.output_channels(), 1);
        // Stripping one TRC breaks cmsIsMatrixShaper: the default threshold returns even
        // though the proof still links through its remaining (LUT-free) tags... it cannot
        // link at all then — so pin the threshold rule on a LUT-bearing proof instead.
        let mut lut_proof = scaled_shaper(0.5);
        lut_proof.tags.push((
            Signature(*b"A2B1"),
            TagData::LutAToB(LutAToB {
                input_channels: 3,
                output_channels: 3,
                a_curves: None,
                clut: None,
                m_curves: None,
                matrix: None,
                b_curves: vec![CurveOrParametric::Curve(Curve::Identity); 3],
            }),
        ));
        // Tag presence still satisfies cmsIsMatrixShaper (colorants + TRCs are all there),
        // so the tightened threshold applies even though the LUT path is used — the lcms2
        // quirk, replicated.
        let check =
            GamutCheck::new(&src, &lut_proof, RenderingIntent::MediaRelativeColorimetric).unwrap();
        assert_eq!(check.threshold, SHAPER_THRESHOLD);
        // A proof with ONLY LUT tags gets the loose threshold.
        let mut bare_lut = lut_proof.clone();
        bare_lut.tags.retain(|(sig, _)| sig.0 == *b"A2B1");
        bare_lut.tags.push((
            Signature(*b"B2A1"),
            TagData::LutBToA(gamut_icc::LutBToA {
                input_channels: 3,
                output_channels: 3,
                b_curves: vec![CurveOrParametric::Curve(Curve::Identity); 3],
                matrix: None,
                m_curves: None,
                clut: None,
                a_curves: None,
            }),
        ));
        let check =
            GamutCheck::new(&src, &bare_lut, RenderingIntent::MediaRelativeColorimetric).unwrap();
        assert_eq!(check.threshold, ERR_THRESHOLD);
    }

    #[test]
    fn in_gamut_is_exact_zero_and_out_of_gamut_is_positive() {
        // Source = the wide space, proof = the same space halved in gain (γ = 1 keeps every
        // pipeline linear-exact): colours dim enough to survive halving are reproducible,
        // bright saturated ones are not.
        let src = scaled_shaper(1.0);
        let proof = scaled_shaper(0.5);
        let check =
            GamutCheck::new(&src, &proof, RenderingIntent::MediaRelativeColorimetric).unwrap();
        let mut out = [f64::NAN; 2];
        check
            .transform(&[0.2, 0.2, 0.2, 1.0, 0.1, 0.9], &mut out)
            .unwrap();
        assert_eq!(out[0], 0.0, "dim colour survives the halved gamut exactly");
        assert!(out[1] > 0.0, "bright colour must exceed: {}", out[1]);
        // Buffer contract.
        let err = check.transform(&[0.0; 4], &mut [0.0; 1]).unwrap_err();
        assert!(matches!(err, CmmError::BufferLength { channels: 3, .. }));
        let err = check.transform(&[0.0; 3], &mut [0.0; 2]).unwrap_err();
        assert!(matches!(err, CmmError::BufferLength { channels: 1, .. }));
        // Object safety through the trait.
        let dynamic: &dyn Transform = &check;
        assert_eq!(dynamic.input_channels(), 3);
        assert_eq!(dynamic.output_channels(), 1);
    }
}
