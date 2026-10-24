//! The computation behind each sandbox panel — one library primitive per function, so each
//! doubles as an example of it:
//!
//! | Panel | Function | Primitive |
//! | --- | --- | --- |
//! | TRC curve evaluator | [`trc_curves`] | [`ToneCurve::new`], [`ToneCurve::eval`], [`ToneCurve::inverse`], [`ToneCurve::is_monotonic`] |
//! | Tag / matrix inspector | [`inspect`] | [`IccProfile::get`], [`IccProfile::validate`], the header fields |
//! | Malformed-profile diagnostics | [`diagnose`] | [`IccProfile::parse`]'s [`gamut_icc::IccError`] |
//! | CLUT interpolation viewer | [`clut_slice`] | [`ClutTable::with_interpolation`], [`Stage::Clut`], [`Pipeline::eval`] |
//! | Full pipeline | [`to_display`], [`proof_to_display`], [`separate`], [`gamut_mask`] | [`IccTransform::between`], [`IccTransform::proofing`], [`GamutCheck`], [`transform_interleaved_u8`]/[`transform_interleaved_u16`] |
//! | Camera comparison | [`camera_patch_errors`] | [`device_to_pcs`] |
//! | Gamut hull | [`gamut_hull`] | [`device_to_pcs`] / [`pcs_to_device`] bulk PCS round trips |
//!
//! Every pixel a panel shows reaches the screen through [`Display`]: `gamut-cmm`'s own PCS →
//! sRGB link over `gamut-icc`'s built-in sRGB profile.

use gamut_cmm::{
    ClutInterpolation, ClutTable, CmmError, GamutCheck, IccTransform, Pipeline,
    PipelineOptimization, ProofingOptions, Stage, ToneCurve, Transform, TransformOptions,
    device_to_pcs, pcs_to_device, transform_interleaved_u8, transform_interleaved_u16,
};
use gamut_color::lab::{D50_XYZ, delta_e_2000, lab_to_xyz, xyz_to_lab, xyz_to_xyy};
use gamut_color::linalg::mat_mul3;
use gamut_color::matrix::{D50, D65, bradford_adapt, rgb_to_xyz_matrix};
use gamut_core::PixelFormat;
use gamut_icc::{
    Clut, ColorSpace, Curve, CurveOrParametric, IccProfile, KnownTag, RenderingIntent, TagData,
};

use crate::profiles::{display_srgb, signature_text};
use crate::scene;

/// The D50-adapted linear-sRGB → PCS XYZ matrix (Bradford from D65, as the built-in sRGB
/// profile's `chad` records).
#[must_use]
pub fn srgb_d50_matrix() -> [[f64; 3]; 3] {
    const SRGB: [[f64; 2]; 3] = [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06]];
    // Computed once: the printer model evaluates it millions of times while building its tables.
    static MATRIX: std::sync::OnceLock<[[f64; 3]; 3]> = std::sync::OnceLock::new();
    *MATRIX.get_or_init(|| {
        match (rgb_to_xyz_matrix(&SRGB, D65), bradford_adapt(D65, D50)) {
            (Some(to_xyz), Some(adapt)) => mat_mul3(&adapt, &to_xyz),
            // Both are non-degenerate constants; `srgb_matrix_maps_white_to_d50` pins it.
            _ => [[0.0; 3]; 3],
        }
    })
}

/// Linear sRGB → PCS XYZ (D50).
#[must_use]
pub fn linear_srgb_to_xyz_d50(rgb: [f64; 3]) -> [f64; 3] {
    let m = srgb_d50_matrix();
    [0, 1, 2].map(|r| m[r][0] * rgb[0] + m[r][1] * rgb[1] + m[r][2] * rgb[2])
}

/// The rendering knobs every pipeline panel shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// The rendering intent.
    pub intent: RenderingIntent,
    /// Black-point compensation.
    pub bpc: bool,
    /// How far to collapse the linked pipeline.
    pub optimization: PipelineOptimization,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            intent: RenderingIntent::Perceptual,
            bpc: false,
            optimization: PipelineOptimization::None,
        }
    }
}

impl Settings {
    /// The options [`IccTransform::between`] takes.
    #[must_use]
    pub fn transform_options(self) -> TransformOptions {
        TransformOptions {
            intent: self.intent,
            black_point_compensation: self.bpc,
            optimization: self.optimization,
            ..TransformOptions::default()
        }
    }
}

/// The four ICC.1 rendering intents, in header-code order, with display names.
pub const INTENTS: [(RenderingIntent, &str); 4] = [
    (RenderingIntent::Perceptual, "Perceptual"),
    (
        RenderingIntent::MediaRelativeColorimetric,
        "Media-relative colorimetric",
    ),
    (RenderingIntent::Saturation, "Saturation"),
    (
        RenderingIntent::IccAbsoluteColorimetric,
        "ICC-absolute colorimetric",
    ),
];

