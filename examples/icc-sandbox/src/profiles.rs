//! The profile battery, synthesized at runtime.
//!
//! Each profile is built as a [`gamut_icc::IccProfile`] model, serialized with
//! [`IccWriter`] (profile ID recomputed), and **parsed back** with [`IccProfile::parse`], so the
//! sandbox only ever runs profiles that survived a full write → read round trip — the same bytes a
//! file would carry. Nothing here is a committed binary fixture (`gamut-icc`'s own corpus is
//! synthesized the same way).
//!
//! | Profile | Class / version | Model | Exercises |
//! | --- | --- | --- | --- |
//! | [`ProfileKind::SrgbV2`] | `mntr` v2.1 | matrix/TRC, XYZ PCS | sampled `curveType` TRC, v2 `textDescriptionType` |
//! | [`ProfileKind::SrgbV4`] | `mntr` v4.4 | matrix/TRC, XYZ PCS | parametric TRC — the **display** profile every panel renders into |
//! | [`ProfileKind::DisplayP3`] | `mntr` v4.4 | matrix/TRC, XYZ PCS | parametric TRC, Bradford `chad` |
//! | [`ProfileKind::CameraA`] | `scnr` v4.4 | matrix/TRC, XYZ PCS | pure-gamma (`u8Fixed8`) curve |
//! | [`ProfileKind::CameraB`] | `scnr` v4.4 | `lutAToB`, **Lab** PCS | 3-D CLUT, Lab PCS |
//! | [`ProfileKind::Printer`] | `prtr` v4.4 | `lutAToB`/`lutBToA` ×3 + `gamt`, Lab PCS | 4-D CLUT, per-intent tables, non-D50 media white (absolute intent), BPC |
//! | [`ProfileKind::Truncated`] | — | the sRGB bytes cut short | the parser's error path |
//!
//! The camera and printer characterizations are **synthetic device models** documented on the
//! functions that build them, not measurements of real hardware: what the sandbox demonstrates is
//! the code path each profile drives, and the models are only as realistic as that needs.

use gamut_color::lab::{D50_XYZ, encode_lab_v4_16, lab_to_xyz, xyz_to_lab};
use gamut_color::linalg::mat_inv_3x3;
use gamut_color::matrix::{D50, rgb_to_xyz_matrix};
use gamut_color::transfer::srgb_eotf;
use gamut_icc::{
    BuiltinProfile, Clut, ClutPrecision, ColorSpace, Curve, CurveOrParametric, DeviceClass,
    IccProfile, IccWriter, KnownTag, LutAToB, LutBToA, Mluc, MlucRecord, ProfileHeader,
    ProfileVersion, Signature, TagData, TextDescription, U8Fixed8, XyzNumber,
};

/// Which battery entry a [`BatteryProfile`] is. Append-only: the GUI keys its selectors on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileKind {
    /// sRGB as a v2 matrix/TRC display profile with 1024-entry sampled TRCs.
    SrgbV2,
    /// Display P3 as `gamut-icc`'s built-in v4 matrix/TRC profile (parametric TRCs).
    DisplayP3,
    /// Camera A: a linear-raw input profile, matrix/TRC over an XYZ PCS.
    CameraA,
    /// Camera B: a second characterization of the same sensor, a `lutAToB` CLUT over a Lab PCS.
    CameraB,
    /// A v4 CMYK printer: per-intent `lutAToB`/`lutBToA` tables and a `gamt` tag, Lab PCS.
    Printer,
    /// The sRGB profile's bytes truncated mid-tag-data: parsing must fail, never panic.
    Truncated,
    /// sRGB as `gamut-icc`'s built-in v4 profile (parametric TRCs): the display profile.
    SrgbV4,
}

impl ProfileKind {
    /// Every kind, in display order.
    pub const ALL: [ProfileKind; 7] = [
        ProfileKind::SrgbV2,
        ProfileKind::SrgbV4,
        ProfileKind::DisplayP3,
        ProfileKind::CameraA,
        ProfileKind::CameraB,
        ProfileKind::Printer,
        ProfileKind::Truncated,
    ];

