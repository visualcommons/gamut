//! jpegli's distance-scaled quantization tables (issue #1243): the tables
//! `jpegli_set_distance` builds, and jpegli's quality → distance mapping.
//!
//! Transcribed from libjxl v0.11.2 `lib/jpegli/quant.cc` (`SetQuantMatrices` and the constants it
//! reads) and `lib/jpegli/encode.cc` (`jpegli_set_distance`, `jpegli_quality_to_distance`),
//! vendored and hashed in `references/jpegli/`. jpegli computes in `float`, and so does this code:
//! literals jpegli writes with an `f` suffix are `f32` here, and those it writes as `double` and
//! narrows to `float` are `f64` here and narrowed by `as f32` at the point of use, so every
//! constant rounds exactly as jpegli's does.

use super::LUMINANCE;

/// `kGlobalScaleXYB`: the XYB tables' global multiplier (jpegli's literal `1.43951668f`).
#[expect(
    clippy::excessive_precision,
    reason = "jpegli's float literal verbatim, so the constant and its citation compare as text"
)]
const JPEGLI_GLOBAL_SCALE_XYB: f32 = 1.439_516_68;

/// `kGlobalScaleYCbCr`: the YCbCr tables' global multiplier (jpegli's literal `1.73966010f`).
#[expect(
    clippy::excessive_precision,
    reason = "jpegli's float literal verbatim, so the constant and its citation compare as text"
)]
const JPEGLI_GLOBAL_SCALE_YCBCR: f32 = 1.739_660_10;

/// `k420GlobalScale` (a `double` literal narrowed to `float`): the extra global multiplier of a
/// 4:2:0 YCbCr frame.
const JPEGLI_420_GLOBAL_SCALE: f64 = 1.22;

/// The `global_scale = 0.01f` of `SetQuantMatrices`' standard-table branch (grayscale): it turns
/// the percentage `DistanceToLinearQuality` returns into a fraction.
const JPEGLI_STD_GLOBAL_SCALE: f32 = 0.01;

/// `kDist0`: the distance below which a table scales linearly with distance.
const JPEGLI_DIST0: f32 = 1.5;

