//! The XYB ICC profile pin (issue #334): regenerates `gamut_jpeg::XYB_ICC_PROFILE` from
//! `gamut-icc` + `gamut-color` and asserts byte equality, then validates the bytes structurally
//! and end-to-end against the vendored lcms2 oracle.
//!
//! This test lives at the umbrella layer because it spans three publishable crates
//! (jpeg ← the static bytes, icc ← the writer, color ← the constants) that must not gain
//! dev-dependency edges on one another (the release-ordering rule in `AGENTS.md`).
//!
//! Regenerating after a deliberate change: run with `GAMUT_REGEN_XYB_ICC=1` to write the new
//! bytes to `crates/gamut-jpeg/src/xyb/xyb-srgb.icc`, then re-run normally.

use gamut::color::matrix::{D50, D65, bradford_adapt, rgb_to_xyz_matrix};
use gamut::color::xyb::{
    OPSIN_ABSORBANCE_BIAS, OPSIN_INVERSE, SCALED_XYB_OFFSET, SCALED_XYB_SCALE,
};
use gamut::icc::{
    Clut, ClutPrecision, ColorSpace, CurveOrParametric, DeviceClass, IccProfile, IccWriter,
    LutAToB, LutBToA, Matrix3x4, Mluc, MlucRecord, ParametricCurve, ProfileHeader, S15Fixed16,
    Signature, Tag, TagData, XyzNumber,
};

/// The `A2B0` matrix stage, transcribed from libjxl 0.12.0 (`jxl_cms_internal.h`,
/// `CreateICCLutAtoBTagForXYB`): `0.5 · XYZ(D50)←linear-sRGB · OPSIN_INVERSE`, the 0.5 baking in
/// the mAB PCS-XYZ encoding ceiling. Verified against a fresh derivation in
/// `a2b_matrix_matches_its_derivation` before being trusted.
const A2B_MATRIX: [f64; 9] = [
    1.5170095, -1.1065225, 0.071623, //
    -0.050022, 0.5683655, -0.018344, //
    -1.387676, 1.1145555, 0.6857255,
];

/// The sRGB primaries (IEC 61966-2-1 / BT.709), passed explicitly because `gamut-color` keeps its
/// chromaticity tables private.
const SRGB_PRIMARIES: [[f64; 2]; 3] = [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06]];

/// The derived per-channel constants of the unscaled A2B cube (libjxl `kXYBOffset`/`kXYBScale`):
/// offsets/scales that renormalize the mixed cube-root-domain corners into `[0, 1]` for the CLUT.
fn a2b_offset_scale() -> ([f64; 3], [f64; 3]) {
    let o = SCALED_XYB_OFFSET;
    let s = SCALED_XYB_SCALE;
    let reciprocal_sum = |a: f64, b: f64| (a * b) / (a + b);
    let offset = [o[0] + o[1], o[1] - o[0] + 1.0 / s[0], o[1] + o[2]];
    let scale = [
        reciprocal_sum(s[0], s[1]),
        reciprocal_sum(s[0], s[1]),
        reciprocal_sum(s[1], s[2]),
    ];
    (offset, scale)
}

/// One corner of the 2×2×2 A2B CLUT for device inputs `(x, y, b) ∈ {0, 1}³`: undo the scaled-XYB
/// affine, mix the opponent channels into the cube-root (LMS′) domain (`L′ = Y + X`, `M′ = Y − X`,
/// `S′ = (B−Y) + Y`), and renormalize into `[0, 1]` (trilinear interpolation is exact for this
/// affine chain, which is why two grid points per axis suffice).
fn a2b_corner(x: f64, y: f64, b: f64) -> [f64; 3] {
    let unscale = |v: f64, i: usize| v / SCALED_XYB_SCALE[i] - SCALED_XYB_OFFSET[i];
    let (cx, cy, cb) = (unscale(x, 0), unscale(y, 1), unscale(b, 2));
    let mixed = [cy + cx, cy - cx, cb + cy];
    let (offset, scale) = a2b_offset_scale();
    let mut out = [0.0; 3];
    for i in 0..3 {
        out[i] = (mixed[i] + offset[i]) * scale[i];
    }
    out
}

/// Three identity parametric curves (type 0, γ = 1) — the A and B stages of both LUTs.
fn identity_curves() -> Vec<CurveOrParametric> {
    (0..3)
        .map(|_| {
            CurveOrParametric::Parametric(ParametricCurve {
                function_type: 0,
                params: vec![S15Fixed16::from_f64(1.0)],
            })
        })
        .collect()
}

/// An `enUS` mluc tag with `text`.
fn mluc(text: &str) -> TagData {
    TagData::MultiLocalizedUnicode(Mluc {
        records: vec![MlucRecord {
            language: *b"en",
            country: *b"US",
            text: text.to_owned(),
        }],
    })
}

