//! The DNG "Mapping Raw Values to Linear Reference Values" stage (DNG 1.7.1 Chapter 5, p. 99):
//! linearization-table lookup, black subtraction (pattern + per-column/per-row deltas), rescaling
//! so the white level maps to `1.0`, and clipping.
//!
//! The decoder deliberately returns the *stored* sensor values; this module is the explicit
//! opt-in mapping behind [`RawImage::to_linear`](crate::RawImage::to_linear). Its output geometry
//! is the **active-area crop** — the deltas are only defined over the active area, and the Adobe
//! SDK's stage-2 image (the differential oracle for this code) is active-area-sized too.

use gamut_core::{Error, Result};

use crate::raw::RawImage;

/// The Chapter-5 linear reference image: the active-area crop of the raw, mapped to linear
/// `[0.0, 1.0]` values (`1.0` = the white level; black maps to `0.0`).
///
/// Values are clipped to `[0.0, 1.0]`. The spec makes low clipping optional ("may be clipped",
/// preserving negatives can help some noise-reduction pipelines); this implementation clips both
/// ends, matching the Adobe SDK's default-host stage-2 image.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct LinearImage {
    /// Active-area width in pixels.
    pub width: u32,
    /// Active-area height in pixels.
    pub height: u32,
    /// Interleaved samples per pixel (same as the source raw).
    pub samples_per_pixel: u16,
    /// Row-major, interleaved linear reference values in `[0.0, 1.0]`.
    pub samples: Vec<f32>,
}

/// Applies the Chapter-5 mapping to `raw` (see [`RawImage::to_linear`]).
pub(crate) fn linearize(raw: &RawImage) -> Result<LinearImage> {
    let dims = raw.dimensions();
    let (width, height) = (dims.width() as usize, dims.height() as usize);
    let [top, left, bottom, right] =
        raw.active_area()
            .unwrap_or([0, 0, dims.height(), dims.width()]);
    let (top, left, bottom, right) = (top as usize, left as usize, bottom as usize, right as usize);
    if top >= bottom || left >= right || bottom > height || right > width {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: active area must be a non-empty rectangle within the image",
        ));
    }
    let (aa_width, aa_height) = (right - left, bottom - top);

    let levels = raw.levels();
    let spp = usize::from(raw.samples_per_pixel());
    let (rows, cols) = levels.black_repeat();
    let (rows, cols) = (usize::from(rows), usize::from(cols));

    let delta_h = levels.black_delta_h();
    let delta_v = levels.black_delta_v();
    if let Some(deltas) = delta_h
        && (deltas.len() != aa_width || deltas.iter().any(|d| !d.is_finite()))
    {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: BlackLevelDeltaH needs one finite value per active-area column",
        ));
    }
    if let Some(deltas) = delta_v
        && (deltas.len() != aa_height || deltas.iter().any(|d| !d.is_finite()))
    {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: BlackLevelDeltaV needs one finite value per active-area row",
        ));
    }
    let table = levels.linearization_table();
    if let Some(table) = table
        && table.is_empty()
    {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: LinearizationTable must not be empty",
        ));
    }

    // Per-plane scale = 1 / (WhiteLevel − maximum computed black level for the plane), p. 99.
    // The maximum uses per-phase delta maxima (as the SDK's MaxBlackLevel does) — exact, because
    // the column phase and row phase vary independently. A phase with no active-area member is
    // skipped: pixels with that phase never occur, so its cells cannot set the maximum.
    let phase_max = |len: usize, period: usize, deltas: Option<&[f64]>| -> Vec<f64> {
        let mut max = vec![f64::NEG_INFINITY; period];
        match deltas {
            Some(deltas) => {
                for (i, &d) in deltas.iter().enumerate() {
                    let phase = i % period;
                    max[phase] = max[phase].max(d);
                }
            }
            None => {
                for phase in max.iter_mut().take(period.min(len)) {
                    *phase = 0.0;
                }
            }
        }
        max
    };
    let max_dh = phase_max(aa_width, cols, delta_h);
    let max_dv = phase_max(aa_height, rows, delta_v);

    let mut scale = vec![0.0f64; spp];
    for (plane, slot) in scale.iter_mut().enumerate() {
        let mut max_black = f64::NEG_INFINITY;
        for (j, &dv) in max_dv.iter().enumerate() {
            if dv == f64::NEG_INFINITY {
                continue;
            }
            for (k, &dh) in max_dh.iter().enumerate() {
                if dh == f64::NEG_INFINITY {
                    continue;
                }
                let cell = levels.black()[(j * cols + k) * spp + plane];
                max_black = max_black.max(cell + dh + dv);
            }
        }
        let range = levels.white()[plane] - max_black;
        if range <= 0.0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: white level must exceed the maximum computed black level",
            ));
        }
        *slot = 1.0 / range;
    }

    let samples = raw.samples();
    let mut out = Vec::with_capacity(aa_height * aa_width * spp);
    for r in 0..aa_height {
        let dv = delta_v.map_or(0.0, |d| d[r]);
        for c in 0..aa_width {
            let dh = delta_h.map_or(0.0, |d| d[c]);
            for plane in 0..spp {
                let stored = samples[((top + r) * width + (left + c)) * spp + plane];
                // Linearization: inputs at or beyond the table length map to the last entry.
                let linearized = match table {
                    Some(t) => f64::from(t[usize::from(stored).min(t.len() - 1)]),
                    None => f64::from(stored),
                };
                // `plane < spp` by loop construction, so the lookup cannot miss.
                let black = levels.black_at(r, c, plane).unwrap_or(0.0) + dh + dv;
                let value = (linearized - black) * scale[plane];
                out.push(value.clamp(0.0, 1.0) as f32);
            }
        }
    }
    Ok(LinearImage {
        width: aa_width as u32,
        height: aa_height as u32,
        samples_per_pixel: spp as u16,
        samples: out,
    })
}