/// `kBaseQuantMatrixXYB`: the X, Y and B base tables, natural order, 64 entries each.
#[expect(
    clippy::excessive_precision,
    reason = "jpegli's float literals verbatim, so the table and its citation compare as text"
)]
const JPEGLI_BASE_XYB: [[f32; 64]; 3] = [
    [
        7.5629935265,
        19.8247814178,
        22.5724945068,
        20.6706695557, //
        22.6864585876,
        23.5696277618,
        25.8129081726,
        36.3307571411, //
        19.8247814178,
        21.5503177643,
        19.9372234344,
        20.5424213409, //
        21.8645496368,
        23.9041385651,
        28.2844066620,
        32.6609764099, //
        22.5724945068,
        19.9372234344,
        21.9017257690,
        19.1223449707, //
        21.7515811920,
        24.6724700928,
        25.4249649048,
        32.6653823853, //
        20.6706695557,
        20.5424213409,
        19.1223449707,
        20.1610221863, //
        25.3719692230,
        25.9668903351,
        30.9804954529,
        31.3406009674, //
        22.6864585876,
        21.8645496368,
        21.7515811920,
        25.3719692230, //
        26.2431850433,
        40.5992202759,
        43.2624626160,
        63.3010940552, //
        23.5696277618,
        23.9041385651,
        24.6724700928,
        25.9668903351, //
        40.5992202759,
        48.3026771545,
        34.0964355469,
        61.9852142334, //
        25.8129081726,
        28.2844066620,
        25.4249649048,
        30.9804954529, //
        43.2624626160,
        34.0964355469,
        34.4937438965,
        66.9702758789, //
        36.3307571411,
        32.6609764099,
        32.6653823853,
        31.3406009674, //
        63.3010940552,
        61.9852142334,
        66.9702758789,
        39.9652709961, //
    ],
    [
        1.6262000799,
        3.2199242115,
        3.4903779030,
        3.9148359299, //
        4.8337211609,
        4.9108843803,
        5.3137121201,
        6.1676793098, //
        3.2199242115,
        3.4547898769,
        3.6036829948,
        4.2652835846, //
        4.8368387222,
        4.8226222992,
        5.6120514870,
        6.3431472778, //
        3.4903779030,
        3.6036829948,
        3.9044559002,
        4.3374395370, //
        4.8435096741,
        5.4057979584,
        5.6066360474,
        6.1075134277, //
        3.9148359299,
        4.2652835846,
        4.3374395370,
        4.6064834595, //
        5.1751475334,
        5.4013924599,
        6.0399808884,
        6.7825231552, //
        4.8337211609,
        4.8368387222,
        4.8435096741,
        5.1751475334, //
        5.3748049736,
        6.1410837173,
        7.6529307365,
        7.5235214233, //
        4.9108843803,
        4.8226222992,
        5.4057979584,
        5.4013924599, //
        6.1410837173,
        6.3431472778,
        7.1083049774,
        7.6008300781, //
        5.3137121201,
        5.6120514870,
        5.6066360474,
        6.0399808884, //
        7.6529307365,
        7.1083049774,
        7.0943155289,
        7.0478363037, //
        6.1676793098,
        6.3431472778,
        6.1075134277,
        6.7825231552, //
        7.5235214233,
        7.6008300781,
        7.0478363037,
        6.9186143875, //
    ],
    [
        3.3038473129,
        10.0689258575,
        12.2785224915,
        14.6041173935, //
        16.2107315063,
        19.2314529419,
        28.0129547119,
        55.6682891846, //
        10.0689258575,
        11.4085016251,
        11.3871345520,
        15.4934167862, //
        16.5364933014,
        14.9153423309,
        26.3748722076,
        40.8614425659, //
        12.2785224915,
        11.3871345520,
        17.0886878967,
        13.9500350952, //
        16.0003223419,
        28.5660629272,
        26.2124195099,
        30.1260128021, //
        14.6041173935,
        15.4934167862,
        13.9500350952,
        21.1235027313, //
        26.1579780579,
        25.5579223633,
        40.6859359741,
        33.8056335449, //
        16.2107315063,
        16.5364933014,
        16.0003223419,
        26.1579780579, //
        26.8042831421,
        26.1587715149,
        35.7343978882,
        43.6857032776, //
        19.2314529419,
        14.9153423309,
        28.5660629272,
        25.5579223633, //
        26.1587715149,
        34.5418128967,
        41.3197937012,
        48.7867660522, //
        28.0129547119,
        26.3748722076,
        26.2124195099,
        40.6859359741, //
        35.7343978882,
        41.3197937012,
        47.6329460144,
        55.3498458862, //
        55.6682891846,
        40.8614425659,
        30.1260128021,
        33.8056335449, //
        43.6857032776,
        48.7867660522,
        55.3498458862,
        63.6065597534, //
    ],
];

