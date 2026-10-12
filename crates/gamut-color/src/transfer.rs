//! Transfer functions (EOTF / inverse-EOTF) in `f64`, covering the gamuts the
//! gamut ecosystem encodes through.
//!
//! Two flavours are exposed for the curves that have a *deliberately simplified*
//! encoder form (see `references/color/README.md`):
//!
//! * **encoder-exact** — what the encoder actually applied, so a metrics tool
//!   predicts the same bitstream: Adobe RGB is pure `x^2.2` (not the standard
//!   `x^2.19921875`), ProPhoto RGB is pure `x^1.8` (no linear toe), and the
//!   BT.2020 PQ path (H.273 code point 16) is PQ EOTF → nits → Reinhard@203
//!   (tone-mapped to SDR).
//! * **standard** — the textbook curve, reachable via the `*_standard` helpers.
//!
//! Determinism is **Tier-1** (correctness only): these use `std` `f64::powf`, not
//! a bit-reproducible substrate. Inputs are assumed in `[0, 1]`; the curves are
//! evaluated as written (no clamping) so they stay faithful to the reference.

// The reference luminance levels are defined once in `gamut_core::luminance` (see
// `references/color/README.md`) and re-exported here so existing `gamut_color::transfer::*` paths
// keep resolving.
pub use gamut_core::luminance::{HDR_REFERENCE_WHITE_NITS, PQ_PEAK_NITS};

use crate::cicp::TransferCharacteristics;

// --- Linear (H.273 code point 8) -------------------------------------------

/// Linear EOTF: the identity, `V = Lc` (CICP [`TransferCharacteristics::Linear`]).
///
/// Named so a linear working space — e.g. linear sRGB, the space a RAW pipeline
/// demosaics and white-balances in — dispatches through [`eotf_for`] like any
/// other code point instead of special-casing the identity at each call site.
///
/// # Examples
///
/// ```
/// use gamut_color::transfer::linear_eotf;
/// assert_eq!(linear_eotf(0.0), 0.0);
/// assert_eq!(linear_eotf(0.5), 0.5);
/// assert_eq!(linear_eotf(1.0), 1.0);
/// ```
#[must_use]
pub fn linear_eotf(x: f64) -> f64 {
    x
}

/// Linear inverse EOTF: the identity, the exact inverse of [`linear_eotf`]. Present so
/// [`oetf_for`] can mirror [`eotf_for`] with a named function rather than a closure.
#[must_use]
pub fn linear_oetf(x: f64) -> f64 {
    x
}

// --- sRGB (IEC 61966-2-1) --------------------------------------------------

