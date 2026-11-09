//! Typed views over the DNG colour tags a raw pipeline reads beyond the calibration
//! [`CameraProfile`](crate::CameraProfile) models.
//!
//! Two projections live here:
//!
//! - [`ColorProfileInfo`] — IFD 0's remaining *camera profile* tags (DNG 1.7.1 chapter 6): the
//!   hue/saturation/value mapping tables and the "look" table, the default tone curve, the
//!   profile's exposure offset, the third calibration set (DNG 1.6) and the dimensionality
//!   reduction matrices.
//! - [`NoiseProfile`] — the raw IFD's `NoiseProfile` (51041), the sensor's two-parameter noise
//!   model.
//!
//! Both are **convenience projections, not lossless models of a directory**: `from_ifd` lifts the
//! tags it names out of the IFD it is given and ignores everything else, and a value it cannot
//! read as the spec describes is left unprojected rather than guessed at. Neither has a write
//! direction — [`DngEncoder`](crate::DngEncoder) builds IFD 0 from a [`CameraProfile`](crate::CameraProfile),
//! not from an [`Ifd`], so writing these tags is its own (deferred) feature; see `STATUS.md`.
//!
//! Nothing is lost either way: the decoder surfaces every field it does not project verbatim as a
//! [`RawTag`](crate::RawTag) in
//! [`DecodedDng::ifd0_extra`](crate::DecodedDng::ifd0_extra) / [`raw_extra`](crate::DecodedDng::raw_extra),
//! so a malformed — or simply unmodelled — tag stays visible with its typed value.

use gamut_ifd::{Ifd, Value};

use crate::decoder::f64_vec;
use crate::tags;
use crate::values::{CalibrationIlluminant, TableEncoding};

/// The source of tag values a projection reads.
///
/// Implemented for a plain [`Ifd`] (the public [`ColorProfileInfo::from_ifd`] entry point) and,
/// inside the decoder, for its consumption-tracking reader — so a tag this module reads is marked
/// consumed there, and a tag it *rejects* as malformed is un-marked and still reaches the extras.
pub(crate) trait TagSource {
    /// The value stored under `tag`, if the directory carries it.
    fn value(&self, tag: u16) -> Option<&Value>;

    /// Reports that `tag` was read but rejected, so the caller can keep surfacing it verbatim.
    fn reject(&self, tag: u16);
}

impl TagSource for Ifd {
    fn value(&self, tag: u16) -> Option<&Value> {
        self.get(tag)
    }

    fn reject(&self, _tag: u16) {}
}

/// One entry of a hue/saturation/value mapping table (DNG 1.7.1 pp. 49-50).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HsvDelta {
    /// The hue shift, in degrees.
    pub hue_shift_degrees: f32,
    /// The saturation scale factor.
    pub saturation_scale: f32,
    /// The value scale factor.
    pub value_scale: f32,
}

/// A hue/saturation/value mapping table: its input divisions, its entries, and the encoding used
/// to index it.
///
/// This is the shape shared by `ProfileHueSatMapData1`/`2`/`3` (50938/50939/52537) and the
/// `ProfileLookTableData` (50982) — the spec defines the look table as "the same format", applied
/// later in the pipeline.
///
/// Entries are stored in the spec's nested loop order: value divisions outermost, then hue, with
/// saturation innermost. Use [`entry`](Self::entry) rather than indexing
/// [`entries`](Self::entries) by hand.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HsvTable {
    /// `HueDivisions` — the number of hue samples (>= 1).
    pub hue_divisions: u32,
    /// `SaturationDivisions` — the number of saturation samples (>= 2).
    pub saturation_divisions: u32,
    /// `ValueDivisions` — the number of value samples (>= 1); 1 for the common 2.5D table.
    pub value_divisions: u32,
    /// How a 3D table is indexed (`ProfileHueSatMapEncoding` 51107 / `ProfileLookTableEncoding`
    /// 51108); [`Linear`](TableEncoding::Linear) when the file omits the tag.
    pub encoding: TableEncoding,
    /// The `HueDivisions * SaturationDivisions * ValueDivisions` entries, in the spec's loop
    /// order.
    pub entries: Vec<HsvDelta>,
}

