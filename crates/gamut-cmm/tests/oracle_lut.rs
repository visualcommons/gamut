//! Differential tests of LUT-profile linking (`gamut_cmm::link`, phase P5/#328) against
//! Little-CMS.
//!
//! Methodology as in `tests/oracle_shaper.rs`: every oracle profile is synthesized in memory
//! by `tooling/lcms2-oracle`, serialized once, and **both** sides read the same bytes
//! (`gamut-icc` parses them, lcms2 reopens them), so tag quantization is identical and the
//! comparison isolates evaluation semantics. The lcms2 side runs end-to-end transforms
//! (`NOOPTIMIZE|NOCACHE`) against a built-in Lab identity profile with `TYPE_Lab_DBL` /
//! `TYPE_CMYK_DBL` formatters — decoded Lab on the PCS side (directly comparable to this
//! crate's decoded-PCS pipelines) and ink percentages 0..100 on the CMYK side (rescaled).
//!
//! Expected agreement: lcms2 evaluates profile-borne 16-bit CLUTs through its fixed-point
//! interpolators even in double transforms (`EvaluateCLUTfloatIn16` quantizes each input to
//! 16 bits), so the bound is 16-bit-quantization-tight, amplified by the per-axis slope
//! (`grid − 1`) and the decoded-Lab scaling (`×100` on `L*`, `×255` on `a*`/`b*`) — the same
//! reasoning as the CLUT phase's profile-route bound. Measured values at each assert.
//!
//! # Keeping lcms2's implicit v4 BPC out of the comparison
//!
//! lcms2 **forces black-point compensation for v4 profiles under the perceptual and
//! saturation intents** (`_cmsLinkProfiles`, `cmscnvrt.c` — "BPC … applies always on V4
//! perceptual and saturation", following Adobe's document), keyed on the *output-side* (or
//! abstract) profile of each hop. BPC is issue #329's scope, so these per-intent
//! differentials are arranged to never trigger it: the PCS endpoint is the **v2** Lab
//! identity profile ([`lab2`], never BPC-forced), which fully covers every device→PCS
//! transform (the LUT profile sits on the never-compensated *input* hop) — and the
//! PCS→device perceptual/saturation runs use a **version-downgraded twin** of the v4
//! profile (header version 2.4, `mAB `/`mBA ` tag payloads intact; lcms2 reads LUT tags and
//! picks the v2-Lab fixup by the tag's *true type*, never the header version, so the twin
//! evaluates identically minus the BPC forcing — and doubles as a pin that this crate's
//! encoding selection likewise keys on the element type). Media-relative (never BPC-forced)
//! is additionally compared on the true v4 profile.

use gamut_cmm::{ClutInterpolation, ClutTable, Pipeline, Stage, device_to_pcs, pcs_to_device};
use gamut_icc::{IccProfile, KnownTag, ProfileVersion, RenderingIntent, Signature, TagData};
use lcms2_oracle::{
    FLAGS_NOCACHE, FLAGS_NOOPTIMIZE, INTENT_PERCEPTUAL, INTENT_RELATIVE_COLORIMETRIC,
    INTENT_SATURATION, Profile, TYPE_CMYK_DBL, TYPE_Lab_DBL, TYPE_RGB_DBL, Transform, cmyk_prtr_v2,
    cmyk_prtr_v4, lab2, lab4, scnr_lut, set_quiet_log_handler,
};

/// The three intents both sides can compare end to end at this phase (BPC arrangement in
/// the module docs). ICC-absolute is excluded from the lcms2 comparison — lcms2 adds the
/// absolute white-point scaling (`diag(whiteIn/whiteOut)`, non-identity for these
/// D65-`wtpt` profiles), which this crate adds with issue #329; our absolute pipeline is
/// pinned against our relative one instead (same tag, by lcms2's own
/// `Device2PCS16`/`PCS2Device16` tables).
const INTENTS: [(RenderingIntent, u32); 3] = [
    (RenderingIntent::Perceptual, INTENT_PERCEPTUAL),
    (
        RenderingIntent::MediaRelativeColorimetric,
        INTENT_RELATIVE_COLORIMETRIC,
    ),
    (RenderingIntent::Saturation, INTENT_SATURATION),
];

/// A deterministic 64-bit LCG (Knuth's MMIX constants) for seeded sweeps.
struct Lcg(u64);