/// The display path: decoded PCS colorimetry → sRGB 8-bit, through `gamut-cmm`'s PCS → device
/// link over the built-in sRGB profile (media-relative).
#[derive(Debug, Clone)]
pub struct Display {
    /// The built-in sRGB profile every panel renders into.
    pub profile: IccProfile,
    /// PCS XYZ → sRGB device values.
    from_xyz: Pipeline,
}

impl Display {
    /// Builds the display link.
    ///
    /// # Errors
    ///
    /// Whatever [`pcs_to_device`] reports — never, for the built-in sRGB profile.
    pub fn new() -> Result<Display, CmmError> {
        let profile = display_srgb();
        let from_xyz = pcs_to_device(&profile, RenderingIntent::MediaRelativeColorimetric)?;
        Ok(Display { profile, from_xyz })
    }

    /// One decoded PCS XYZ colour as sRGB 8-bit.
    #[must_use]
    pub fn xyz_to_rgb8(&self, xyz: [f64; 3]) -> [u8; 3] {
        let mut rgb = [0.0; 3];
        // A 3 → 3 pipeline fed 3 → 3 buffers cannot fail its length check.
        let _ = self.from_xyz.eval(&xyz, &mut rgb);
        rgb.map(to_u8)
    }

    /// One decoded PCS Lab colour as sRGB 8-bit.
    #[must_use]
    pub fn lab_to_rgb8(&self, lab: [f64; 3]) -> [u8; 3] {
        self.xyz_to_rgb8(lab_to_xyz(lab, D50_XYZ))
    }
}

/// A device value in `[0, 1]` as an 8-bit code, rounded half up and saturated (lcms2's
/// re-encoding, as `gamut-cmm`'s image entry points use).
#[must_use]
pub fn to_u8(v: f64) -> u8 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..=255"
    )]
    let code = (v.clamp(0.0, 1.0) * 255.0 + 0.5).floor().min(255.0) as u8;
    code
}

// --- TRC curve evaluator -------------------------------------------------------------------

/// Points sampled per plotted curve.
pub const CURVE_SAMPLES: usize = 256;

/// One tone-curve tag, evaluated for the plot.
#[derive(Debug, Clone)]
pub struct TrcCurve {
    /// The tag it was read from.
    pub tag: String,
    /// How the curve is encoded (`curveType` identity/gamma/sampled, or `parametricCurveType`).
    pub encoding: String,
    /// `(x, f(x))` over `[0, 1]`.
    pub forward: Vec<[f64; 2]>,
    /// `(y, f⁻¹(y))`, when the curve inverts.
    pub inverse: Option<Vec<[f64; 2]>>,
    /// [`ToneCurve::is_monotonic`].
    pub monotonic: bool,
    /// `max |f⁻¹(f(x)) − x|` over the samples, when the curve inverts.
    pub round_trip_error: Option<f64>,
}

/// The encoding label of a tone curve.
fn encoding(curve: &CurveOrParametric) -> String {
    match curve {
        CurveOrParametric::Curve(Curve::Identity) => "curveType, identity".to_owned(),
        CurveOrParametric::Curve(Curve::Gamma(g)) => format!("curveType, gamma {:.4}", g.to_f64()),
        CurveOrParametric::Curve(Curve::Sampled(t)) => format!("curveType, {} samples", t.len()),
        CurveOrParametric::Parametric(p) => {
            format!("parametricCurveType, function type {}", p.function_type)
        }
    }
}

/// Every `curveType`/`parametricCurveType` tag in `profile`, evaluated through
/// [`ToneCurve`] forward and inverse.
///
/// # Errors
///
/// [`CmmError`] if a curve cannot be built (an unsupported parametric type).
pub fn trc_curves(profile: &IccProfile) -> Result<Vec<TrcCurve>, CmmError> {
    let mut out = Vec::new();
    for (sig, data) in &profile.tags {
        let curve = match data {
            TagData::Curve(c) => CurveOrParametric::Curve(c.clone()),
            TagData::ParametricCurve(p) => CurveOrParametric::Parametric(p.clone()),
            _ => continue,
        };
        let tone = ToneCurve::new(&curve)?;
        let last = (CURVE_SAMPLES - 1) as f64;
        let xs: Vec<f64> = (0..CURVE_SAMPLES).map(|i| i as f64 / last).collect();
        let forward = xs.iter().map(|&x| [x, tone.eval(x)]).collect();
        let inverse_curve = tone.inverse().ok();
        let inverse = inverse_curve
            .as_ref()
            .map(|inv| xs.iter().map(|&y| [y, inv.eval(y)]).collect());
        let round_trip_error = inverse_curve.as_ref().map(|inv| {
            xs.iter()
                .map(|&x| (inv.eval(tone.eval(x)) - x).abs())
                .fold(0.0, f64::max)
        });
        out.push(TrcCurve {
            tag: signature_text(*sig),
            encoding: encoding(&curve),
            forward,
            inverse,
            monotonic: tone.is_monotonic(),
            round_trip_error,
        });
    }
    Ok(out)
}

