//! Correlated colour temperature (CCT) of a CIE 1931 chromaticity, by Robertson's method — and,
//! the other way, the CIE daylight locus ([`daylight_xy`]), the chromaticity of daylight at a
//! temperature.
//!
//! Robertson's method walks a table of *isotemperature lines* — points on the Planckian locus in
//! the CIE 1960 UCS `(u, v)` plane, each with the slope of the line of constant CCT crossing it —
//! finds the pair of lines the sample falls between, and interpolates in **reciprocal** temperature
//! (mired), which is where the locus is close to linear. The table is Wyszecki & Stiles,
//! *Color Science*, 2nd edition, p. 228; `references/color/README.md` records its provenance.
//!
//! This is the definition raw-image formats build on: DNG 1.7.1 interpolates a camera's colour
//! calibrations by "linear interpolation using inverse correlated color temperature", so a white
//! balance given as a chromaticity has to become a temperature before the calibrations can be
//! weighted. [`gamut_dng`](https://docs.rs/gamut-dng) uses this for exactly that.
//!
//! Like the rest of this crate's `f64` colour science, the result is Tier-1 (correctness only) and
//! not bit-reproducible across platforms.

/// One row of Robertson's table: an isotemperature line on the Planckian locus.
struct IsoTemperature {
    /// Reciprocal temperature in mired (µK⁻¹) — the table is uniform in this, not in kelvin.
    mired: f64,
    /// CIE 1960 UCS `u` of the locus at this temperature.
    u: f64,
    /// CIE 1960 UCS `v` of the locus at this temperature.
    v: f64,
    /// Slope `dv/du` of the isotemperature line through `(u, v)`.
    slope: f64,
}

/// Robertson's 31 isotemperature lines, from ∞ K (0 mired) down to 1667 K (600 mired).
const LINES: [IsoTemperature; 31] = {
    // A row reads `(mired, u, v, slope)`; the array form keeps the table checkable against the
    // published one line for line.
    const fn row(mired: f64, u: f64, v: f64, slope: f64) -> IsoTemperature {
        IsoTemperature { mired, u, v, slope }
    }
    [
        row(0.0, 0.18006, 0.26352, -0.24341),
        row(10.0, 0.18066, 0.26589, -0.25479),
        row(20.0, 0.18133, 0.26846, -0.26876),
        row(30.0, 0.18208, 0.27119, -0.28539),
        row(40.0, 0.18293, 0.27407, -0.30470),
        row(50.0, 0.18388, 0.27709, -0.32675),
        row(60.0, 0.18494, 0.28021, -0.35156),
        row(70.0, 0.18611, 0.28342, -0.37915),
        row(80.0, 0.18740, 0.28668, -0.40955),
        row(90.0, 0.18880, 0.28997, -0.44278),
        row(100.0, 0.19032, 0.29326, -0.47888),
        row(125.0, 0.19462, 0.30141, -0.58204),
        row(150.0, 0.19962, 0.30921, -0.70471),
        row(175.0, 0.20525, 0.31647, -0.84901),
        row(200.0, 0.21142, 0.32312, -1.0182),
        row(225.0, 0.21807, 0.32909, -1.2168),
        row(250.0, 0.22511, 0.33439, -1.4512),
        row(275.0, 0.23247, 0.33904, -1.7298),
        row(300.0, 0.24010, 0.34308, -2.0637),
        row(325.0, 0.24702, 0.34655, -2.4681),
        row(350.0, 0.25591, 0.34951, -2.9641),
        row(375.0, 0.26400, 0.35200, -3.5814),
        row(400.0, 0.27218, 0.35407, -4.3633),
        row(425.0, 0.28039, 0.35577, -5.3762),
        row(450.0, 0.28863, 0.35714, -6.7262),
        row(475.0, 0.29685, 0.35823, -8.5955),
        row(500.0, 0.30505, 0.35907, -11.324),
        row(525.0, 0.31320, 0.35968, -15.628),
        row(550.0, 0.32129, 0.36011, -23.325),
        row(575.0, 0.32931, 0.36038, -40.770),
        row(600.0, 0.33724, 0.36051, -116.45),
    ]
};

