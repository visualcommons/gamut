//! Reference luminance levels and luma weights, shared across the colour, tone-mapping, and
//! conversion paths so a single definition is authoritative.
//!
//! Two independent groups live here:
//!
//! - **Reference levels** in candela per square metre (cd/m², "nits"). Provenance is recorded in
//!   `references/color/README.md` (Report ITU-R BT.2408 — HDR Reference White; SMPTE ST 2084 /
//!   ITU-R BT.2100 — PQ peak).
//! - **Luma weights** — the `KR`/`KG`/`KB` coefficients that collapse an RGB triple to a single
//!   luma sample, as fixed-point integers (see [`LUMA_FIX`]). They are what
//!   [`crate::convert`] uses for an RGB → grayscale reduction, and what `gamut_color`'s BT.601
//!   YCbCr luma row is built from.

/// SDR diffuse-white reference luminance (cd/m²): the classic 100-nit reference.
///
/// ITU-R BT.2408 is the framework that maps SDR 100 % diffuse white onto the HDR reference level
/// ([`HDR_REFERENCE_WHITE_NITS`]).
pub const SDR_REFERENCE_WHITE_NITS: f64 = 100.0;

/// HDR Reference White (cd/m²): 203, per Report ITU-R BT.2408.
///
/// The nominal signal level of graphics / diffuse white in PQ and HLG production. (Not "SDR" white —
/// SDR diffuse white is [`SDR_REFERENCE_WHITE_NITS`].)
pub const HDR_REFERENCE_WHITE_NITS: f64 = 203.0;

/// PQ peak luminance (cd/m²): the 10 000-nit maximum of SMPTE ST 2084 / ITU-R BT.2100.
pub const PQ_PEAK_NITS: f64 = 10_000.0;

/// Fractional bits of the fixed-point luma weights: coefficients are scaled by `2^16`.
///
/// A weighted sum is therefore `(wr*r + wg*g + wb*b + HALF) >> LUMA_FIX`, where `HALF` is
/// [`LUMA_ONE`]`/2`. Sixteen bits is the scale libjpeg and libwebp use, so the BT.601 row below is
/// bit-identical to those implementations rather than merely close.
pub const LUMA_FIX: u32 = 16;

/// The fixed-point representation of `1.0` at [`LUMA_FIX`] fractional bits (`65536`).
///
/// Each weight triple sums to exactly this value, so a weighted sum of equal channels reproduces
/// that channel — a neutral grey converts to itself with no drift.
pub const LUMA_ONE: u32 = 1 << LUMA_FIX;

/// BT.601 / SMPTE 170M luma weights (`KR = 0.299`, `KG = 0.587`, `KB = 0.114`) at [`LUMA_FIX`].
///
/// The JFIF/libjpeg row, and the coefficients `gamut_color::bt601_rgb_to_ycbcr` applies on its
/// full-range path.
pub const BT601_LUMA_WEIGHTS: [u32; 3] = [19_595, 38_470, 7_471];

/// BT.709 luma weights (`KR = 0.2126`, `KG = 0.7152`, `KB = 0.0722`) at [`LUMA_FIX`].
///
/// The sRGB/HD row — the default for an RGB → grayscale reduction, since gamut's 8- and 16-bit
/// interleaved buffers are sRGB-encoded unless a colour profile says otherwise.
pub const BT709_LUMA_WEIGHTS: [u32; 3] = [13_933, 46_871, 4_732];