impl HsvTable {
    /// The entry at the given hue/saturation/value sample indices, or `None` if any index is
    /// outside its division count.
    #[must_use]
    pub fn entry(&self, hue: u32, saturation: u32, value: u32) -> Option<HsvDelta> {
        if hue >= self.hue_divisions
            || saturation >= self.saturation_divisions
            || value >= self.value_divisions
        {
            return None;
        }
        // Value outermost, hue in the middle, saturation innermost (DNG 1.7.1 p. 49).
        let plane = value as usize * self.hue_divisions as usize + hue as usize;
        let index = plane * self.saturation_divisions as usize + saturation as usize;
        self.entries.get(index).copied()
    }
}

/// The two-parameter noise model of one colour plane: `N(x) = sqrt(scale * x + offset)`, where
/// `x` is a recorded linear signal in `[0, 1]` (DNG 1.7.1 pp. 58-59).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NoiseModel {
    /// The scale term `S`, modelling signal-dependent photon (shot) noise. Positive.
    pub scale: f64,
    /// The offset term `O`, modelling signal-independent sensor readout noise. Non-negative.
    pub offset: f64,
}

impl NoiseModel {
    /// The modelled noise — the standard deviation of the recorded linear signal `signal` — at
    /// that signal level.
    #[must_use]
    pub fn std_dev(&self, signal: f64) -> f64 {
        (self.scale * signal + self.offset).max(0.0).sqrt()
    }
}

/// The `NoiseProfile` (51041): the camera's noise model, either one model shared by every colour
/// plane or one model per plane, in `CFAPlaneColor` order.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NoiseProfile {
    /// The per-plane models. Exactly one entry means the same model applies to every plane.
    pub planes: Vec<NoiseModel>,
}

impl NoiseProfile {
    /// Lifts the noise model out of an IFD, or `None` when the directory carries no usable
    /// `NoiseProfile`.
    ///
    /// The spec stores this tag in the raw (or enhanced) IFD, not IFD 0.
    #[must_use]
    pub fn from_ifd(ifd: &Ifd) -> Option<Self> {
        project_noise(ifd)
    }

    /// The model for colour plane `plane`; a profile carrying a single model applies it to every
    /// plane, as the spec prescribes.
    #[must_use]
    pub fn for_plane(&self, plane: usize) -> Option<NoiseModel> {
        if self.planes.len() == 1 {
            self.planes.first().copied()
        } else {
            self.planes.get(plane).copied()
        }
    }
}

