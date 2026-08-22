//! VP8 dequantization (RFC 6386 §9.6, §14.1).
//!
//! The frame header carries a base quantizer index plus per-plane deltas
//! ([`QuantIndices`](super::header::QuantIndices)); this module maps those indices through the fixed
//! DC/AC lookup tables to the six per-plane dequant factors (Y1 DC/AC, Y2 DC/AC, UV DC/AC) used
//! during reconstruction, and provides the matching forward quantization for the encoder.
//!
//! The lookup tables are RFC 6386 §14.1 (`dc_qlookup` / `ac_qlookup`); the factor scaling and
//! clamping (Y2 DC ×2, Y2 AC ×155/100 floored at 8, UV DC capped at 132, every index clamped to
//! `0..=127`) is the dixie `dequant_init` reference (§20.4). Dequant products are stored as 16-bit
//! signed integers, as the spec mandates. Tracked in `../STATUS.md` section L.

use gamut_dsp::math::round_div_nearest;

use super::header::QuantIndices;

/// `QINDEX_RANGE`: the number of quantizer indices (RFC 6386 §14.1).
const QINDEX_RANGE: usize = 128;

/// DC dequantization factors indexed by quantizer index (RFC 6386 §14.1 `dc_qlookup`).
#[rustfmt::skip]
const DC_QLOOKUP: [i16; QINDEX_RANGE] = [
      4,   5,   6,   7,   8,   9,  10,  10,  11,  12,  13,  14,  15,
     16,  17,  17,  18,  19,  20,  20,  21,  21,  22,  22,  23,  23,
     24,  25,  25,  26,  27,  28,  29,  30,  31,  32,  33,  34,  35,
     36,  37,  37,  38,  39,  40,  41,  42,  43,  44,  45,  46,  46,
     47,  48,  49,  50,  51,  52,  53,  54,  55,  56,  57,  58,  59,
     60,  61,  62,  63,  64,  65,  66,  67,  68,  69,  70,  71,  72,
     73,  74,  75,  76,  76,  77,  78,  79,  80,  81,  82,  83,  84,
     85,  86,  87,  88,  89,  91,  93,  95,  96,  98, 100, 101, 102,
    104, 106, 108, 110, 112, 114, 116, 118, 122, 124, 126, 128, 130,
    132, 134, 136, 138, 140, 143, 145, 148, 151, 154, 157,
];

/// AC dequantization factors indexed by quantizer index (RFC 6386 §14.1 `ac_qlookup`).
#[rustfmt::skip]
const AC_QLOOKUP: [i16; QINDEX_RANGE] = [
      4,   5,   6,   7,   8,   9,  10,  11,  12,  13,  14,  15,  16,
     17,  18,  19,  20,  21,  22,  23,  24,  25,  26,  27,  28,  29,
     30,  31,  32,  33,  34,  35,  36,  37,  38,  39,  40,  41,  42,
     43,  44,  45,  46,  47,  48,  49,  50,  51,  52,  53,  54,  55,
     56,  57,  58,  60,  62,  64,  66,  68,  70,  72,  74,  76,  78,
     80,  82,  84,  86,  88,  90,  92,  94,  96,  98, 100, 102, 104,
    106, 108, 110, 112, 114, 116, 119, 122, 125, 128, 131, 134, 137,
    140, 143, 146, 149, 152, 155, 158, 161, 164, 167, 170, 173, 177,
    181, 185, 189, 193, 197, 201, 205, 209, 213, 217, 221, 225, 229,
    234, 239, 245, 249, 254, 259, 264, 269, 274, 279, 284,
];

/// Clamps a quantizer index to the valid range `0..=127` (dixie `clamp_q`).
fn clamp_q(q: i32) -> usize {
    q.clamp(0, (QINDEX_RANGE - 1) as i32) as usize
}

/// DC dequant factor for quantizer index `q` (after clamping).
fn dc_q(q: i32) -> i16 {
    DC_QLOOKUP[clamp_q(q)]
}

/// AC dequant factor for quantizer index `q` (after clamping).
fn ac_q(q: i32) -> i16 {
    AC_QLOOKUP[clamp_q(q)]
}

/// The six dequantization factors for one quantizer state (RFC 6386 §14.1): DC and AC for each of the
/// Y1 (luma), Y2 (luma-DC WHT), and UV (chroma) planes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantFactors {
    /// Y1 (luma) DC factor.
    pub y1_dc: i16,
    /// Y1 (luma) AC factor.
    pub y1_ac: i16,
    /// Y2 (luma-DC WHT) DC factor (`dc_q ×2`).
    pub y2_dc: i16,
    /// Y2 (luma-DC WHT) AC factor (`ac_q ×155/100`, floored at 8).
    pub y2_ac: i16,
    /// UV (chroma) DC factor (`dc_q`, capped at 132).
    pub uv_dc: i16,
    /// UV (chroma) AC factor.
    pub uv_ac: i16,
}