/// Builds the XYB→sRGB-PCS profile from `gamut-color`'s constants through `gamut-icc`'s model —
/// the same structure libjxl's XYB profile carries (input class, D50 XYZ PCS, perceptual;
/// desc/cprt/wtpt/chad/A2B0/B2A0), serialized by gamut-icc's writer.
fn build_xyb_icc() -> IccProfile {
    let mut header = ProfileHeader::new(DeviceClass::Input, ColorSpace::Rgb);
    header.preferred_cmm = Signature(*b"gamt");
    header.creator = Signature(*b"gamt");

    // chad: Bradford D65 (the sRGB white the samples came from) → the D50 PCS.
    let chad = bradford_adapt(D65, D50).expect("Bradford D65→D50 is well-defined");
    let chad_tag = TagData::S15Fixed16Array(
        chad.iter()
            .flatten()
            .map(|&v| S15Fixed16::from_f64(v))
            .collect(),
    );

    // A2B0: identity A curves → 2×2×2 16-bit CLUT → cube-root-undoing M curves → matrix → identity
    // B curves.
    let mut samples = Vec::with_capacity(8 * 3);
    for x in 0..2 {
        for y in 0..2 {
            for b in 0..2 {
                let corner = a2b_corner(f64::from(x), f64::from(y), f64::from(b));
                for v in corner {
                    let scaled = (v * 65535.0).round();
                    assert!(
                        (0.0..=65535.0).contains(&scaled),
                        "CLUT corner {v} out of range"
                    );
                    samples.push(scaled as u16);
                }
            }
        }
    }
    let clut = Clut {
        grid_points: vec![2, 2, 2],
        output_channels: 3,
        precision: ClutPrecision::U16,
        samples,
    };

    // M curves (ICC type 3: Y = (aX + b)^g for X ≥ d, else cX): undo the [0,1] renormalization
    // and the cube root, yielding linear LMS + bias.
    let (offset, scale) = a2b_offset_scale();
    let cbrt_bias = OPSIN_ABSORBANCE_BIAS.cbrt();
    let m_curves: Vec<CurveOrParametric> = (0..3)
        .map(|i| {
            let b = -offset[i] + cbrt_bias;
            CurveOrParametric::Parametric(ParametricCurve {
                function_type: 3,
                params: vec![
                    S15Fixed16::from_f64(3.0),                      // g
                    S15Fixed16::from_f64(1.0 / scale[i]),           // a
                    S15Fixed16::from_f64(b),                        // b
                    S15Fixed16::from_f64(0.0),                      // c
                    S15Fixed16::from_f64((-b * scale[i]).max(0.0)), // d
                ],
            })
        })
        .collect();

    // Matrix stage: rows of A2B_MATRIX, with the offset column folding away the +bias the M
    // curves leave on each channel (`intercept = row · (−bias)`).
    let matrix = Matrix3x4 {
        matrix: A2B_MATRIX.map(S15Fixed16::from_f64),
        offset: [0, 1, 2].map(|i| {
            let row = &A2B_MATRIX[i * 3..i * 3 + 3];
            S15Fixed16::from_f64(row.iter().map(|m| m * -OPSIN_ABSORBANCE_BIAS).sum())
        }),
    };

    let a2b = TagData::LutAToB(LutAToB {
        input_channels: 3,
        output_channels: 3,
        a_curves: Some(identity_curves()),
        clut: Some(clut),
        m_curves: Some(m_curves),
        matrix: Some(matrix),
        b_curves: identity_curves(),
    });
    // B2A0: the no-op inverse libjxl also writes (identity B curves only) — XYB is an encode-side
    // space; PCS→XYB is not a supported direction.
    let b2a = TagData::LutBToA(LutBToA {
        input_channels: 3,
        output_channels: 3,
        b_curves: identity_curves(),
        matrix: None,
        m_curves: None,
        clut: None,
        a_curves: None,
    });

    IccProfile {
        header,
        tags: vec![
            Tag::new(*b"desc", mluc("XYB_Per")),
            Tag::new(*b"cprt", mluc("CC0")),
            Tag::new(*b"wtpt", TagData::Xyz(vec![XyzNumber::D50])),
            Tag::new(*b"chad", chad_tag),
            Tag::new(*b"A2B0", a2b),
            Tag::new(*b"B2A0", b2a),
        ],
    }
}

fn serialized_xyb_icc() -> Vec<u8> {
    IccWriter::new()
        .recompute_profile_id(true)
        .write(&build_xyb_icc())
        .expect("serialize XYB profile")
}

