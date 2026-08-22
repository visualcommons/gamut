//! LUT-tag pipeline builders — the `lut8Type`/`lut16Type` (`mft1`/`mft2`, ICC.1:2022
//! §10.10/§10.11) and `lutAToBType`/`lutBToAType` (`mAB `/`mBA `, §10.12/§10.13) transform
//! elements as runnable [`Pipeline`]s, shaped after lcms2's `_cmsReadInputLUT` /
//! `_cmsReadOutputLUT` (`cmsio1.c`) and the LUT type readers (`cmstypes.c`).
//!
//! # Domain plan (the one deliberate re-arrangement of lcms2)
//!
//! lcms2 evaluates whole pipelines in **encoded** channel space (`[0, 1]` per channel, the
//! 16-bit tag encodings normalized), bracketing them with formatters. This crate's PCS seams
//! are **decoded** colorimetry (crate convention: XYZ with D50 `Y = 1.0`, Lab with `L*` in
//! `0..=100`). Rather than re-derive every LUT-internal element's domain, the builders keep
//! the **entire tag-internal chain in the tag's native encoded `[0, 1]` domain** — exactly
//! lcms2's arrangement — and convert at the single PCS end: a device→PCS pipeline appends one
//! affine **PCS-decode** stage after the tag's last stage, a PCS→device pipeline prepends the
//! inverse **PCS-encode** stage. Every ICC 16-bit PCS encoding is per-channel affine, so the
//! seam stage is a plain [`Stage::Matrix`] whose constants are transcribed below with their
//! derivations. Device ends stay encoded `[0, 1]` (the crate's device convention — for a Lab
//! or XYZ *device* space this is the tag's native encoding, passed through unchanged).
//!
//! # The v2-Lab rule (`lut16Type` only)
//!
//! A `lut16Type` element uses the **legacy v2 PCSLAB encoding** even inside a v4 profile:
//! lcms2 brackets lut16 pipelines with `_cmsStageAllocLabV4ToV2`/`LabV2ToV4` fixups whenever
//! the PCS is Lab and the tag's true type is `cmsSigLut16Type` (`_cmsReadInputLUT` /
//! `_cmsReadOutputLUT`, `cmsio1.c:304-397/578-651`). In the decoded-PCS design that fixup
//! collapses into the seam stage: a lut16 tag's Lab end decodes/encodes with the **v2**
//! constants, everything else (`lut8` included — the fixup is gated on the lut16 true type
//! alone, and the 8-bit Lab encoding widened by `FROM_8_TO_16` lands on the v4 scaling) with
//! the **v4** constants.
//!
//! # Lab-indexed CLUTs are trilinear
//!
//! lcms2 forces every CLUT of a PCS→device LUT whose PCS is Lab to trilinear interpolation
//! (`ChangeInterpolationToTrilinear`, `cmsio1.c:516-533` — "for 3D LUTS using Lab used as
//! indexer space, trilinear interpolation should be used"). The builders mirror the rule:
//! [`Direction::PcsToDevice`] with a Lab PCS selects
//! [`ClutInterpolation::Multilinear`]; every other CLUT takes [`ClutTable::new`]'s default
//! (tetrahedral from 3 inputs).
//!
//! # Lenient stage combinations
//!
//! §10.12.1/§10.13.1 permit only certain `mAB `/`mBA ` stage combinations (B alone;
//! M+matrix+B; A+CLUT+B; all five). Both `gamut-icc`'s parser and lcms2 accept **any**
//! combination the offsets signal, so these builders do too: absent stages are simply
//! omitted, and a combination whose channel counts cannot chain (e.g. A-curves directly into
//! M-curves on a 4→3 element) fails [`Pipeline::new`]'s seam validation rather than being
//! special-cased here.

use gamut_icc::{
    Clut, ClutPrecision, ColorSpace, Curve, CurveOrParametric, Lut8, Lut16, LutAToB, LutBToA,
    Matrix3x3, Matrix3x4, Signature, TagData,
};

use crate::clut::{ClutInterpolation, ClutTable};
use crate::curve::ToneCurve;
use crate::error::{CmmError, Result};
use crate::pipeline::{Pipeline, Stage};

/// Which way the built pipeline runs, i.e. which end is the PCS seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    /// Device channels in, decoded PCS out: the PCS-decode stage is appended last.
    DeviceToPcs,
    /// Decoded PCS in, device channels out: the PCS-encode stage is prepended first.
    PcsToDevice,
}

/// `MAX_ENCODEABLE_XYZ = 1 + 32767/32768 = 65535/32768` (lcms2 `lcms2_internal.h`): the
/// decoded XYZ value of the all-ones encoded channel. The 16-bit PCSXYZ encoding is
/// u1Fixed15 (`raw = X · 32768`, ICC.1:2022 §6.3.4.2 / lcms2 `cmspcs.c:368-434`), so a
/// normalized channel `v = raw / 65535` decodes as `X = raw / 32768 = v · 65535/32768`.
/// Exact in `f64` (a dyadic rational).
const XYZ_DECODE_SCALE: f64 = 65535.0 / 32768.0;

/// v4 PCSLAB `L*` scale: `raw = L · 655.35` with `655.35 = 65535/100` (ICC.1:2022 §10.12.2 /
/// lcms2 `cmspcs.c` `L2float4`), so `v = raw/65535` decodes as `L = v · 100`.
const LAB4_L_DECODE_SCALE: f64 = 100.0;