// --- Tag / matrix inspector ----------------------------------------------------------------

/// One row of the tag table.
#[derive(Debug, Clone)]
pub struct TagRow {
    /// The tag signature.
    pub signature: String,
    /// The element type decoded.
    pub element: &'static str,
    /// A one-line summary of the contents.
    pub summary: String,
}

/// What the inspector shows for one profile.
#[derive(Debug, Clone)]
pub struct Inspection {
    /// `(field, value)` lines from the 128-byte header.
    pub header: Vec<(&'static str, String)>,
    /// Every tag, in file order.
    pub tags: Vec<TagRow>,
    /// The `rXYZ`/`gXYZ`/`bXYZ` colorants as matrix columns, when all three are present.
    pub colorants: Option<[[f64; 3]; 3]>,
    /// The `wtpt` media white point.
    pub white: Option<[f64; 3]>,
    /// The `chad` chromatic-adaptation matrix, row-major.
    pub chad: Option<[[f64; 3]; 3]>,
    /// [`IccProfile::validate`]'s findings, as text (empty for a conformant profile).
    pub conformance: Vec<String>,
}

/// The single XYZ value of a one-entry `XYZType` tag.
fn xyz_tag(profile: &IccProfile, tag: KnownTag) -> Option<[f64; 3]> {
    match profile.get(tag) {
        Some(TagData::Xyz(v)) => v.first().map(|x| x.to_f64()),
        _ => None,
    }
}

/// The element-type name and summary of one tag.
fn describe(data: &TagData) -> (&'static str, String) {
    match data {
        TagData::Xyz(v) => (
            "XYZType",
            v.first()
                .map(|x| {
                    let [a, b, c] = x.to_f64();
                    format!("{a:.4} {b:.4} {c:.4}")
                })
                .unwrap_or_default(),
        ),
        TagData::Curve(c) => ("curveType", encoding(&CurveOrParametric::Curve(c.clone()))),
        TagData::ParametricCurve(p) => (
            "parametricCurveType",
            encoding(&CurveOrParametric::Parametric(p.clone())),
        ),
        TagData::Text(t) => ("textType", t.clone()),
        TagData::TextDescription(t) => ("textDescriptionType", t.ascii.clone()),
        TagData::MultiLocalizedUnicode(m) => (
            "multiLocalizedUnicodeType",
            m.first().unwrap_or_default().to_owned(),
        ),
        TagData::S15Fixed16Array(v) => ("s15Fixed16ArrayType", format!("{} values", v.len())),
        TagData::LutAToB(l) => (
            "lutAToBType",
            format!(
                "{}→{}, CLUT grid {}",
                l.input_channels,
                l.output_channels,
                grid_text(l.clut.as_ref())
            ),
        ),
        TagData::LutBToA(l) => (
            "lutBToAType",
            format!(
                "{}→{}, CLUT grid {}",
                l.input_channels,
                l.output_channels,
                grid_text(l.clut.as_ref())
            ),
        ),
        TagData::Lut16(l) => (
            "lut16Type",
            format!(
                "{}→{}, grid {}",
                l.input_channels, l.output_channels, l.grid_points
            ),
        ),
        TagData::Lut8(l) => (
            "lut8Type",
            format!(
                "{}→{}, grid {}",
                l.input_channels, l.output_channels, l.grid_points
            ),
        ),
        TagData::Cicp(c) => ("cicpType", format!("{c:?}")),
        _ => ("other", String::new()),
    }
}

/// A CLUT's grid as `a×b×c`.
fn grid_text(clut: Option<&Clut>) -> String {
    clut.map_or_else(
        || "none".to_owned(),
        |c| {
            c.grid_points
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join("×")
        },
    )
}

/// The inspector's view of `profile`.
#[must_use]
pub fn inspect(profile: &IccProfile) -> Inspection {
    let h = &profile.header;
    let header = vec![
        (
            "version",
            format!(
                "{}.{}.{}",
                h.version.major, h.version.minor, h.version.bugfix
            ),
        ),
        ("device class", format!("{:?}", h.device_class)),
        ("data colour space", format!("{:?}", h.data_color_space)),
        ("PCS", format!("{:?}", h.pcs)),
        ("rendering intent", format!("{:?}", h.rendering_intent)),
        ("profile ID", hex(&h.profile_id.0)),
    ];
    let tags = profile
        .tags
        .iter()
        .map(|(sig, data)| {
            let (element, summary) = describe(data);
            TagRow {
                signature: signature_text(*sig),
                element,
                summary,
            }
        })
        .collect();
    let colorants = match (
        xyz_tag(profile, KnownTag::RedColorant),
        xyz_tag(profile, KnownTag::GreenColorant),
        xyz_tag(profile, KnownTag::BlueColorant),
    ) {
        (Some(r), Some(g), Some(b)) => Some([0, 1, 2].map(|row| [r[row], g[row], b[row]])),
        _ => None,
    };
    let chad = match profile.get(KnownTag::ChromaticAdaptation) {
        Some(TagData::S15Fixed16Array(v)) if v.len() == 9 => {
            Some([0, 1, 2].map(|r| [0, 1, 2].map(|c| v[r * 3 + c].to_f64())))
        }
        _ => None,
    };
    Inspection {
        header,
        tags,
        colorants,
        white: xyz_tag(profile, KnownTag::MediaWhitePoint),
        chad,
        conformance: profile
            .validate()
            .into_iter()
            .map(|c| c.requirement)
            .collect(),
    }
}

/// Lower-case hex.
fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// --- Malformed-profile diagnostics ---------------------------------------------------------

/// What the sandbox can say about bytes that failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    /// The parser's message ([`gamut_icc::IccError`]'s text — it names the structure at fault).
    pub message: String,
    /// The profile size the header declares (bytes 0–3), when at least four bytes are present.
    pub declared_size: Option<u32>,
    /// How many bytes were actually supplied.
    pub actual_size: usize,
    /// What to do about it.
    pub advice: String,
}