    /// A short human-readable name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ProfileKind::SrgbV2 => "sRGB (v2, sampled TRC)",
            ProfileKind::DisplayP3 => "Display P3 (v4, parametric TRC)",
            ProfileKind::CameraA => "Camera A (scnr, matrix/TRC, XYZ PCS)",
            ProfileKind::CameraB => "Camera B (scnr, lutAToB, Lab PCS)",
            ProfileKind::Printer => "CMYK printer (prtr, v4 LUTs, Lab PCS)",
            ProfileKind::Truncated => "Truncated sRGB (malformed)",
            ProfileKind::SrgbV4 => "sRGB (v4 built-in, display)",
        }
    }
}

/// One battery entry: the serialized bytes and what parsing them produced.
#[derive(Debug, Clone)]
pub struct BatteryProfile {
    /// Which entry this is.
    pub kind: ProfileKind,
    /// The serialized profile — exactly what a file would embed.
    pub bytes: Vec<u8>,
    /// [`IccProfile::parse`] over [`bytes`](Self::bytes): the profile, or the parser's message.
    pub parsed: Result<IccProfile, String>,
}

impl BatteryProfile {
    /// The parsed profile, if parsing succeeded.
    #[must_use]
    pub fn profile(&self) -> Option<&IccProfile> {
        self.parsed.as_ref().ok()
    }
}

/// The whole battery, in [`ProfileKind::ALL`] order.
#[derive(Debug, Clone)]
pub struct Battery {
    /// The entries, one per [`ProfileKind`].
    pub entries: Vec<BatteryProfile>,
}

/// An error building the battery: a synthesized model the writer refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildError {
    /// The entry whose model failed.
    pub kind: ProfileKind,
    /// What failed.
    pub message: String,
}

impl core::fmt::Display for BuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.kind.name(), self.message)
    }
}

impl std::error::Error for BuildError {}

impl Battery {
    /// Synthesizes, serializes and re-parses every profile.
    ///
    /// # Errors
    ///
    /// [`BuildError`] if a model fails to build or the writer refuses it — a defect in this
    /// module, never in the input. The truncated entry is *expected* to fail parsing; that is
    /// recorded in its [`BatteryProfile::parsed`], not raised here.
    pub fn build() -> Result<Battery, BuildError> {
        let entries = ProfileKind::ALL
            .iter()
            .map(|&kind| build_entry(kind))
            .collect::<Result<_, _>>()?;
        Ok(Battery { entries })
    }

    /// The entry for `kind`.
    #[must_use]
    pub fn get(&self, kind: ProfileKind) -> &BatteryProfile {
        // `build` emits one entry per `ProfileKind::ALL` element, in that order.
        let index = ProfileKind::ALL
            .iter()
            .position(|&k| k == kind)
            .unwrap_or_default();
        &self.entries[index]
    }

    /// The parsed profile for `kind`, if it parsed.
    #[must_use]
    pub fn profile(&self, kind: ProfileKind) -> Option<&IccProfile> {
        self.get(kind).profile()
    }
}

/// Builds one entry.
fn build_entry(kind: ProfileKind) -> Result<BatteryProfile, BuildError> {
    let fail = |message: String| BuildError { kind, message };
    let bytes = match kind {
        ProfileKind::Truncated => truncated(&serialize(&srgb_v2().map_err(fail)?).map_err(fail)?),
        _ => serialize(&model(kind).map_err(fail)?).map_err(fail)?,
    };
    let parsed = IccProfile::parse(&bytes).map_err(|e| e.to_string());
    Ok(BatteryProfile {
        kind,
        bytes,
        parsed,
    })
}

/// The in-memory model for a well-formed kind.
fn model(kind: ProfileKind) -> Result<IccProfile, String> {
    match kind {
        ProfileKind::SrgbV2 | ProfileKind::Truncated => srgb_v2(),
        ProfileKind::SrgbV4 => Ok(display_srgb()),
        ProfileKind::DisplayP3 => builtin(BuiltinProfile::DisplayP3),
        ProfileKind::CameraA => Ok(camera_a()),
        ProfileKind::CameraB => Ok(camera_b()),
        ProfileKind::Printer => Ok(printer()),
    }
}

/// Serializes with the profile ID stamped, as an embedding application would.
fn serialize(profile: &IccProfile) -> Result<Vec<u8>, String> {
    IccWriter::new()
        .recompute_profile_id(true)
        .write(profile)
        .map_err(|e| e.to_string())
}

/// Where [`truncated`] cuts: past the header and tag table, inside the tag data, so the parser
/// gets far enough to find a tag pointing beyond the end.
pub const TRUNCATED_LEN: usize = 300;