/// v4 PCSLAB `a*`/`b*` scale: `a = raw/257 − 128` with `257 = 65535/255` (`ab2float4`), so
/// `a = v · 255 − 128` for `v = raw/65535`.
const LAB4_AB_DECODE_SCALE: f64 = 255.0;

/// Legacy v2 PCSLAB `L*` scale: `L = raw/652.8` with `652.8 = 0xFF00/100` (`L2float2`), so
/// `L = v · 65535/652.8 = v · 100.390625`. Exact in `f64` (`0.390625 = 25/64`).
const LAB2_L_DECODE_SCALE: f64 = 65535.0 / 652.8;

/// Legacy v2 PCSLAB `a*`/`b*` scale: `a = raw/256 − 128` (`ab2float2`), so
/// `a = v · 65535/256 − 128 = v · 255.99609375 − 128`. Exact in `f64`.
const LAB2_AB_DECODE_SCALE: f64 = 65535.0 / 256.0;

/// The decoded value at encoded `a* = b* = 0` in both Lab encodings (`−128.0`).
const LAB_AB_DECODE_OFFSET: f64 = -128.0;

/// The 16-bit PCS encoding at a LUT tag's PCS end: every one is per-channel **affine** over
/// the normalized `[0, 1]` channels, so the decode/encode seam stages are diagonal
/// [`Stage::Matrix`] values built from the constants above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PcsEncoding {
    /// PCSXYZ, u1Fixed15: `X = v · 65535/32768`, no offset.
    Xyz,
    /// PCSLAB, v4 16-bit: `L = v·100`, `a/b = v·255 − 128`.
    LabV4,
    /// PCSLAB, legacy v2 16-bit: `L = v·100.390625`, `a/b = v·255.99609375 − 128`.
    LabV2,
}

impl PcsEncoding {
    /// The per-channel decode scales and offsets, `decoded = v · scale + offset`.
    fn scales_and_offsets(self) -> ([f64; 3], [f64; 3]) {
        match self {
            PcsEncoding::Xyz => ([XYZ_DECODE_SCALE; 3], [0.0; 3]),
            PcsEncoding::LabV4 => (
                [
                    LAB4_L_DECODE_SCALE,
                    LAB4_AB_DECODE_SCALE,
                    LAB4_AB_DECODE_SCALE,
                ],
                [0.0, LAB_AB_DECODE_OFFSET, LAB_AB_DECODE_OFFSET],
            ),
            PcsEncoding::LabV2 => (
                [
                    LAB2_L_DECODE_SCALE,
                    LAB2_AB_DECODE_SCALE,
                    LAB2_AB_DECODE_SCALE,
                ],
                [0.0, LAB_AB_DECODE_OFFSET, LAB_AB_DECODE_OFFSET],
            ),
        }
    }

    /// The PCS-decode seam stage: encoded `[0, 1]` → decoded colorimetry
    /// (`decoded = v · scale + offset`, diagonal).
    fn decode_stage(self) -> Stage {
        let (scales, offsets) = self.scales_and_offsets();
        let mut m = [[0.0; 3]; 3];
        for (i, row) in m.iter_mut().enumerate() {
            row[i] = scales[i];
        }
        Stage::Matrix { m, offset: offsets }
    }

    /// The PCS-encode seam stage, the exact inverse of [`decode_stage`](Self::decode_stage):
    /// `v = (decoded − offset) / scale = decoded/scale − offset/scale`.
    fn encode_stage(self) -> Stage {
        let (scales, offsets) = self.scales_and_offsets();
        let mut m = [[0.0; 3]; 3];
        let mut offset = [0.0; 3];
        for (i, row) in m.iter_mut().enumerate() {
            row[i] = 1.0 / scales[i];
            offset[i] = -offsets[i] / scales[i];
        }
        Stage::Matrix { m, offset }
    }
}

/// The PCS encoding a tag's PCS end uses, or `None` when the header's "PCS" is not a
/// connection space (a devicelink/abstract profile whose output is a device space — the
/// pipeline then stays encoded end to end, as in lcms2's `_cmsReadDevicelinkLUT`).
/// `legacy_lab` selects the v2 Lab scaling — set only for `lut16Type` (module docs).
fn pcs_encoding(pcs: ColorSpace, legacy_lab: bool) -> Option<PcsEncoding> {
    match pcs {
        ColorSpace::Xyz => Some(PcsEncoding::Xyz),
        ColorSpace::Lab if legacy_lab => Some(PcsEncoding::LabV2),
        ColorSpace::Lab => Some(PcsEncoding::LabV4),
        _ => None,
    }
}

/// Whether the direction/PCS pair forces multilinear (trilinear) CLUT interpolation —
/// lcms2's Lab-indexed-CLUT rule (module docs).
fn lab_indexed(direction: Direction, pcs: ColorSpace) -> bool {
    direction == Direction::PcsToDevice && pcs == ColorSpace::Lab
}