#[cfg(test)]
mod tests {
    use gamut_core::Dimensions;

    use crate::levels::RawLevels;
    use crate::raw::RawImage;

    fn dims(w: u32, h: u32) -> Dimensions {
        Dimensions::new(w, h).unwrap()
    }

    /// A 4x4 CFA whose stored values are all 100, with a 2x2 black pattern of four distinct
    /// values and an active area at a non-zero origin: the black pattern must anchor at the
    /// active area's top-left (spec p. 28), so each output pixel's phase is its *active-relative*
    /// position. Anchoring at the image origin instead would swap the phases (the active area
    /// starts at the odd offset (1, 1)) and shift every expected value.
    #[test]
    fn black_pattern_anchors_at_the_active_area_origin() {
        let raw = RawImage::new_cfa(dims(4, 4), 8, (2, 2), vec![0, 1, 1, 2], vec![100u16; 16])
            .unwrap()
            .with_active_area([1, 1, 4, 4])
            .with_levels(
                RawLevels::new(1, (2, 2), vec![10.0, 20.0, 30.0, 40.0], vec![250.0]).unwrap(),
            )
            .unwrap();
        let linear = raw.to_linear().expect("linearize");
        assert_eq!((linear.width, linear.height), (3, 3));
        assert_eq!(linear.samples_per_pixel, 1);
        // scale = 1 / (250 - 40); black at active-relative (r, c) is pattern[(r%2)*2 + (c%2)].
        let scale = 1.0 / 210.0;
        let expect = |black: f64| ((100.0 - black) * scale) as f32;
        #[rustfmt::skip]
        let expected = vec![
            expect(10.0), expect(20.0), expect(10.0),
            expect(30.0), expect(40.0), expect(30.0),
            expect(10.0), expect(20.0), expect(10.0),
        ];
        assert_eq!(linear.samples, expected);
    }