/// `bytes` cut to [`TRUNCATED_LEN`].
fn truncated(bytes: &[u8]) -> Vec<u8> {
    bytes[..TRUNCATED_LEN.min(bytes.len())].to_vec()
}

/// `gamut-icc`'s built-in profile for `space`.
fn builtin(space: BuiltinProfile) -> Result<IccProfile, String> {
    IccProfile::builtin(space).ok_or_else(|| format!("no built-in profile for {space:?}"))
}

/// The display profile every rendered pixel is converted into: `gamut-icc`'s built-in v4 sRGB.
///
/// # Panics
///
/// Never: [`IccProfile::builtin`] yields `Some` for every [`BuiltinProfile`] (gamut-icc pins
/// that with `every_builtin_space_is_buildable`).
#[must_use]
pub fn display_srgb() -> IccProfile {
    IccProfile::builtin(BuiltinProfile::Srgb).unwrap_or_else(|| unreachable!())
}

/// A one-record `en-US` `multiLocalizedUnicodeType` — the v4 `desc`/`cprt` form.
fn mluc(text: &str) -> TagData {
    TagData::MultiLocalizedUnicode(Mluc {
        records: vec![MlucRecord {
            language: *b"en",
            country: *b"US",
            text: text.to_owned(),
        }],
    })
}

/// The copyright every synthesized profile carries.
const COPYRIGHT: &str = "No copyright; synthesized by the gamut ICC sandbox";

/// Entries in the sampled TRC of [`ProfileKind::SrgbV2`] — the length real v2 sRGB profiles use.
pub const SRGB_V2_TRC_ENTRIES: usize = 1024;

/// sRGB as a **v2** profile: the built-in v4 profile's colorants re-dressed in v2 form — version
/// 2.1, a `textDescriptionType` description, a `textType` copyright, no `chad` (a v4 tag), and a
/// 1024-entry sampled TRC tabulating IEC 61966-2-1 in place of the parametric curve.
fn srgb_v2() -> Result<IccProfile, String> {
    let mut profile = builtin(BuiltinProfile::Srgb)?;
    profile.header.version = ProfileVersion {
        major: 2,
        minor: 1,
        bugfix: 0,
    };
    let last = (SRGB_V2_TRC_ENTRIES - 1) as f64;
    let table: Vec<u16> = (0..SRGB_V2_TRC_ENTRIES)
        .map(|i| to_u16(srgb_eotf(i as f64 / last)))
        .collect();
    let trc = TagData::Curve(Curve::Sampled(table));
    let description = TagData::TextDescription(TextDescription {
        ascii: "sRGB (v2, sampled TRC)".to_owned(),
        ..TextDescription::default()
    });
    profile.tags.retain(|(sig, _)| {
        *sig != Signature::from(KnownTag::ChromaticAdaptation)
            && *sig != Signature::from(KnownTag::Cicp)
    });
    for (sig, data) in &mut profile.tags {
        let tag = *sig;
        if [KnownTag::RedTrc, KnownTag::GreenTrc, KnownTag::BlueTrc]
            .iter()
            .any(|&t| Signature::from(t) == tag)
        {
            *data = trc.clone();
        } else if tag == Signature::from(KnownTag::ProfileDescription) {
            *data = description.clone();
        } else if tag == Signature::from(KnownTag::Copyright) {
            *data = TagData::Text(COPYRIGHT.to_owned());
        }
    }
    Ok(profile)
}

/// `v` in `[0, 1]` as a 16-bit code, rounded and saturated.
fn to_u16(v: f64) -> u16 {
    // Saturated to the u16 range first, so the cast is exact.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..=65535"
    )]
    let code = (v.clamp(0.0, 1.0) * 65535.0).round() as u16;
    code
}

/// The sensor primaries camera A is characterized with (CIE xy) — a wide, slightly
/// unbalanced set, as camera spectral sensitivities give.
pub const CAMERA_A_PRIMARIES: [[f64; 2]; 3] = [[0.7, 0.29], [0.18, 0.78], [0.13, 0.04]];

/// Camera B's characterization of the *same* sensor: the red and green primaries shifted, as a
/// generic profile built from a different target would be.
pub const CAMERA_B_PRIMARIES: [[f64; 2]; 3] = [[0.68, 0.31], [0.21, 0.74], [0.14, 0.05]];