/// The camera-profile colour tags of IFD 0 that [`CameraProfile`](crate::CameraProfile) does not
/// model — the rendering tables and curve, the profile exposure offset, the DNG 1.6 third
/// calibration set, and the dimensionality reduction matrices.
///
/// Every field is independently optional: a file carrying only a tone curve projects one with the
/// rest `None`. All-`None` never occurs — the projection reports `None` for a directory carrying
/// none of these tags.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ColorProfileInfo {
    /// `ColorMatrix3` (52531) — the row-major 3x3 XYZ → reference-camera-native matrix for the
    /// third calibration illuminant.
    pub color_matrix3: Option<[f64; 9]>,
    /// `CalibrationIlluminant3` (52529) — the illuminant [`color_matrix3`](Self::color_matrix3)
    /// is calibrated for.
    pub calibration_illuminant3: Option<CalibrationIlluminant>,
    /// `CameraCalibration3` (52530) — the row-major 3x3 per-camera calibration matrix for the
    /// third illuminant.
    pub camera_calibration3: Option<[f64; 9]>,
    /// `ForwardMatrix3` (52532) — the row-major 3x3 white-balanced-camera → XYZ(D50) matrix for
    /// the third illuminant.
    pub forward_matrix3: Option<[f64; 9]>,
    /// `ReductionMatrix1` (50725) — the `3 x ColorPlanes` dimensionality reduction matrix for the
    /// first illuminant, in row-scan order (only meaningful above three colour planes).
    pub reduction_matrix1: Option<Vec<f64>>,
    /// `ReductionMatrix2` (50726) — the same for the second illuminant.
    pub reduction_matrix2: Option<Vec<f64>>,
    /// `ReductionMatrix3` (52538) — the same for the third illuminant.
    pub reduction_matrix3: Option<Vec<f64>>,
    /// `ProfileHueSatMapData1` (50938) with the shared `ProfileHueSatMapDims`/`Encoding`.
    pub hue_sat_map1: Option<HsvTable>,
    /// `ProfileHueSatMapData2` (50939) — the second illuminant's table.
    pub hue_sat_map2: Option<HsvTable>,
    /// `ProfileHueSatMapData3` (52537) — the third illuminant's table.
    pub hue_sat_map3: Option<HsvTable>,
    /// `ProfileLookTableData` (50982) with its own `ProfileLookTableDims`/`Encoding` — the
    /// profile's default "look", applied after exposure but before the tone curve.
    pub look_table: Option<HsvTable>,
    /// `ProfileToneCurve` (50940) — the default tone curve as `(input, output)` pairs, both in
    /// `[0, 1]` and strictly increasing in `input`. Readers interpolate it with a cubic spline.
    pub tone_curve: Option<Vec<(f32, f32)>>,
    /// `BaselineExposureOffset` (51109) — the EV to add to
    /// [`CameraProfile::baseline_exposure`](crate::CameraProfile::baseline_exposure) when
    /// rendering with this profile.
    pub baseline_exposure_offset: Option<f64>,
}

impl ColorProfileInfo {
    /// Lifts the camera-profile colour tags out of an IFD, or `None` when it carries none of
    /// them.
    ///
    /// The spec stores these in IFD 0 (or a camera profile IFD, which this crate does not open).
    #[must_use]
    pub fn from_ifd(ifd: &Ifd) -> Option<Self> {
        project(ifd)
    }
}

/// Projects the camera-profile colour tags out of any [`TagSource`].
pub(crate) fn project<S: TagSource>(src: &S) -> Option<ColorProfileInfo> {
    let mut hue_sat = hsv_tables(
        src,
        tags::PROFILE_HUE_SAT_MAP_DIMS,
        &[
            tags::PROFILE_HUE_SAT_MAP_DATA1,
            tags::PROFILE_HUE_SAT_MAP_DATA2,
            tags::PROFILE_HUE_SAT_MAP_DATA3,
        ],
        tags::PROFILE_HUE_SAT_MAP_ENCODING,
    )
    .into_iter();
    let look_table = hsv_tables(
        src,
        tags::PROFILE_LOOK_TABLE_DIMS,
        &[tags::PROFILE_LOOK_TABLE_DATA],
        tags::PROFILE_LOOK_TABLE_ENCODING,
    )
    .into_iter()
    .next()
    .flatten();
    let info = ColorProfileInfo {
        color_matrix3: matrix9(src, tags::COLOR_MATRIX3),
        calibration_illuminant3: illuminant(src, tags::CALIBRATION_ILLUMINANT3),
        camera_calibration3: matrix9(src, tags::CAMERA_CALIBRATION3),
        forward_matrix3: matrix9(src, tags::FORWARD_MATRIX3),
        reduction_matrix1: reduction_matrix(src, tags::REDUCTION_MATRIX1),
        reduction_matrix2: reduction_matrix(src, tags::REDUCTION_MATRIX2),
        reduction_matrix3: reduction_matrix(src, tags::REDUCTION_MATRIX3),
        hue_sat_map1: hue_sat.next().flatten(),
        hue_sat_map2: hue_sat.next().flatten(),
        hue_sat_map3: hue_sat.next().flatten(),
        look_table,
        tone_curve: tone_curve(src, tags::PROFILE_TONE_CURVE),
        baseline_exposure_offset: scalar(src, tags::BASELINE_EXPOSURE_OFFSET),
    };
    (info != ColorProfileInfo::default()).then_some(info)
}