/// Parses `bytes`; on failure, explains why and what to do. `None` when the bytes parse.
///
/// `gamut-icc` reports a parse failure as one static message naming the structure at fault (no
/// byte offset or tag identity — #1004 tracks structured context in `IccError`), so the sandbox
/// adds the one fact it can establish itself: whether the header's declared size matches the
/// bytes supplied, which is what tells a truncated embed from a corrupt one.
#[must_use]
pub fn diagnose(bytes: &[u8]) -> Option<Diagnosis> {
    let error = IccProfile::parse(bytes).err()?;
    let declared_size = bytes
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_be_bytes);
    let advice = match declared_size {
        Some(size) if (size as usize) > bytes.len() => format!(
            "The header declares {size} bytes but only {} are present: the profile was \
             truncated in transit. Re-extract it from the container (a JPEG profile spans \
             several APP2 segments that must all be joined). The image is shown untagged, as sRGB.",
            bytes.len()
        ),
        Some(_) => "The size matches, so the data itself is corrupt. Replace the profile; the \
                    image is shown untagged, as sRGB."
            .to_owned(),
        None => "Fewer than four bytes: this is not an ICC profile.".to_owned(),
    };
    Some(Diagnosis {
        message: error.to_string(),
        declared_size,
        actual_size: bytes.len(),
        advice,
    })
}

// --- CLUT interpolation viewer -------------------------------------------------------------

/// Every CLUT a profile's LUT tags carry, with the tag it came from.
#[must_use]
pub fn clut_tags(profile: &IccProfile) -> Vec<(String, &Clut)> {
    profile
        .tags
        .iter()
        .filter_map(|(sig, data)| {
            let clut = match data {
                TagData::LutAToB(l) => l.clut.as_ref(),
                TagData::LutBToA(l) => l.clut.as_ref(),
                _ => None,
            }?;
            Some((signature_text(*sig), clut))
        })
        .collect()
}

/// A 2-D slice through a CLUT, interpolated on a dense grid.
#[derive(Debug, Clone)]
pub struct ClutSlice {
    /// Samples per side.
    pub size: usize,
    /// Output channels per sample.
    pub outputs: usize,
    /// `size × size` pixels of `outputs` samples, row-major, `y` growing down the image.
    pub samples: Vec<f64>,
    /// The table's grid, per input axis.
    pub grid_points: Vec<u8>,
}

/// Interpolates `clut` over a `size × size` slice: input `x_axis` sweeps left → right, `y_axis`
/// top → bottom, every other input held at `fixed[axis]`, through a one-stage
/// [`Pipeline`] holding a [`ClutTable`] at `interpolation`.
///
/// # Errors
///
/// [`CmmError`] for a table the engine refuses (bad geometry, tetrahedral below three inputs).
pub fn clut_slice(
    clut: &Clut,
    interpolation: ClutInterpolation,
    x_axis: usize,
    y_axis: usize,
    fixed: &[f64],
    size: usize,
) -> Result<ClutSlice, CmmError> {
    let table = ClutTable::with_interpolation(clut, interpolation)?;
    let inputs = table.input_channels();
    let outputs = table.output_channels();
    let grid_points = table.grid_points().to_vec();
    let pipeline = Pipeline::new(inputs, outputs, vec![Stage::Clut(table)])?;
    let mut input = vec![0.0; usize::from(inputs)];
    let mut samples = vec![0.0; size * size * usize::from(outputs)];
    let last = (size.max(2) - 1) as f64;
    for (i, out) in samples.chunks_exact_mut(usize::from(outputs)).enumerate() {
        for (axis, v) in input.iter_mut().enumerate() {
            *v = fixed.get(axis).copied().unwrap_or(0.0);
        }
        input[x_axis] = (i % size) as f64 / last;
        input[y_axis] = (i / size) as f64 / last;
        pipeline.eval(&input, out)?;
    }
    Ok(ClutSlice {
        size,
        outputs: usize::from(outputs),
        samples,
        grid_points,
    })
}