/// The camera-RGB → PCS XYZ (D50) matrix for `primaries` under a D50 white — a raw sensor has no
/// display white to adapt from, so the characterization is built at the PCS white directly.
///
/// # Panics
///
/// Never for the two primary sets above, which are non-degenerate (`camera_matrices_invert`
/// pins it).
#[must_use]
pub fn camera_matrix(primaries: &[[f64; 2]; 3]) -> [[f64; 3]; 3] {
    rgb_to_xyz_matrix(primaries, D50).unwrap_or_else(|| unreachable!())
}

/// A v4 input-class header.
fn input_header(pcs: ColorSpace) -> ProfileHeader {
    let mut header = ProfileHeader::new(DeviceClass::Input, ColorSpace::Rgb);
    header.pcs = pcs;
    header
}

/// Camera A: matrix/TRC over an XYZ PCS, `gamma 1.0` (`u8Fixed8`) TRCs — linear raw.
fn camera_a() -> IccProfile {
    let m = camera_matrix(&CAMERA_A_PRIMARIES);
    let column = |j: usize| TagData::Xyz(vec![XyzNumber::from_f64([m[0][j], m[1][j], m[2][j]])]);
    let linear = TagData::Curve(Curve::Gamma(U8Fixed8::from_f64(1.0)));
    IccProfile {
        header: input_header(ColorSpace::Xyz),
        tags: vec![
            (KnownTag::ProfileDescription.into(), mluc("Camera A")),
            (KnownTag::Copyright.into(), mluc(COPYRIGHT)),
            (
                KnownTag::MediaWhitePoint.into(),
                TagData::Xyz(vec![XyzNumber::D50]),
            ),
            (KnownTag::RedColorant.into(), column(0)),
            (KnownTag::GreenColorant.into(), column(1)),
            (KnownTag::BlueColorant.into(), column(2)),
            (KnownTag::RedTrc.into(), linear.clone()),
            (KnownTag::GreenTrc.into(), linear.clone()),
            (KnownTag::BlueTrc.into(), linear),
        ],
    }
}

/// Grid points per axis of camera B's CLUT.
pub const CAMERA_B_GRID: u8 = 17;

/// Camera B: an `A2B0` `lutAToB` (identity A and B curves around a 17³ CLUT) over a **Lab** PCS.
/// Each node holds the v4 16-bit Lab encoding of `camera_matrix(CAMERA_B_PRIMARIES) · rgb`.
fn camera_b() -> IccProfile {
    let m = camera_matrix(&CAMERA_B_PRIMARIES);
    let clut = grid_clut(3, CAMERA_B_GRID, 3, |rgb, out| {
        let xyz = mat_vec(&m, [rgb[0], rgb[1], rgb[2]]);
        out.copy_from_slice(&encode_lab_v4_16(xyz_to_lab(xyz, D50_XYZ)));
    });
    let a2b0 = TagData::LutAToB(LutAToB {
        input_channels: 3,
        output_channels: 3,
        a_curves: Some(identity_curves(3)),
        clut: Some(clut),
        m_curves: None,
        matrix: None,
        b_curves: identity_curves(3),
    });
    IccProfile {
        header: input_header(ColorSpace::Lab),
        tags: vec![
            (KnownTag::ProfileDescription.into(), mluc("Camera B")),
            (KnownTag::Copyright.into(), mluc(COPYRIGHT)),
            (
                KnownTag::MediaWhitePoint.into(),
                TagData::Xyz(vec![XyzNumber::D50]),
            ),
            (KnownTag::AToB0.into(), a2b0),
        ],
    }
}

/// `n` identity curves.
fn identity_curves(n: usize) -> Vec<CurveOrParametric> {
    vec![CurveOrParametric::Curve(Curve::Identity); n]
}

/// `m · v`.
fn mat_vec(m: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|r| m[r][0] * v[0] + m[r][1] * v[1] + m[r][2] * v[2])
}

/// A 16-bit CLUT of `inputs` axes × `grid` points, filled by `node(coordinates, outputs)` with
/// each node's coordinates in `[0, 1]`, in ICC grid order (last input axis fastest).
fn grid_clut(
    inputs: usize,
    grid: u8,
    outputs: usize,
    mut node: impl FnMut(&[f64], &mut [u16]),
) -> Clut {
    let g = usize::from(grid);
    let nodes = g.pow(u32::try_from(inputs).unwrap_or(u32::MAX));
    let mut samples = vec![0u16; nodes * outputs];
    let mut coords = vec![0.0; inputs];
    let last = f64::from(grid - 1);
    for (index, out) in samples.chunks_exact_mut(outputs).enumerate() {
        let mut rest = index;
        for axis in (0..inputs).rev() {
            coords[axis] = (rest % g) as f64 / last;
            rest /= g;
        }
        node(&coords, out);
    }
    Clut {
        grid_points: vec![grid; inputs],
        output_channels: u8::try_from(outputs).unwrap_or(u8::MAX),
        precision: ClutPrecision::U16,
        samples,
    }
}