/// Converts a CIE 1931 chromaticity to CIE 1960 UCS `(u, v)`, or `None` if it is degenerate.
fn uv_from_xy(xy: [f64; 2]) -> Option<(f64, f64)> {
    let [x, y] = xy;
    let denominator = 1.5 - x + 6.0 * y;
    let (u, v) = (2.0 * x / denominator, 3.0 * y / denominator);
    (u.is_finite() && v.is_finite()).then_some((u, v))
}

/// The correlated colour temperature of chromaticity `xy`, in kelvin.
///
/// Returns `None` for a chromaticity with no CIE 1960 image — one that is not finite, or that sits
/// on the `1.5 - x + 6y = 0` line where the UCS transform is undefined.
///
/// The result is clamped to the table's span, `1667 K`..`100_000 K`: a sample beyond either end
/// takes the endpoint's temperature rather than an extrapolation the table cannot support. Distance
/// from the locus (the "tint" axis) is not reported — CCT alone is what calibration interpolation
/// weights on.
#[must_use]
pub fn cct_from_xy(xy: [f64; 2]) -> Option<f64> {
    let (u, v) = uv_from_xy(xy)?;
    let last = LINES.len() - 1;
    // Distance from the previous line, kept so the crossing can be interpolated between the two.
    let mut previous_distance = 0.0;
    for (index, line) in LINES.iter().enumerate().skip(1) {
        // The isotemperature line's direction, normalised.
        let length = (1.0 + line.slope * line.slope).sqrt();
        let (du, dv) = (1.0 / length, line.slope / length);
        // Signed distance from the line; its sign flips once the sample lies on its far side,
        // which is the crossing this walk is looking for.
        let distance = -(u - line.u) * dv + (v - line.v) * du;
        if distance.is_sign_positive() && index != last {
            previous_distance = distance;
            continue;
        }
        // Crossed (or ran out of table): interpolate in mired between this line and the last.
        let distance = (-distance).max(0.0);
        let span = previous_distance + distance;
        // `index == 1` has no previous line to interpolate towards, and a zero span means the
        // sample sits on both lines at once; either way the crossing is this line itself.
        let f = if index == 1 || span == 0.0 {
            0.0
        } else {
            distance / span
        };
        // `f` is pinned to 0 at `index == 1`, so the blend never reaches `LINES[0]`'s 0 mired
        // (infinite temperature): the smallest value it can take is `LINES[1]`'s 10 mired.
        let mired = LINES[index - 1].mired * f + line.mired * (1.0 - f);
        return Some(1.0e6 / mired);
    }
    None
}