/// `kBaseQuantMatrixYCbCr` (`double` literals narrowed to `float`): the Y, Cb and Cr base tables,
/// natural order, 64 entries each.
const JPEGLI_BASE_YCBCR: [[f64; 64]; 3] = [
    [
        1.2397409345866273,
        1.7227115097630963,
        2.9212167156636855,
        2.812737435286529, //
        3.339819711906184,
        3.463603762596166,
        3.840915217993518,
        3.86956, //
        1.7227115097630963,
        2.0928894413636874,
        2.8456760904429297,
        2.704506820909662, //
        3.4407673520905337,
        3.166232352090534,
        4.025208741558432,
        4.035324490952577, //
        2.9212167156636855,
        2.8456760904429297,
        2.9587403520905338,
        3.3862948970669273, //
        3.619523781336757,
        3.9046279999999998,
        3.757835838431854,
        4.237447515714274, //
        2.812737435286529,
        2.704506820909662,
        3.3862948970669273,
        3.380058821812233, //
        4.1679867415584315,
        4.805510627261856,
        4.784259,
        4.605934, //
        3.339819711906184,
        3.4407673520905337,
        3.619523781336757,
        4.1679867415584315, //
        4.579851258441568,
        4.923237,
        5.574107,
        5.48533336146308, //
        3.463603762596166,
        3.166232352090534,
        3.9046279999999998,
        4.805510627261856, //
        4.923237,
        5.43936,
        5.093895741558431,
        6.0872254423617225, //
        3.840915217993518,
        4.025208741558432,
        3.757835838431854,
        4.784259, //
        5.574107,
        5.093895741558431,
        5.438461,
        5.4037359493250845, //
        3.86956,
        4.035324490952577,
        4.237447515714274,
        4.605934, //
        5.48533336146308,
        6.0872254423617225,
        5.4037359493250845,
        4.37787101190424, //
    ],
    [
        2.8236197786377537,
        6.495639358561486,
        9.310489207538302,
        10.64747864717083, //
        11.07419143098738,
        17.146390223910462,
        18.463982229408998,
        29.087001644203088, //
        6.495639358561486,
        8.890103846667353,
        8.976895794294748,
        13.666270550318826, //
        16.547071905624193,
        16.63871382827686,
        26.778396930893695,
        21.33034294694781, //
        9.310489207538302,
        8.976895794294748,
        11.08737706005991,
        18.20548239870446, //
        19.752481654011646,
        23.985660533114896,
        102.6457378402362,
        24.450989, //
        10.64747864717083,
        13.666270550318826,
        18.20548239870446,
        18.628012327860365, //
        16.042509519487183,
        25.04918273242625,
        25.017140189353015,
        35.79788782635831, //
        11.07419143098738,
        16.547071905624193,
        19.752481654011646,
        16.042509519487183, //
        19.373482748612577,
        14.677529999999999,
        19.94695960400931,
        51.094112, //
        17.146390223910462,
        16.63871382827686,
        23.985660533114896,
        25.04918273242625, //
        14.677529999999999,
        31.320412426835304,
        46.357234000000005,
        67.48111451705412, //
        18.463982229408998,
        26.778396930893695,
        102.6457378402362,
        25.017140189353015, //
        19.94695960400931,
        46.357234000000005,
        61.315764694388044,
        88.34665293823721, //
        29.087001644203088,
        21.33034294694781,
        24.450989,
        35.79788782635831, //
        51.094112,
        67.48111451705412,
        88.34665293823721,
        112.16099098350989, //
    ],
    [
        2.9217254961255255,
        4.497681013199305,
        7.356344520940414,
        6.583891506504051, //
        8.535608740100237,
        8.799434353234647,
        9.188341534163023,
        9.482700481227672, //
        4.497681013199305,
        6.309548851989123,
        7.024608962670982,
        7.156445324163424, //
        8.049059218663244,
        7.0124290657218555,
        6.711923184393611,
        8.380307846134853, //
        7.356344520940414,
        7.024608962670982,
        6.892101177327445,
        6.882819916277163, //
        8.782226090078568,
        6.8774750000000004,
        7.8858175969577955,
        8.67909, //
        6.583891506504051,
        7.156445324163424,
        6.882819916277163,
        7.003072944847055, //
        7.7223464701024875,
        7.955425720217421,
        7.4734110000000005,
        8.362933242943903, //
        8.535608740100237,
        8.049059218663244,
        8.782226090078568,
        7.7223464701024875, //
        6.778005927001542,
        9.484922741558432,
        9.043702663686046,
        8.053178199770173, //
        8.799434353234647,
        7.0124290657218555,
        6.8774750000000004,
        7.955425720217421, //
        9.484922741558432,
        8.607606527385098,
        9.922697394370815,
        64.25135180237939, //
        9.188341534163023,
        6.711923184393611,
        7.8858175969577955,
        7.4734110000000005, //
        9.043702663686046,
        9.922697394370815,
        63.184936549738225,
        83.35294340273799, //
        9.482700481227672,
        8.380307846134853,
        8.67909,
        8.362933242943903, //
        8.053178199770173,
        64.25135180237939,
        83.35294340273799,
        114.89202448569779, //
    ],
];

/// `k420Rescale` (`double` literals narrowed to `float`): the per-coefficient multiplier of both
/// chroma tables in a 4:2:0 YCbCr frame, natural order.
const JPEGLI_420_RESCALE: [f64; 64] = [
    0.4093, 0.3209, 0.3477, 0.3333, 0.3144, 0.2823, 0.3214, 0.3354, //
    0.3209, 0.3111, 0.3489, 0.2801, 0.3059, 0.3119, 0.4135, 0.3445, //
    0.3477, 0.3489, 0.3586, 0.3257, 0.2727, 0.3754, 0.3369, 0.3484, //
    0.3333, 0.2801, 0.3257, 0.3020, 0.3515, 0.3410, 0.3971, 0.3839, //
    0.3144, 0.3059, 0.2727, 0.3515, 0.3105, 0.3397, 0.2716, 0.3836, //
    0.2823, 0.3119, 0.3754, 0.3410, 0.3397, 0.3212, 0.3203, 0.0726, //
    0.3214, 0.4135, 0.3369, 0.3971, 0.2716, 0.3203, 0.0798, 0.0553, //
    0.3354, 0.3445, 0.3484, 0.3839, 0.3836, 0.0726, 0.0553, 0.3368, //
];