/// Builds the pipeline for one LUT tag: dispatches on the element type actually stored under
/// the tag (an A2B or B2A slot may legally hold any of the four LUT element types — lcms2
/// registers all four readers for both tag families and evaluates whatever pipeline results).
/// The element type fixes the tag-internal stage order; `direction` fixes which end is the
/// PCS seam and whether the Lab-indexed trilinear rule applies.
///
/// # Errors
///
/// [`CmmError::BadTagType`] if the tag holds a non-LUT element (or a `lut16Type` with a
/// zero-entry table); [`ToneCurve::new`]/[`ClutTable`] construction errors for malformed
/// curves or CLUT geometry; [`Pipeline::new`] channel-seam errors for a stage combination
/// whose channel counts cannot chain.
pub(super) fn build(
    sig: Signature,
    data: &TagData,
    direction: Direction,
    pcs: ColorSpace,
) -> Result<Pipeline> {
    match data {
        TagData::Lut8(lut) => lut8_pipeline(lut, direction, pcs),
        TagData::Lut16(lut) => lut16_pipeline(sig, lut, direction, pcs),
        TagData::LutAToB(lut) => lut_a_to_b_pipeline(lut, direction, pcs),
        TagData::LutBToA(lut) => lut_b_to_a_pipeline(lut, direction, pcs),
        _ => Err(CmmError::BadTagType(sig)),
    }
}

/// Attaches the PCS seam stage per `direction` (append decode / prepend encode; none when
/// the "PCS" is a device space) and validates the finished chain.
fn finish(
    direction: Direction,
    encoding: Option<PcsEncoding>,
    input_channels: u8,
    output_channels: u8,
    mut stages: Vec<Stage>,
) -> Result<Pipeline> {
    if let Some(encoding) = encoding {
        match direction {
            Direction::DeviceToPcs => stages.push(encoding.decode_stage()),
            Direction::PcsToDevice => stages.insert(0, encoding.encode_stage()),
        }
    }
    Pipeline::new(input_channels, output_channels, stages)
}

/// lcms2's `_cmsMAT3isIdentity` tolerance (`CloseEnough`, `cmsmtrx.c`): entries within
/// `1/65535` of the identity — one quantum looser than the s15Fixed16 resolution, so a
/// quantized identity always passes.
const IDENTITY_TOLERANCE: f64 = 1.0 / 65535.0;

/// The embedded `lut8`/`lut16` 3×3 matrix as a stage, or `None` when it must not apply.
///
/// lcms2 applies the matrix whenever `InputChannels == 3` **and** it is not the identity
/// (`Type_LUT8_Read`/`Type_LUT16_Read`, `cmstypes.c:2042-2048/2345-2351`) — deliberately
/// **not** gated on an XYZ PCS, although ICC.1:2022 §10.10/§10.11 say the matrix "shall be"
/// the identity unless the input is PCSXYZ. This crate follows lcms2's behaviour over the
/// strict spec reading (the divergence the phase documents): whatever non-identity matrix a
/// 3-input tag carries is applied, first, in the tag's encoded domain.
fn legacy_matrix_stage(matrix: &Matrix3x3, input_channels: u8) -> Option<Stage> {
    if input_channels != 3 {
        return None;
    }
    let mut m = [[0.0; 3]; 3];
    let mut is_identity = true;
    for (i, row) in m.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = matrix.elements[i * 3 + j].to_f64();
            let id = if i == j { 1.0 } else { 0.0 };
            if (*cell - id).abs() >= IDENTITY_TOLERANCE {
                is_identity = false;
            }
        }
    }
    (!is_identity).then_some(Stage::Matrix {
        m,
        offset: [0.0; 3],
    })
}

/// The `mAB `/`mBA ` matrix element as a stage: the twelve `e1..e12` s15Fixed16 parameters as
/// a row-major 3×3 plus offsets (`Matrix3x4`, ICC.1:2022 §10.12.5), operating — offsets
/// included — in the tag's encoded `[0, 1]` domain (as in lcms2, whose whole pipeline is
/// encoded; the module's domain plan keeps that placement).
fn mab_matrix_stage(matrix: &Matrix3x4) -> Stage {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = matrix.matrix[i * 3 + j].to_f64();
        }
    }
    let mut offset = [0.0; 3];
    for (off, raw) in offset.iter_mut().zip(&matrix.offset) {
        *off = raw.to_f64();
    }
    Stage::Matrix { m, offset }
}

/// A curve set (`mAB `/`mBA ` A/M/B curves) as a [`Stage::Curves`].
fn curves_stage(curves: &[CurveOrParametric]) -> Result<Stage> {
    Ok(Stage::Curves(
        curves.iter().map(ToneCurve::new).collect::<Result<_>>()?,
    ))
}

/// `lut8` input/output tables as per-channel sampled curves: each 256-entry `u8` table widens
/// to `u16` as `v · 257` — lcms2's `FROM_8_TO_16` (`255 → 65535`, preserving full scale) —
/// and becomes a [`Curve::Sampled`] evaluated over `raw / 65535`.
fn lut8_tables_stage(table: &[u8]) -> Result<Stage> {
    let curves: Vec<ToneCurve> = table
        .as_chunks::<256>()
        .0
        .iter()
        .map(|chunk| {
            let widened: Vec<u16> = chunk.iter().map(|&v| u16::from(v) * 257).collect();
            ToneCurve::new(&CurveOrParametric::Curve(Curve::Sampled(widened)))
        })
        .collect::<Result<_>>()?;
    Ok(Stage::Curves(curves))
}