/// The chromaticity of CIE daylight at correlated colour temperature `cct` kelvin — the CIE
/// daylight locus (CIE 15:2004), the inverse direction of [`cct_from_xy`] for daylight whites.
///
/// `x` is the CIE cubic in `1/T` — one set of coefficients for `4000 K ..= 7000 K`, another above
/// 7000 K up to `25 000 K` — and `y = −3.000 x² + 2.870 x − 0.275`. Those are the two ranges the
/// CIE defines the polynomial over, so a temperature outside `4000 K ..= 25 000 K` (or a
/// non-finite one) returns `None` rather than an extrapolation. The CIE standard illuminants come
/// back to their published chromaticities: D50 at 5003 K, D65 at 6504 K, D75 at 7504 K.
///
/// Little-CMS's `cmsWhitePointFromTemp` is this function, with the same ranges; its
/// incomplete-adaptation absolute intent turns a mixed temperature back into a white with it,
/// which is what `gamut-cmm` uses it for.
#[must_use]
pub fn daylight_xy(cct: f64) -> Option<[f64; 2]> {
    let (t, t2, t3) = (cct, cct * cct, cct * cct * cct);
    let x = if (4000.0..=7000.0).contains(&cct) {
        -4.6070e9 / t3 + 2.9678e6 / t2 + 0.09911e3 / t + 0.244063
    } else if (7000.0..=25_000.0).contains(&cct) {
        -2.0064e9 / t3 + 1.9018e6 / t2 + 0.24748e3 / t + 0.237040
    } else {
        return None;
    };
    Some([x, -3.000 * x * x + 2.870 * x - 0.275])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CIE D-series illuminants' published chromaticities (to five places) sit on the
    /// daylight locus at their nominal temperatures, `1.4388 / 1.4380` × the round figure — D50,
    /// D55 and D65 on the first polynomial, D75 on the second. The published values come from the
    /// illuminants' spectra, not from the polynomial, which is a fit: measured, it lands within
    /// 2.6e-5 of them in `x` and 1.3e-4 in `y`, and the tolerances are those bounds rounded up.
    #[test]
    fn daylight_locus_recovers_the_d_series() {
        for (name, cct, want) in [
            ("D50", 5003.0, [0.34567, 0.35850]),
            ("D55", 5503.0, [0.33242, 0.34743]),
            ("D65", 6504.0, [0.31271, 0.32902]),
            ("D75", 7504.0, [0.29902, 0.31485]),
        ] {
            let got = daylight_xy(cct).expect("inside the daylight locus");
            for (k, tolerance) in [(0, 3e-5), (1, 1.5e-4)] {
                assert!(
                    (got[k] - want[k]).abs() < tolerance,
                    "{name}[{k}]: {} vs {}",
                    got[k],
                    want[k]
                );
            }
        }
    }

    /// The locus is defined over `4000 K ..= 25 000 K`, both ends included, and 7000 K belongs
    /// to the lower polynomial; outside it there is no answer. The 7000 K value is the lower
    /// polynomial's to the last bit, which the upper one misses by ~1e-6 in `x`.
    #[test]
    fn daylight_locus_range_is_closed_at_both_ends() {
        assert!(daylight_xy(4000.0).is_some());
        assert!(daylight_xy(25_000.0).is_some());
        for outside in [3999.999, 25_000.001, f64::NAN, f64::INFINITY, -6504.0] {
            assert_eq!(daylight_xy(outside), None, "{outside}");
        }
        let t: f64 = 7000.0;
        let lower = -4.6070e9 / t.powi(3) + 2.9678e6 / t.powi(2) + 0.09911e3 / t + 0.244063;
        let upper = -2.0064e9 / t.powi(3) + 1.9018e6 / t.powi(2) + 0.24748e3 / t + 0.237040;
        let x = daylight_xy(t).expect("7000 K is on the locus")[0];
        assert!(
            (x - lower).abs() < 1e-12,
            "{x} vs the lower polynomial's {lower}"
        );
        assert!(
            (x - upper).abs() > 1e-7,
            "the two polynomials must be told apart here"
        );
    }

    /// The CIE standard illuminants sit on (or very near) the Planckian locus at their nominal
    /// temperatures, so they pin the table and the interpolation end to end. Tolerances are the
    /// documented accuracy of Robertson's method, not slop: it interpolates a 31-row table.
    #[test]
    fn standard_illuminants_recover_their_nominal_temperature() {
        for (name, xy, nominal, tolerance) in [
            ("A", [0.44757, 0.40745], 2856.0, 10.0),
            ("D50", [0.34567, 0.35850], 5003.0, 10.0),
            ("D55", [0.33242, 0.34743], 5503.0, 10.0),
            ("D65", [0.31272, 0.32903], 6504.0, 15.0),
            ("D75", [0.29902, 0.31485], 7504.0, 20.0),
        ] {
            let cct = cct_from_xy(xy).expect("a standard illuminant has a CCT");
            assert!(
                (cct - nominal).abs() < tolerance,
                "illuminant {name}: got {cct} K, expected {nominal} K"
            );
        }
    }

    /// Reciprocal temperature is monotone along the locus, so walking the table's own points must
    /// give back strictly decreasing temperatures.
    #[test]
    fn temperature_decreases_along_the_locus() {
        let mut previous = f64::INFINITY;
        for line in LINES.iter().skip(1) {
            // Recover an xy from the table's uv: x = 3u / (2u - 8v + 4), y = 2v / (2u - 8v + 4).
            let d = 2.0 * line.u - 8.0 * line.v + 4.0;
            let cct =
                cct_from_xy([3.0 * line.u / d, 2.0 * line.v / d]).expect("a locus point has a CCT");
            assert!(
                cct < previous,
                "{cct} K should be cooler than the previous {previous} K"
            );
            previous = cct;
        }
    }

    /// A sample far off the locus still resolves: Robertson's method reports the nearest
    /// isotemperature line, and only a chromaticity with no UCS image at all has no answer.
    #[test]
    fn off_locus_and_degenerate_inputs() {
        // Well above the locus (strongly green) and well below it (strongly magenta).
        assert!(cct_from_xy([0.30, 0.40]).is_some());
        assert!(cct_from_xy([0.35, 0.25]).is_some());
        // Beyond both ends of the table, clamped to its span.
        let hot = cct_from_xy([0.24, 0.24]).expect("blue-of-locus still resolves");
        let cold = cct_from_xy([0.55, 0.40]).expect("red-of-locus still resolves");
        assert!(hot > cold, "{hot} K should be hotter than {cold} K");
        // Past the *cold* end of the table entirely — deep red, well beyond the 1667 K last row.
        // The walk must fall back to that row rather than running off the end with no answer.
        let coldest = cct_from_xy([0.6500, 0.3300]).expect("past the last isotemperature line");
        assert!(
            (1600.0..1750.0).contains(&coldest),
            "a sample past the table's cold end takes its endpoint, got {coldest} K"
        );
        // 1.5 - x + 6y == 0 has no CIE 1960 image, and neither does a non-finite chromaticity —
        // including the ones where the transform leaves exactly one of `u`/`v` finite.
        assert_eq!(cct_from_xy([1.5, 0.0]), None);
        assert_eq!(cct_from_xy([f64::NAN, 0.33]), None);
        assert_eq!(cct_from_xy([f64::INFINITY, 0.33]), None);
        assert_eq!(cct_from_xy([0.33, f64::INFINITY]), None);
        assert_eq!(cct_from_xy([f64::NEG_INFINITY, 0.33]), None);
    }

    /// The table is transcribed by hand from a book, so its shape is pinned rather than trusted:
    /// walking the Planckian locus from infinite temperature down to 1667 K, reciprocal temperature
    /// rises, both UCS coordinates rise, and every isotemperature line slopes negatively and more
    /// steeply than the last. A dropped sign or a transposed digit breaks one of those.
    #[test]
    fn the_isotemperature_table_is_transcribed_consistently() {
        assert_eq!(LINES.len(), 31, "Robertson's table has 31 rows");
        assert_eq!(LINES[0].mired, 0.0, "the first row is infinite temperature");
        assert_eq!(LINES[30].mired, 600.0, "the last row is 1667 K");
        for (index, pair) in LINES.windows(2).enumerate() {
            let (previous, line) = (&pair[0], &pair[1]);
            assert!(
                line.mired > previous.mired,
                "row {index}: mired must rise, {} then {}",
                previous.mired,
                line.mired
            );
            assert!(
                line.u > previous.u && line.v > previous.v,
                "row {index}: the locus must advance in u and v"
            );
            assert!(
                line.slope < 0.0 && previous.slope < 0.0,
                "row {index}: isotemperature lines slope negatively"
            );
            assert!(
                line.slope < previous.slope,
                "row {index}: the slope must steepen, {} then {}",
                previous.slope,
                line.slope
            );
        }
    }
}