    /// `BlackLevelDeltaV` must carry one finite value per active-area row.
    ///
    /// The guard is `deltas.len() != aa_height || deltas.iter().any(|d| !d.is_finite())`, and
    /// only well-formed deltas were ever passed, so relaxing the `||` to `&&` accepted a delta
    /// that failed one condition but not both (#110). Both malformations are asserted, because
    /// either alone leaves the other disjunct unexercised.
    #[test]
    fn a_malformed_black_level_delta_v_is_rejected() {
        let linearized = |dv: Vec<f64>| {
            RawImage::new_cfa(dims(3, 3), 8, (1, 1), vec![0], vec![60u16; 9])
                .unwrap()
                .with_levels(
                    RawLevels::uniform(1, 10.0, 110.0)
                        .unwrap()
                        .with_black_delta_v(dv),
                )
                .unwrap()
                .to_linear()
        };

        let short = linearized(vec![0.0, 1.0]).expect_err("two deltas for three rows");
        assert!(
            short
                .to_string()
                .contains("BlackLevelDeltaV needs one finite"),
            "{short}"
        );

        let nan = linearized(vec![0.0, f64::NAN, 1.0]).expect_err("a non-finite delta");
        assert!(
            nan.to_string()
                .contains("BlackLevelDeltaV needs one finite"),
            "{nan}"
        );
    }

    /// Distinct per-column and per-row deltas (asymmetric, so swapping H/V is caught): the black
    /// for pixel (r, c) is `base + dh[c] + dv[r]`, and the scale uses the *maximum* computed
    /// black (base + max dh + max dv), not the base alone.
    #[test]
    fn deltas_add_per_column_and_per_row() {
        let raw = RawImage::new_cfa(dims(3, 3), 8, (1, 1), vec![0], vec![60u16; 9])
            .unwrap()
            .with_levels(
                RawLevels::uniform(1, 10.0, 110.0)
                    .unwrap()
                    .with_black_delta_h(vec![0.0, 5.0, 10.0])
                    .with_black_delta_v(vec![0.0, -2.0, 4.0]),
            )
            .unwrap();
        let linear = raw.to_linear().expect("linearize");
        // max black = 10 + 10 + 4 = 24; scale = 1 / (110 - 24) = 1/86. Stored 60 keeps every
        // pixel inside the unit range, so no clipping masks the per-pixel black.
        let scale = 1.0 / 86.0;
        let expect = |dh: f64, dv: f64| ((60.0 - (10.0 + dh + dv)) * scale) as f32;
        #[rustfmt::skip]
        let expected = vec![
            expect(0.0, 0.0),  expect(5.0, 0.0),  expect(10.0, 0.0),
            expect(0.0, -2.0), expect(5.0, -2.0), expect(10.0, -2.0),
            expect(0.0, 4.0),  expect(5.0, 4.0),  expect(10.0, 4.0),
        ];
        assert_eq!(linear.samples, expected);
    }

    /// The linearization table is applied before black subtraction, and stored values at or
    /// beyond the table length map to the last entry (spec pp. 27, 99).
    #[test]
    fn linearization_table_maps_and_saturates() {
        // Stored values 0..3 map through the table; 5 is beyond its length and saturates to 30.
        let samples = vec![0u16, 1, 2, 3, 5, 0];
        let raw = RawImage::new_cfa(dims(3, 2), 8, (1, 1), vec![0], samples)
            .unwrap()
            .with_levels(
                RawLevels::uniform(1, 0.0, 30.0)
                    .unwrap()
                    .with_linearization_table(vec![0, 10, 20, 30]),
            )
            .unwrap();
        let linear = raw.to_linear().expect("linearize");
        let expected: Vec<f32> = [0.0f64, 10.0, 20.0, 30.0, 30.0, 0.0]
            .iter()
            .map(|&v| (v / 30.0) as f32)
            .collect();
        assert_eq!(linear.samples, expected);
    }

    /// Values above white clip to 1.0 and values below the local black clip to 0.0. A pixel whose
    /// *local* black is below the plane maximum can exceed 1.0 before clipping — the scale uses
    /// the maximum black, so (white − local black) · scale > 1.
    #[test]
    fn output_clips_to_the_unit_range() {
        // 1x2 pattern: blacks 0 and 20; white 120. scale = 1/(120-20) = 1/100.
        // Pixel (0,0): black 0, stored 120 -> (120-0)/100 = 1.2 -> clips to 1.0.
        // Pixel (0,1): black 20, stored 10 -> (10-20)/100 < 0 -> clips to 0.0.
        let raw = RawImage::new_cfa(dims(2, 1), 8, (1, 2), vec![0, 1], vec![120u16, 10])
            .unwrap()
            .with_levels(RawLevels::new(1, (1, 2), vec![0.0, 20.0], vec![120.0]).unwrap())
            .unwrap();
        let linear = raw.to_linear().expect("linearize");
        assert_eq!(linear.samples, vec![1.0f32, 0.0]);
    }