impl Lcg {
    fn next_unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        f64::from((self.0 >> 33) as u32) / f64::from(u32::MAX)
    }
}

/// Serializes an oracle-synthesized profile once and hands the same bytes to both sides
/// (see `tests/oracle_shaper.rs` for why the reopen matters).
fn reopen(profile: &Profile) -> (IccProfile, Profile) {
    let bytes = profile.to_bytes();
    let parsed = IccProfile::parse(&bytes).expect("gamut-icc parses the lcms2-written profile");
    let oracle = Profile::from_bytes(&bytes).expect("lcms2 reopens its own bytes");
    (parsed, oracle)
}

/// Re-serializes a parsed profile with its header version dropped to 2.4 — `mAB `/`mBA `
/// payloads intact — and hands the same bytes to both sides. Sidesteps lcms2's implicit v4
/// perceptual/saturation BPC without changing a single tag byte (module docs).
fn down_versioned(parsed: &IccProfile) -> (IccProfile, Profile) {
    let mut twin = parsed.clone();
    twin.header.version = ProfileVersion {
        major: 2,
        minor: 4,
        bugfix: 0,
    };
    let bytes = twin.to_bytes().expect("gamut-icc serializes the twin");
    let reparsed = IccProfile::parse(&bytes).expect("twin round-trips");
    let oracle = Profile::from_bytes(&bytes).expect("lcms2 opens the down-versioned twin");
    (reparsed, oracle)
}

/// CMYK device sweep: all 16 ink corners, gray ramps, and seeded random fill, in `[0, 1]`.
fn cmyk_sweep(seed: u64) -> Vec<[f64; 4]> {
    let mut points: Vec<[f64; 4]> = Vec::new();
    for corner in 0..16u32 {
        points.push([
            f64::from(corner & 1),
            f64::from((corner >> 1) & 1),
            f64::from((corner >> 2) & 1),
            f64::from((corner >> 3) & 1),
        ]);
    }
    for i in 0..=8 {
        let v = f64::from(i) / 8.0;
        points.push([v; 4]);
    }
    let mut lcg = Lcg(seed);
    while points.len() < 180 {
        points.push([
            lcg.next_unit(),
            lcg.next_unit(),
            lcg.next_unit(),
            lcg.next_unit(),
        ]);
    }
    points
}

/// Decoded-Lab sweep: a coarse L/a/b lattice plus seeded random fill, kept inside
/// `L ∈ [0, 100]`, `a, b ∈ [−120, 120]` (away from the v2/v4 `a*`/`b*` ceiling mismatch —
/// `127.0` v4 vs `127.996` v2 — where the two sides' *input clamps* differ by design).
fn lab_sweep(seed: u64) -> Vec<[f64; 3]> {
    let mut points: Vec<[f64; 3]> = Vec::new();
    for l in [0.0, 25.0, 50.0, 75.0, 100.0] {
        for ab in [-100.0, -40.0, 0.0, 40.0, 100.0] {
            points.push([l, ab, -ab]);
            points.push([l, ab, 40.0]);
        }
    }
    let mut lcg = Lcg(seed);
    while points.len() < 180 {
        points.push([
            lcg.next_unit() * 100.0,
            lcg.next_unit() * 240.0 - 120.0,
            lcg.next_unit() * 240.0 - 120.0,
        ]);
    }
    points
}

/// RGB device sweep: corners, gray ramp, seeded random fill, in `[0, 1]`.
fn rgb_sweep(seed: u64) -> Vec<[f64; 3]> {
    let mut points: Vec<[f64; 3]> = Vec::new();
    for corner in 0..8u32 {
        points.push([
            f64::from(corner & 1),
            f64::from((corner >> 1) & 1),
            f64::from((corner >> 2) & 1),
        ]);
    }
    for i in 0..=8 {
        let v = f64::from(i) / 8.0;
        points.push([v; 3]);
    }
    let mut lcg = Lcg(seed);
    while points.len() < 160 {
        points.push([lcg.next_unit(), lcg.next_unit(), lcg.next_unit()]);
    }
    points
}

fn eval(pipeline: &Pipeline, input: &[f64]) -> Vec<f64> {
    let mut out = vec![0.0; usize::from(pipeline.output_channels())];
    pipeline.eval(input, &mut out).unwrap();
    out
}