/// Renders a slice for the screen. Three outputs are read as v4-encoded PCS Lab (every
/// three-output CLUT in the battery is a device → Lab table) and shown through [`Display`].
/// Four outputs are CMYK device values, shown as `profile` renders them: through its own
/// device → PCS link (media-relative) and then [`Display`], so the preview is the printer's
/// colour, not a guess at it. Any other count (the one-channel `gamt` table) is data rather than
/// colour, and is drawn as its first channel in grey.
///
/// # Errors
///
/// [`CmmError`] when a four-output slice's `profile` has no device → PCS rendition.
pub fn slice_rgb8(
    slice: &ClutSlice,
    profile: &IccProfile,
    display: &Display,
) -> Result<Vec<u8>, CmmError> {
    let device = if slice.outputs == 4 {
        Some(device_to_pcs(
            profile,
            RenderingIntent::MediaRelativeColorimetric,
        )?)
    } else {
        None
    };
    let mut out = Vec::with_capacity(slice.samples.len() / slice.outputs.max(1) * 3);
    for s in slice.samples.chunks_exact(slice.outputs.max(1)) {
        let rgb = match (s, &device) {
            ([l, a, b], _) => {
                display.lab_to_rgb8([l * 100.0, a * 255.0 - 128.0, b * 255.0 - 128.0])
            }
            ([_, _, _, _], Some(link)) => {
                let mut pcs = [0.0; 3];
                link.eval(s, &mut pcs)?;
                display.lab_to_rgb8(pcs_to_lab(profile.header.pcs, pcs))
            }
            ([v, ..], _) => [to_u8(*v); 3],
            ([], _) => [0; 3],
        };
        out.extend_from_slice(&rgb);
    }
    Ok(out)
}

// --- Full pipeline -------------------------------------------------------------------------

/// Interleaved RGB8 `pixels` tagged `src` → the display's sRGB, at `settings`.
///
/// # Errors
///
/// [`CmmError`] when the profiles cannot be linked at the intent, or the buffer is not whole
/// pixels.
pub fn to_display(
    src: &IccProfile,
    display: &Display,
    pixels: &[u8],
    settings: Settings,
) -> Result<Vec<u8>, CmmError> {
    let transform = IccTransform::between(src, &display.profile, settings.transform_options())?;
    let mut out = vec![0; pixels.len()];
    transform_interleaved_u8(
        &transform,
        PixelFormat::Rgb8,
        pixels,
        PixelFormat::Rgb8,
        &mut out,
    )?;
    Ok(out)
}

/// Interleaved RGB16 `pixels` (a raw capture) tagged `src` → the display's sRGB 8-bit.
///
/// # Errors
///
/// As [`to_display`].
pub fn raw_to_display(
    src: &IccProfile,
    display: &Display,
    pixels: &[u16],
    settings: Settings,
) -> Result<Vec<u8>, CmmError> {
    let transform = IccTransform::between(src, &display.profile, settings.transform_options())?;
    let mut out = vec![0; pixels.len()];
    transform_interleaved_u16(
        &transform,
        PixelFormat::Rgb16,
        pixels,
        PixelFormat::Rgb16,
        &mut out,
    )?;
    Ok(out.iter().map(|&v| to_u8(f64::from(v) / 65535.0)).collect())
}

/// Interleaved RGB8 `pixels` tagged `src`, **soft-proofed** on `printer` and shown on the
/// display: rendered onto the printer at `settings.intent` (BPC as set), read back
/// media-relative, and carried to sRGB at `proofing_intent`.
///
/// # Errors
///
/// As [`to_display`].
pub fn proof_to_display(
    src: &IccProfile,
    printer: &IccProfile,
    display: &Display,
    pixels: &[u8],
    settings: Settings,
    proofing_intent: RenderingIntent,
) -> Result<Vec<u8>, CmmError> {
    let options = ProofingOptions {
        intent: settings.intent,
        proofing_intent,
        black_point_compensation: settings.bpc,
        optimization: settings.optimization,
        prefer_cicp_transfer: false,
    };
    let transform = IccTransform::proofing(src, &display.profile, printer, options)?;
    let mut out = vec![0; pixels.len()];
    transform_interleaved_u8(
        &transform,
        PixelFormat::Rgb8,
        pixels,
        PixelFormat::Rgb8,
        &mut out,
    )?;
    Ok(out)
}

/// Interleaved RGB8 `pixels` tagged `src` separated into the printer's CMYK, as `f64` ink
/// coverage per pixel.
///
/// # Errors
///
/// As [`to_display`].
pub fn separate(
    src: &IccProfile,
    printer: &IccProfile,
    pixels: &[u8],
    settings: Settings,
) -> Result<Vec<f64>, CmmError> {
    let transform = IccTransform::between(src, printer, settings.transform_options())?;
    let input: Vec<f64> = pixels.iter().map(|&v| f64::from(v) / 255.0).collect();
    let mut out = vec![0.0; input.len() / 3 * 4];
    transform.transform(&input, &mut out)?;
    Ok(out)
}