/// `kExponent`: the per-coefficient exponent of `DistanceToScale`'s non-linear branch, natural
/// order.
const JPEGLI_EXPONENT: [f32; 64] = [
    1.00, 0.51, 0.67, 0.74, 1.00, 1.00, 1.00, 1.00, //
    0.51, 0.66, 0.69, 0.87, 1.00, 1.00, 1.00, 1.00, //
    0.67, 0.69, 0.84, 0.83, 0.96, 1.00, 1.00, 1.00, //
    0.74, 0.87, 0.83, 1.00, 1.00, 0.91, 0.91, 1.00, //
    1.00, 1.00, 0.96, 1.00, 1.00, 1.00, 1.00, 1.00, //
    1.00, 1.00, 1.00, 0.91, 1.00, 1.00, 1.00, 1.00, //
    1.00, 1.00, 1.00, 0.91, 1.00, 1.00, 1.00, 1.00, //
    1.00, 1.00, 1.00, 1.00, 1.00, 1.00, 1.00, 1.00, //
];

/// jpegli's quality → butteraugli distance mapping, `jpegli_quality_to_distance` (libjxl v0.11.2
/// `lib/jpegli/encode.cc`) bit for bit: `0.01` from quality 100 up, `0.1 + 0.09·(100 − q)` from 30
/// to 99, and the quadratic `53/3000·q² − 23/20·q + 25` below 30 (so quality 0 is distance 25).
///
/// `cjpegli -q` encodes at the distance this returns; hand it to
/// [`JpegEncoder::with_distance`](crate::JpegEncoder::with_distance) to do the same. Defined for
/// every `u8`, as jpegli's is for every `int`.
#[must_use]
pub fn jpegli_quality_to_distance(quality: u8) -> f32 {
    let q = f32::from(quality);
    if quality >= 100 {
        0.01
    } else if quality >= 30 {
        0.1 + f32::from(100 - quality) * 0.09
    } else {
        // Left to right, as jpegli's single-precision expression evaluates.
        53.0 / 3000.0 * q * q - 23.0 / 20.0 * q + 25.0
    }
}

/// `DistanceToScale`: the factor a non-linear (XYB/YCbCr) table entry at natural position `k` is
/// scaled by. Linear in `distance` below [`JPEGLI_DIST0`]; above it, the larger of `distance / 2`
/// and `1.5^(1 − e)·distance^e` for the coefficient's exponent `e` — continuous at the knee, where
/// both equal `distance`. `1.5^(1 − e)` is computed in `f64` and narrowed, as jpegli's
/// `std::pow(kDist0, 1.0 - exp)` is.
fn jpegli_distance_to_scale(distance: f32, k: usize) -> f32 {
    if distance < JPEGLI_DIST0 {
        return distance;
    }
    let exp = JPEGLI_EXPONENT[k];
    let mul = f64::from(JPEGLI_DIST0).powf(1.0 - f64::from(exp)) as f32;
    (0.5 * distance).max(mul * distance.powf(exp))
}

/// `DistanceToLinearQuality`: the IJG scale percentage (`5000/q` or `200 − 2·q`, as a real
/// number) jpegli's standard-table branch uses for `distance` — the inverse of its quality →
/// distance mapping, piecewise like it, saturating at 1 below distance 0.1 and at 5000 from 25.
fn jpegli_distance_to_linear_quality(distance: f32) -> f32 {
    if distance <= 0.1 {
        1.0
    } else if distance <= 4.6 {
        (200.0 / 9.0) * (distance - 0.1)
    } else if distance <= 6.4 {
        5000.0 / (100.0 - (distance - 0.1) / 0.09)
    } else if distance < 25.0 {
        530_000.0 / (3450.0 - 300.0 * ((848.0 * distance - 5330.0) / 120.0).sqrt())
    } else {
        5000.0
    }
}