/// Projects the `NoiseProfile` out of any [`TagSource`].
pub(crate) fn project_noise<S: TagSource>(src: &S) -> Option<NoiseProfile> {
    let value = src.value(tags::NOISE_PROFILE)?;
    // DOUBLE per the spec; a FLOAT-typed writer converts without loss.
    let parameters = match value {
        Value::Double(v) => v.clone(),
        Value::Float(v) => v.iter().map(|&x| f64::from(x)).collect(),
        _ => Vec::new(),
    };
    // Count is 2 or 2 * ColorPlanes, each S positive and each O non-negative (DNG 1.7.1 p. 58).
    let (models, remainder) = parameters.as_chunks::<2>();
    let usable = !models.is_empty()
        && remainder.is_empty()
        && models.iter().all(|&[scale, offset]| {
            scale.is_finite() && offset.is_finite() && scale > 0.0 && offset >= 0.0
        });
    if !usable {
        src.reject(tags::NOISE_PROFILE);
        return None;
    }
    Some(NoiseProfile {
        planes: models
            .iter()
            .map(|&[scale, offset]| NoiseModel { scale, offset })
            .collect(),
    })
}

/// Reads a nine-element `(S)RATIONAL` colour matrix, rejecting any other shape.
fn matrix9<S: TagSource>(src: &S, tag: u16) -> Option<[f64; 9]> {
    let value = src.value(tag)?;
    match f64_vec(Some(value)).filter(|v| v.len() == 9) {
        Some(v) => {
            let mut m = [0.0; 9];
            m.copy_from_slice(&v);
            Some(m)
        }
        None => {
            src.reject(tag);
            None
        }
    }
}

/// Reads a `3 x ColorPlanes` reduction matrix — any positive multiple of three `(S)RATIONAL`s.
fn reduction_matrix<S: TagSource>(src: &S, tag: u16) -> Option<Vec<f64>> {
    let value = src.value(tag)?;
    match f64_vec(Some(value)).filter(|v| !v.is_empty() && v.len() % 3 == 0) {
        Some(v) => Some(v),
        None => {
            src.reject(tag);
            None
        }
    }
}

/// Reads a single-valued `(S)RATIONAL` tag.
fn scalar<S: TagSource>(src: &S, tag: u16) -> Option<f64> {
    let value = src.value(tag)?;
    match f64_vec(Some(value)).filter(|v| v.len() == 1) {
        Some(v) => v.first().copied(),
        None => {
            src.reject(tag);
            None
        }
    }
}

/// Reads a `CalibrationIlluminant` tag, rejecting a code the crate does not model.
fn illuminant<S: TagSource>(src: &S, tag: u16) -> Option<CalibrationIlluminant> {
    let value = src.value(tag)?;
    match value
        .as_u32()
        .and_then(|c| u16::try_from(c).ok())
        .and_then(|c| CalibrationIlluminant::try_from(c).ok())
    {
        Some(illuminant) => Some(illuminant),
        None => {
            src.reject(tag);
            None
        }
    }
}

/// Reads a `FLOAT` array, rejecting an empty or differently-typed value.
fn floats<S: TagSource>(src: &S, tag: u16) -> Option<Vec<f32>> {
    let value = src.value(tag)?;
    match value {
        Value::Float(v) if !v.is_empty() => Some(v.clone()),
        _ => {
            src.reject(tag);
            None
        }
    }
}