/// Per pixel of `pixels` (RGB8 tagged `src`): whether [`GamutCheck`] finds it outside the
/// printer's gamut at `settings.intent`.
///
/// # Errors
///
/// As [`to_display`].
pub fn gamut_mask(
    src: &IccProfile,
    printer: &IccProfile,
    pixels: &[u8],
    settings: Settings,
) -> Result<Vec<bool>, CmmError> {
    let check = GamutCheck::new(src, printer, settings.transform_options())?;
    let input: Vec<f64> = pixels.iter().map(|&v| f64::from(v) / 255.0).collect();
    let mut out = vec![0.0; input.len() / 3];
    check.transform(&input, &mut out)?;
    Ok(out.iter().map(|&v| v > 0.0).collect())
}

// --- Camera comparison ---------------------------------------------------------------------

/// Decoded PCS colour as Lab, whichever PCS the profile declares.
fn pcs_to_lab(pcs: ColorSpace, value: [f64; 3]) -> [f64; 3] {
    if pcs == ColorSpace::Lab {
        value
    } else {
        xyz_to_lab(value, D50_XYZ)
    }
}

/// Per chart patch: the ΔE₀₀ between the scene's true colour and what `camera` reconstructs
/// from the raw capture ([`scene::raw_patches`]), media-relative, with exposure undone.
///
/// # Errors
///
/// [`CmmError`] when the profile has no device → PCS rendition.
pub fn camera_patch_errors(camera: &IccProfile) -> Result<Vec<f64>, CmmError> {
    let pipeline = device_to_pcs(camera, RenderingIntent::MediaRelativeColorimetric)?;
    let mut errors = Vec::with_capacity(scene::PATCHES.len());
    for (raw, patch) in scene::raw_patches().iter().zip(scene::PATCHES) {
        let unexposed = raw.map(|v| v / scene::RAW_EXPOSURE);
        let mut pcs = [0.0; 3];
        pipeline.eval(&unexposed, &mut pcs)?;
        let lab = pcs_to_lab(camera.header.pcs, pcs);
        errors.push(delta_e_2000(lab, scene::patch_lab(patch)));
    }
    Ok(errors)
}

// --- Gamut hull ----------------------------------------------------------------------------

/// A profile's gamut, projected onto the CIE 1931 xy chromaticity plane.
#[derive(Debug, Clone)]
pub struct Hull {
    /// The chromaticity of every device-surface sample.
    pub points: Vec<[f64; 2]>,
    /// Their convex hull, counter-clockwise.
    pub hull: Vec<[f64; 2]>,
    /// The PCS → device → PCS round trip's worst ΔE₀₀ over the samples, when the profile
    /// renders in both directions.
    pub round_trip_max: Option<f64>,
    /// The same round trip's mean ΔE₀₀.
    pub round_trip_mean: Option<f64>,
}

/// Steps per edge of the device-surface sampling.
pub const HULL_STEPS: usize = 12;

/// Device values on the surface of the unit cube (RGB) or of the CMY cube at `K = 0` plus the
/// grey axis (CMYK) — where a device's gamut boundary lives.
fn surface(channels: usize) -> Vec<Vec<f64>> {
    let n = HULL_STEPS;
    let step = |i: usize| i as f64 / n as f64;
    let mut out = Vec::new();
    for a in 0..=n {
        for b in 0..=n {
            for face in 0..6 {
                let (fixed_axis, fixed) = (face / 2, if face % 2 == 0 { 0.0 } else { 1.0 });
                let mut v = [0.0; 3];
                let (p, q) = match fixed_axis {
                    0 => (1, 2),
                    1 => (0, 2),
                    _ => (0, 1),
                };
                v[fixed_axis] = fixed;
                v[p] = step(a);
                v[q] = step(b);
                let mut sample = v.to_vec();
                sample.resize(channels, 0.0);
                out.push(sample);
            }
        }
    }
    if channels == 4 {
        out.extend((0..=n).map(|k| vec![0.0, 0.0, 0.0, step(k)]));
    }
    out
}