    /// The maximum black is found per plane across the repeat pattern's cells.
    ///
    /// `levels.black()[(j * cols + k) * spp + plane]` walks the pattern to find each plane's
    /// largest black, which sets that plane's scale. With one sample per pixel `* spp` and `/ spp`
    /// agree, and with a 1x1 pattern the index is `plane` either way -- so the fixtures had
    /// covered multiple planes and multiple cells, but never both at once, and the mutant went
    /// unnoticed (#110).
    ///
    /// Here two planes meet a 1x2 pattern. Plane 0's blacks are 0 and 40, so its range is
    /// `140 - 40`; dividing instead collapses both cells onto index 0, finds a maximum black of 0,
    /// and stretches the range to the full 140.
    #[test]
    fn the_maximum_black_is_found_per_plane_across_the_pattern() {
        let raw = RawImage::new_linear_raw(dims(2, 1), 8, 2, vec![50u16, 50, 50, 50])
            .unwrap()
            .with_levels(
                RawLevels::new(2, (1, 2), vec![0.0, 0.0, 40.0, 0.0], vec![140.0, 100.0]).unwrap(),
            )
            .unwrap();
        let linear = raw.to_linear().expect("linearize");

        // Plane 0 scales by 1/(140 - 40); plane 1 by 1/(100 - 0).
        let p0 = |stored: f64, black: f64| ((stored - black) / 100.0) as f32;
        let p1 = |stored: f64, black: f64| ((stored - black) / 100.0) as f32;
        assert_eq!(
            linear.samples,
            vec![
                p0(50.0, 0.0),  // pixel 0, plane 0: cell (0,0) black 0
                p1(50.0, 0.0),  // pixel 0, plane 1: cell (0,0) black 0
                p0(50.0, 40.0), // pixel 1, plane 0: cell (0,1) black 40
                p1(50.0, 0.0),  // pixel 1, plane 1: cell (0,1) black 0
            ]
        );
    }

    /// Each plane of a LinearRaw image scales by its own white level.
    #[test]
    fn per_plane_whites_scale_independently() {
        let raw = RawImage::new_linear_raw(dims(1, 1), 8, 3, vec![50u16, 50, 50])
            .unwrap()
            .with_levels(RawLevels::new(3, (1, 1), vec![0.0; 3], vec![50.0, 100.0, 200.0]).unwrap())
            .unwrap();
        let linear = raw.to_linear().expect("linearize");
        assert_eq!(linear.samples, vec![1.0f32, 0.5, 0.25]);
    }

    #[test]
    fn to_linear_rejects_impossible_inputs() {
        let base = |raw: RawImage| raw;
        let raw = base(RawImage::new_cfa(dims(2, 2), 8, (1, 1), vec![0], vec![0u16; 4]).unwrap());
        // White not above the maximum black.
        let bad = raw
            .clone()
            .with_levels(RawLevels::uniform(1, 255.0, 255.0).unwrap())
            .unwrap();
        assert!(bad.to_linear().is_err());
        // Degenerate / out-of-bounds active area.
        assert!(
            raw.clone()
                .with_active_area([1, 0, 1, 2])
                .to_linear()
                .is_err()
        );
        assert!(
            raw.clone()
                .with_active_area([0, 0, 3, 2])
                .to_linear()
                .is_err()
        );
        // Delta length mismatched to the active area.
        let bad = raw
            .clone()
            .with_levels(
                RawLevels::uniform(1, 0.0, 255.0)
                    .unwrap()
                    .with_black_delta_h(vec![0.0; 3]),
            )
            .unwrap();
        assert!(bad.to_linear().is_err());
        // An empty linearization table.
        let bad = raw
            .with_levels(
                RawLevels::uniform(1, 0.0, 255.0)
                    .unwrap()
                    .with_linearization_table(vec![]),
            )
            .unwrap();
        assert!(bad.to_linear().is_err());
    }
}