/// `lut16` input/output tables as per-channel sampled curves over their native `u16` samples.
///
/// # Errors
///
/// [`CmmError::BadTagType`] for a zero entry count (only reachable from hand-built values —
/// the ICC encoding requires at least 2 entries and lcms2 likewise refuses 0).
fn lut16_tables_stage(sig: Signature, table: &[u16], entries: u16) -> Result<Stage> {
    if entries == 0 {
        return Err(CmmError::BadTagType(sig));
    }
    let curves: Vec<ToneCurve> = table
        .chunks_exact(usize::from(entries))
        .map(|chunk| ToneCurve::new(&CurveOrParametric::Curve(Curve::Sampled(chunk.to_vec()))))
        .collect::<Result<_>>()?;
    Ok(Stage::Curves(curves))
}

/// A CLUT as a stage, honouring the Lab-indexed trilinear rule.
fn clut_stage(clut: &Clut, multilinear: bool) -> Result<Stage> {
    let table = if multilinear {
        ClutTable::with_interpolation(clut, ClutInterpolation::Multilinear)?
    } else {
        ClutTable::new(clut)?
    };
    Ok(Stage::Clut(table))
}

/// `lut8Type`: matrix (3-input, non-identity only) → input tables → CLUT → output tables
/// (§10.11 evaluation order), all 8-bit data. The CLUT keeps `ClutPrecision::U8` (samples
/// widened to `u16` without rescaling, normalized by 255 inside [`ClutTable`] — numerically
/// identical to lcms2's `FROM_8_TO_16` widening over 65535). A Lab PCS uses the **v4**
/// encoding (the v2 fixup is lut16-only; module docs).
fn lut8_pipeline(lut: &Lut8, direction: Direction, pcs: ColorSpace) -> Result<Pipeline> {
    let mut stages = Vec::new();
    if let Some(stage) = legacy_matrix_stage(&lut.matrix, lut.input_channels) {
        stages.push(stage);
    }
    stages.push(lut8_tables_stage(&lut.input_table)?);
    let clut = Clut {
        grid_points: vec![lut.grid_points; usize::from(lut.input_channels)],
        output_channels: lut.output_channels,
        precision: ClutPrecision::U8,
        samples: lut.clut.iter().map(|&v| u16::from(v)).collect(),
    };
    stages.push(clut_stage(&clut, lab_indexed(direction, pcs))?);
    stages.push(lut8_tables_stage(&lut.output_table)?);
    finish(
        direction,
        pcs_encoding(pcs, false),
        lut.input_channels,
        lut.output_channels,
        stages,
    )
}

/// `lut16Type`: matrix (3-input, non-identity only) → input tables → CLUT → output tables
/// (§10.10), 16-bit data with per-table entry counts. A Lab PCS uses the **v2** encoding
/// (module docs).
fn lut16_pipeline(
    sig: Signature,
    lut: &Lut16,
    direction: Direction,
    pcs: ColorSpace,
) -> Result<Pipeline> {
    let mut stages = Vec::new();
    if let Some(stage) = legacy_matrix_stage(&lut.matrix, lut.input_channels) {
        stages.push(stage);
    }
    stages.push(lut16_tables_stage(
        sig,
        &lut.input_table,
        lut.input_table_entries,
    )?);
    let clut = Clut {
        grid_points: vec![lut.grid_points; usize::from(lut.input_channels)],
        output_channels: lut.output_channels,
        precision: ClutPrecision::U16,
        samples: lut.clut.clone(),
    };
    stages.push(clut_stage(&clut, lab_indexed(direction, pcs))?);
    stages.push(lut16_tables_stage(
        sig,
        &lut.output_table,
        lut.output_table_entries,
    )?);
    finish(
        direction,
        pcs_encoding(pcs, true),
        lut.input_channels,
        lut.output_channels,
        stages,
    )
}

/// `lutAToBType`: A-curves → CLUT → M-curves → matrix → B-curves (the §10.12 evaluation
/// order; the element *stores* B first but B applies last on the way to the PCS). Optional
/// stages are omitted; a Lab PCS uses the v4 encoding.
fn lut_a_to_b_pipeline(lut: &LutAToB, direction: Direction, pcs: ColorSpace) -> Result<Pipeline> {
    let mut stages = Vec::new();
    if let Some(a_curves) = &lut.a_curves {
        stages.push(curves_stage(a_curves)?);
    }
    if let Some(clut) = &lut.clut {
        stages.push(clut_stage(clut, lab_indexed(direction, pcs))?);
    }
    if let Some(m_curves) = &lut.m_curves {
        stages.push(curves_stage(m_curves)?);
    }
    if let Some(matrix) = &lut.matrix {
        stages.push(mab_matrix_stage(matrix));
    }
    stages.push(curves_stage(&lut.b_curves)?);
    finish(
        direction,
        pcs_encoding(pcs, false),
        lut.input_channels,
        lut.output_channels,
        stages,
    )
}