/// The printer's paper white, ICC-absolute (D50-relative XYZ): a slightly warm, slightly dim
/// stock. It is the `wtpt` tag, so the absolute intent visibly differs from media-relative.
pub const PAPER_WHITE: [f64; 3] = [0.9300, 0.9600, 0.7600];

/// Grid points per axis of the printer's 4-D `A2Bx` CLUTs.
pub const PRINTER_A2B_GRID: u8 = 9;

/// Grid points per axis of the printer's 3-D `B2Ax` and `gamt` CLUTs.
pub const PRINTER_B2A_GRID: u8 = 17;

/// The synthetic printer's ink model: CMYK coverage → **media-relative** PCS Lab (paper → `L* =
/// 100`).
///
/// Each ink absorbs mostly its own third of an RGB basis with some impurity (the matrix below),
/// coverage gets a parabolic dot gain, and the absorptions multiply (Beer–Lambert-like
/// subtractive mixing). The resulting linear "reflectance RGB" goes to XYZ through the
/// D50-adapted sRGB matrix, so unprinted paper is exactly the PCS white.
#[must_use]
pub fn printer_forward(cmyk: [f64; 4]) -> [f64; 3] {
    // Rows: C, M, Y, K. Columns: absorption of the R, G, B reflectance basis.
    const ABSORB: [[f64; 3]; 4] = [
        [0.95, 0.15, 0.05],
        [0.10, 0.92, 0.25],
        [0.02, 0.08, 0.90],
        [0.93, 0.93, 0.93],
    ];
    let mut rgb = [1.0; 3];
    for (ink, row) in cmyk.iter().zip(ABSORB) {
        let x = ink.clamp(0.0, 1.0);
        let coverage = x + 0.15 * x * (1.0 - x);
        for (channel, absorb) in rgb.iter_mut().zip(row) {
            *channel *= 1.0 - absorb * coverage;
        }
    }
    xyz_to_lab(mat_vec(&crate::panels::srgb_d50_matrix(), rgb), D50_XYZ)
}

/// The black generation the separation uses: K from the darkest a CMY-only mix would need.
fn black_generation(lab: [f64; 3]) -> f64 {
    let lightness = lab[0] / 100.0;
    ((0.55 - lightness) / 0.55).clamp(0.0, 1.0).powf(1.4)
}

/// The printer's separation: media-relative Lab → CMYK, by fixing K from
/// [`black_generation`] and solving CMY against [`printer_forward`] with a damped Gauss–Newton
/// iteration clamped to the unit cube. Out-of-gamut targets land on the nearest reachable
/// colour the iteration finds — the clip.
#[must_use]
pub fn printer_separate(lab: [f64; 3]) -> [f64; 4] {
    let k = black_generation(lab);
    // Start from a naive complement of the target's reflectance.
    let xyz = lab_to_xyz(lab, D50_XYZ);
    let to_rgb = mat_inv_3x3(&crate::panels::srgb_d50_matrix()).unwrap_or_default();
    let rgb = mat_vec(&to_rgb, xyz);
    let mut cmy = rgb.map(|v| (1.0 - v).clamp(0.0, 1.0));
    let residual = |cmy: [f64; 3]| {
        let got = printer_forward([cmy[0], cmy[1], cmy[2], k]);
        [got[0] - lab[0], got[1] - lab[1], got[2] - lab[2]]
    };
    const H: f64 = 1e-4;
    for _ in 0..40 {
        let r = residual(cmy);
        let mut jacobian = [[0.0; 3]; 3];
        for axis in 0..3 {
            let mut probe = cmy;
            probe[axis] += H;
            let rp = residual(probe);
            for row in 0..3 {
                jacobian[row][axis] = (rp[row] - r[row]) / H;
            }
        }
        let Some(inverse) = mat_inv_3x3(&jacobian) else {
            break;
        };
        let step = mat_vec(&inverse, r);
        for axis in 0..3 {
            cmy[axis] = (cmy[axis] - 0.8 * step[axis]).clamp(0.0, 1.0);
        }
    }
    [cmy[0], cmy[1], cmy[2], k]
}