/// Worst per-component |Δ| between our device→PCS pipeline (CMYK in `[0, 1]` → decoded Lab)
/// and the lcms2 profile→Lab4 transform (ink percentages → `TYPE_Lab_DBL`), per intent.
fn a2b_worst(
    parsed: &IccProfile,
    oracle: &Profile,
    our_intent: RenderingIntent,
    lcms_intent: u32,
    seed: u64,
) -> f64 {
    let ours = device_to_pcs(parsed, our_intent).unwrap();
    // The v2 Lab identity endpoint keeps lcms2's implicit v4 BPC out of the chain (module
    // docs); its identity pipeline (v2 fixups cancelling) adds no measurable noise.
    let transform = Transform::new(
        oracle,
        TYPE_CMYK_DBL,
        &lab2(),
        TYPE_Lab_DBL,
        lcms_intent,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let mut worst = 0.0_f64;
    for point in cmyk_sweep(seed) {
        let got = eval(&ours, &point);
        let ink: Vec<f64> = point.iter().map(|&v| v * 100.0).collect();
        let want = transform.apply_f64(&ink, 1, 3);
        for ch in 0..3 {
            worst = worst.max((got[ch] - want[ch]).abs());
        }
    }
    worst
}

/// Worst per-component |Δ| between our PCS→device pipeline (decoded Lab → CMYK `[0, 1]`)
/// and the lcms2 Lab4→profile transform, per intent.
fn b2a_worst(
    parsed: &IccProfile,
    oracle: &Profile,
    our_intent: RenderingIntent,
    lcms_intent: u32,
    seed: u64,
) -> f64 {
    let ours = pcs_to_device(parsed, our_intent).unwrap();
    // v2 Lab endpoint: see the module docs' BPC arrangement.
    let transform = Transform::new(
        &lab2(),
        TYPE_Lab_DBL,
        oracle,
        TYPE_CMYK_DBL,
        lcms_intent,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let mut worst = 0.0_f64;
    for point in lab_sweep(seed) {
        let got = eval(&ours, &point);
        let want = transform.apply_f64(&point, 1, 4);
        for ch in 0..4 {
            worst = worst.max((got[ch] - want[ch] / 100.0).abs());
        }
    }
    worst
}

#[test]
fn cmyk_v4_a2b_matches_lcms2_per_intent() {
    set_quiet_log_handler();
    let (parsed, oracle) = reopen(&cmyk_prtr_v4(9));
    for (our_intent, lcms_intent) in INTENTS {
        let worst = a2b_worst(&parsed, &oracle, our_intent, lcms_intent, 11);
        // Profile-borne 16-bit CLUT route (module docs): input snap 0.5/65535 × slope 8
        // (grid 9), decoded to Lab (×255 on a*/b*), plus S15.16 interpolant rounding.
        // Measured worst 3.8e-3 across the three intents.
        assert!(worst < 2e-2, "{our_intent:?}: worst Lab |Δ| = {worst:e}");
    }
}

#[test]
fn cmyk_v2_a2b_matches_lcms2_per_intent() {
    set_quiet_log_handler();
    // The lut16 route: same tags serialized as mft2, whose Lab seam is the v2 encoding —
    // a v4-encoded seam would miss by up to 0.39% of L* (≈ 0.39 at L* = 100), far above
    // the asserted bound, so this differential doubles as the v2-rule regression.
    let (parsed, oracle) = reopen(&cmyk_prtr_v2(9));
    for (our_intent, lcms_intent) in INTENTS {
        let worst = a2b_worst(&parsed, &oracle, our_intent, lcms_intent, 13);
        // Measured worst 4.2e-3 (same route as v4; the seam constants differ, the CLUT
        // quantization dominates).
        assert!(worst < 2e-2, "{our_intent:?}: worst Lab |Δ| = {worst:e}");
    }
}

#[test]
fn cmyk_v4_b2a_matches_lcms2_per_intent() {
    set_quiet_log_handler();
    // Media-relative on the true v4 profile (never BPC-forced); perceptual and saturation
    // on the down-versioned twin — same mBA tag bytes, no implicit v4 BPC (module docs).
    let (parsed, oracle) = reopen(&cmyk_prtr_v4(9));
    let worst = b2a_worst(
        &parsed,
        &oracle,
        RenderingIntent::MediaRelativeColorimetric,
        INTENT_RELATIVE_COLORIMETRIC,
        17,
    );
    // Device-side outputs in [0, 1]: 16-bit CLUT route without the Lab decode
    // amplification. Measured worst 2.1e-5.
    assert!(worst < 5e-4, "relative: worst CMYK |Δ| = {worst:e}");
    let (twin, twin_oracle) = down_versioned(&parsed);
    assert!(
        matches!(twin.get(KnownTag::BToA0), Some(TagData::LutBToA(_))),
        "the twin's B2A tags stay mBA-typed"
    );
    for (our_intent, lcms_intent) in INTENTS {
        let worst = b2a_worst(&twin, &twin_oracle, our_intent, lcms_intent, 17);
        // Measured worst 2.1e-5 per intent.
        assert!(worst < 5e-4, "{our_intent:?}: worst CMYK |Δ| = {worst:e}");
    }
}

#[test]
fn cmyk_v2_b2a_matches_lcms2_per_intent() {
    set_quiet_log_handler();
    let (parsed, oracle) = reopen(&cmyk_prtr_v2(9));
    for (our_intent, lcms_intent) in INTENTS {
        let worst = b2a_worst(&parsed, &oracle, our_intent, lcms_intent, 19);
        // The v2 seam here is the *encode* direction (Lab input); a v4-encoded seam would
        // shift the CLUT index by up to 0.39% of the axis (≈ 2e-3 in the output through the
        // warp's slope) — an order above this bound. Measured worst 2.4e-5.
        assert!(worst < 5e-4, "{our_intent:?}: worst CMYK |Δ| = {worst:e}");
    }
}

#[test]
fn scnr_mab_rgb_to_lab_matches_lcms2() {
    set_quiet_log_handler();
    // Camera/scanner-shaped input profile: one A2B0 mAB tag. Requesting media-relative also
    // exercises the perceptual fallback (A2B1 is absent) against lcms2's identical fallback.
    let (parsed, oracle) = reopen(&scnr_lut(9));
    let ours = device_to_pcs(&parsed, RenderingIntent::MediaRelativeColorimetric).unwrap();
    let transform = Transform::new(
        &oracle,
        TYPE_RGB_DBL,
        &lab4(),
        TYPE_Lab_DBL,
        INTENT_RELATIVE_COLORIMETRIC,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let mut worst = 0.0_f64;
    for point in rgb_sweep(23) {
        let got = eval(&ours, &point);
        let want = transform.apply_f64(&point, 1, 3);
        for ch in 0..3 {
            worst = worst.max((got[ch] - want[ch]).abs());
        }
    }
    // Measured 2.4e-3 (16-bit CLUT route, grid 9, Lab-decoded).
    assert!(worst < 2e-2, "scnr mAB: worst Lab |Δ| = {worst:e}");
}

#[test]
fn absolute_intent_builds_exactly_the_relative_pipeline() {
    set_quiet_log_handler();
    // lcms2's intent tables map absolute to the RELATIVE tag (Device2PCS16[3] = A2B1,
    // PCS2Device16[3] = B2A1): with the white-point scaling deferred to #329, our absolute
    // and relative pipelines must agree bit for bit — while being distinct from perceptual
    // and saturation (proving the tag selection is observable).
    let (parsed, _) = reopen(&cmyk_prtr_v4(9));
    let relative = device_to_pcs(&parsed, RenderingIntent::MediaRelativeColorimetric).unwrap();
    let absolute = device_to_pcs(&parsed, RenderingIntent::IccAbsoluteColorimetric).unwrap();
    for point in cmyk_sweep(29).into_iter().take(60) {
        assert_eq!(eval(&relative, &point), eval(&absolute, &point));
    }
    let rel_b2a = pcs_to_device(&parsed, RenderingIntent::MediaRelativeColorimetric).unwrap();
    let abs_b2a = pcs_to_device(&parsed, RenderingIntent::IccAbsoluteColorimetric).unwrap();
    for point in lab_sweep(31).into_iter().take(60) {
        assert_eq!(eval(&rel_b2a, &point), eval(&abs_b2a, &point));
    }
}

#[test]
fn per_intent_outputs_are_pairwise_distinct() {
    set_quiet_log_handler();
    // The prtr synthesizer warps each intent slot differently: the three per-intent A2B
    // pipelines must produce visibly different Lab for one mid-gamut ink — guarding against
    // an intent index that collapses to one tag.
    let (parsed, _) = reopen(&cmyk_prtr_v4(9));
    let input = [0.2, 0.45, 0.7, 0.1];
    let outs: Vec<Vec<f64>> = [
        RenderingIntent::Perceptual,
        RenderingIntent::MediaRelativeColorimetric,
        RenderingIntent::Saturation,
    ]
    .into_iter()
    .map(|intent| eval(&device_to_pcs(&parsed, intent).unwrap(), &input))
    .collect();
    for i in 0..3 {
        for j in i + 1..3 {
            let distance: f64 = (0..3).map(|ch| (outs[i][ch] - outs[j][ch]).abs()).sum();
            assert!(
                distance > 0.05,
                "intents {i} and {j} coincide: {:?} vs {:?}",
                outs[i],
                outs[j]
            );
        }
    }
}

#[test]
fn fallback_to_perceptual_matches_lcms2_on_the_same_modified_profile() {
    set_quiet_log_handler();
    // Remove A2B2 from the parsed profile: a saturation request must fall back to A2B0 —
    // equal to the perceptual pipeline bit for bit — and lcms2, fed the SAME modified bytes,
    // must make the same choice (its tag16 fallback in `_cmsReadInputLUT`).
    let (mut parsed, _) = reopen(&cmyk_prtr_v4(9));
    parsed.tags.retain(|(sig, _)| sig.0 != *b"A2B2");
    assert!(parsed.get(KnownTag::AToB2).is_none(), "A2B2 removed");
    let saturation = device_to_pcs(&parsed, RenderingIntent::Saturation).unwrap();
    let perceptual = device_to_pcs(&parsed, RenderingIntent::Perceptual).unwrap();
    for point in cmyk_sweep(37).into_iter().take(60) {
        assert_eq!(eval(&saturation, &point), eval(&perceptual, &point));
    }
    // Round-trip the modified profile through gamut-icc's writer into lcms2.
    let bytes = parsed.to_bytes().expect("gamut-icc serializes the profile");
    let oracle = Profile::from_bytes(&bytes).expect("lcms2 opens the gamut-icc-written bytes");
    // v2 Lab endpoint: keeps lcms2's implicit v4 BPC out of the saturation chain (module
    // docs — the v4 prtr sits on the input hop, which is never compensated).
    let transform = Transform::new(
        &oracle,
        TYPE_CMYK_DBL,
        &lab2(),
        TYPE_Lab_DBL,
        INTENT_SATURATION,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let mut worst = 0.0_f64;
    for point in cmyk_sweep(41).into_iter().take(80) {
        let got = eval(&saturation, &point);
        let ink: Vec<f64> = point.iter().map(|&v| v * 100.0).collect();
        let want = transform.apply_f64(&ink, 1, 3);
        for ch in 0..3 {
            worst = worst.max((got[ch] - want[ch]).abs());
        }
    }
    // Same bound family as the direct A2B differential. Measured 3.4e-3.
    assert!(worst < 2e-2, "fallback vs lcms2: worst Lab |Δ| = {worst:e}");
}

#[test]
fn v2_and_v4_profiles_decode_with_their_own_lab_constants() {
    set_quiet_log_handler();
    // The dedicated encoding-selection pin at the integration level: the SAME synthesized
    // colorimetry serialized as mAB (v4 profile) vs mft2 (v2 profile) must produce pipelines
    // whose Lab seam stages carry the v4 and v2 constants respectively.
    let (v4, _) = reopen(&cmyk_prtr_v4(9));
    let (v2, _) = reopen(&cmyk_prtr_v2(9));
    assert!(matches!(v4.get(KnownTag::AToB0), Some(TagData::LutAToB(_))));
    assert!(matches!(v2.get(KnownTag::AToB0), Some(TagData::Lut16(_))));
    let seam_of = |profile: &IccProfile| {
        let pipeline = device_to_pcs(profile, RenderingIntent::Perceptual).unwrap();
        let Some(Stage::Matrix { m, offset }) = pipeline.stages().last() else {
            panic!("last stage must be the Lab decode matrix");
        };
        ([m[0][0], m[1][1], m[2][2]], *offset)
    };
    let (v4_diag, v4_offset) = seam_of(&v4);
    assert_eq!(v4_diag, [100.0, 255.0, 255.0]);
    assert_eq!(v4_offset, [0.0, -128.0, -128.0]);
    let (v2_diag, v2_offset) = seam_of(&v2);
    assert_eq!(v2_diag, [100.390625, 255.99609375, 255.99609375]);
    assert_eq!(v2_offset, [0.0, -128.0, -128.0]);
}

#[test]
fn lab_indexed_b2a_matches_lcms2_only_with_trilinear_interpolation() {
    set_quiet_log_handler();
    // A coarse grid (3 nodes per axis) separates tetrahedral and trilinear interpolation
    // measurably: our B2A pipeline (which forces multilinear for the Lab-indexed CLUT) must
    // track lcms2, while an otherwise-identical tetrahedral evaluation must not.
    // Media-relative intent: never BPC-forced (module docs), and the trilinear rule under
    // test is intent-independent (lcms2 applies it to every Lab-PCS output LUT).
    let (parsed, oracle) = reopen(&cmyk_prtr_v4(3));
    let ours = pcs_to_device(&parsed, RenderingIntent::MediaRelativeColorimetric).unwrap();
    let transform = Transform::new(
        &lab2(),
        TYPE_Lab_DBL,
        &oracle,
        TYPE_CMYK_DBL,
        INTENT_RELATIVE_COLORIMETRIC,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    // The synthesized B2A is identity curves → CLUT → identity curves, so our pipeline is
    // exactly CLUT(encode(Lab)); rebuild the same CLUT (the relative tag, B2A1) in both
    // interpolation modes.
    let Some(TagData::LutBToA(lut)) = parsed.get(KnownTag::BToA1) else {
        panic!("B2A1 must parse as lutBToAType");
    };
    let clut = lut.clut.as_ref().expect("B2A1 carries a CLUT");
    let trilinear = ClutTable::with_interpolation(clut, ClutInterpolation::Multilinear).unwrap();
    let tetrahedral = ClutTable::new(clut).unwrap();
    assert_eq!(tetrahedral.interpolation(), ClutInterpolation::Tetrahedral);
    let tri_pipeline = Pipeline::new(3, 4, vec![Stage::Clut(trilinear)]).unwrap();
    let tet_pipeline = Pipeline::new(3, 4, vec![Stage::Clut(tetrahedral)]).unwrap();

    let (mut worst_ours, mut worst_tetra) = (0.0_f64, 0.0_f64);
    for point in lab_sweep(43) {
        let got = eval(&ours, &point);
        let want: Vec<f64> = transform
            .apply_f64(&point, 1, 4)
            .into_iter()
            .map(|v| v / 100.0)
            .collect();
        for ch in 0..4 {
            worst_ours = worst_ours.max((got[ch] - want[ch]).abs());
        }
        // The same encoded coordinate our pipeline feeds the CLUT (v4 encode, identity
        // B-curves).
        let encoded = [
            point[0] / 100.0,
            (point[1] + 128.0) / 255.0,
            (point[2] + 128.0) / 255.0,
        ];
        let tri = eval(&tri_pipeline, &encoded);
        let tet = eval(&tet_pipeline, &encoded);
        for ch in 0..4 {
            // Our pipeline really evaluates the trilinear table (identity curves collapse).
            assert!(
                (got[ch] - tri[ch]).abs() < 1e-9,
                "pipeline != trilinear CLUT"
            );
            worst_tetra = worst_tetra.max((tet[ch] - want[ch]).abs());
        }
    }
    // Our (trilinear) route is 16-bit-tight vs lcms2 — measured 2.2e-5 — while the
    // tetrahedral evaluation of the SAME table misses lcms2 three orders of magnitude wider
    // — measured 2.7e-2 — proving lcms2 used trilinear here and that the mode is honoured.
    assert!(worst_ours < 5e-4, "trilinear vs lcms2: {worst_ours:e}");
    assert!(
        worst_tetra > 1e-2,
        "tetrahedral should diverge visibly: {worst_tetra:e}"
    );
    assert!(
        worst_tetra > 10.0 * worst_ours,
        "tetrahedral should diverge: {worst_tetra:e} vs {worst_ours:e}"
    );
}

/// Hand-builds a Lab-PCS RGB matrix/TRC shaper with s15Fixed16-exact colorants and an exact
/// `u8Fixed8` gamma, serializes it with gamut-icc, and returns both sides' views.
fn lab_pcs_rgb_shaper() -> (IccProfile, Profile) {
    use gamut_icc::{ColorSpace, Curve, DeviceClass, ProfileHeader, TagData, U8Fixed8, XyzNumber};
    let mut header = ProfileHeader::new(DeviceClass::Display, ColorSpace::Rgb);
    header.pcs = ColorSpace::Lab;
    let xyz_tag = |v: [f64; 3]| TagData::Xyz(vec![XyzNumber::from_f64(v)]);
    let gamma = || TagData::Curve(Curve::Gamma(U8Fixed8(0x0233)));
    let profile = IccProfile {
        header,
        tags: vec![
            (Signature(*b"rXYZ"), xyz_tag([0.436, 0.2225, 0.0139])),
            (Signature(*b"gXYZ"), xyz_tag([0.3851, 0.7169, 0.0971])),
            (Signature(*b"bXYZ"), xyz_tag([0.1431, 0.0606, 0.7141])),
            (Signature(*b"rTRC"), gamma()),
            (Signature(*b"gTRC"), gamma()),
            (Signature(*b"bTRC"), gamma()),
        ],
    };
    let bytes = profile.to_bytes().expect("serializes");
    let parsed = IccProfile::parse(&bytes).expect("round-trips");
    let oracle = Profile::from_bytes(&bytes).expect("lcms2 opens the hand-built shaper");
    (parsed, oracle)
}

#[test]
fn lab_pcs_rgb_shaper_matches_lcms2_in_both_directions() {
    set_quiet_log_handler();
    // The #327 refusal, lifted: an RGB shaper whose PCS is Lab builds via the appended
    // XyzToLab stage (device→PCS) / prepended LabToXyz (PCS→device), matching lcms2's
    // BuildRGBInputMatrixShaper/BuildRGBOutputMatrixShaper bridge stages end to end.
    let (parsed, oracle) = lab_pcs_rgb_shaper();
    let forward = device_to_pcs(&parsed, RenderingIntent::MediaRelativeColorimetric).unwrap();
    let to_lab = Transform::new(
        &oracle,
        TYPE_RGB_DBL,
        &lab4(),
        TYPE_Lab_DBL,
        INTENT_RELATIVE_COLORIMETRIC,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let mut worst = 0.0_f64;
    for point in rgb_sweep(47) {
        let got = eval(&forward, &point);
        let want = to_lab.apply_f64(&point, 1, 3);
        for ch in 0..3 {
            worst = worst.max((got[ch] - want[ch]).abs());
        }
    }
    // lcms2 evaluates in f32 and its XYZ2Lab stage uses the truncated cmsD50 constants
    // (this crate: the s15Fixed16 PCS illuminant, ≤ 5.5e-6 apart in X/Z — amplified by the
    // Lab derivative ~500 on a*/b*). Measured 4.9e-4.
    assert!(worst < 5e-3, "forward: worst Lab |Δ| = {worst:e}");

    let reverse = pcs_to_device(&parsed, RenderingIntent::MediaRelativeColorimetric).unwrap();
    let from_lab = Transform::new(
        &lab4(),
        TYPE_Lab_DBL,
        &oracle,
        TYPE_RGB_DBL,
        INTENT_RELATIVE_COLORIMETRIC,
        FLAGS_NOOPTIMIZE | FLAGS_NOCACHE,
    );
    let (mut worst_dev, mut worst_bright) = (0.0_f64, 0.0_f64);
    for point in rgb_sweep(53) {
        // In-gamut Lab inputs: the forward image of the device sweep.
        let pcs = eval(&forward, &point);
        let got = eval(&reverse, &pcs);
        let want = from_lab.apply_f64(&pcs, 1, 3);
        for ch in 0..3 {
            let delta = (got[ch] - want[ch]).abs();
            worst_dev = worst_dev.max(delta);
            if point.iter().all(|&v| v >= 0.15) {
                worst_bright = worst_bright.max(delta);
            }
        }
    }
    // Away from black both sides invert the pure-gamma TRC analytically over the same
    // Lab→XYZ bridge — measured 3.6e-6; near black the γ≈2.2 inverse's unbounded slope
    // amplifies the D50/f32 splits — measured 2.3e-3.
    assert!(
        worst_bright < 1e-4,
        "reverse away from black: worst |Δ| = {worst_bright:e}"
    );
    assert!(worst_dev < 2e-2, "reverse: worst |Δ| = {worst_dev:e}");
}
