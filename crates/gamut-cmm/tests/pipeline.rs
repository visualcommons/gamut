//! Integration tests for the pipeline/stage model: construction-time validation, evaluation,
//! composition, and the `Transform` buffer contract.

use gamut_cmm::{ClutTable, CmmError, IccCurve, MAX_CHANNELS, Pipeline, Stage, Transform};
use gamut_icc::{
    Clut, ClutPrecision, Curve, CurveOrParametric, ParametricCurve, S15Fixed16, U8Fixed8,
};

/// A non-trivial affine stage (negative entries, distinct coefficients) whose arithmetic is
/// exact in `f64`, so results assert with `==`.
fn dyadic_matrix() -> Stage {
    Stage::Matrix {
        m: [[0.5, -0.25, 0.125], [1.0, 2.0, -0.5], [-2.0, 0.25, 1.0]],
        offset: [0.5, -0.25, 2.0],
    }
}

#[test]
fn max_channels_is_the_icc_clut_bound() {
    assert_eq!(MAX_CHANNELS, 16);
}

#[test]
fn adjacent_stage_mismatch_reports_index_and_counts() {
    let err = Pipeline::new(
        3,
        2,
        vec![
            Stage::Identity { channels: 3 },
            Stage::Clamp { channels: 2 },
        ],
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            CmmError::StageChannelMismatch {
                index: 1,
                expected: 2,
                found: 3,
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn declared_input_must_match_first_stage() {
    let err = Pipeline::new(2, 3, vec![Stage::Identity { channels: 3 }]).unwrap_err();
    assert!(
        matches!(
            err,
            CmmError::PipelineEndsMismatch {
                end: "input",
                declared: 2,
                found: 3,
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn declared_output_must_match_last_stage() {
    let err = Pipeline::new(3, 2, vec![Stage::Identity { channels: 3 }]).unwrap_err();
    assert!(
        matches!(
            err,
            CmmError::PipelineEndsMismatch {
                end: "output",
                declared: 2,
                found: 3,
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn zero_declared_channels_rejected() {
    let err = Pipeline::new(0, 3, vec![Stage::Identity { channels: 3 }]).unwrap_err();
    assert!(matches!(err, CmmError::TooManyChannels(0)));
    let err = Pipeline::new(3, 0, vec![Stage::Identity { channels: 3 }]).unwrap_err();
    assert!(matches!(err, CmmError::TooManyChannels(0)));
}

#[test]
fn seventeen_declared_channels_rejected() {
    let err = Pipeline::new(17, 17, vec![]).unwrap_err();
    assert!(matches!(err, CmmError::TooManyChannels(17)));
}

#[test]
fn stage_channel_range_checked_even_when_ends_are_valid() {
    // The declared ends are in range; the stage's own count is not — and the range check runs
    // before the ends-equality check, so the count is what gets reported.
    let err = Pipeline::new(3, 3, vec![Stage::Identity { channels: 0 }]).unwrap_err();
    assert!(matches!(err, CmmError::TooManyChannels(0)));
    let err = Pipeline::new(3, 3, vec![Stage::Identity { channels: 17 }]).unwrap_err();
    assert!(matches!(err, CmmError::TooManyChannels(17)));
}

#[test]
fn one_and_sixteen_channels_are_valid_boundaries() {
    let one = Pipeline::new(1, 1, vec![Stage::Clamp { channels: 1 }]).unwrap();
    assert_eq!(one.input_channels(), 1);
    let sixteen = Pipeline::new(16, 16, vec![Stage::Identity { channels: 16 }]).unwrap();
    assert_eq!(sixteen.output_channels(), 16);
}

#[test]
fn empty_pipeline_is_identity() {
    let identity = Pipeline::new(4, 4, vec![]).unwrap();
    assert_eq!(identity.input_channels(), 4);
    assert_eq!(identity.output_channels(), 4);
    assert!(identity.stages().is_empty());

    let input = [0.25, -1.5, 7.0, 0.0];
    let mut output = [9.0; 4];
    identity.eval(&input, &mut output).unwrap();
    assert_eq!(output, input);
}

#[test]
fn mismatched_empty_pipeline_rejected() {
    let err = Pipeline::new(3, 2, vec![]).unwrap_err();
    assert!(
        matches!(
            err,
            CmmError::PipelineEndsMismatch {
                end: "output",
                declared: 2,
                found: 3,
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn matrix_pipeline_matches_hand_computation() {
    let pipeline = Pipeline::new(3, 3, vec![dyadic_matrix()]).unwrap();
    let mut out = [0.0; 3];
    pipeline.eval(&[0.25, 0.5, -1.0], &mut out).unwrap();
    // Row by row: 0.5·0.25 − 0.25·0.5 + 0.125·(−1) + 0.5  = 0.375
    //             1.0·0.25 + 2.0·0.5  − 0.5·(−1)   − 0.25 = 1.5
    //            −2.0·0.25 + 0.25·0.5 + 1.0·(−1)   + 2.0  = 0.625
    assert_eq!(out, [0.375, 1.5, 0.625]);
}

#[test]
fn clamp_pipeline_applies_lcms2_fclamp_semantics() {
    let pipeline = Pipeline::new(4, 4, vec![Stage::Clamp { channels: 4 }]).unwrap();
    let mut out = [9.0; 4];
    pipeline
        .eval(&[-3.0, f64::NAN, 1.0625, 0.75], &mut out)
        .unwrap();
    // Documented on `Stage::Clamp`: negatives and NaN → 0.0, above 1.0 → 1.0.
    assert_eq!(out, [0.0, 0.0, 1.0, 0.75]);
}

#[test]
fn eval_rejects_wrong_slice_lengths() {
    let pipeline = Pipeline::new(3, 3, vec![Stage::Identity { channels: 3 }]).unwrap();
    let mut out3 = [0.0; 3];
    let err = pipeline.eval(&[0.0; 2], &mut out3).unwrap_err();
    assert!(matches!(
        err,
        CmmError::BufferLength {
            channels: 3,
            found: 2
        }
    ));
    let mut out4 = [0.0; 4];
    let err = pipeline.eval(&[0.0; 3], &mut out4).unwrap_err();
    assert!(matches!(
        err,
        CmmError::BufferLength {
            channels: 3,
            found: 4
        }
    ));
}

#[test]
fn transform_processes_each_pixel_independently() {
    let pipeline =
        Pipeline::new(3, 3, vec![dyadic_matrix(), Stage::Clamp { channels: 3 }]).unwrap();
    // Three distinct pixels, so a chunking bug cannot cancel out.
    let src = [
        0.25, 0.5, -1.0, // matrix → [0.375, 1.5, 0.625]  → clamp → [0.375, 1.0, 0.625]
        0.0, 0.0, 0.0, //   matrix → [0.5, -0.25, 2.0]    → clamp → [0.5, 0.0, 1.0]
        1.0, 0.0, 0.0, //   matrix → [1.0, 0.75, 0.0]     → clamp → [1.0, 0.75, 0.0]
    ];
    let mut dst = [9.0; 9];
    pipeline.transform(&src, &mut dst).unwrap();
    assert_eq!(&dst[0..3], &[0.375, 1.0, 0.625]);
    assert_eq!(&dst[3..6], &[0.5, 0.0, 1.0]);
    assert_eq!(&dst[6..9], &[1.0, 0.75, 0.0]);
}

#[test]
fn transform_rejects_partial_source_pixels() {
    let pipeline = Pipeline::new(3, 3, vec![Stage::Identity { channels: 3 }]).unwrap();
    let mut dst = [0.0; 9];
    let err = pipeline.transform(&[0.0; 10], &mut dst).unwrap_err();
    assert!(matches!(
        err,
        CmmError::BufferLength {
            channels: 3,
            found: 10,
        }
    ));
}

#[test]
fn transform_rejects_destination_pixel_count_disagreement() {
    let pipeline = Pipeline::new(3, 3, vec![Stage::Identity { channels: 3 }]).unwrap();
    // dst is a whole number of pixels, but for two pixels while src carries three.
    let mut dst = [0.0; 6];
    let err = pipeline.transform(&[0.0; 9], &mut dst).unwrap_err();
    assert!(matches!(
        err,
        CmmError::BufferLength {
            channels: 3,
            found: 6,
        }
    ));
}

#[test]
fn compose_concatenates_and_keeps_outer_ends() {
    let front = Pipeline::new(3, 3, vec![dyadic_matrix()]).unwrap();
    let back = Pipeline::new(3, 3, vec![Stage::Clamp { channels: 3 }]).unwrap();
    let composed = front.compose(back).unwrap();
    assert_eq!(composed.input_channels(), 3);
    assert_eq!(composed.output_channels(), 3);
    assert_eq!(composed.stages().len(), 2);

    let mut out = [0.0; 3];
    composed.eval(&[0.25, 0.5, -1.0], &mut out).unwrap();
    assert_eq!(out, [0.375, 1.0, 0.625]); // matrix result with 1.5 clamped
}

#[test]
fn compose_rejects_seam_mismatch() {
    let front = Pipeline::new(3, 3, vec![Stage::Identity { channels: 3 }]).unwrap();
    let back = Pipeline::new(4, 4, vec![Stage::Identity { channels: 4 }]).unwrap();
    let err = front.compose(back).unwrap_err();
    assert!(
        matches!(
            err,
            CmmError::StageChannelMismatch {
                index: 1, // `front` has one stage, so the seam sits at index 1
                expected: 4,
                found: 3,
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn compose_with_empty_identity_is_a_no_op() {
    let front = Pipeline::new(3, 3, vec![dyadic_matrix()]).unwrap();
    let identity = Pipeline::new(3, 3, vec![]).unwrap();
    let composed = front.compose(identity).unwrap();
    assert_eq!(composed.stages().len(), 1);
    assert_eq!(composed.input_channels(), 3);
    assert_eq!(composed.output_channels(), 3);
}

#[test]
fn error_messages_name_the_cmm_and_the_counts() {
    assert_eq!(
        CmmError::StageChannelMismatch {
            index: 2,
            expected: 3,
            found: 4,
        }
        .to_string(),
        "cmm: stage 2 expects 3 input channels, previous stage produces 4"
    );
    assert_eq!(
        CmmError::PipelineEndsMismatch {
            end: "input",
            declared: 3,
            found: 4,
        }
        .to_string(),
        "cmm: pipeline input declares 3 channels, found 4"
    );
    assert_eq!(
        CmmError::TooManyChannels(17).to_string(),
        "cmm: channel count 17 outside 1..=16"
    );
    assert_eq!(
        CmmError::BufferLength {
            channels: 3,
            found: 10,
        }
        .to_string(),
        "cmm: buffer length 10 is not a multiple of 3 channels"
    );
}

/// The identity as an [`IccCurve`], for channel-count plumbing tests.
fn identity_curve() -> IccCurve {
    IccCurve::new(&CurveOrParametric::Curve(Curve::Identity)).unwrap()
}

#[test]
fn curves_stage_applies_a_distinct_curve_per_channel() {
    // Three distinct hand-checkable curves: x² (u8Fixed8 0x0200 is exactly 2.0), identity,
    // and x³ (parametric type 0, s15Fixed16 3.0 exact).
    let square = IccCurve::new(&CurveOrParametric::Curve(Curve::Gamma(U8Fixed8(0x0200)))).unwrap();
    let cube = IccCurve::new(&CurveOrParametric::Parametric(ParametricCurve {
        function_type: 0,
        params: vec![S15Fixed16::from_f64(3.0)],
    }))
    .unwrap();
    let pipeline = Pipeline::new(
        3,
        3,
        vec![Stage::Curves(vec![square, identity_curve(), cube])],
    )
    .unwrap();
    let dynamic: &dyn Transform = &pipeline;
    // Two pixels through the Transform trait, so per-channel assignment and per-pixel
    // chunking are both pinned.
    let src = [0.5, 0.25, 0.5, 0.25, 0.75, 1.0];
    let mut dst = [9.0; 6];
    dynamic.transform(&src, &mut dst).unwrap();
    let want = [0.25, 0.25, 0.125, 0.0625, 0.75, 1.0];
    for (i, (got, want)) in dst.iter().zip(&want).enumerate() {
        assert!((got - want).abs() < 1e-12, "sample {i}: {got} vs {want}");
    }
}

#[test]
fn curves_stage_channel_counts_flow_through_validation() {
    // Zero curves: structurally meaningless, rejected as a zero channel count.
    let err = Pipeline::new(1, 1, vec![Stage::Curves(vec![])]).unwrap_err();
    assert!(matches!(err, CmmError::TooManyChannels(0)));
    // Seventeen curves: above the ICC bound.
    let seventeen: Vec<IccCurve> = (0..17).map(|_| identity_curve()).collect();
    let err = Pipeline::new(16, 16, vec![Stage::Curves(seventeen)]).unwrap_err();
    assert!(matches!(err, CmmError::TooManyChannels(17)));
    // Sixteen is the accepted boundary, and the count must match the declared ends.
    let sixteen: Vec<IccCurve> = (0..16).map(|_| identity_curve()).collect();
    let pipeline = Pipeline::new(16, 16, vec![Stage::Curves(sixteen)]).unwrap();
    assert_eq!(pipeline.input_channels(), 16);
    let err = Pipeline::new(3, 3, vec![Stage::Curves(vec![identity_curve()])]).unwrap_err();
    assert!(matches!(
        err,
        CmmError::PipelineEndsMismatch {
            end: "input",
            declared: 3,
            found: 1,
        }
    ));
}

#[test]
fn curves_stage_reports_its_channel_count_saturating() {
    let two = Stage::Curves(vec![identity_curve(), identity_curve()]);
    assert_eq!(two.input_channels(), 2);
    assert_eq!(two.output_channels(), 2);
    // Counts above 255 saturate for reporting and still fail validation.
    let many: Vec<IccCurve> = (0..300).map(|_| identity_curve()).collect();
    let stage = Stage::Curves(many);
    assert_eq!(stage.input_channels(), u8::MAX);
    let err = Pipeline::new(16, 16, vec![stage]).unwrap_err();
    assert!(matches!(err, CmmError::TooManyChannels(255)));
}

/// A hand-checkable 3→3 CLUT on a 2×2×2 grid: outputs `[x, z, y]` (an axis-swapping affine
/// map, so addressing slips show). Node values are exactly `0.0`/`1.0` after normalization
/// and both interpolants reproduce an affine map exactly, keeping downstream arithmetic
/// dyadic-exact.
fn axis_swap_clut_stage() -> Stage {
    let mut samples = Vec::new();
    for x in 0..2_u16 {
        for y in 0..2_u16 {
            for z in 0..2_u16 {
                samples.extend_from_slice(&[x * 65535, z * 65535, y * 65535]);
            }
        }
    }
    Stage::Clut(
        ClutTable::new(&Clut {
            grid_points: vec![2, 2, 2],
            output_channels: 3,
            precision: ClutPrecision::U16,
            samples,
        })
        .unwrap(),
    )
}

#[test]
fn clut_stage_reports_grid_channel_counts() {
    let stage = axis_swap_clut_stage();
    assert_eq!(stage.input_channels(), 3);
    assert_eq!(stage.output_channels(), 3);
    // An asymmetric grid: 4-D in, 2 out.
    let stage = Stage::Clut(
        ClutTable::new(&Clut {
            grid_points: vec![2, 2, 2, 2],
            output_channels: 2,
            precision: ClutPrecision::U16,
            samples: vec![0; 32],
        })
        .unwrap(),
    );
    assert_eq!(stage.input_channels(), 4);
    assert_eq!(stage.output_channels(), 2);
}

#[test]
fn clut_stage_channel_counts_flow_through_validation() {
    // A 4-input CLUT cannot follow the 3-output matrix stage.
    let four_in = Stage::Clut(
        ClutTable::new(&Clut {
            grid_points: vec![2, 2, 2, 2],
            output_channels: 3,
            precision: ClutPrecision::U16,
            samples: vec![0; 48],
        })
        .unwrap(),
    );
    let err = Pipeline::new(3, 3, vec![dyadic_matrix(), four_in]).unwrap_err();
    assert!(
        matches!(
            err,
            CmmError::StageChannelMismatch {
                index: 1,
                expected: 4,
                found: 3,
            }
        ),
        "unexpected error: {err:?}"
    );
    // Declared ends must match the grid shape.
    let err = Pipeline::new(2, 2, vec![axis_swap_clut_stage()]).unwrap_err();
    assert!(matches!(
        err,
        CmmError::PipelineEndsMismatch {
            end: "input",
            declared: 2,
            found: 3,
        }
    ));
}

#[test]
fn curves_clut_matrix_composition_evaluates_hand_checked_pixels() {
    // curves (x² on channel 0) → CLUT (3→3 axis swap [x, z, y]) → matrix: all dyadic, exact.
    let square = IccCurve::new(&CurveOrParametric::Curve(Curve::Gamma(U8Fixed8(0x0200)))).unwrap();
    let pipeline = Pipeline::new(
        3,
        3,
        vec![
            Stage::Curves(vec![square, identity_curve(), identity_curve()]),
            axis_swap_clut_stage(),
            dyadic_matrix(),
        ],
    )
    .unwrap();
    // Pixel 1: (0.5, 1, 0.25) → curves → (0.25, 1, 0.25) → CLUT → (0.25, 0.25, 1) → matrix:
    //   0.5·0.25 − 0.25·0.25 + 0.125·1 + 0.5  = 0.6875
    //   1.0·0.25 + 2.0·0.25  − 0.5·1   − 0.25 = 0.0
    //  −2.0·0.25 + 0.25·0.25 + 1.0·1   + 2.0  = 2.5625
    // Pixel 2: (1, 0, 0.5) → curves → (1, 0, 0.5) → CLUT → (1, 0.5, 0) → matrix:
    //   0.5·1 − 0.25·0.5 + 0 + 0.5  = 0.875
    //   1.0·1 + 2.0·0.5  − 0 − 0.25 = 1.75
    //  −2.0·1 + 0.25·0.5 + 0 + 2.0  = 0.125
    let src = [0.5, 1.0, 0.25, 1.0, 0.0, 0.5];
    let mut dst = [9.0; 6];
    pipeline.transform(&src, &mut dst).unwrap();
    assert_eq!(dst, [0.6875, 0.0, 2.5625, 0.875, 1.75, 0.125]);
}

#[test]
fn transform_trait_reports_pipeline_channels() {
    let pipeline = Pipeline::new(3, 3, vec![Stage::Clamp { channels: 3 }]).unwrap();
    let dynamic: &dyn Transform = &pipeline;
    assert_eq!(dynamic.input_channels(), 3);
    assert_eq!(dynamic.output_channels(), 3);
}