/// `lutBToAType`: B-curves → matrix → M-curves → CLUT → A-curves (the §10.13 evaluation
/// order, PCS side first). Optional stages are omitted; a Lab PCS uses the v4 encoding.
fn lut_b_to_a_pipeline(lut: &LutBToA, direction: Direction, pcs: ColorSpace) -> Result<Pipeline> {
    let mut stages = Vec::new();
    stages.push(curves_stage(&lut.b_curves)?);
    if let Some(matrix) = &lut.matrix {
        stages.push(mab_matrix_stage(matrix));
    }
    if let Some(m_curves) = &lut.m_curves {
        stages.push(curves_stage(m_curves)?);
    }
    if let Some(clut) = &lut.clut {
        stages.push(clut_stage(clut, lab_indexed(direction, pcs))?);
    }
    if let Some(a_curves) = &lut.a_curves {
        stages.push(curves_stage(a_curves)?);
    }
    finish(
        direction,
        pcs_encoding(pcs, false),
        lut.input_channels,
        lut.output_channels,
        stages,
    )
}

#[cfg(test)]
mod tests {
    use gamut_icc::S15Fixed16;

    use super::*;

    /// s15Fixed16 1.0 / 0.0, for hand-built matrices.
    const ONE: S15Fixed16 = S15Fixed16(0x0001_0000);
    const ZERO: S15Fixed16 = S15Fixed16(0);

    fn identity3x3() -> Matrix3x3 {
        let mut elements = [ZERO; 9];
        for i in 0..3 {
            elements[i * 4] = ONE;
        }
        Matrix3x3 { elements }
    }

    /// A visibly non-identity matrix (diagonal 0.5, exact in s15Fixed16).
    fn half_diag3x3() -> Matrix3x3 {
        let mut elements = [ZERO; 9];
        for i in 0..3 {
            elements[i * 4] = S15Fixed16(0x8000);
        }
        Matrix3x3 { elements }
    }

    #[test]
    fn legacy_matrix_layout_is_row_major_and_tolerance_is_lcms2s() {
        // Asymmetric matrix: only element e2 (row 0, column 1) is 0.5 — a transposed
        // assembly would land it at [1][0] instead.
        let mut asymmetric = identity3x3();
        asymmetric.elements[1] = S15Fixed16(0x8000);
        let Some(Stage::Matrix { m, offset }) = legacy_matrix_stage(&asymmetric, 3) else {
            panic!("a non-identity matrix must produce a stage");
        };
        assert_eq!(m[0][1], 0.5, "row-major e2");
        assert_eq!(m[1][0], 0.0, "transposed position stays 0");
        assert_eq!(offset, [0.0; 3]);
        // Identity tolerance is lcms2's CloseEnough (1/65535): one s15Fixed16 quantum off
        // the identity (1/65536 < 1/65535) still counts as identity and is skipped...
        let mut near = identity3x3();
        near.elements[0] = S15Fixed16(0x0001_0001);
        assert!(legacy_matrix_stage(&near, 3).is_none(), "1/65536 within");
        // ...while two quanta (2/65536 > 1/65535) do not.
        let mut off_identity = identity3x3();
        off_identity.elements[0] = S15Fixed16(0x0001_0002);
        assert!(
            legacy_matrix_stage(&off_identity, 3).is_some(),
            "2/65536 outside"
        );
    }

    /// Per-channel identity ramp tables for a `lut8` (each channel `v → v`).
    fn ramp_u8(channels: u8) -> Vec<u8> {
        let mut table = Vec::with_capacity(usize::from(channels) * 256);
        for _ in 0..channels {
            table.extend(0..=255u8);
        }
        table
    }

    /// Per-channel 2-entry identity tables for a `lut16`.
    fn identity_u16(channels: u8) -> Vec<u16> {
        let mut table = Vec::new();
        for _ in 0..channels {
            table.extend([0, 65535]);
        }
        table
    }

    fn lut8_3x3(matrix: Matrix3x3) -> Lut8 {
        Lut8 {
            input_channels: 3,
            output_channels: 3,
            grid_points: 2,
            matrix,
            input_table: ramp_u8(3),
            clut: (0..24u8).map(|v| v * 10).collect(),
            output_table: ramp_u8(3),
        }
    }

    fn lut16_3x3(matrix: Matrix3x3) -> Lut16 {
        Lut16 {
            input_channels: 3,
            output_channels: 3,
            grid_points: 2,
            matrix,
            input_table_entries: 2,
            output_table_entries: 2,
            input_table: identity_u16(3),
            clut: (0..24u16).map(|v| v * 2500).collect(),
            output_table: identity_u16(3),
        }
    }