/// How each `B2Ax` table maps the PCS colour before separating it — what makes the three
/// intents differ.
#[derive(Debug, Clone, Copy)]
enum IntentMap {
    /// `B2A0`: compress lightness onto the printer's black and chroma by 0.85.
    Perceptual,
    /// `B2A1`: none — a straight clip.
    Colorimetric,
    /// `B2A2`: boost chroma by 1.2.
    Saturation,
}

/// The `L*` of the printer's solid four-colour black.
fn printer_black_lightness() -> f64 {
    printer_forward([1.0; 4])[0]
}

impl IntentMap {
    /// Maps a PCS Lab colour for this table.
    fn apply(self, lab: [f64; 3]) -> [f64; 3] {
        match self {
            IntentMap::Perceptual => {
                let black = printer_black_lightness();
                [
                    black + lab[0] * (100.0 - black) / 100.0,
                    lab[1] * 0.85,
                    lab[2] * 0.85,
                ]
            }
            IntentMap::Colorimetric => lab,
            IntentMap::Saturation => [lab[0], lab[1] * 1.2, lab[2] * 1.2],
        }
    }
}

/// The decoded Lab of a v4 16-bit-encoded node coordinate in `[0, 1]³`.
fn node_lab(coords: &[f64]) -> [f64; 3] {
    [
        coords[0] * 100.0,
        coords[1] * 255.0 - 128.0,
        coords[2] * 255.0 - 128.0,
    ]
}

/// The ΔE₇₆ beyond which [`printer`]'s `gamt` tag calls a colour out of gamut: a separation that
/// reproduces the target worse than this is a clip.
pub const GAMT_THRESHOLD: f64 = 2.0;

/// The v4 CMYK printer: `A2B0..2` from [`printer_forward`] (one table serves all three —
/// device colorimetry does not depend on intent), `B2A0..2` from [`printer_separate`] after the
/// per-intent [`IntentMap`], `gamt` from the separation's round-trip error, and a non-D50
/// [`PAPER_WHITE`] media white point.
fn printer() -> IccProfile {
    let a2b_clut = grid_clut(4, PRINTER_A2B_GRID, 3, |c, out| {
        let lab = printer_forward([c[0], c[1], c[2], c[3]]);
        out.copy_from_slice(&encode_lab_v4_16(lab));
    });
    let a2b = TagData::LutAToB(LutAToB {
        input_channels: 4,
        output_channels: 3,
        a_curves: Some(identity_curves(4)),
        clut: Some(a2b_clut),
        m_curves: None,
        matrix: None,
        b_curves: identity_curves(3),
    });
    let b2a = |map: IntentMap| {
        let clut = grid_clut(3, PRINTER_B2A_GRID, 4, |c, out| {
            let cmyk = printer_separate(map.apply(node_lab(c)));
            for (o, v) in out.iter_mut().zip(cmyk) {
                *o = to_u16(v);
            }
        });
        TagData::LutBToA(LutBToA {
            input_channels: 3,
            output_channels: 4,
            b_curves: identity_curves(3),
            matrix: None,
            m_curves: None,
            clut: Some(clut),
            a_curves: Some(identity_curves(4)),
        })
    };
    let gamt_clut = grid_clut(3, PRINTER_B2A_GRID, 1, |c, out| {
        let lab = node_lab(c);
        let back = printer_forward(printer_separate(lab));
        let error = gamut_color::lab::delta_e_76(lab, back);
        out[0] = if error > GAMT_THRESHOLD { u16::MAX } else { 0 };
    });
    let gamt = TagData::LutBToA(LutBToA {
        input_channels: 3,
        output_channels: 1,
        b_curves: identity_curves(3),
        matrix: None,
        m_curves: None,
        clut: Some(gamt_clut),
        a_curves: Some(identity_curves(1)),
    });
    let mut header = ProfileHeader::new(DeviceClass::Output, ColorSpace::Cmyk);
    header.pcs = ColorSpace::Lab;
    IccProfile {
        header,
        tags: vec![
            (
                KnownTag::ProfileDescription.into(),
                mluc("Synthetic CMYK printer"),
            ),
            (KnownTag::Copyright.into(), mluc(COPYRIGHT)),
            (
                KnownTag::MediaWhitePoint.into(),
                TagData::Xyz(vec![XyzNumber::from_f64(PAPER_WHITE)]),
            ),
            (KnownTag::AToB0.into(), a2b.clone()),
            (KnownTag::AToB1.into(), a2b.clone()),
            (KnownTag::AToB2.into(), a2b),
            (KnownTag::BToA0.into(), b2a(IntentMap::Perceptual)),
            (KnownTag::BToA1.into(), b2a(IntentMap::Colorimetric)),
            (KnownTag::BToA2.into(), b2a(IntentMap::Saturation)),
            (KnownTag::Gamut.into(), gamt),
        ],
    }
}

