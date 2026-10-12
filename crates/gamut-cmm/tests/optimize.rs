//! Integration tests for the opt-in pipeline optimization (#372): that the knob reaches every
//! transform constructor, that leaving it off changes nothing, and that each pass does what it
//! claims through the public API.
//!
//! The *precision budget* of the lossy tier is gated separately, against the oracle, in
//! `tests/oracle_conformance.rs` — this file only needs profiles it can build by hand.

use gamut_cmm::{
    ClutInterpolation, GamutCheck, IccTransform, Pipeline, PipelineOptimization, Stage, ToneCurve,
    Transform as _, TransformOptions, device_to_pcs, pcs_to_device,
};
use gamut_icc::{
    ColorSpace, Curve, CurveOrParametric, DeviceClass, IccProfile, ProfileHeader, RenderingIntent,
    Signature, TagData, U8Fixed8, XyzNumber,
};

/// An RGB→XYZ matrix/TRC display shaper over exact-dyadic colorants at gamma `g`
/// (`u8Fixed8`), optionally carrying `wtpt`.
fn rgb_shaper(g: u16, wtpt: Option<[f64; 3]>) -> IccProfile {
    let xyz_tag = |v: [f64; 3]| TagData::Xyz(vec![XyzNumber::from_f64(v)]);
    let trc = || TagData::Curve(Curve::Gamma(U8Fixed8(g)));
    let mut tags = vec![
        (Signature(*b"rXYZ"), xyz_tag([0.5, 0.25, 0.0625])),
        (Signature(*b"gXYZ"), xyz_tag([0.375, 0.625, 0.125])),
        (Signature(*b"bXYZ"), xyz_tag([0.125, 0.125, 0.625])),
        (Signature(*b"rTRC"), trc()),
        (Signature(*b"gTRC"), trc()),
        (Signature(*b"bTRC"), trc()),
    ];
    if let Some(white) = wtpt {
        tags.push((Signature(*b"wtpt"), xyz_tag(white)));
    }
    IccProfile {
        header: ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb),
        tags,
    }
}

fn options(intent: RenderingIntent, optimization: PipelineOptimization) -> TransformOptions {
    TransformOptions {
        intent,
        black_point_compensation: false,
        optimization,
        prefer_cicp_transfer: false,
    }
}

/// A seeded device sweep over the RGB cube: the eight corners, a neutral ramp, and a
/// deterministic scatter.
fn sweep() -> Vec<[f64; 3]> {
    let mut points: Vec<[f64; 3]> = Vec::new();
    for corner in 0..8u32 {
        points.push([
            f64::from(corner & 1),
            f64::from((corner >> 1) & 1),
            f64::from((corner >> 2) & 1),
        ]);
    }
    for i in 0..=16u32 {
        let v = f64::from(i) / 16.0;
        points.push([v; 3]);
    }
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    while points.len() < 200 {
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            f64::from((state >> 33) as u32) / f64::from(u32::MAX)
        };
        points.push([next(), next(), next()]);
    }
    points
}

fn eval(transform: &IccTransform, rgb: [f64; 3]) -> [f64; 3] {
    let mut out = [0.0; 3];
    transform.transform(&rgb, &mut out).unwrap();
    out
}

/// The stage-kind fingerprint of a pipeline.
fn kinds(pipeline: &Pipeline) -> Vec<&'static str> {
    pipeline
        .stages()
        .iter()
        .map(|stage| match stage {
            Stage::Curves(_) => "curves",
            Stage::Clut(_) => "clut",
            Stage::Matrix { .. } => "matrix",
            Stage::MatrixN { .. } => "matrixn",
            Stage::Clamp { .. } => "clamp",
            Stage::Identity { .. } => "identity",
            Stage::XyzToLab => "xyz2lab",
            Stage::LabToXyz => "lab2xyz",
            _ => "other",
        })
        .collect()
}

fn gamma_curve(g: u16) -> ToneCurve {
    ToneCurve::new(&CurveOrParametric::Curve(Curve::Gamma(U8Fixed8(g)))).unwrap()
}