/// sRGB EOTF: gamma-encoded signal → linear light, on `[0, 1]`.
///
/// # Examples
///
/// ```
/// use gamut_color::transfer::srgb_eotf;
/// assert_eq!(srgb_eotf(0.0), 0.0);
/// assert!((srgb_eotf(1.0) - 1.0).abs() < 1e-12);
/// // sRGB 0.5 is ~0.214 in linear light.
/// assert!((srgb_eotf(0.5) - 0.214).abs() < 1e-3);
/// ```
#[must_use]
pub fn srgb_eotf(x: f64) -> f64 {
    if x <= 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB inverse EOTF (the "gamma" encode): linear light → gamma-encoded signal.
#[must_use]
pub fn srgb_oetf(x: f64) -> f64 {
    if x <= 0.0031308 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

// --- Adobe RGB (1998) ------------------------------------------------------

/// Adobe RGB EOTF, **encoder-exact** simplification: pure `x^2.2`.
#[must_use]
pub fn adobe_rgb_eotf(x: f64) -> f64 {
    x.powf(2.2)
}

/// Adobe RGB EOTF, **standard** curve: `x^(563/256)` = `x^2.19921875`.
#[must_use]
pub fn adobe_rgb_eotf_standard(x: f64) -> f64 {
    x.powf(563.0 / 256.0)
}

// --- ProPhoto RGB / ROMM (ISO 22028-2) -------------------------------------

/// ProPhoto RGB EOTF, **encoder-exact** simplification: pure `x^1.8` (no toe).
#[must_use]
pub fn prophoto_rgb_eotf(x: f64) -> f64 {
    x.powf(1.8)
}

/// ProPhoto RGB EOTF, **standard** ROMM curve: linear toe of slope 16 below
/// `16·Eₜ = 1/32`, then `x^1.8`.
#[must_use]
pub fn prophoto_rgb_eotf_standard(x: f64) -> f64 {
    if x < 1.0 / 32.0 {
        x / 16.0
    } else {
        x.powf(1.8)
    }
}

// --- BT.709 family (H.273 code points 1, 6, 14, 15) -----------------------

/// β of the BT.709-family transfer: ITU-T H.273 (07/2024) §8.2 defines it, for
/// `TransferCharacteristics` 1, 6, 14 and 15, as "the positive constant necessary for the curve
/// segments that meet at the value β to have continuity of both value and slope", and states it
/// as `0.018053968510807...`. Rec. ITU-R BT.709-6 and BT.2020-2 use the same curve.
pub const BT709_BETA: f64 = 0.018_053_968_510_807;

/// α of the BT.709-family transfer: ITU-T H.273 (07/2024) §8.2, `α = 1 + 5.5 * β =
/// 1.099296826809442...`. Derived from [`BT709_BETA`] rather than restated, so the two cannot
/// disagree.
pub const BT709_ALPHA: f64 = 1.0 + 5.5 * BT709_BETA;

/// BT.709-family OETF: scene-linear light `Lc` on `[0, 1]` → signal `V`, exactly as ITU-T H.273
/// Table 3 states it for code points 1, 6, 14 and 15 (Rec. ITU-R BT.709-6, BT.2020-2):
/// `V = α Lc^0.45 − (α − 1)` for `1 >= Lc >= β`, and `V = 4.500 Lc` for `β > Lc >= 0`.
///
/// # Examples
///
/// ```
/// use gamut_color::transfer::bt709_oetf;
/// assert_eq!(bt709_oetf(0.0), 0.0);
/// assert!((bt709_oetf(1.0) - 1.0).abs() < 1e-12);
/// // Below β the curve is the linear toe.
/// assert_eq!(bt709_oetf(0.01), 4.5 * 0.01);
/// ```
#[must_use]
pub fn bt709_oetf(lc: f64) -> f64 {
    if lc < BT709_BETA {
        4.5 * lc
    } else {
        BT709_ALPHA * lc.powf(0.45) - (BT709_ALPHA - 1.0)
    }
}

/// The exact inverse of [`bt709_oetf`]: signal `V` → scene-linear light `Lc` on `[0, 1]`, for
/// H.273 code points 1, 6, 14 and 15. `Lc = ((V + α − 1) / α)^(1/0.45)` for `V >= 4.5 β`, and
/// `Lc = V / 4.5` below it.
///
/// Named `_eotf` to pair with [`bt709_oetf`] the way [`srgb_eotf`] pairs with [`srgb_oetf`], but
/// it is the **inverse OETF** (scene-referred), not the Rec. ITU-R BT.1886 display EOTF that
/// H.273 §8.2 NOTE 1 suggests for flat-panel displays: H.273 defines these code points by their
/// opto-electronic function, and this is that function undone.
///
/// # Examples
///
/// ```
/// use gamut_color::transfer::{bt709_eotf, bt709_oetf};
/// // Signal 0.5 is ~0.2597 in scene-linear light.
/// assert!((bt709_eotf(0.5) - 0.259_719_437).abs() < 1e-9);
/// assert!((bt709_eotf(bt709_oetf(0.3)) - 0.3).abs() < 1e-12);
/// ```
#[must_use]
pub fn bt709_eotf(v: f64) -> f64 {
    if v < 4.5 * BT709_BETA {
        v / 4.5
    } else {
        ((v + BT709_ALPHA - 1.0) / BT709_ALPHA).powf(1.0 / 0.45)
    }
}

// --- BT.2020 PQ (SMPTE ST 2084 / ITU-R BT.2100) ----------------------------

/// PQ (ST 2084) EOTF: gamma-encoded signal → **absolute luminance in
/// cd/m² (nits)**, in `[0, 10000]`. (ST 2084 defines the EOTF in this signal → light
/// direction; the encode direction is its inverse.) This is the standards-pure curve, with no
/// tone mapping. Use [`bt2020_pq_to_sdr`] for the encoder-exact BT.2020 path.
#[must_use]
pub fn pq_eotf(x: f64) -> f64 {
    // ST 2084 constants (exact dyadic rationals; see references/color/README.md).
    const M1: f64 = 0.1593017578125;
    const M2: f64 = 78.84375;
    const C1: f64 = 0.8359375;
    const C2: f64 = 18.8515625;
    const C3: f64 = 18.6875;

    let n = x.powf(1.0 / M2);
    let num = (n - C1).max(0.0);
    let den = C2 - C3 * n;
    let y_normalized = (num / den).powf(1.0 / M1);
    y_normalized * PQ_PEAK_NITS
}

/// PQ (ST 2084) inverse EOTF: **absolute luminance in cd/m² (nits)** → gamma-encoded signal on
/// `[0, 1]`. The exact inverse of [`pq_eotf`], so it takes nits, not a normalized value.
///
/// Note this is *not* what [`oetf_for`] returns for [`TransferCharacteristics::Pq`]: that
/// dispatch mirrors [`eotf_for`], whose PQ arm is the tone-mapping [`bt2020_pq_to_sdr`], which
/// has no inverse. Reach for this function when you want the standards-pure curve.
///
/// # Examples
///
/// ```
/// use gamut_color::transfer::{pq_eotf, pq_oetf};
/// // 203 nits is BT.2408 HDR reference white.
/// let signal = pq_oetf(203.0);
/// assert!((pq_eotf(signal) - 203.0).abs() < 1e-9);
/// ```
#[must_use]
pub fn pq_oetf(nits: f64) -> f64 {
    // Same ST 2084 constants as `pq_eotf`, applied in the opposite direction.
    const M1: f64 = 0.1593017578125;
    const M2: f64 = 78.84375;
    const C1: f64 = 0.8359375;
    const C2: f64 = 18.8515625;
    const C3: f64 = 18.6875;

    let y = (nits / PQ_PEAK_NITS).clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

/// BT.2020 **encoder-exact** path: PQ EOTF → nits → Reinhard tone map to
/// SDR `[0, 1)` relative to the BT.2408 HDR reference white (203 nits; `L / (1 + L)`).
#[must_use]
pub fn bt2020_pq_to_sdr(x: f64) -> f64 {
    let l = pq_eotf(x) / HDR_REFERENCE_WHITE_NITS;
    l / (1.0 + l)
}

// --- CICP dispatch ---------------------------------------------------------

/// The encoder-exact EOTF for a CICP [`TransferCharacteristics`] code point, as
/// a `fn` mapping signal → scene/display-linear `[0, 1]`.
///
/// `Srgb` returns scene-linear via [`srgb_eotf`]; `Bt709` / `Bt2020_10` — which ITU-T H.273
/// Table 3 gives one curve, "functionally the same" as each other — return scene-linear via
/// [`bt709_eotf`]; `Pq` returns the tone-mapped SDR value via [`bt2020_pq_to_sdr`]; `Linear`
/// returns the identity via [`linear_eotf`]. Code points without a curve implemented here
/// (`Hlg`, `Unspecified`) return `None`. Adobe RGB and ProPhoto have no CICP transfer code point
/// and are reached via their named functions or [`crate::profile`].
///
/// # Examples
///
/// ```
/// use gamut_color::cicp::TransferCharacteristics;
/// use gamut_color::transfer::{bt709_eotf, eotf_for};
///
/// // Code point 14 (BT.2020 10-bit) is the BT.709 curve, not PQ.
/// let f = eotf_for(TransferCharacteristics::Bt2020_10).unwrap();
/// assert_eq!(f(0.5), bt709_eotf(0.5));
/// ```
#[must_use]
pub fn eotf_for(tc: TransferCharacteristics) -> Option<fn(f64) -> f64> {
    match tc {
        TransferCharacteristics::Linear => Some(linear_eotf),
        TransferCharacteristics::Srgb => Some(srgb_eotf),
        TransferCharacteristics::Bt709 | TransferCharacteristics::Bt2020_10 => Some(bt709_eotf),
        TransferCharacteristics::Pq => Some(bt2020_pq_to_sdr),
        TransferCharacteristics::Hlg | TransferCharacteristics::Unspecified => None,
    }
}

/// The inverse of [`eotf_for`]: the OETF for a CICP [`TransferCharacteristics`] code point, as a
/// `fn` mapping scene/display-linear `[0, 1]` → signal. Use it to encode *into* a code point's
/// transfer, the direction [`eotf_for`] cannot express.
///
/// Where both return `Some`, they are exact inverses: `oetf_for(tc)(eotf_for(tc)(x)) == x` to
/// within floating-point error. `Linear` returns the identity via [`linear_oetf`], `Srgb` the
/// gamma encode via [`srgb_oetf`], and `Bt709` / `Bt2020_10` the H.273 Table 3 curve via
/// [`bt709_oetf`].
///
/// `None` for the same code points [`eotf_for`] declines (`Hlg`, `Unspecified` — no curve is
/// implemented for them), **and additionally for `Pq`**: [`eotf_for`] maps it to
/// [`bt2020_pq_to_sdr`], which tone-maps HDR to SDR and is therefore not invertible.
/// [`pq_oetf`] is the inverse of the standards-pure [`pq_eotf`] if that is what you want.
///
/// # Examples
///
/// ```
/// use gamut_color::cicp::TransferCharacteristics;
/// use gamut_color::transfer::{eotf_for, oetf_for};
///
/// let eotf = eotf_for(TransferCharacteristics::Srgb).unwrap();
/// let oetf = oetf_for(TransferCharacteristics::Srgb).unwrap();
/// assert!((oetf(eotf(0.5)) - 0.5).abs() < 1e-12);
///
/// // PQ has an EOTF here but no inverse: its EOTF arm is a tone map.
/// assert!(eotf_for(TransferCharacteristics::Pq).is_some());
/// assert!(oetf_for(TransferCharacteristics::Pq).is_none());
/// ```
#[must_use]
pub fn oetf_for(tc: TransferCharacteristics) -> Option<fn(f64) -> f64> {
    match tc {
        TransferCharacteristics::Linear => Some(linear_oetf),
        TransferCharacteristics::Srgb => Some(srgb_oetf),
        TransferCharacteristics::Bt709 | TransferCharacteristics::Bt2020_10 => Some(bt709_oetf),
        // `eotf_for`'s PQ arm is `bt2020_pq_to_sdr`, a tone map, so it has no inverse here.
        TransferCharacteristics::Pq
        | TransferCharacteristics::Hlg
        | TransferCharacteristics::Unspecified => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_roundtrip() {
        for &x in &[0.0, 0.01, 0.04045, 0.1, 0.5, 0.9, 1.0] {
            let back = srgb_oetf(srgb_eotf(x));
            // Tolerance accounts for the ~3e-8 discontinuity the sRGB standard
            // bakes into its two piecewise breakpoints (0.04045 vs 0.0031308·12.92).
            assert!((back - x).abs() < 1e-4, "sRGB roundtrip at {x}: {back}");
        }
    }

    #[test]
    fn srgb_piecewise_continuous_at_thresholds() {
        assert!((srgb_eotf(0.04045) - 0.04045 / 12.92).abs() < 1e-12);
        assert!((srgb_oetf(0.0031308) - 12.92 * 0.0031308).abs() < 1e-12);
    }

    #[test]
    fn boundaries_map_zero_and_one() {
        for f in [
            srgb_eotf as fn(f64) -> f64,
            srgb_oetf,
            adobe_rgb_eotf,
            adobe_rgb_eotf_standard,
            prophoto_rgb_eotf,
            prophoto_rgb_eotf_standard,
        ] {
            assert_eq!(f(0.0), 0.0);
            assert!((f(1.0) - 1.0).abs() < 1e-12, "f(1.0) should be 1.0");
        }
    }

    #[test]
    fn adobe_and_prophoto_encoder_exact_vs_standard_differ() {
        // The simplified curves must measurably differ from the textbook ones,
        // or the "selectable" distinction is meaningless.
        let x = 0.5;
        assert!((adobe_rgb_eotf(x) - adobe_rgb_eotf_standard(x)).abs() > 1e-4);
        assert!((prophoto_rgb_eotf(0.02) - prophoto_rgb_eotf_standard(0.02)).abs() > 1e-4);
        // Pin the *exact* standard exponent: a mutated `563 / 256` (e.g. `563 % 256` or `563 * 256`)
        // still "differs" from x^2.2, so the inequality above can't see it.
        assert!((adobe_rgb_eotf_standard(0.5) - 0.5_f64.powf(563.0 / 256.0)).abs() < 1e-15);
    }

    #[test]
    fn prophoto_standard_toe_is_linear() {
        // Below 1/32 the standard curve is exactly x/16.
        assert!((prophoto_rgb_eotf_standard(1.0 / 64.0) - (1.0 / 64.0) / 16.0).abs() < 1e-15);
        // The toe meets the power curve exactly at the x = 1/32 breakpoint — both are 2^-9, since
        // 32 = 2^5 and 5 * 1.8 = 9 — so the curve is continuous there. That continuity is precisely
        // why `<` vs `<=` at the threshold is unobservable (an equivalent mutant, excluded in
        // .cargo/mutants.toml). Above the breakpoint the curve is x^1.8; x = 0.5 pins the power side
        // and catches a mutated `1.0 / 32.0` threshold (e.g. `1.0 % 32.0 == 1.0`, extending the toe).
        assert_eq!(
            prophoto_rgb_eotf_standard(1.0 / 32.0),
            (1.0_f64 / 32.0).powf(1.8)
        );
        assert!((prophoto_rgb_eotf_standard(0.5) - 0.5_f64.powf(1.8)).abs() < 1e-15);
    }

    // `pq_eotf`'s endpoints need no dedicated test: `bt2020_pq_to_sdr_matches_reference_formula`
    // pins x = 0 and 1 at 1e-12 through the composed curve, and the BT.2100 inversion below pins
    // the 0- and 10000-nit anchors independently.

    #[test]
    fn bt2020_pq_to_sdr_matches_reference_formula() {
        // Pin every arithmetic step against an independent powf reference so a
        // mutated operator diverges past 1e-12 across the curve's interior.
        fn reference(x: f64) -> f64 {
            const M1: f64 = 0.1593017578125;
            const M2: f64 = 78.84375;
            const C1: f64 = 0.8359375;
            const C2: f64 = 18.8515625;
            const C3: f64 = 18.6875;
            let n = x.powf(1.0 / M2);
            let num = (n - C1).max(0.0);
            let den = C2 - C3 * n;
            let y = (num / den).powf(1.0 / M1);
            let l = y * 10000.0 / 203.0;
            l / (1.0 + l)
        }
        for &x in &[0.0, 0.05, 0.1, 0.25, 0.5, 0.6, 0.75, 0.9, 1.0] {
            assert!((bt2020_pq_to_sdr(x) - reference(x)).abs() < 1e-12, "at {x}");
        }
    }

    /// Independent oracle for [`pq_eotf`]: the BT.2100 Table 4 *forward* PQ OETF (luminance →
    /// signal) is a different formula arrangement than the inverse `pq_eotf` implements, so
    /// composing them must recover the input. A transcription slip in the inverse (swapped
    /// `c2`/`c3`, a wrong reciprocal) would break the round-trip even though both share the
    /// constants. (`bt2020_pq_full_signal_near_one` was dropped: the reference-formula test above
    /// already pins `bt2020_pq_to_sdr(1.0)` exactly.)
    /// The BT.2100 Table 4 forward PQ OETF, transcribed here independently of the crate's own
    /// [`pq_oetf`]. Kept separate on purpose: it is the reference both `pq_eotf` and `pq_oetf`
    /// are checked against, so it must not be either of them.
    ///
    /// E' = ((c1 + c2·Y^m1) / (1 + c3·Y^m1))^m2, with Y = L / 10000.
    fn bt2100_forward_oetf(nits: f64) -> f64 {
        const M1: f64 = 0.1593017578125;
        const M2: f64 = 78.84375;
        const C1: f64 = 0.8359375;
        const C2: f64 = 18.8515625;
        const C3: f64 = 18.6875;
        let yp = (nits / PQ_PEAK_NITS).powf(M1);
        ((C1 + C2 * yp) / (1.0 + C3 * yp)).powf(M2)
    }

    /// A representative sweep of the PQ domain, from black to the 10000-nit peak.
    const PQ_NITS_SWEEP: [f64; 8] = [0.0, 1.0, 10.0, 100.0, 203.0, 1000.0, 4000.0, 10000.0];

    #[test]
    fn pq_eotf_inverts_bt2100_forward_oetf() {
        for &nits in &PQ_NITS_SWEEP {
            let back = pq_eotf(bt2100_forward_oetf(nits));
            let tol = 1e-9 * nits.max(1.0);
            assert!(
                (back - nits).abs() <= tol,
                "pq round-trip at {nits} nits → {back}"
            );
        }
    }

    /// The public [`pq_oetf`] must equal that same independent reference — not merely invert
    /// `pq_eotf`, which a matching pair of transcription slips could still satisfy.
    #[test]
    fn pq_oetf_matches_bt2100_forward_oetf() {
        for &nits in &PQ_NITS_SWEEP {
            let (got, want) = (pq_oetf(nits), bt2100_forward_oetf(nits));
            assert!(
                (got - want).abs() < 1e-12,
                "pq_oetf({nits}) = {got}, BT.2100 reference {want}"
            );
        }
        // `pq_oetf` clamps its domain, so out-of-range luminance saturates rather than
        // producing a NaN from `powf` on a negative base.
        assert_eq!(pq_oetf(-1.0), pq_oetf(0.0));
        assert_eq!(pq_oetf(20_000.0), pq_oetf(PQ_PEAK_NITS));
    }

    /// H.273 Table 3, code points 1, 6, 14 and 15, transcribed **forward** exactly as the spec
    /// states it. α and β are written out again on purpose: reusing [`BT709_ALPHA`] /
    /// [`BT709_BETA`] would share any mistyped digit with the code under test.
    fn h273_bt709_oetf(lc: f64) -> f64 {
        let (alpha, beta) = (1.099_296_826_809_442, 0.018_053_968_510_807);
        if lc >= beta {
            alpha * lc.powf(0.45) - (alpha - 1.0)
        } else {
            4.500 * lc
        }
    }

    /// Samples on both segments of the BT.709-family curve, either side of β.
    const BT709_LIGHT_SWEEP: [f64; 9] = [0.0, 0.001, 0.01, 0.018, 0.02, 0.1, 0.5, 0.9, 1.0];

    /// H.273 §8.2 defines α and β as the constants that make the two segments meet with
    /// continuous value *and* slope; the stated digits of α and β must satisfy that definition.
    #[test]
    fn bt709_constants_meet_h273_continuity_definition() {
        let value_gap =
            BT709_ALPHA * BT709_BETA.powf(0.45) - (BT709_ALPHA - 1.0) - 4.5 * BT709_BETA;
        assert!(value_gap.abs() < 1e-14, "value gap {value_gap}");
        let slope = 0.45 * BT709_ALPHA * BT709_BETA.powf(-0.55);
        assert!((slope - 4.5).abs() < 1e-12, "slope {slope}");
        assert!((BT709_ALPHA - 1.099_296_826_809_442).abs() < 1e-14);
    }

    #[test]
    fn bt709_oetf_matches_h273_table3() {
        for &lc in &BT709_LIGHT_SWEEP {
            let (got, want) = (bt709_oetf(lc), h273_bt709_oetf(lc));
            assert!(
                (got - want).abs() < 1e-14,
                "bt709_oetf({lc}) = {got}, Table 3 {want}"
            );
        }
        // Table 3 puts `Lc == β` on the power segment (`1 >= Lc >= β`). The two segments differ
        // there only in the last bits, so the exact comparison is what pins the boundary.
        let power = BT709_ALPHA * BT709_BETA.powf(0.45) - (BT709_ALPHA - 1.0);
        assert_ne!(power, 4.5 * BT709_BETA);
        assert_eq!(bt709_oetf(BT709_BETA), power);
    }

    #[test]
    fn bt709_eotf_inverts_h273_table3() {
        for &lc in &BT709_LIGHT_SWEEP {
            let back = bt709_eotf(h273_bt709_oetf(lc));
            assert!((back - lc).abs() < 1e-12, "bt709_eotf(oetf({lc})) = {back}");
        }
        // Mid-grey anchor, from inverting Table 3 row 14 independently (issue #605).
        assert!((bt709_eotf(0.5) - 0.259_719_437).abs() < 1e-9);
        // `V == 4.5 β` is on the power segment: the toe's image is `V < 4.5 β` (it covers
        // `Lc < β`). As above, the two segments differ there only in the last bits.
        let knee = 4.5 * BT709_BETA;
        let power = ((knee + BT709_ALPHA - 1.0) / BT709_ALPHA).powf(1.0 / 0.45);
        assert_ne!(power, knee / 4.5);
        assert_eq!(bt709_eotf(knee), power);
    }

    #[test]
    fn eotf_for_dispatch() {
        assert!(eotf_for(TransferCharacteristics::Srgb).is_some());
        assert!(eotf_for(TransferCharacteristics::Pq).is_some());
        assert!(eotf_for(TransferCharacteristics::Hlg).is_none());
        assert!(eotf_for(TransferCharacteristics::Unspecified).is_none());
        // Code points 1 and 14 are one Table 3 curve; each alternative of the or-pattern is
        // asserted on its own. At 0.5 the BT.709 curve and the PQ tone map differ by ~0.05.
        for tc in [
            TransferCharacteristics::Bt709,
            TransferCharacteristics::Bt2020_10,
        ] {
            let f = eotf_for(tc).unwrap();
            assert_eq!(f(0.5), bt709_eotf(0.5), "{tc:?}");
        }
        let f = eotf_for(TransferCharacteristics::Pq).unwrap();
        assert_eq!(f(0.5), bt2020_pq_to_sdr(0.5));
        assert!((bt2020_pq_to_sdr(0.5) - bt709_eotf(0.5)).abs() > 0.05);
        // The Srgb dispatch is the sRGB EOTF.
        let f = eotf_for(TransferCharacteristics::Srgb).unwrap();
        assert_eq!(f(0.5), srgb_eotf(0.5));
        // The Linear dispatch is the identity across the domain — distinct from every
        // other modelled curve, which bend away from `x` in the interior.
        let f = eotf_for(TransferCharacteristics::Linear).unwrap();
        for &x in &[0.0, 0.25, 0.5, 0.75, 1.0] {
            assert_eq!(f(x), x);
        }
    }

    #[test]
    fn oetf_for_dispatch() {
        // Some for exactly the invertible code points, None for the rest. Spelled out per
        // variant rather than as a loop so a mutant collapsing one arm cannot hide.
        assert!(oetf_for(TransferCharacteristics::Linear).is_some());
        assert!(oetf_for(TransferCharacteristics::Srgb).is_some());
        assert!(oetf_for(TransferCharacteristics::Pq).is_none());
        assert!(oetf_for(TransferCharacteristics::Hlg).is_none());
        assert!(oetf_for(TransferCharacteristics::Unspecified).is_none());

        // The Srgb dispatch is the sRGB OETF, not the EOTF: at 0.5 the two differ by ~0.52, so
        // a mutant returning `srgb_eotf` here is caught.
        let f = oetf_for(TransferCharacteristics::Srgb).unwrap();
        assert_eq!(f(0.5), srgb_oetf(0.5));
        assert!((f(0.5) - srgb_eotf(0.5)).abs() > 0.5);

        // The Linear dispatch is the identity.
        let f = oetf_for(TransferCharacteristics::Linear).unwrap();
        for &x in &[0.0, 0.25, 0.5, 0.75, 1.0] {
            assert_eq!(f(x), x);
        }

        // Code points 1 and 14 both encode through Table 3's curve, each alternative asserted on
        // its own.
        for tc in [
            TransferCharacteristics::Bt709,
            TransferCharacteristics::Bt2020_10,
        ] {
            let f = oetf_for(tc).unwrap();
            assert_eq!(f(0.5), bt709_oetf(0.5), "{tc:?}");
        }
    }

    /// Where both dispatches answer, they are exact inverses — the property that makes
    /// `oetf_for` usable for the encode direction of a CICP-tagged conversion.
    ///
    /// The sample points avoid sRGB's piecewise breakpoint, which is a special case in its own
    /// right (see `srgb_breakpoint_round_trip_is_inexact_by_construction`).
    #[test]
    fn oetf_for_inverts_eotf_for() {
        for tc in [
            TransferCharacteristics::Linear,
            TransferCharacteristics::Srgb,
            TransferCharacteristics::Bt709,
            TransferCharacteristics::Bt2020_10,
        ] {
            let eotf = eotf_for(tc).expect("modeled");
            let oetf = oetf_for(tc).expect("modeled");
            for &x in &[0.0, 0.01, 0.03, 0.2, 0.5, 0.8, 1.0] {
                let back = oetf(eotf(x));
                assert!((back - x).abs() < 1e-12, "{tc:?}: oetf(eotf({x})) = {back}");
            }
        }
    }

    /// sRGB's two published breakpoints are not exact reciprocals: IEC 61966-2-1 gives the decode
    /// threshold as 0.04045 and the encode threshold as 0.0031308, but 0.04045 / 12.92 =
    /// 0.0031308049…, which is *above* the encode threshold. So a value decoded at the breakpoint
    /// re-encodes through the power branch rather than the linear one, and the round trip is off
    /// by ~3e-8. That is a property of the standard's constants, not of this implementation —
    /// pinned here so the looser tolerance is a documented fact rather than a silent fudge.
    #[test]
    fn srgb_breakpoint_round_trip_is_inexact_by_construction() {
        const DECODE_THRESHOLD: f64 = 0.04045;
        let back = srgb_oetf(srgb_eotf(DECODE_THRESHOLD));
        let err = (back - DECODE_THRESHOLD).abs();
        assert!(
            err > 1e-12,
            "expected the known breakpoint mismatch, got {err}"
        );
        assert!(
            err < 1e-7,
            "breakpoint error grew beyond the known ~3e-8: {err}"
        );
        // The cause: the decoded value lands just above the encode threshold.
        assert!(srgb_eotf(DECODE_THRESHOLD) > 0.0031308);
    }

    /// `oetf_for` declines strictly more than `eotf_for`, and PQ is the whole of the difference.
    /// This pins the asymmetry as deliberate rather than an oversight.
    #[test]
    fn pq_is_the_only_code_point_with_an_eotf_but_no_oetf() {
        for code in 0..=u16::from(u8::MAX) {
            let Some(tc) = TransferCharacteristics::from_code_point(code) else {
                continue;
            };
            if oetf_for(tc).is_some() {
                assert!(
                    eotf_for(tc).is_some(),
                    "{tc:?} has an inverse but no forward"
                );
            }
            let asymmetric = eotf_for(tc).is_some() && oetf_for(tc).is_none();
            assert_eq!(
                asymmetric,
                tc == TransferCharacteristics::Pq,
                "{tc:?} disagrees with the PQ-only asymmetry"
            );
        }
    }
}