/// One `SetQuantMatrices` table: each entry `round(scale(k) · base[k])` clamped to the baseline
/// range `1..=255` (`force_baseline = TRUE`, as `cjpegli` sets it). `round` is half away from
/// zero, as C's `roundf`; a saturating `as` keeps an infinite product at the ceiling.
fn jpegli_table(base: impl Fn(usize) -> f32, scale: impl Fn(usize) -> f32) -> [u16; 64] {
    std::array::from_fn(|k| ((scale(k) * base(k)).round() as i32).clamp(1, 255) as u16)
}

/// The frame a jpegli distance table set is built for: what `SetQuantMatrices` branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JpegliFrame {
    /// One grayscale component: jpegli's standard-table branch, the Annex K luminance table
    /// scaled linearly.
    Gray,
    /// YCbCr; `yuv420` is jpegli's `IsYUV420` (Y sampled 2×2, Cb and Cr 1×1), which scales the
    /// chroma tables by [`JPEGLI_420_RESCALE`] and every table by [`JPEGLI_420_GLOBAL_SCALE`].
    Ycbcr {
        /// Whether the frame is 4:2:0.
        yuv420: bool,
    },
    /// jpegli's XYB mode: one table per channel (X, Y, B).
    Xyb,
}

/// The tables `jpegli_set_distance(distance, TRUE)` gives a `frame`, in DQT destination order: one
/// table for [`JpegliFrame::Gray`]; three for YCbCr (Y, Cb, Cr — `jpegli_set_distance` gives Cr a
/// table of its own) and for XYB (X, Y, B). Every entry is in `1..=255`.
pub(crate) fn jpegli_tables(distance: f32, frame: JpegliFrame) -> Vec<[u16; 64]> {
    match frame {
        JpegliFrame::Gray => {
            let scale = JPEGLI_STD_GLOBAL_SCALE * jpegli_distance_to_linear_quality(distance);
            vec![jpegli_table(|k| f32::from(LUMINANCE[k]), |_| scale)]
        }
        JpegliFrame::Ycbcr { yuv420 } => {
            let global = if yuv420 {
                JPEGLI_GLOBAL_SCALE_YCBCR * JPEGLI_420_GLOBAL_SCALE as f32
            } else {
                JPEGLI_GLOBAL_SCALE_YCBCR
            };
            JPEGLI_BASE_YCBCR
                .iter()
                .enumerate()
                .map(|(t, base)| {
                    // Only the chroma tables (destinations 1 and 2) take the 4:2:0 rescale.
                    let rescale = yuv420 && t > 0;
                    jpegli_table(
                        |k| base[k] as f32,
                        |k| {
                            let s = global * jpegli_distance_to_scale(distance, k);
                            if rescale {
                                s * JPEGLI_420_RESCALE[k] as f32
                            } else {
                                s
                            }
                        },
                    )
                })
                .collect()
        }
        JpegliFrame::Xyb => JPEGLI_BASE_XYB
            .iter()
            .map(|base| {
                jpegli_table(
                    |k| base[k],
                    |k| JPEGLI_GLOBAL_SCALE_XYB * jpegli_distance_to_scale(distance, k),
                )
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts `got` is within 1e-5 of the real-number `want`.
    fn close(got: f32, want: f64) {
        assert!((f64::from(got) - want).abs() < 1e-5, "{got} vs {want}");
    }

    #[test]
    fn jpegli_quality_to_distance_takes_each_branch_at_its_bounds() {
        // 100 and above: 0.01. 30..=99: 0.1 + 0.09·(100 − q). Below 30: 53/3000·q² − 23/20·q + 25.
        assert_eq!(jpegli_quality_to_distance(100), 0.01);
        assert_eq!(jpegli_quality_to_distance(255), 0.01);
        close(jpegli_quality_to_distance(99), 0.19);
        close(jpegli_quality_to_distance(90), 1.0);
        close(jpegli_quality_to_distance(30), 6.4);
        close(
            jpegli_quality_to_distance(29),
            53.0 / 3000.0 * 841.0 - 23.0 / 20.0 * 29.0 + 25.0,
        );
        close(jpegli_quality_to_distance(10), 53.0 / 30.0 - 11.5 + 25.0);
        assert_eq!(jpegli_quality_to_distance(0), 25.0);
    }

    #[test]
    fn jpegli_distance_to_scale_is_linear_below_the_knee_and_bounded_by_half_above() {
        // Below 1.5 the scale is the distance itself, at every coefficient.
        assert_eq!(jpegli_distance_to_scale(1.0, 1), 1.0);
        assert_eq!(jpegli_distance_to_scale(1.49, 63), 1.49);
        // At exponent 1 (k = 0) the power law is the identity: 1.5^0 · 3 = 3.
        close(jpegli_distance_to_scale(3.0, 0), 3.0);
        // At k = 1 (exponent 0.51): 1.5^0.49 · 3^0.51 beats 3/2 ...
        close(
            jpegli_distance_to_scale(3.0, 1),
            1.5f64.powf(0.49) * 3f64.powf(0.51),
        );
        // ... but at 10 the power law falls below 10/2, which then wins.
        assert_eq!(jpegli_distance_to_scale(10.0, 1), 5.0);
    }

    #[test]
    fn jpegli_distance_to_linear_quality_takes_each_branch() {
        assert_eq!(jpegli_distance_to_linear_quality(0.05), 1.0);
        assert_eq!(jpegli_distance_to_linear_quality(0.1), 1.0);
        close(jpegli_distance_to_linear_quality(1.0), 200.0 / 9.0 * 0.9);
        close(
            jpegli_distance_to_linear_quality(5.5),
            5000.0 / (100.0 - 5.4 / 0.09),
        );
        let d = 10.0f64;
        let want = 530_000.0 / (3450.0 - 300.0 * ((848.0 * d - 5330.0) / 120.0).sqrt());
        assert!((f64::from(jpegli_distance_to_linear_quality(10.0)) - want).abs() < 1e-3);
        assert_eq!(jpegli_distance_to_linear_quality(25.0), 5000.0);
        assert_eq!(jpegli_distance_to_linear_quality(1e6), 5000.0);
    }

    #[test]
    fn jpegli_tables_scale_jpeglis_bases() {
        // Below the knee every entry is round(global · distance · base), clamped into 1..=255.
        let d = 1.0f32;
        let ycbcr = jpegli_tables(d, JpegliFrame::Ycbcr { yuv420: false });
        assert_eq!(ycbcr.len(), 3);
        for (table, base) in ycbcr.iter().zip(&JPEGLI_BASE_YCBCR) {
            let want: [u16; 64] = std::array::from_fn(|k| {
                ((JPEGLI_GLOBAL_SCALE_YCBCR * d * base[k] as f32).round() as u16).clamp(1, 255)
            });
            assert_eq!(table, &want);
        }
        // Y's DC: 1.7396601 · 1.2397409 = 2.157 → 2; Cr's corner 114.892 · 1.7397 → 200.
        assert_eq!((ycbcr[0][0], ycbcr[2][63]), (2, 200));
        // 4:2:0 multiplies every table by 1.22 and the chroma tables by `k420Rescale` too.
        let yuv420 = jpegli_tables(d, JpegliFrame::Ycbcr { yuv420: true });
        let g = JPEGLI_GLOBAL_SCALE_YCBCR * 1.22f64 as f32;
        assert_eq!(
            yuv420[0][0],
            (g * JPEGLI_BASE_YCBCR[0][0] as f32).round() as u16
        );
        for t in 1..3 {
            let want =
                (g * JPEGLI_420_RESCALE[63] as f32 * JPEGLI_BASE_YCBCR[t][63] as f32).round();
            assert_eq!(yuv420[t][63], want as u16, "table {t}");
        }
        // XYB: kGlobalScaleXYB over each channel's base; X's DC 1.4395 · 7.563 = 10.89 → 11.
        let xyb = jpegli_tables(d, JpegliFrame::Xyb);
        assert_eq!((xyb.len(), xyb[0][0], xyb[1][0], xyb[2][0]), (3, 11, 2, 5));
        // Grayscale: Annex K.1 scaled by 0.01 · DistanceToLinearQuality(1) = 0.2: 16 → 3.2 → 3.
        let gray = jpegli_tables(d, JpegliFrame::Gray);
        assert_eq!(gray.len(), 1);
        assert_eq!((gray[0][0], gray[0][63]), (3, 20));
        // Coarse distances saturate at the baseline ceiling, never above.
        let coarse = jpegli_tables(1000.0, JpegliFrame::Ycbcr { yuv420: false });
        assert_eq!(coarse[0][0], 255);
        assert!(coarse.iter().flatten().all(|&v| v <= 255));
    }
}