#[test]
fn optimization_is_off_by_default() {
    assert_eq!(
        TransformOptions::default().optimization,
        PipelineOptimization::None
    );
    assert_eq!(PipelineOptimization::default(), PipelineOptimization::None);
}

#[test]
fn optimization_off_evaluates_the_link_exactly_as_composed() {
    // Media-relative between two profiles with the same (default) media white: the PCS seam
    // is an empty layer, so `between` must produce exactly the two halves concatenated —
    // which is a construction this test can build independently, out of the public link API.
    let src = rgb_shaper(0x0233, None);
    let dst = rgb_shaper(0x0100, None);
    let intent = RenderingIntent::MediaRelativeColorimetric;
    let composed = device_to_pcs(&src, intent)
        .unwrap()
        .compose(pcs_to_device(&dst, intent).unwrap())
        .unwrap();
    let transform =
        IccTransform::between(&src, &dst, options(intent, PipelineOptimization::None)).unwrap();
    for rgb in sweep() {
        let mut expected = [0.0; 3];
        composed.eval(&rgb, &mut expected).unwrap();
        // Bit-exact: with the knob off nothing in the funnel touches the chain.
        assert_eq!(eval(&transform, rgb), expected, "at {rgb:?}");
    }
}

#[test]
fn collapse_shortens_the_chain_and_keeps_the_colours() {
    let src = rgb_shaper(0x0233, None);
    let dst = rgb_shaper(0x0100, Some([0.9504, 1.0, 1.0889]));
    // Absolute rendering inserts the white-scaling matrix, so the link carries three
    // adjacent matrices for folding to work on.
    let intent = RenderingIntent::IccAbsoluteColorimetric;
    let plain =
        IccTransform::between(&src, &dst, options(intent, PipelineOptimization::None)).unwrap();
    let collapsed =
        IccTransform::between(&src, &dst, options(intent, PipelineOptimization::Collapse)).unwrap();
    for rgb in sweep() {
        let a = eval(&plain, rgb);
        let b = eval(&collapsed, rgb);
        for (x, y) in a.iter().zip(&b) {
            // Folding only re-associates the same products: agreement is at `f64` noise.
            assert!((x - y).abs() < 1e-12, "{a:?} vs {b:?} at {rgb:?}");
        }
    }
}

#[test]
fn precalculate_collapses_a_shaper_pair_into_one_clut() {
    let src = rgb_shaper(0x0233, None);
    let dst = rgb_shaper(0x0100, None);
    let intent = RenderingIntent::MediaRelativeColorimetric;
    let pipeline = device_to_pcs(&src, intent)
        .unwrap()
        .compose(pcs_to_device(&dst, intent).unwrap())
        .unwrap();
    assert_eq!(
        kinds(&pipeline),
        ["curves", "matrix", "matrix", "curves"],
        "the unoptimized shaper pair"
    );
    let optimized = pipeline
        .clone()
        .optimized(PipelineOptimization::Precalculate)
        .unwrap();
    assert_eq!(kinds(&optimized), ["clut"]);
    let Stage::Clut(table) = &optimized.stages()[0] else {
        panic!("the collapsed stage is the resampled CLUT");
    };
    // lcms2's default RGB resolution, and its default device interpolation.
    assert_eq!(table.grid_points(), [33, 33, 33]);
    assert_eq!(table.interpolation(), ClutInterpolation::Tetrahedral);
    // Off the grid the resampling interpolates, which is the lossy part — bounded here in
    // device units (the ΔE₀₀ budget is gated against the oracle in oracle_conformance.rs).
    let mut worst = 0.0_f64;
    for rgb in sweep() {
        let (mut plain, mut fast) = ([0.0; 3], [0.0; 3]);
        pipeline.eval(&rgb, &mut plain).unwrap();
        optimized.eval(&rgb, &mut fast).unwrap();
        for (x, y) in plain.iter().zip(&fast) {
            worst = worst.max((x - y).abs());
        }
    }
    assert!(worst < 2e-2, "worst device-unit deviation {worst}");
}