impl QuantFactors {
    /// Derives the six factors from a base quantizer index `base_q` (the segment-adjusted `q_index`;
    /// pass `i32::from(indices.y_ac)` when segmentation is disabled) and the per-plane deltas in
    /// `indices`, following the dixie `dequant_init` scaling and clamping rules (RFC 6386 §14.1).
    #[must_use]
    pub fn new(base_q: i32, indices: &QuantIndices) -> Self {
        let y2_ac = (i32::from(ac_q(base_q + i32::from(indices.y2_ac_delta))) * 155 / 100).max(8);
        let uv_dc = dc_q(base_q + i32::from(indices.uv_dc_delta)).min(132);
        Self {
            y1_dc: dc_q(base_q + i32::from(indices.y_dc_delta)),
            y1_ac: ac_q(base_q),
            y2_dc: dc_q(base_q + i32::from(indices.y2_dc_delta)) * 2,
            y2_ac: y2_ac as i16,
            uv_dc,
            uv_ac: ac_q(base_q + i32::from(indices.uv_ac_delta)),
        }
    }

    /// Derives the factors for a frame with segmentation disabled, using `indices.y_ac` as the base
    /// quantizer index.
    #[cfg(test)]
    #[must_use]
    pub fn for_frame(indices: &QuantIndices) -> Self {
        Self::new(i32::from(indices.y_ac), indices)
    }
}

/// Dequantizes a coefficient `level` by a dequant `factor`, stored as a 16-bit signed integer
/// (RFC 6386 §14.1): the product wraps to 16 bits exactly as the reference does.
#[must_use]
pub fn dequantize(level: i16, factor: i16) -> i16 {
    (i32::from(level) * i32::from(factor)) as i16
}

/// Forward-quantizes a transform `coeff` by a dequant `factor`, rounding to the nearest level (ties
/// away from zero). Reconstruction re-derives residue through [`dequantize`], so any rounding yields
/// bit-exact recon; nearest minimizes the reconstruction error.
#[must_use]
pub fn quantize(coeff: i16, factor: i16) -> i16 {
    debug_assert!(factor > 0, "dequant factor is always >= 4");
    round_div_nearest(i32::from(coeff), i32::from(factor)) as i16
}

/// Rounding bias for the DC coefficient, in 256ths of a level: **128, i.e. round-to-nearest, with
/// no dead zone at all**.
///
/// This is deliberate rather than an oversight. A DC error shifts a whole 4x4 block's mean, which
/// shows up as blocking — the most objectionable artefact the codec can produce — and on flat
/// content the DC is the only coefficient there is, so a dead zone there costs several dB for
/// almost no rate. Dead-zoning DC only pays alongside a rate-distortion search that can veto the
/// cases where it hurts, which this encoder does not have.
pub const BIAS_DC: u16 = 128;

/// Dead-zone bias for AC coefficients, in 256ths of a level. Below 128, so coefficients just over
/// the halfway mark collapse to zero.
///
/// AC error removes high-frequency detail, which is masked by the detail that remains, and AC
/// coefficients are where nearly all the tokens are — so this is where a dead zone pays.
pub const BIAS_AC: u16 = 120;