/// Reads a `ProfileHueSatMapDims`-shaped tag: three divisions within the spec's legal ranges.
fn table_dims<S: TagSource>(src: &S, tag: u16) -> Option<[u32; 3]> {
    let value = src.value(tag)?;
    // HueDivisions >= 1, SaturationDivisions >= 2, ValueDivisions >= 1 (DNG 1.7.1 pp. 48, 55).
    match value
        .as_u32_vec()
        .filter(|v| v.len() == 3 && v[0] >= 1 && v[1] >= 2 && v[2] >= 1)
    {
        Some(v) => Some([v[0], v[1], v[2]]),
        None => {
            src.reject(tag);
            None
        }
    }
}

/// Reads a table-encoding tag, falling back to the spec's default for an absent or unmodelled
/// value.
fn table_encoding<S: TagSource>(src: &S, tag: u16) -> TableEncoding {
    let Some(value) = src.value(tag) else {
        return TableEncoding::default();
    };
    match value.as_u32().and_then(|c| TableEncoding::try_from(c).ok()) {
        Some(encoding) => encoding,
        None => {
            src.reject(tag);
            TableEncoding::default()
        }
    }
}

/// Reads the tables sharing one dimensions tag and one encoding tag, one result per data tag.
///
/// The dimensions and encoding tags are consumed only if at least one table is usable, so a
/// dangling `…Dims` with no matching data still surfaces verbatim.
fn hsv_tables<S: TagSource>(
    src: &S,
    dims_tag: u16,
    data_tags: &[u16],
    encoding_tag: u16,
) -> Vec<Option<HsvTable>> {
    let data: Vec<Option<Vec<f32>>> = data_tags.iter().map(|&tag| floats(src, tag)).collect();
    if data.iter().all(Option::is_none) {
        return vec![None; data_tags.len()];
    }
    let dims = table_dims(src, dims_tag);
    // Count is HueDivisions * SaturationDivisions * ValueDivisions * 3 (DNG 1.7.1 p. 49).
    let expected = dims.and_then(|[hue, saturation, value]| {
        (hue as usize)
            .checked_mul(saturation as usize)?
            .checked_mul(value as usize)?
            .checked_mul(3)
    });
    let mut tables = Vec::with_capacity(data_tags.len());
    for (&tag, entries) in data_tags.iter().zip(data) {
        let Some(entries) = entries else {
            tables.push(None);
            continue;
        };
        match (dims, expected) {
            (Some([hue, saturation, value]), Some(count)) if entries.len() == count => {
                tables.push(Some(HsvTable {
                    hue_divisions: hue,
                    saturation_divisions: saturation,
                    value_divisions: value,
                    encoding: TableEncoding::default(),
                    entries: entries
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .map(
                            |&[hue_shift_degrees, saturation_scale, value_scale]| HsvDelta {
                                hue_shift_degrees,
                                saturation_scale,
                                value_scale,
                            },
                        )
                        .collect(),
                }));
            }
            _ => {
                src.reject(tag);
                tables.push(None);
            }
        }
    }
    if tables.iter().any(Option::is_some) {
        let encoding = table_encoding(src, encoding_tag);
        for table in tables.iter_mut().flatten() {
            table.encoding = encoding;
        }
    } else if dims.is_some() {
        src.reject(dims_tag);
    }
    tables
}