#[test]
fn every_constructor_takes_the_knob() {
    let src = rgb_shaper(0x0233, None);
    let dst = rgb_shaper(0x0100, None);
    let proof = rgb_shaper(0x01cc, None);
    let intent = RenderingIntent::MediaRelativeColorimetric;
    for level in [
        PipelineOptimization::None,
        PipelineOptimization::Collapse,
        PipelineOptimization::Precalculate,
    ] {
        let between = IccTransform::between(&src, &dst, options(intent, level)).unwrap();
        let chained = IccTransform::chain(&[&src, &proof, &dst], options(intent, level)).unwrap();
        let check = GamutCheck::new(&src, &proof, options(intent, level)).unwrap();
        assert_eq!(
            (between.input_channels(), between.output_channels()),
            (3, 3)
        );
        assert_eq!(
            (chained.input_channels(), chained.output_channels()),
            (3, 3)
        );
        assert_eq!(check.output_channels(), 1);
        // Every level still runs, and the levels agree closely enough that no pass has
        // silently changed what the transform means.
        let mid = [0.4, 0.6, 0.2];
        let plain =
            IccTransform::between(&src, &dst, options(intent, PipelineOptimization::None)).unwrap();
        let (a, b) = (eval(&plain, mid), eval(&between, mid));
        for (x, y) in a.iter().zip(&b) {
            assert!((x - y).abs() < 2e-2, "{level:?}: {a:?} vs {b:?}");
        }
    }
}

#[test]
fn matrix_folding_is_visible_through_the_public_pipeline_api() {
    let matrix = Stage::Matrix {
        m: [[0.5, -0.25, 0.125], [1.0, 2.0, -0.5], [-2.0, 0.25, 1.0]],
        offset: [0.5, -0.25, 2.0],
    };
    let pipeline = Pipeline::new(
        3,
        3,
        vec![
            matrix.clone(),
            Stage::Identity { channels: 3 },
            matrix.clone(),
            matrix,
        ],
    )
    .unwrap();
    let optimized = pipeline.optimized(PipelineOptimization::Collapse).unwrap();
    assert_eq!(kinds(&optimized), ["matrix"]);
}

#[test]
fn curve_joining_is_visible_through_the_public_pipeline_api() {
    // Two curve sets around a stage that blocks resampling, so the joined set survives into
    // the optimized pipeline instead of being swallowed by a CLUT.
    let pipeline = Pipeline::new(
        3,
        3,
        vec![
            Stage::Curves(vec![gamma_curve(0x0200); 3]),
            Stage::Curves(vec![gamma_curve(0x0200); 3]),
            Stage::XyzToLab,
        ],
    )
    .unwrap();
    let optimized = pipeline
        .optimized(PipelineOptimization::Precalculate)
        .unwrap();
    assert_eq!(kinds(&optimized), ["curves", "xyz2lab"]);
    let Stage::Curves(joined) = &optimized.stages()[0] else {
        panic!("the two curve sets joined into one");
    };
    assert_eq!(joined.len(), 3);
    // γ2 ∘ γ2 = γ4 at the tabulated resolution (chord error 2.2e-8 at this probe).
    assert!((joined[0].eval(0.5) - 0.0625).abs() < 1e-7);
}

#[test]
fn optimization_never_changes_a_transforms_shape() {
    // The knob is a performance knob: channel counts, and so the buffer contract, are
    // identical at every level.
    let src = rgb_shaper(0x0233, None);
    let dst = rgb_shaper(0x0100, None);
    let reference = IccTransform::between(
        &src,
        &dst,
        options(RenderingIntent::Perceptual, PipelineOptimization::None),
    )
    .unwrap();
    for level in [
        PipelineOptimization::Collapse,
        PipelineOptimization::Precalculate,
    ] {
        let transform =
            IccTransform::between(&src, &dst, options(RenderingIntent::Perceptual, level)).unwrap();
        assert_eq!(transform.input_channels(), reference.input_channels());
        assert_eq!(transform.output_channels(), reference.output_channels());
    }
}