#[test]
fn a2b_matrix_matches_its_derivation() {
    // The transcribed literal must be 0.5 · Bradford(D65→D50) · XYZ←sRGB · OPSIN_INVERSE — the
    // f32 provenance of the upstream literals bounds the agreement at ~1e-4.
    let to_xyz = rgb_to_xyz_matrix(&SRGB_PRIMARIES, D65).expect("sRGB→XYZ");
    let adapt = bradford_adapt(D65, D50).expect("Bradford");
    let mut derived = [[0.0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            let mut d50 = 0.0;
            for k in 0..3 {
                // (adapt · to_xyz) folded on the fly to avoid exposing a matrix-multiply helper.
                let mut adapted = 0.0;
                for l in 0..3 {
                    adapted += adapt[i][l] * to_xyz[l][k];
                }
                d50 += adapted * OPSIN_INVERSE[k][j];
            }
            derived[i][j] = 0.5 * d50;
        }
    }
    for i in 0..3 {
        for j in 0..3 {
            assert!(
                (derived[i][j] - A2B_MATRIX[i * 3 + j]).abs() < 5e-4,
                "matrix[{i}][{j}]: derived {} vs literal {}",
                derived[i][j],
                A2B_MATRIX[i * 3 + j]
            );
        }
    }
}

#[test]
fn vendored_profile_bytes_regenerate_exactly() {
    let bytes = serialized_xyb_icc();
    if std::env::var_os("GAMUT_REGEN_XYB_ICC").is_some() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../gamut-jpeg/src/xyb/xyb-srgb.icc"
        );
        std::fs::write(path, &bytes).expect("write regenerated profile");
    }
    assert_eq!(
        gamut::jpeg::XYB_ICC_PROFILE,
        bytes.as_slice(),
        "vendored XYB profile drifted from its generator (GAMUT_REGEN_XYB_ICC=1 regenerates)"
    );
}

#[test]
fn profile_is_structurally_valid() {
    let bytes = serialized_xyb_icc();
    let parsed = IccProfile::parse(&bytes).expect("re-parse");
    let findings = parsed.validate();
    assert!(findings.is_empty(), "conformance findings: {findings:?}");
    assert_eq!(parsed.header.device_class, DeviceClass::Input);
    assert_eq!(parsed.header.data_color_space, ColorSpace::Rgb);
    assert_eq!(parsed.tags.len(), 6);
}

#[test]
fn lcms_reproduces_srgb_from_scaled_xyb_samples() {
    use gamut::color::transfer::srgb_eotf;
    use gamut::color::xyb::{linear_srgb_to_xyb, scale_xyb};

    // The end-to-end claim of issue #334: any ICC-aware decoder reproduces sRGB from the XYB
    // samples plus this profile. Feed lcms2 the exact u8 samples gamut-jpeg's XYB mode would
    // store for a grid of sRGB colours; transforming them through (XYB profile → lcms2's own
    // sRGB profile) must land within a few 8-bit codes of the original — the residual comes from
    // the 8-bit sample quantization, the s15Fixed16 curve/matrix parameters, and lcms2's own
    // integer pipeline.
    let xyb_profile = lcms2_oracle::Profile::from_bytes(&serialized_xyb_icc())
        .expect("lcms2 accepts the profile");
    let srgb = lcms2_oracle::srgb();

    let mut sources = Vec::new();
    let mut samples = Vec::new();
    for r in (0..=255u16).step_by(51) {
        for g in (0..=255u16).step_by(51) {
            for b in (0..=255u16).step_by(51) {
                sources.extend_from_slice(&[r as u8, g as u8, b as u8]);
                let linear = [
                    srgb_eotf(f64::from(r) / 255.0),
                    srgb_eotf(f64::from(g) / 255.0),
                    srgb_eotf(f64::from(b) / 255.0),
                ];
                let scaled = scale_xyb(linear_srgb_to_xyb(linear));
                for s in scaled {
                    samples.push((s * 255.0).round() as u8);
                }
            }
        }
    }

    let recovered = lcms2_oracle::transform_rgb8(&xyb_profile, &srgb, 0, &samples);
    let mut worst = 0u8;
    let mut total = 0u64;
    for (out, src) in recovered.iter().zip(sources.iter()) {
        worst = worst.max(out.abs_diff(*src));
        total += u64::from(out.abs_diff(*src));
    }
    let mean = total as f64 / recovered.len() as f64;
    // The worst cells are an inherent property of the 8-bit scaled-XYB representation, not a
    // profile defect (jpegli shares them): a half-LSB error in the X sample moves L′ and M′ in
    // opposite directions, the opsin-inverse rows amplify the difference ~21×, and the result
    // lands on the steep toe of the sRGB OETF — measured worst 25 codes, always in a near-zero
    // red channel under saturated green/blue, where the visual impact is negligible. Measured
    // mean 2.46 codes over this deliberately saturation-heavy grid. Both asserted with margin.
    assert!(worst <= 32, "worst sRGB reproduction error {worst} codes");
    assert!(mean <= 3.5, "mean sRGB reproduction error {mean:.2} codes");
}