/// The four-character text of a signature, non-printable bytes as `.`.
#[must_use]
pub fn signature_text(sig: Signature) -> String {
    sig.0
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                char::from(b)
            } else {
                '.'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery() -> Battery {
        Battery::build().expect("every synthesized model serializes")
    }

    #[test]
    fn every_well_formed_profile_parses_and_validates() {
        let battery = battery();
        for kind in ProfileKind::ALL {
            if kind == ProfileKind::Truncated {
                continue;
            }
            let profile = battery
                .profile(kind)
                .unwrap_or_else(|| panic!("{kind:?} parses"));
            assert!(
                profile.validate().is_empty(),
                "{kind:?}: {:?}",
                profile.validate()
            );
        }
    }

    #[test]
    fn truncated_profile_fails_to_parse_with_a_message() {
        let battery = battery();
        let entry = battery.get(ProfileKind::Truncated);
        assert_eq!(entry.bytes.len(), TRUNCATED_LEN);
        let message = entry
            .parsed
            .as_ref()
            .expect_err("truncated bytes must not parse");
        assert!(message.starts_with("icc:"), "{message}");
    }

    #[test]
    fn srgb_v2_is_version_2_with_a_sampled_trc() {
        let battery = battery();
        let profile = battery.profile(ProfileKind::SrgbV2).expect("parses");
        assert_eq!(profile.header.version.major, 2);
        assert!(matches!(
            profile.get(KnownTag::RedTrc),
            Some(TagData::Curve(Curve::Sampled(t))) if t.len() == SRGB_V2_TRC_ENTRIES
        ));
        assert!(matches!(
            profile.get(KnownTag::ProfileDescription),
            Some(TagData::TextDescription(_))
        ));
        assert!(profile.get(KnownTag::ChromaticAdaptation).is_none());
    }

    #[test]
    fn camera_matrices_invert() {
        for primaries in [CAMERA_A_PRIMARIES, CAMERA_B_PRIMARIES] {
            let m = camera_matrix(&primaries);
            assert!(mat_inv_3x3(&m).is_some());
            // White (1, 1, 1) is the PCS white.
            let white = mat_vec(&m, [1.0; 3]);
            for (got, want) in white.iter().zip(D50_XYZ) {
                assert!((got - want).abs() < 1e-3, "{white:?}");
            }
        }
    }

    #[test]
    fn printer_paper_is_pcs_white_and_black_is_dark() {
        let paper = printer_forward([0.0; 4]);
        // The sRGB primaries' published 4-digit chromaticities sum to D50 only to ~1e-4.
        for (got, want) in paper.iter().zip([100.0, 0.0, 0.0]) {
            assert!((got - want).abs() < 0.05, "paper Lab {paper:?}");
        }
        assert!(printer_black_lightness() < 10.0);
    }

    #[test]
    fn printer_separation_round_trips_in_gamut_colours() {
        for cmyk in [
            [0.2, 0.4, 0.1, 0.0],
            [0.6, 0.1, 0.5, 0.0],
            [0.1, 0.1, 0.1, 0.0],
        ] {
            let lab = printer_forward(cmyk);
            let back = printer_forward(printer_separate(lab));
            let error = gamut_color::lab::delta_e_76(lab, back);
            assert!(error < 0.5, "{cmyk:?}: ΔE76 {error}");
        }
    }

    #[test]
    fn grid_clut_orders_last_axis_fastest() {
        let clut = grid_clut(2, 3, 2, |c, out| {
            out[0] = to_u16(c[0]);
            out[1] = to_u16(c[1]);
        });
        // Node 1 is (0, 0.5): the last axis moved first.
        assert_eq!(&clut.samples[2..4], &[0, 32768]);
        assert_eq!(clut.samples.len(), 9 * 2);
    }
}