/// BT.2020 / BT.2100 luma weights (`KR = 0.2627`, `KG = 0.6780`, `KB = 0.0593`) at [`LUMA_FIX`].
///
/// The wide-gamut UHD row, for callers whose samples carry BT.2020 primaries. Rounding all three
/// coefficients to nearest would sum to `65535`, one short of [`LUMA_ONE`]; the spare unit goes to
/// green by largest-remainder apportionment (green's remainder, `0.4`, is the largest of the
/// three), so the sum is exact and every weight stays within one unit of its real coefficient.
pub const BT2020_LUMA_WEIGHTS: [u32; 3] = [17_216, 44_434, 3_886];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_levels_match_standards() {
        // Pin the literals against their standards so an accidental edit is caught.
        assert_eq!(SDR_REFERENCE_WHITE_NITS, 100.0);
        assert_eq!(HDR_REFERENCE_WHITE_NITS, 203.0);
        assert_eq!(PQ_PEAK_NITS, 10_000.0);
        // HDR Reference White is 2.03x the SDR reference (the gamut-tonemap default white point).
        assert!((HDR_REFERENCE_WHITE_NITS / SDR_REFERENCE_WHITE_NITS - 2.03).abs() < 1e-12);
    }

    #[test]
    fn luma_weights_sum_to_unity() {
        // The invariant the conversion path depends on: a neutral grey must map to itself. If a
        // triple did not sum to LUMA_ONE, every grey would drift by the shortfall.
        for weights in [BT601_LUMA_WEIGHTS, BT709_LUMA_WEIGHTS, BT2020_LUMA_WEIGHTS] {
            assert_eq!(weights.iter().sum::<u32>(), LUMA_ONE);
        }
        assert_eq!(LUMA_ONE, 65_536);
        assert_eq!(LUMA_FIX, 16);
    }

    #[test]
    fn luma_weights_match_their_standard_coefficients() {
        // Each integer weight is the standard's real coefficient scaled by LUMA_ONE. Rounding to
        // nearest is the rule, but the sum-to-unity invariant above wins where the two conflict
        // (BT.2020 needs one spare unit apportioned to green), so the bound here is one unit.
        let cases: [([u32; 3], [f64; 3]); 3] = [
            (BT601_LUMA_WEIGHTS, [0.299, 0.587, 0.114]),
            (BT709_LUMA_WEIGHTS, [0.2126, 0.7152, 0.0722]),
            (BT2020_LUMA_WEIGHTS, [0.2627, 0.6780, 0.0593]),
        ];
        for (fixed, real) in cases {
            for (got, want) in fixed.iter().zip(real) {
                let exact = want * f64::from(LUMA_ONE);
                assert!(
                    (f64::from(*got) - exact).abs() <= 1.0,
                    "{got} is not {want} at {LUMA_FIX} fractional bits (exact {exact})"
                );
            }
        }
        // BT.601 and BT.709 have no such conflict, so they are pinned to nearest-rounding exactly.
        assert_eq!(BT601_LUMA_WEIGHTS, [19_595, 38_470, 7_471]);
        assert_eq!(BT709_LUMA_WEIGHTS, [13_933, 46_871, 4_732]);
    }

    #[test]
    fn a_neutral_grey_maps_to_itself() {
        // The practical consequence of sum-to-unity, and the property `convert` relies on when it
        // runs the luma reduction unconditionally for a grayscale target: applying any weight
        // triple to three equal channels must reproduce that channel for every input value.
        for weights in [BT601_LUMA_WEIGHTS, BT709_LUMA_WEIGHTS, BT2020_LUMA_WEIGHTS] {
            for grey in [0u16, 1, 127, 128, 32_768, 65_534, u16::MAX] {
                let value = u64::from(grey);
                let sum: u64 = weights.iter().map(|&w| u64::from(w) * value).sum();
                let luma = (sum + u64::from(LUMA_ONE / 2)) >> LUMA_FIX;
                assert_eq!(luma, value, "{weights:?} drifted on grey {grey}");
            }
        }
    }

    #[test]
    fn distinct_standards_have_distinct_weights() {
        // Guards against a copy-paste that would silently make one standard alias another.
        assert_ne!(BT601_LUMA_WEIGHTS, BT709_LUMA_WEIGHTS);
        assert_ne!(BT709_LUMA_WEIGHTS, BT2020_LUMA_WEIGHTS);
        assert_ne!(BT601_LUMA_WEIGHTS, BT2020_LUMA_WEIGHTS);
    }
}