/// The gamut hull of `profile` at `intent`: device-surface samples converted to PCS in bulk
/// ([`device_to_pcs`]), projected to xy, and — where the profile also renders PCS → device —
/// round-tripped back ([`pcs_to_device`] then [`device_to_pcs`]) to measure how faithfully the
/// pair inverts.
///
/// # Errors
///
/// [`CmmError`] when the profile has no device → PCS rendition at the intent.
pub fn gamut_hull(profile: &IccProfile, intent: RenderingIntent) -> Result<Hull, CmmError> {
    let forward = device_to_pcs(profile, intent)?;
    let backward = pcs_to_device(profile, intent).ok();
    let channels = usize::from(forward.input_channels());
    let pcs = profile.header.pcs;
    let mut points = Vec::new();
    let mut errors = Vec::new();
    let mut device = vec![0.0; channels];
    for sample in surface(channels) {
        let mut value = [0.0; 3];
        forward.eval(&sample, &mut value)?;
        let lab = pcs_to_lab(pcs, value);
        let xyz = lab_to_xyz(lab, D50_XYZ);
        if xyz[1] > 1e-6 {
            let [x, y, _] = xyz_to_xyy(xyz);
            points.push([x, y]);
        }
        if let Some(back) = &backward {
            back.eval(&value, &mut device)?;
            let mut again = [0.0; 3];
            forward.eval(&device, &mut again)?;
            errors.push(delta_e_2000(lab, pcs_to_lab(pcs, again)));
        }
    }
    let (round_trip_max, round_trip_mean) = if errors.is_empty() {
        (None, None)
    } else {
        let max = errors.iter().copied().fold(0.0, f64::max);
        (
            Some(max),
            Some(errors.iter().sum::<f64>() / errors.len() as f64),
        )
    };
    Ok(Hull {
        hull: convex_hull(&points),
        points,
        round_trip_max,
        round_trip_mean,
    })
}