    /// The stage-kind fingerprint of a pipeline, for order assertions.
    fn kinds(pipeline: &Pipeline) -> Vec<&'static str> {
        pipeline
            .stages()
            .iter()
            .map(|stage| match stage {
                Stage::Curves(_) => "curves",
                Stage::Clut(_) => "clut",
                Stage::Matrix { .. } => "matrix",
                _ => "other",
            })
            .collect()
    }

    /// The diagonal and offset of a [`Stage::Matrix`], asserting the off-diagonals are zero.
    fn diagonal_of(stage: &Stage) -> ([f64; 3], [f64; 3]) {
        let Stage::Matrix { m, offset } = stage else {
            panic!("expected a Matrix stage, got {stage:?}");
        };
        for (i, row) in m.iter().enumerate() {
            for (j, cell) in row.iter().enumerate() {
                if i != j {
                    assert_eq!(*cell, 0.0, "off-diagonal [{i}][{j}]");
                }
            }
        }
        ([m[0][0], m[1][1], m[2][2]], *offset)
    }

    #[test]
    fn lut8_device_to_pcs_stage_order_with_embedded_matrix() {
        let pipeline = lut8_pipeline(
            &lut8_3x3(half_diag3x3()),
            Direction::DeviceToPcs,
            ColorSpace::Xyz,
        )
        .unwrap();
        // Matrix first (§10.11 evaluation order), then tables → CLUT → tables, then the
        // appended PCS decode.
        assert_eq!(
            kinds(&pipeline),
            ["matrix", "curves", "clut", "curves", "matrix"]
        );
        let (diag, _) = diagonal_of(&pipeline.stages()[0]);
        assert_eq!(diag, [0.5, 0.5, 0.5], "embedded matrix applied as tagged");
        // XYZ decode scale is exactly 65535/32768 = MAX_ENCODEABLE_XYZ, offset-free.
        let (diag, offset) = diagonal_of(&pipeline.stages()[4]);
        assert_eq!(diag, [65535.0 / 32768.0; 3]);
        assert_eq!(offset, [0.0; 3]);
    }

    #[test]
    fn lut8_identity_matrix_is_skipped() {
        let pipeline = lut8_pipeline(
            &lut8_3x3(identity3x3()),
            Direction::DeviceToPcs,
            ColorSpace::Xyz,
        )
        .unwrap();
        assert_eq!(kinds(&pipeline), ["curves", "clut", "curves", "matrix"]);
    }

    #[test]
    fn lut8_matrix_is_skipped_for_non_3_input_tags() {
        // 4 input channels: lcms2 applies the matrix only when InputChannels == 3, even if
        // the tag carries a non-identity one.
        let lut = Lut8 {
            input_channels: 4,
            output_channels: 3,
            grid_points: 2,
            matrix: half_diag3x3(),
            input_table: ramp_u8(4),
            clut: vec![0; 16 * 3],
            output_table: ramp_u8(3),
        };
        let pipeline = lut8_pipeline(&lut, Direction::DeviceToPcs, ColorSpace::Lab).unwrap();
        assert_eq!(kinds(&pipeline), ["curves", "clut", "curves", "matrix"]);
        assert_eq!(pipeline.input_channels(), 4);
    }

    #[test]
    fn lut8_lab_pcs_uses_v4_constants() {
        // The v2 fixup is gated on the lut16 true type alone: lut8's Lab seam is v4.
        let pipeline = lut8_pipeline(
            &lut8_3x3(identity3x3()),
            Direction::DeviceToPcs,
            ColorSpace::Lab,
        )
        .unwrap();
        let (diag, offset) = diagonal_of(pipeline.stages().last().unwrap());
        assert_eq!(diag, [100.0, 255.0, 255.0]);
        assert_eq!(offset, [0.0, -128.0, -128.0]);
    }

    #[test]
    fn lut16_lab_pcs_uses_v2_constants() {
        // THE v2 rule: a lut16 tag's Lab seam decodes with the legacy scaling
        // 65535/652.8 = 100.390625 and 65535/256 = 255.99609375 (both exact in f64).
        let pipeline = lut16_pipeline(
            Signature(*b"A2B0"),
            &lut16_3x3(identity3x3()),
            Direction::DeviceToPcs,
            ColorSpace::Lab,
        )
        .unwrap();
        let (diag, offset) = diagonal_of(pipeline.stages().last().unwrap());
        assert_eq!(diag, [100.390625, 255.99609375, 255.99609375]);
        assert_eq!(offset, [0.0, -128.0, -128.0]);
    }

    #[test]
    fn lut16_v2_lab_end_to_end_decodes_the_classic_white() {
        // CLUT corner 0xFF00 (the classic v2 Lab white raw) must decode to L* = 100 exactly:
        // (65280/65535) · (65535/652.8) = 65280/652.8 = 100.
        let mut lut = lut16_3x3(identity3x3());
        // Node (1,1,1) is the last node; L is its first output channel.
        lut.clut[7 * 3] = 0xFF00;
        let pipeline = lut16_pipeline(
            Signature(*b"A2B0"),
            &lut,
            Direction::DeviceToPcs,
            ColorSpace::Lab,
        )
        .unwrap();
        let mut out = [0.0; 3];
        pipeline.eval(&[1.0, 1.0, 1.0], &mut out).unwrap();
        assert!((out[0] - 100.0).abs() < 1e-12, "L* = {}", out[0]);
        // The same raw through the v4 seam (an mAB-style decode) would read 99.6109…, so the
        // pin above dies if the v2/v4 selection flips.
    }

    #[test]
    fn lab_pcs_to_device_encode_is_the_exact_inverse() {
        // v2 encode stage: v = L/100.390625, v = (a + 128)/255.99609375 — spelled as
        // 1/scale and 128/scale.
        let pipeline = lut16_pipeline(
            Signature(*b"B2A0"),
            &lut16_3x3(identity3x3()),
            Direction::PcsToDevice,
            ColorSpace::Lab,
        )
        .unwrap();
        let (diag, offset) = diagonal_of(&pipeline.stages()[0]);
        assert_eq!(
            diag,
            [1.0 / 100.390625, 1.0 / 255.99609375, 1.0 / 255.99609375]
        );
        assert_eq!(offset, [0.0, 128.0 / 255.99609375, 128.0 / 255.99609375]);
        // Encode really is decode's inverse: decoded white round-trips through both stages.
        let decode = PcsEncoding::LabV2.decode_stage();
        let mut encoded = [0.0; 3];
        pipeline.stages()[0].eval(&[100.0, 20.0, -30.0], &mut encoded);
        let mut back = [0.0; 3];
        decode.eval(&encoded, &mut back);
        for (got, want) in back.iter().zip([100.0, 20.0, -30.0]) {
            assert!((got - want).abs() < 1e-12, "{back:?}");
        }
    }

    #[test]
    fn lab_indexed_cluts_are_multilinear_only_in_the_pcs_to_device_direction() {
        // PCS→device with a Lab PCS forces trilinear (lcms2's
        // ChangeInterpolationToTrilinear); the same element in the device→PCS direction (and
        // any XYZ-PCS CLUT) keeps the tetrahedral default.
        let cases = [
            (
                Direction::PcsToDevice,
                ColorSpace::Lab,
                ClutInterpolation::Multilinear,
            ),
            (
                Direction::DeviceToPcs,
                ColorSpace::Lab,
                ClutInterpolation::Tetrahedral,
            ),
            (
                Direction::PcsToDevice,
                ColorSpace::Xyz,
                ClutInterpolation::Tetrahedral,
            ),
        ];
        for (direction, pcs, want) in cases {
            let pipeline = lut16_pipeline(
                Signature(*b"B2A0"),
                &lut16_3x3(identity3x3()),
                direction,
                pcs,
            )
            .unwrap();
            let clut = pipeline
                .stages()
                .iter()
                .find_map(|stage| match stage {
                    Stage::Clut(table) => Some(table),
                    _ => None,
                })
                .expect("pipeline carries a CLUT");
            assert_eq!(clut.interpolation(), want, "{direction:?} {pcs:?}");
        }
    }

    fn gamma2() -> CurveOrParametric {
        CurveOrParametric::Curve(Curve::Gamma(gamut_icc::U8Fixed8(0x0200)))
    }

    fn full_mab() -> LutAToB {
        LutAToB {
            input_channels: 3,
            output_channels: 3,
            a_curves: Some(vec![gamma2(); 3]),
            clut: Some(Clut {
                grid_points: vec![2; 3],
                output_channels: 3,
                precision: ClutPrecision::U16,
                samples: (0..24u16).map(|v| v * 2500).collect(),
            }),
            m_curves: Some(vec![gamma2(); 3]),
            matrix: Some(Matrix3x4 {
                // Asymmetric linear part: e2 (row 0, col 1) = 0.25, its transposed
                // position 0 — pins the row-major layout.
                matrix: [
                    ONE,
                    S15Fixed16(0x4000),
                    ZERO,
                    ZERO,
                    ONE,
                    ZERO,
                    ZERO,
                    ZERO,
                    ONE,
                ],
                offset: [S15Fixed16(0x8000), ZERO, S15Fixed16(-0x4000)],
            }),
            b_curves: vec![gamma2(); 3],
        }
    }

    #[test]
    fn mab_device_to_pcs_runs_a_clut_m_matrix_b_then_decodes() {
        let pipeline =
            lut_a_to_b_pipeline(&full_mab(), Direction::DeviceToPcs, ColorSpace::Xyz).unwrap();
        // §10.12 evaluation order (B-curves stored first but applied last), decode appended.
        assert_eq!(
            kinds(&pipeline),
            ["curves", "clut", "curves", "matrix", "curves", "matrix"]
        );
        // The mAB matrix element carries its s15Fixed16 parameters verbatim, in the encoded
        // domain: row-major e1..e9 (the asymmetric e2 = 0.25 lands at [0][1], not [1][0])
        // and offsets e10..e12 (0x8000/65536 = 0.5, −0x4000/65536 = −0.25).
        let Stage::Matrix { m, offset } = &pipeline.stages()[3] else {
            panic!("stage 3 must be the mAB matrix");
        };
        assert_eq!(m[0][1], 0.25, "row-major e2");
        assert_eq!(m[1][0], 0.0, "transposed position stays 0");
        assert_eq!(*offset, [0.5, 0.0, -0.25]);
    }

    #[test]
    fn mab_optional_stages_are_omitted() {
        let minimal = LutAToB {
            input_channels: 3,
            output_channels: 3,
            a_curves: None,
            clut: None,
            m_curves: None,
            matrix: None,
            b_curves: vec![CurveOrParametric::Curve(Curve::Identity); 3],
        };
        let pipeline =
            lut_a_to_b_pipeline(&minimal, Direction::DeviceToPcs, ColorSpace::Lab).unwrap();
        assert_eq!(kinds(&pipeline), ["curves", "matrix"]);
        // And the mAB Lab seam is v4.
        let (diag, _) = diagonal_of(&pipeline.stages()[1]);
        assert_eq!(diag, [100.0, 255.0, 255.0]);
    }

    #[test]
    fn mba_pcs_to_device_encodes_then_runs_b_matrix_m_clut_a() {
        let full = LutBToA {
            input_channels: 3,
            output_channels: 4,
            b_curves: vec![gamma2(); 3],
            matrix: Some(Matrix3x4 {
                matrix: [ONE, ZERO, ZERO, ZERO, ONE, ZERO, ZERO, ZERO, ONE],
                offset: [ZERO; 3],
            }),
            m_curves: Some(vec![gamma2(); 3]),
            clut: Some(Clut {
                grid_points: vec![2; 3],
                output_channels: 4,
                precision: ClutPrecision::U8,
                samples: vec![128; 32],
            }),
            a_curves: Some(vec![gamma2(); 4]),
        };
        let pipeline = lut_b_to_a_pipeline(&full, Direction::PcsToDevice, ColorSpace::Lab).unwrap();
        // Encode prepended, then the §10.13 order: B → matrix → M → CLUT → A.
        assert_eq!(
            kinds(&pipeline),
            ["matrix", "curves", "matrix", "curves", "clut", "curves"]
        );
        assert_eq!(pipeline.input_channels(), 3);
        assert_eq!(pipeline.output_channels(), 4);
        // Lab-indexed CLUT → multilinear.
        let Stage::Clut(table) = &pipeline.stages()[4] else {
            panic!("stage 4 must be the CLUT");
        };
        assert_eq!(table.interpolation(), ClutInterpolation::Multilinear);
    }

    #[test]
    fn zero_entry_lut16_tables_are_a_bad_tag_type() {
        let mut lut = lut16_3x3(identity3x3());
        lut.input_table_entries = 0;
        lut.input_table = Vec::new();
        let err = lut16_pipeline(
            Signature(*b"A2B0"),
            &lut,
            Direction::DeviceToPcs,
            ColorSpace::Lab,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "cmm: tag A2B0 holds an unusable element type"
        );
    }

    #[test]
    fn unchainable_stage_combination_fails_seam_validation() {
        // A 4→3 mAB with A-curves but no CLUT: the 4-channel A-curves cannot feed the
        // 3-channel B-curves — rejected by Pipeline::new, not special-cased.
        let bad = LutAToB {
            input_channels: 4,
            output_channels: 3,
            a_curves: Some(vec![gamma2(); 4]),
            clut: None,
            m_curves: None,
            matrix: None,
            b_curves: vec![gamma2(); 3],
        };
        let err = lut_a_to_b_pipeline(&bad, Direction::DeviceToPcs, ColorSpace::Lab).unwrap_err();
        assert!(
            matches!(err, CmmError::StageChannelMismatch { .. }),
            "got {err}"
        );
    }

    #[test]
    fn lut8_widening_preserves_full_scale() {
        // Input tables widen u8 → u16 as v·257 (FROM_8_TO_16): a ramp table stays the
        // identity to 8-bit resolution, and full-scale 255 maps to exactly 1.0. The CLUT
        // keeps 8-bit precision and normalizes by 255, so a 255 sample is exactly 1.0 too:
        // an all-255 lut8 emits 1.0 exactly at every corner.
        let lut = Lut8 {
            input_channels: 3,
            output_channels: 3,
            grid_points: 2,
            matrix: identity3x3(),
            input_table: ramp_u8(3),
            clut: vec![255; 24],
            output_table: ramp_u8(3),
        };
        let pipeline = lut8_pipeline(&lut, Direction::DeviceToPcs, ColorSpace::Lab).unwrap();
        let mut out = [0.0; 3];
        pipeline.eval(&[1.0, 1.0, 1.0], &mut out).unwrap();
        // v4 Lab decode of the all-ones encoded pixel: L = 100, a = b = 127.
        assert_eq!(out, [100.0, 127.0, 127.0]);
    }

    #[test]
    fn build_dispatches_on_element_type_not_tag_slot() {
        // An mAB element under a B2A signature still evaluates in its own A→…→B order, with
        // the PCS seam decided by the direction: PCS→device prepends the encode.
        let pipeline = build(
            Signature(*b"B2A0"),
            &TagData::LutAToB(full_mab()),
            Direction::PcsToDevice,
            ColorSpace::Xyz,
        )
        .unwrap();
        assert_eq!(
            kinds(&pipeline),
            ["matrix", "curves", "clut", "curves", "matrix", "curves"]
        );
        // The prepended XYZ encode is the reciprocal scale.
        let (diag, offset) = diagonal_of(&pipeline.stages()[0]);
        assert_eq!(diag, [32768.0 / 65535.0; 3]);
        assert_eq!(offset, [0.0; 3]);
    }

    #[test]
    fn build_accepts_all_four_lut_element_types() {
        // Every LUT element family must route through the dispatcher itself (not only the
        // per-type builders the other tests call directly) — a dropped dispatch arm would
        // misreport a valid element as BadTagType.
        let cases: [TagData; 4] = [
            TagData::Lut8(lut8_3x3(identity3x3())),
            TagData::Lut16(lut16_3x3(identity3x3())),
            TagData::LutAToB(full_mab()),
            TagData::LutBToA(LutBToA {
                input_channels: 3,
                output_channels: 3,
                b_curves: vec![gamma2(); 3],
                matrix: None,
                m_curves: None,
                clut: None,
                a_curves: None,
            }),
        ];
        for data in cases {
            let pipeline = build(
                Signature(*b"A2B0"),
                &data,
                Direction::DeviceToPcs,
                ColorSpace::Xyz,
            )
            .unwrap();
            // Whatever the internals, the device→PCS build ends at the XYZ decode seam.
            let (diag, _) = diagonal_of(pipeline.stages().last().unwrap());
            assert_eq!(diag, [65535.0 / 32768.0; 3], "{data:?}");
        }
    }
}