/// Forward-quantizes with an explicit rounding `bias` in 256ths of a level: a coefficient becomes
/// `floor(|coeff| / factor + bias / 256)`, signed back.
///
/// A bias under 128 is a **dead zone**. Coefficients that sit just above the halfway mark round
/// down to zero instead of up to one, which costs a little distortion and saves the token, its
/// sign, and often the end-of-block position too — a trade that pays because the rate saved is
/// superlinear in the number of coded coefficients while the distortion added is not.
///
/// The spec has nothing to say here: RFC 6386 defines only *de*quantization, so how an encoder
/// arrives at a level is entirely its own choice, and any choice reconstructs bit-exactly.
#[must_use]
pub fn quantize_biased(coeff: i16, factor: i16, bias: u16) -> i16 {
    debug_assert!(factor > 0, "dequant factor is always >= 4");
    let magnitude = i32::from(coeff).unsigned_abs();
    let factor = u32::from(factor.unsigned_abs());
    // `(|coeff| * 256 + bias * factor) / (factor * 256)`, kept in integers.
    let level = (magnitude * 256 + u32::from(bias) * factor) / (factor * 256);
    let level = level.min(i32::from(i16::MAX) as u32) as i16;
    if coeff < 0 { -level } else { level }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indices(y_ac: u8) -> QuantIndices {
        QuantIndices {
            y_ac,
            y_dc_delta: 0,
            y2_dc_delta: 0,
            y2_ac_delta: 0,
            uv_dc_delta: 0,
            uv_ac_delta: 0,
        }
    }

    #[test]
    fn lookup_tables_have_expected_shape() {
        assert_eq!(DC_QLOOKUP.len(), 128);
        assert_eq!(AC_QLOOKUP.len(), 128);
        // Boundary values from RFC 6386 §14.1.
        assert_eq!((DC_QLOOKUP[0], DC_QLOOKUP[127]), (4, 157));
        assert_eq!((AC_QLOOKUP[0], AC_QLOOKUP[127]), (4, 284));
        // Both tables are monotonically non-decreasing — a structural check against transcription
        // errors.
        assert!(DC_QLOOKUP.windows(2).all(|w| w[0] <= w[1]));
        assert!(AC_QLOOKUP.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn factors_at_q0_apply_floors() {
        // q=0: dc=4, ac=4. Y2 DC = 4*2 = 8; Y2 AC = max(4*155/100=6, 8) = 8; UV DC = min(4,132) = 4.
        let f = QuantFactors::for_frame(&indices(0));
        assert_eq!(
            f,
            QuantFactors {
                y1_dc: 4,
                y1_ac: 4,
                y2_dc: 8,
                y2_ac: 8,
                uv_dc: 4,
                uv_ac: 4,
            }
        );
    }

    #[test]
    fn factors_at_q127_apply_caps() {
        // q=127: dc=157, ac=284. Y2 DC = 314; Y2 AC = 284*155/100 = 440; UV DC = min(157,132) = 132.
        let f = QuantFactors::for_frame(&indices(127));
        assert_eq!(
            f,
            QuantFactors {
                y1_dc: 157,
                y1_ac: 284,
                y2_dc: 314,
                y2_ac: 440,
                uv_dc: 132,
                uv_ac: 284,
            }
        );
    }

    #[test]
    fn uv_dc_cap_engages_at_high_q() {
        // dc_qlookup first reaches 132 at index 117; for any q >= 117 the UV DC factor is capped.
        for q in [117u8, 120, 127] {
            assert_eq!(QuantFactors::for_frame(&indices(q)).uv_dc, 132);
        }
        // Just below the cap it tracks the table (dc_qlookup[116] = 130).
        assert_eq!(QuantFactors::for_frame(&indices(116)).uv_dc, 130);
    }

    #[test]
    fn y2_ac_floor_engages_at_low_q() {
        // ac_q*155/100 < 8 for q in {0,1}; q=2 gives 6*155/100 = 9.
        assert_eq!(QuantFactors::for_frame(&indices(0)).y2_ac, 8);
        assert_eq!(QuantFactors::for_frame(&indices(1)).y2_ac, 8);
        assert_eq!(QuantFactors::for_frame(&indices(2)).y2_ac, 9);
    }

    #[test]
    fn deltas_are_added_then_clamped() {
        // A positive delta past 127 saturates to the top of the table; a negative delta past 0 to the
        // bottom. Base q=125, +10 -> clamp(135)=127 -> dc[127]=157.
        let mut idx = indices(125);
        idx.y_dc_delta = 10;
        assert_eq!(QuantFactors::new(i32::from(idx.y_ac), &idx).y1_dc, 157);
        let mut idx2 = indices(3);
        idx2.uv_ac_delta = -50;
        assert_eq!(
            QuantFactors::new(i32::from(idx2.y_ac), &idx2).uv_ac,
            AC_QLOOKUP[0]
        );
    }

    #[test]
    fn quantize_dequantize_round_trip_is_near() {
        // Nearest quantization keeps |coeff - dequantize(quantize(coeff))| < factor.
        for &factor in &[4i16, 8, 26, 157, 284, 440] {
            for coeff in [-2000i16, -301, -1, 0, 1, 17, 255, 2000] {
                let level = quantize(coeff, factor);
                let back = dequantize(level, factor);
                assert!(
                    (i32::from(coeff) - i32::from(back)).abs() < i32::from(factor),
                    "coeff={coeff} factor={factor} back={back}"
                );
            }
        }
        assert_eq!(quantize(0, 26), 0);
        assert_eq!(dequantize(0, 26), 0);
    }
}