/// Reads `ProfileToneCurve` as `(input, output)` pairs, rejecting a curve whose inputs are not
/// strictly increasing (DNG 1.7.1 p. 50).
fn tone_curve<S: TagSource>(src: &S, tag: u16) -> Option<Vec<(f32, f32)>> {
    let values = floats(src, tag)?;
    let (points, remainder) = values.as_chunks::<2>();
    let increasing = remainder.is_empty() && points.windows(2).all(|pair| pair[1][0] > pair[0][0]);
    if !increasing {
        src.reject(tag);
        return None;
    }
    Some(
        points
            .iter()
            .map(|&[input, output]| (input, output))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory carrying nothing this module projects.
    fn empty() -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(tags::BASELINE_SHARPNESS, Value::Rational(vec![(3, 2)]));
        ifd
    }

    fn identity_matrix() -> Value {
        Value::SRational(vec![
            (1, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 1),
        ])
    }

    /// A 1x2x1 hue/sat table: the smallest the spec's division rules allow.
    fn small_table() -> Value {
        Value::Float(vec![0.0, 1.0, 1.0, 10.0, 1.5, 0.5])
    }

    /// A `FLOAT` tag present but empty is rejected, not read as an empty array.
    ///
    /// `floats` guards on `!v.is_empty()`, and only the well-formed and wrong-type cases were
    /// tested, so relaxing that guard to `true` let an empty array through as `Some(vec![])` and
    /// nothing noticed (#110). Both sides are asserted, since a `floats` that always returned
    /// `None` would satisfy the rejection on its own.
    #[test]
    fn an_empty_float_array_is_rejected() {
        let mut ifd = empty();
        ifd.set(tags::PROFILE_TONE_CURVE, Value::Float(vec![]));
        assert_eq!(floats(&ifd, tags::PROFILE_TONE_CURVE), None);

        ifd.set(
            tags::PROFILE_TONE_CURVE,
            Value::Float(vec![0.0, 0.0, 1.0, 1.0]),
        );
        assert_eq!(
            floats(&ifd, tags::PROFILE_TONE_CURVE),
            Some(vec![0.0, 0.0, 1.0, 1.0])
        );
    }

    #[test]
    fn a_directory_without_profile_tags_projects_nothing() {
        assert_eq!(ColorProfileInfo::from_ifd(&empty()), None);
        assert_eq!(NoiseProfile::from_ifd(&empty()), None);
    }

    #[test]
    fn third_calibration_set_projects() {
        let mut ifd = empty();
        ifd.set(tags::COLOR_MATRIX3, identity_matrix());
        ifd.set(tags::CALIBRATION_ILLUMINANT3, Value::Short(vec![21]));
        ifd.set(tags::CAMERA_CALIBRATION3, identity_matrix());
        ifd.set(tags::FORWARD_MATRIX3, identity_matrix());
        let info = ColorProfileInfo::from_ifd(&ifd).expect("third calibration present");
        assert_eq!(
            info.color_matrix3,
            Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0])
        );
        assert_eq!(
            info.calibration_illuminant3,
            Some(CalibrationIlluminant::D65)
        );
        assert!(info.camera_calibration3.is_some());
        assert!(info.forward_matrix3.is_some());
        assert_eq!(info.tone_curve, None);
    }

    #[test]
    fn a_malformed_matrix_or_illuminant_is_not_projected() {
        let mut ifd = empty();
        // Eight entries: not a 3x3 matrix.
        ifd.set(tags::COLOR_MATRIX3, Value::SRational(vec![(1, 1); 8]));
        ifd.set(tags::CALIBRATION_ILLUMINANT3, Value::Short(vec![9999]));
        assert_eq!(ColorProfileInfo::from_ifd(&ifd), None);
    }

    #[test]
    fn reduction_matrices_take_any_multiple_of_three() {
        let mut ifd = empty();
        // A four-plane camera: 3 x 4 entries.
        ifd.set(tags::REDUCTION_MATRIX1, Value::SRational(vec![(1, 2); 12]));
        ifd.set(tags::REDUCTION_MATRIX2, Value::SRational(vec![(1, 4); 7]));
        let info = ColorProfileInfo::from_ifd(&ifd).expect("a reduction matrix is present");
        assert_eq!(info.reduction_matrix1, Some(vec![0.5; 12]));
        assert_eq!(info.reduction_matrix2, None, "7 is not a multiple of three");
        assert_eq!(info.reduction_matrix3, None);
    }

    #[test]
    fn hue_sat_tables_share_dims_and_encoding() {
        let mut ifd = empty();
        ifd.set(tags::PROFILE_HUE_SAT_MAP_DIMS, Value::Long(vec![1, 2, 1]));
        ifd.set(tags::PROFILE_HUE_SAT_MAP_DATA1, small_table());
        ifd.set(tags::PROFILE_HUE_SAT_MAP_DATA2, small_table());
        ifd.set(tags::PROFILE_HUE_SAT_MAP_ENCODING, Value::Long(vec![1]));
        let info = ColorProfileInfo::from_ifd(&ifd).expect("tables present");
        let map1 = info.hue_sat_map1.expect("first table");
        assert_eq!(map1.hue_divisions, 1);
        assert_eq!(map1.saturation_divisions, 2);
        assert_eq!(map1.value_divisions, 1);
        assert_eq!(map1.encoding, TableEncoding::Srgb);
        assert_eq!(map1.entries.len(), 2);
        assert_eq!(
            info.hue_sat_map2.map(|t| t.encoding),
            Some(TableEncoding::Srgb),
            "the encoding tag applies to every table sharing the dims"
        );
        assert_eq!(info.hue_sat_map3, None);
    }

    #[test]
    fn table_entries_follow_the_spec_loop_order() {
        let mut ifd = empty();
        // 2 hue x 2 saturation x 2 value; the hue shift of each entry is its storage index.
        ifd.set(tags::PROFILE_LOOK_TABLE_DIMS, Value::Long(vec![2, 2, 2]));
        let entries: Vec<f32> = (0..8).flat_map(|i| [i as f32, 1.0, 1.0]).collect();
        ifd.set(tags::PROFILE_LOOK_TABLE_DATA, Value::Float(entries));
        let table = ColorProfileInfo::from_ifd(&ifd)
            .and_then(|info| info.look_table)
            .expect("look table");
        // Value outermost, hue in the middle, saturation innermost.
        assert_eq!(table.entry(0, 0, 0).map(|e| e.hue_shift_degrees), Some(0.0));
        assert_eq!(table.entry(0, 1, 0).map(|e| e.hue_shift_degrees), Some(1.0));
        assert_eq!(table.entry(1, 0, 0).map(|e| e.hue_shift_degrees), Some(2.0));
        assert_eq!(table.entry(0, 0, 1).map(|e| e.hue_shift_degrees), Some(4.0));
        assert_eq!(table.entry(1, 1, 1).map(|e| e.hue_shift_degrees), Some(7.0));
        assert_eq!(table.entry(2, 0, 0), None);
        assert_eq!(table.entry(0, 2, 0), None);
        assert_eq!(table.entry(0, 0, 2), None);
    }

    #[test]
    fn a_table_contradicting_its_dims_is_not_projected() {
        let mut ifd = empty();
        ifd.set(tags::PROFILE_HUE_SAT_MAP_DIMS, Value::Long(vec![2, 2, 1]));
        // Two entries where the dims call for four.
        ifd.set(tags::PROFILE_HUE_SAT_MAP_DATA1, small_table());
        assert_eq!(ColorProfileInfo::from_ifd(&ifd), None);

        // Divisions outside the spec's ranges are equally unusable.
        let mut short_saturation = empty();
        short_saturation.set(tags::PROFILE_HUE_SAT_MAP_DIMS, Value::Long(vec![1, 1, 1]));
        short_saturation.set(
            tags::PROFILE_HUE_SAT_MAP_DATA1,
            Value::Float(vec![0.0, 1.0, 1.0]),
        );
        assert_eq!(ColorProfileInfo::from_ifd(&short_saturation), None);
    }

    #[test]
    fn table_data_without_dims_is_not_projected() {
        let mut ifd = empty();
        ifd.set(tags::PROFILE_HUE_SAT_MAP_DATA1, small_table());
        assert_eq!(ColorProfileInfo::from_ifd(&ifd), None);
    }

    #[test]
    fn tone_curve_requires_strictly_increasing_inputs() {
        let mut ifd = empty();
        ifd.set(
            tags::PROFILE_TONE_CURVE,
            Value::Float(vec![0.0, 0.0, 0.5, 0.6, 1.0, 1.0]),
        );
        let info = ColorProfileInfo::from_ifd(&ifd).expect("curve present");
        assert_eq!(
            info.tone_curve,
            Some(vec![(0.0, 0.0), (0.5, 0.6), (1.0, 1.0)])
        );

        let mut flat = empty();
        flat.set(
            tags::PROFILE_TONE_CURVE,
            Value::Float(vec![0.0, 0.0, 0.0, 1.0]),
        );
        assert_eq!(ColorProfileInfo::from_ifd(&flat), None);

        let mut odd = empty();
        odd.set(tags::PROFILE_TONE_CURVE, Value::Float(vec![0.0, 0.0, 1.0]));
        assert_eq!(ColorProfileInfo::from_ifd(&odd), None);
    }

    #[test]
    fn baseline_exposure_offset_reads_either_rational_sign() {
        let mut signed = empty();
        signed.set(
            tags::BASELINE_EXPOSURE_OFFSET,
            Value::SRational(vec![(-7, 10)]),
        );
        assert_eq!(
            ColorProfileInfo::from_ifd(&signed).and_then(|i| i.baseline_exposure_offset),
            Some(-0.7)
        );

        let mut unsigned = empty();
        unsigned.set(
            tags::BASELINE_EXPOSURE_OFFSET,
            Value::Rational(vec![(3, 10)]),
        );
        assert_eq!(
            ColorProfileInfo::from_ifd(&unsigned).and_then(|i| i.baseline_exposure_offset),
            Some(0.3)
        );

        let mut plural = empty();
        plural.set(
            tags::BASELINE_EXPOSURE_OFFSET,
            Value::SRational(vec![(1, 1), (2, 1)]),
        );
        assert_eq!(ColorProfileInfo::from_ifd(&plural), None);
    }

    #[test]
    fn noise_profile_projects_per_plane_models() {
        let mut ifd = empty();
        ifd.set(
            tags::NOISE_PROFILE,
            Value::Double(vec![2e-5, 4.5e-7, 3e-5, 5.0e-7, 4e-5, 6.0e-7]),
        );
        let noise = NoiseProfile::from_ifd(&ifd).expect("noise profile");
        assert_eq!(noise.planes.len(), 3);
        assert_eq!(noise.for_plane(1).map(|m| m.scale), Some(3e-5));
        assert_eq!(noise.for_plane(3), None);
        // N(x) = sqrt(S x + O).
        let model = noise.for_plane(0).expect("first plane");
        assert!((model.std_dev(0.18) - f64::sqrt(2e-5 * 0.18 + 4.5e-7)).abs() < 1e-12);
        assert_eq!(NoiseModel::default().std_dev(1.0), 0.0);
    }

    #[test]
    fn a_single_noise_model_applies_to_every_plane() {
        let mut ifd = empty();
        ifd.set(tags::NOISE_PROFILE, Value::Float(vec![2e-5, 0.0]));
        let noise = NoiseProfile::from_ifd(&ifd).expect("noise profile");
        assert_eq!(noise.for_plane(0), noise.for_plane(2));
        assert_eq!(noise.for_plane(7).map(|m| m.offset), Some(0.0));
    }

    #[test]
    fn a_noise_profile_outside_the_models_domain_is_not_projected() {
        for parameters in [
            vec![0.0, 1.0],      // scale must be positive
            vec![1.0, -1.0],     // offset must be non-negative
            vec![1.0, 1.0, 1.0], // count must be even
            vec![f64::NAN, 1.0], // non-finite
        ] {
            let mut ifd = empty();
            ifd.set(tags::NOISE_PROFILE, Value::Double(parameters.clone()));
            assert_eq!(
                NoiseProfile::from_ifd(&ifd),
                None,
                "{parameters:?} is not a usable noise model"
            );
        }
        // A wrongly-typed value is equally unusable.
        let mut wrong_type = empty();
        wrong_type.set(tags::NOISE_PROFILE, Value::Long(vec![1, 2]));
        assert_eq!(NoiseProfile::from_ifd(&wrong_type), None);
    }
}