/// Andrew's monotone-chain convex hull, counter-clockwise, no repeated closing point.
#[must_use]
pub fn convex_hull(points: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut sorted = points.to_vec();
    sorted.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    sorted.dedup();
    if sorted.len() < 3 {
        return sorted;
    }
    let cross = |o: [f64; 2], a: [f64; 2], b: [f64; 2]| {
        (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    };
    let mut hull: Vec<[f64; 2]> = Vec::with_capacity(sorted.len() * 2);
    for pass in [false, true] {
        let start = hull.len();
        let iter: Box<dyn Iterator<Item = &[f64; 2]>> = if pass {
            Box::new(sorted.iter().rev())
        } else {
            Box::new(sorted.iter())
        };
        for &p in iter {
            while hull.len() >= start + 2
                && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0
            {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    hull
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{Battery, ProfileKind};

    fn battery() -> Battery {
        Battery::build().expect("battery builds")
    }

    #[test]
    fn srgb_matrix_maps_white_to_d50() {
        let white = linear_srgb_to_xyz_d50([1.0; 3]);
        for (got, want) in white.iter().zip(D50_XYZ) {
            assert!((got - want).abs() < 1e-3, "{white:?}");
        }
    }

    #[test]
    fn display_renders_pcs_white_as_white() {
        let display = Display::new().expect("display link");
        assert_eq!(display.xyz_to_rgb8(D50_XYZ), [255; 3]);
        assert_eq!(display.lab_to_rgb8([0.0; 3]), [0; 3]);
    }

    #[test]
    fn every_trc_is_monotonic_and_inverts_tightly() {
        let battery = battery();
        for kind in [
            ProfileKind::SrgbV2,
            ProfileKind::DisplayP3,
            ProfileKind::CameraA,
        ] {
            let curves = trc_curves(battery.profile(kind).expect("parses")).expect("curves build");
            assert_eq!(curves.len(), 3, "{kind:?}");
            for c in curves {
                assert!(c.monotonic, "{kind:?} {}", c.tag);
                let error = c.round_trip_error.expect("monotonic curves invert");
                // lcms2-shaped numeric reversal of a 1024-entry table: well under one 8-bit code.
                assert!(error < 1.0 / 255.0, "{kind:?} {}: {error}", c.tag);
            }
        }
    }

    #[test]
    fn inspector_reads_the_colorant_matrix() {
        let battery = battery();
        let view = inspect(battery.profile(ProfileKind::DisplayP3).expect("parses"));
        let m = view.colorants.expect("matrix/TRC profile");
        // The colorant columns sum to the D50 media white (ICC.1 §8.4.3).
        for (row, want) in m.iter().zip(D50_XYZ) {
            assert!((row.iter().sum::<f64>() - want).abs() < 1e-3);
        }
        assert!(view.chad.is_some());
        assert!(view.conformance.is_empty());
    }

    #[test]
    fn diagnosis_names_the_truncation() {
        let battery = battery();
        let entry = battery.get(ProfileKind::Truncated);
        let d = diagnose(&entry.bytes).expect("truncated bytes do not parse");
        assert_eq!(d.actual_size, crate::profiles::TRUNCATED_LEN);
        assert!(d.declared_size.expect("header present") as usize > d.actual_size);
        assert!(d.advice.contains("truncated"));
        assert!(diagnose(&battery.get(ProfileKind::SrgbV2).bytes).is_none());
    }

    #[test]
    fn clut_slice_reaches_the_grid_corners() {
        let battery = battery();
        let printer = battery.profile(ProfileKind::Printer).expect("parses");
        let (_, clut) = clut_tags(printer)
            .into_iter()
            .next()
            .expect("A2B0 has a CLUT");
        let slice =
            clut_slice(clut, ClutInterpolation::Multilinear, 0, 1, &[0.0; 4], 5).expect("slice");
        // The (0, 0) corner of a CMYK → Lab table at K = 0 is paper: L* = 100.
        assert!(
            (slice.samples[0] - 1.0).abs() < 1e-3,
            "{}",
            slice.samples[0]
        );
        assert_eq!(slice.samples.len(), 5 * 5 * 3);
    }

    #[test]
    fn cmyk_slice_renders_through_the_printer() {
        let battery = battery();
        let printer = battery.profile(ProfileKind::Printer).expect("parses");
        let display = Display::new().expect("display link");
        let (_, b2a1) = clut_tags(printer)
            .into_iter()
            .find(|(tag, _)| tag == "B2A1")
            .expect("B2A1 has a CLUT");
        // L* = 100 held, a* across, b* down: the centre pixel is the PCS white, which the
        // separation leaves unprinted and the printer's A2B1 renders as white.
        let slice = clut_slice(
            b2a1,
            ClutInterpolation::Tetrahedral,
            1,
            2,
            &[1.0, 0.0, 0.0],
            3,
        )
        .expect("slice");
        let rgb = slice_rgb8(&slice, printer, &display).expect("A2B1 renders the inks");
        assert_eq!(rgb.len(), 9 * 3);
        let centre = &rgb[4 * 3..5 * 3];
        assert!(centre.iter().all(|&c| c >= 250), "{centre:?}");
    }

    #[test]
    fn camera_a_reconstructs_its_own_capture() {
        let battery = battery();
        let errors = camera_patch_errors(battery.profile(ProfileKind::CameraA).expect("parses"))
            .expect("A2B");
        // Camera A *is* the sensor's characterization, so only clipping and the 16-bit TRC
        // separate it from the scene; camera B, a different characterization, does worse.
        let a_max = errors.iter().copied().fold(0.0, f64::max);
        assert!(a_max < 1.0, "camera A worst patch ΔE00 {a_max}");
        let b = camera_patch_errors(battery.profile(ProfileKind::CameraB).expect("parses"))
            .expect("A2B");
        let b_mean = b.iter().sum::<f64>() / b.len() as f64;
        let a_mean = errors.iter().sum::<f64>() / errors.len() as f64;
        assert!(b_mean > a_mean, "camera B mean {b_mean} vs A {a_mean}");
    }

    #[test]
    fn hull_round_trip_is_tight_for_a_matrix_shaper() {
        let battery = battery();
        let hull = gamut_hull(
            battery.profile(ProfileKind::DisplayP3).expect("parses"),
            RenderingIntent::MediaRelativeColorimetric,
        )
        .expect("hull");
        assert!(hull.round_trip_max.expect("both directions") < 1e-2);
        assert!(hull.hull.len() >= 3);
        // Camera B has no B2A: no round trip, but still a hull.
        let b = gamut_hull(
            battery.profile(ProfileKind::CameraB).expect("parses"),
            RenderingIntent::Perceptual,
        )
        .expect("hull");
        assert!(b.round_trip_max.is_none());
    }

    #[test]
    fn convex_hull_drops_interior_points() {
        let square = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.5, 0.5]];
        assert_eq!(
            convex_hull(&square),
            vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]
        );
    }

    #[test]
    fn intents_change_the_print_separation() {
        let battery = battery();
        let src = battery.profile(ProfileKind::SrgbV2).expect("parses");
        let printer = battery.profile(ProfileKind::Printer).expect("parses");
        let pixels = [255u8, 0, 0];
        let mut seen = Vec::new();
        for (intent, _) in INTENTS {
            let settings = Settings {
                intent,
                ..Settings::default()
            };
            seen.push(separate(src, printer, &pixels, settings).expect("links"));
        }
        assert_ne!(seen[0], seen[1], "perceptual vs relative");
        assert_ne!(seen[1], seen[2], "relative vs saturation");
        assert_ne!(seen[1], seen[3], "relative vs absolute");
    }

    #[test]
    fn bpc_changes_the_relative_rendering() {
        let battery = battery();
        let src = battery.profile(ProfileKind::SrgbV2).expect("parses");
        let printer = battery.profile(ProfileKind::Printer).expect("parses");
        let pixels = [20u8, 20, 20];
        let at = |bpc| {
            let settings = Settings {
                intent: RenderingIntent::MediaRelativeColorimetric,
                bpc,
                ..Settings::default()
            };
            separate(src, printer, &pixels, settings).expect("links")
        };
        assert_ne!(at(false), at(true));
    }

    #[test]
    fn saturated_red_is_out_of_print_gamut() {
        let battery = battery();
        let src = battery.profile(ProfileKind::DisplayP3).expect("parses");
        let printer = battery.profile(ProfileKind::Printer).expect("parses");
        let mask = gamut_mask(
            src,
            printer,
            &[255, 0, 0, 128, 128, 128],
            Settings {
                intent: RenderingIntent::MediaRelativeColorimetric,
                ..Settings::default()
            },
        )
        .expect("gamut check");
        assert_eq!(mask, vec![true, false]);
    }
}
