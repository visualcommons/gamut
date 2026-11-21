//! The multi-dimensional LUT transform element types (the crate's keystone): the legacy
//! `lut8Type`/`lut16Type` and the v4 `lutAToBType`/`lutBToAType` (ICC.1:2022 §10.10–10.13).
//!
//! These carry the matrix → curves → CLUT → curves pipeline that maps a device colour space to and
//! from the PCS. gamut-icc decodes their structure faithfully (it does not itself apply the
//! transform); raw lookup samples are preserved as integers so the elements round-trip exactly.
//!
//! One deliberate leniency: §10.12.1/§10.13.1 permit only certain stage *combinations* in an
//! `mAB `/`mBA ` element (B alone; M+matrix+B; A+CLUT+B; all five). gamut-icc accepts and re-emits
//! *any* combination a profile signals through its stage offsets (only the B-curves are required),
//! so real-world profiles that bend this rule still round-trip losslessly.

use crate::bytes::{ByteReader, pad_to_4, push_s15fixed16};
use crate::curve::{CurveOrParametric, read_curve_element, write_curve_element};
use crate::error::{IccError, Result};
use crate::primitives::S15Fixed16;

/// The sample precision of a CLUT.
///
/// `#[repr(u8)]` with permanent discriminants, so the value crosses a C boundary unchanged. Each
/// discriminant is the bytes-per-sample value the `lutAToBType`/`lutBToAType` CLUT header's
/// precision field stores for that width (ICC.1:2022 §10.12.3).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClutPrecision {
    /// 8-bit samples (full scale 255).
    U8 = 1,
    /// 16-bit samples (full scale 65535).
    U16 = 2,
}

impl ClutPrecision {
    /// The full-scale value the samples are normalized against (255 or 65535).
    #[must_use]
    pub fn full_scale(self) -> u16 {
        match self {
            ClutPrecision::U8 => 255,
            ClutPrecision::U16 => 65535,
        }
    }
}

/// A colour lookup table: a regular grid of output samples indexed by the input channels
/// (`lutAToBType`/`lutBToAType` CLUT, ICC.1:2022 §10.12.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clut {
    /// Grid points per input dimension (`len` == the transform's input-channel count).
    pub grid_points: Vec<u8>,
    /// Output channels produced per grid node.
    pub output_channels: u8,
    /// On-disk sample precision (re-emitted at the same width).
    pub precision: ClutPrecision,
    /// Output samples in grid order (last input channel varying fastest, then output channel),
    /// each a raw value in `0..=precision.full_scale()`.
    pub samples: Vec<u16>,
}

/// A row-major 3×3 matrix of `s15Fixed16` values (`lut8Type`/`lut16Type` matrix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Matrix3x3 {
    /// The nine matrix elements, row-major.
    pub elements: [S15Fixed16; 9],
}

/// The `lutAToBType`/`lutBToAType` matrix stage (ICC.1:2022 §10.12.5): the augmented 3×4 affine
/// transform `[A | b]` the spec stores as twelve `s15Fixed16` parameters `e1..e12`.
///
/// `e1..e9` are the row-major 3×3 linear part ([`matrix`](Self::matrix)) and `e10..e12` the
/// per-channel offsets ([`offset`](Self::offset)); the stage computes
/// `out_i = Σ_j A[i][j]·in_j + b_i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Matrix3x4 {
    /// The nine linear-part elements `e1..e9`, row-major.
    pub matrix: [S15Fixed16; 9],
    /// The three offsets `e10..e12`, one per output channel.
    pub offset: [S15Fixed16; 3],
}

/// A `lut8Type` element (`mft1`, ICC.1:2022 §10.11): matrix → input tables → CLUT → output tables,
/// with 8-bit tables and CLUT and a uniform grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lut8 {
    /// Input channel count.
    pub input_channels: u8,
    /// Output channel count.
    pub output_channels: u8,
    /// Grid points per input dimension (uniform across dimensions).
    pub grid_points: u8,
    /// The 3×3 matrix (applies only when the input is PCS XYZ; otherwise the identity).
    pub matrix: Matrix3x3,
    /// Input tables, concatenated: `input_channels` tables of 256 entries each.
    pub input_table: Vec<u8>,
    /// CLUT samples: `grid_points^input_channels * output_channels` entries.
    pub clut: Vec<u8>,
    /// Output tables, concatenated: `output_channels` tables of 256 entries each.
    pub output_table: Vec<u8>,
}

/// A `lut16Type` element (`mft2`, ICC.1:2022 §10.10): like [`Lut8`] but with 16-bit tables and CLUT
/// and a variable per-table entry count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lut16 {
    /// Input channel count.
    pub input_channels: u8,
    /// Output channel count.
    pub output_channels: u8,
    /// Grid points per input dimension (uniform across dimensions).
    pub grid_points: u8,
    /// The 3×3 matrix (applies only when the input is PCS XYZ; otherwise the identity).
    pub matrix: Matrix3x3,
    /// Entries per input table.
    pub input_table_entries: u16,
    /// Entries per output table.
    pub output_table_entries: u16,
    /// Input tables, concatenated: `input_channels * input_table_entries` entries.
    pub input_table: Vec<u16>,
    /// CLUT samples: `grid_points^input_channels * output_channels` entries.
    pub clut: Vec<u16>,
    /// Output tables, concatenated: `output_channels * output_table_entries` entries.
    pub output_table: Vec<u16>,
}

/// A `lutAToBType` element (`mAB `, ICC.1:2022 §10.12): the device→PCS pipeline
/// A-curves → CLUT → M-curves → matrix → B-curves. Every stage but the B-curves is optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LutAToB {
    /// Input (device) channel count.
    pub input_channels: u8,
    /// Output (PCS) channel count.
    pub output_channels: u8,
    /// The input "A" curves (`input_channels` of them), if present.
    pub a_curves: Option<Vec<CurveOrParametric>>,
    /// The multi-dimensional lookup table, if present.
    pub clut: Option<Clut>,
    /// The "M" curves (`output_channels` of them), if present.
    pub m_curves: Option<Vec<CurveOrParametric>>,
    /// The 3×3-plus-offset matrix, if present.
    pub matrix: Option<Matrix3x4>,
    /// The output "B" curves (`output_channels` of them); always present.
    pub b_curves: Vec<CurveOrParametric>,
}

/// A `lutBToAType` element (`mBA `, ICC.1:2022 §10.13): the PCS→device pipeline
/// B-curves → matrix → M-curves → CLUT → A-curves. Every stage but the B-curves is optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LutBToA {
    /// Input (PCS) channel count.
    pub input_channels: u8,
    /// Output (device) channel count.
    pub output_channels: u8,
    /// The input "B" curves (`input_channels` of them); always present.
    pub b_curves: Vec<CurveOrParametric>,
    /// The 3×3-plus-offset matrix, if present.
    pub matrix: Option<Matrix3x4>,
    /// The "M" curves (`input_channels` of them), if present.
    pub m_curves: Option<Vec<CurveOrParametric>>,
    /// The multi-dimensional lookup table, if present.
    pub clut: Option<Clut>,
    /// The output "A" curves (`output_channels` of them), if present.
    pub a_curves: Option<Vec<CurveOrParametric>>,
}

/// Decodes a `lut8Type` element.
pub(crate) fn decode_lut8(element: &[u8]) -> Result<Lut8> {
    let mut r = ByteReader::at(element, 8)?;
    let input_channels = r.u8()?;
    let output_channels = r.u8()?;
    let grid_points = r.u8()?;
    r.skip(1)?; // reserved
    let matrix = read_matrix3x3(&mut r)?;
    let input_table = r.bytes(table_len(input_channels, 256)?)?.to_vec();
    let clut = r
        .bytes(clut_len(grid_points, input_channels, output_channels)?)?
        .to_vec();
    let output_table = r.bytes(table_len(output_channels, 256)?)?.to_vec();
    Ok(Lut8 {
        input_channels,
        output_channels,
        grid_points,
        matrix,
        input_table,
        clut,
        output_table,
    })
}

/// Decodes a `lut16Type` element.
pub(crate) fn decode_lut16(element: &[u8]) -> Result<Lut16> {
    let mut r = ByteReader::at(element, 8)?;
    let input_channels = r.u8()?;
    let output_channels = r.u8()?;
    let grid_points = r.u8()?;
    r.skip(1)?; // reserved
    let matrix = read_matrix3x3(&mut r)?;
    let input_table_entries = r.u16()?;
    let output_table_entries = r.u16()?;
    let input_table = read_u16_vec(
        &mut r,
        table_len(input_channels, input_table_entries.into())?,
    )?;
    let clut = read_u16_vec(
        &mut r,
        clut_len(grid_points, input_channels, output_channels)?,
    )?;
    let output_table = read_u16_vec(
        &mut r,
        table_len(output_channels, output_table_entries.into())?,
    )?;
    Ok(Lut16 {
        input_channels,
        output_channels,
        grid_points,
        matrix,
        input_table_entries,
        output_table_entries,
        input_table,
        clut,
        output_table,
    })
}

/// Decodes a `lutAToBType` element.
pub(crate) fn decode_lut_a_to_b(element: &[u8]) -> Result<LutAToB> {
    let (input_channels, output_channels, offsets) = read_mab_header(element)?;
    let [off_b, off_matrix, off_m, off_clut, off_a] = offsets;
    let i = input_channels as usize;
    let o = output_channels as usize;
    if off_b == 0 {
        return Err(IccError::Malformed("icc: lutAToB missing B-curves"));
    }
    Ok(LutAToB {
        input_channels,
        output_channels,
        a_curves: read_optional_curves(element, off_a, i)?,
        clut: read_optional_clut(element, off_clut, input_channels, output_channels)?,
        m_curves: read_optional_curves(element, off_m, o)?,
        matrix: read_optional_matrix(element, off_matrix)?,
        b_curves: read_curves(element, off_b, o)?,
    })
}

/// Decodes a `lutBToAType` element.
pub(crate) fn decode_lut_b_to_a(element: &[u8]) -> Result<LutBToA> {
    let (input_channels, output_channels, offsets) = read_mab_header(element)?;
    let [off_b, off_matrix, off_m, off_clut, off_a] = offsets;
    let i = input_channels as usize;
    let o = output_channels as usize;
    if off_b == 0 {
        return Err(IccError::Malformed("icc: lutBToA missing B-curves"));
    }
    Ok(LutBToA {
        input_channels,
        output_channels,
        b_curves: read_curves(element, off_b, i)?,
        matrix: read_optional_matrix(element, off_matrix)?,
        m_curves: read_optional_curves(element, off_m, i)?,
        clut: read_optional_clut(element, off_clut, input_channels, output_channels)?,
        a_curves: read_optional_curves(element, off_a, o)?,
    })
}

/// Reads the shared `mAB `/`mBA ` header: input/output channels and the five stage offsets
/// (B-curves, matrix, M-curves, CLUT, A-curves), each relative to the element start.
fn read_mab_header(element: &[u8]) -> Result<(u8, u8, [usize; 5])> {
    let mut r = ByteReader::at(element, 8)?;
    let input_channels = r.u8()?;
    let output_channels = r.u8()?;
    r.skip(2)?; // reserved
    let offsets = [
        r.u32()? as usize,
        r.u32()? as usize,
        r.u32()? as usize,
        r.u32()? as usize,
        r.u32()? as usize,
    ];
    Ok((input_channels, output_channels, offsets))
}

fn read_matrix3x3(r: &mut ByteReader<'_>) -> Result<Matrix3x3> {
    let mut elements = [S15Fixed16(0); 9];
    for element in &mut elements {
        *element = r.s15fixed16()?;
    }
    Ok(Matrix3x3 { elements })
}

fn read_optional_matrix(element: &[u8], offset: usize) -> Result<Option<Matrix3x4>> {
    if offset == 0 {
        return Ok(None);
    }
    let mut r = ByteReader::at(element, offset)?;
    let mut matrix = [S15Fixed16(0); 9];
    for m in &mut matrix {
        *m = r.s15fixed16()?;
    }
    let mut off = [S15Fixed16(0); 3];
    for o in &mut off {
        *o = r.s15fixed16()?;
    }
    Ok(Some(Matrix3x4 {
        matrix,
        offset: off,
    }))
}

fn read_curves(element: &[u8], offset: usize, count: usize) -> Result<Vec<CurveOrParametric>> {
    let mut r = ByteReader::at(element, offset)?;
    let mut curves = Vec::with_capacity(count);
    for _ in 0..count {
        curves.push(read_curve_element(&mut r)?);
    }
    Ok(curves)
}

fn read_optional_curves(
    element: &[u8],
    offset: usize,
    count: usize,
) -> Result<Option<Vec<CurveOrParametric>>> {
    if offset == 0 {
        Ok(None)
    } else {
        Ok(Some(read_curves(element, offset, count)?))
    }
}

fn read_optional_clut(
    element: &[u8],
    offset: usize,
    input_channels: u8,
    output_channels: u8,
) -> Result<Option<Clut>> {
    if offset == 0 {
        return Ok(None);
    }
    if input_channels as usize > 16 {
        return Err(IccError::Malformed("icc: CLUT input channels exceed 16"));
    }
    let mut r = ByteReader::at(element, offset)?;
    let grid_points = r.bytes(16)?[..input_channels as usize].to_vec();
    let precision = match r.u8()? {
        1 => ClutPrecision::U8,
        2 => ClutPrecision::U16,
        _ => return Err(IccError::Malformed("icc: invalid CLUT precision")),
    };
    r.skip(3)?; // reserved
    let sample_count = grid_node_count(&grid_points)?
        .checked_mul(output_channels as usize)
        .ok_or(IccError::Malformed("icc: CLUT sample count overflow"))?;
    let samples = match precision {
        ClutPrecision::U8 => r
            .bytes(sample_count)?
            .iter()
            .map(|&b| u16::from(b))
            .collect(),
        ClutPrecision::U16 => read_u16_vec(&mut r, sample_count)?,
    };
    Ok(Some(Clut {
        grid_points,
        output_channels,
        precision,
        samples,
    }))
}

/// `channels * entries`, checked.
fn table_len(channels: u8, entries: usize) -> Result<usize> {
    (channels as usize)
        .checked_mul(entries)
        .ok_or(IccError::Malformed("icc: LUT table size overflow"))
}

/// `grid_points^input_channels * output_channels`, checked.
fn clut_len(grid_points: u8, input_channels: u8, output_channels: u8) -> Result<usize> {
    grid_node_count(&vec![grid_points; input_channels as usize])?
        .checked_mul(output_channels as usize)
        .ok_or(IccError::Malformed("icc: CLUT size overflow"))
}

/// The product of the per-dimension grid points (the number of CLUT nodes), checked.
fn grid_node_count(grid_points: &[u8]) -> Result<usize> {
    let mut nodes = 1usize;
    for &g in grid_points {
        nodes = nodes
            .checked_mul(g as usize)
            .ok_or(IccError::Malformed("icc: CLUT grid overflow"))?;
    }
    Ok(nodes)
}

fn read_u16_vec(r: &mut ByteReader<'_>, count: usize) -> Result<Vec<u16>> {
    let byte_len = count
        .checked_mul(2)
        .ok_or(IccError::Malformed("icc: LUT size overflow"))?;
    let bytes = r.bytes(byte_len)?;
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect())
}

/// Writes a `lut8Type` element — the inverse of [`decode_lut8`].
///
/// Rejects table or CLUT vectors whose lengths disagree with the declared channel counts and grid
/// (the decoder sizes everything from those counts, so a mismatch would re-decode shifted).
pub(crate) fn encode_lut8(lut: &Lut8, out: &mut Vec<u8>) -> Result<()> {
    if lut.input_table.len() != table_len(lut.input_channels, 256)? {
        return Err(IccError::Malformed(
            "icc: lut8 input-table length does not match its channel count",
        ));
    }
    if lut.clut.len() != clut_len(lut.grid_points, lut.input_channels, lut.output_channels)? {
        return Err(IccError::Malformed(
            "icc: lut8 CLUT length does not match its grid and channels",
        ));
    }
    if lut.output_table.len() != table_len(lut.output_channels, 256)? {
        return Err(IccError::Malformed(
            "icc: lut8 output-table length does not match its channel count",
        ));
    }
    out.extend_from_slice(b"mft1");
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&[lut.input_channels, lut.output_channels, lut.grid_points, 0]);
    write_matrix3x3(&lut.matrix, out);
    out.extend_from_slice(&lut.input_table);
    out.extend_from_slice(&lut.clut);
    out.extend_from_slice(&lut.output_table);
    Ok(())
}

/// Writes a `lut16Type` element — the inverse of [`decode_lut16`].
///
/// Rejects table or CLUT vectors whose lengths disagree with the declared channel counts, table
/// entry counts, and grid.
pub(crate) fn encode_lut16(lut: &Lut16, out: &mut Vec<u8>) -> Result<()> {
    if lut.input_table.len() != table_len(lut.input_channels, lut.input_table_entries.into())? {
        return Err(IccError::Malformed(
            "icc: lut16 input-table length does not match its channels and entry count",
        ));
    }
    if lut.clut.len() != clut_len(lut.grid_points, lut.input_channels, lut.output_channels)? {
        return Err(IccError::Malformed(
            "icc: lut16 CLUT length does not match its grid and channels",
        ));
    }
    if lut.output_table.len() != table_len(lut.output_channels, lut.output_table_entries.into())? {
        return Err(IccError::Malformed(
            "icc: lut16 output-table length does not match its channels and entry count",
        ));
    }
    out.extend_from_slice(b"mft2");
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&[lut.input_channels, lut.output_channels, lut.grid_points, 0]);
    write_matrix3x3(&lut.matrix, out);
    out.extend_from_slice(&lut.input_table_entries.to_be_bytes());
    out.extend_from_slice(&lut.output_table_entries.to_be_bytes());
    write_u16_slice(&lut.input_table, out);
    write_u16_slice(&lut.clut, out);
    write_u16_slice(&lut.output_table, out);
    Ok(())
}

/// Writes a `lutAToBType` element — the inverse of [`decode_lut_a_to_b`].
///
/// Rejects curve sets and a CLUT whose shapes disagree with the declared channel counts
/// (§10.12: A-curves and the CLUT grid follow the input channels; M- and B-curves the output).
pub(crate) fn encode_lut_a_to_b(lut: &LutAToB, out: &mut Vec<u8>) -> Result<()> {
    check_curve_count(
        &lut.b_curves,
        lut.output_channels,
        "icc: lutAToB B-curve count does not match its output channels",
    )?;
    check_optional_curve_count(
        lut.m_curves.as_ref(),
        lut.output_channels,
        "icc: lutAToB M-curve count does not match its output channels",
    )?;
    check_optional_curve_count(
        lut.a_curves.as_ref(),
        lut.input_channels,
        "icc: lutAToB A-curve count does not match its input channels",
    )?;
    check_clut_channels(lut.clut.as_ref(), lut.input_channels, lut.output_channels)?;
    let mut body = Vec::new();
    let off_b = stage_offset(&body);
    for curve in &lut.b_curves {
        write_curve_element(curve, &mut body)?;
    }
    let off_matrix = write_optional_matrix(lut.matrix.as_ref(), &mut body);
    let off_m = write_optional_curves(lut.m_curves.as_ref(), &mut body)?;
    let off_clut = write_optional_clut(lut.clut.as_ref(), &mut body)?;
    let off_a = write_optional_curves(lut.a_curves.as_ref(), &mut body)?;
    write_mab(
        b"mAB ",
        lut.input_channels,
        lut.output_channels,
        [off_b, off_matrix, off_m, off_clut, off_a],
        &body,
        out,
    );
    Ok(())
}

/// Writes a `lutBToAType` element — the inverse of [`decode_lut_b_to_a`].
///
/// Rejects curve sets and a CLUT whose shapes disagree with the declared channel counts
/// (§10.13: B- and M-curves and the CLUT grid follow the input channels; A-curves the output).
pub(crate) fn encode_lut_b_to_a(lut: &LutBToA, out: &mut Vec<u8>) -> Result<()> {
    check_curve_count(
        &lut.b_curves,
        lut.input_channels,
        "icc: lutBToA B-curve count does not match its input channels",
    )?;
    check_optional_curve_count(
        lut.m_curves.as_ref(),
        lut.input_channels,
        "icc: lutBToA M-curve count does not match its input channels",
    )?;
    check_optional_curve_count(
        lut.a_curves.as_ref(),
        lut.output_channels,
        "icc: lutBToA A-curve count does not match its output channels",
    )?;
    check_clut_channels(lut.clut.as_ref(), lut.input_channels, lut.output_channels)?;
    let mut body = Vec::new();
    let off_b = stage_offset(&body);
    for curve in &lut.b_curves {
        write_curve_element(curve, &mut body)?;
    }
    let off_matrix = write_optional_matrix(lut.matrix.as_ref(), &mut body);
    let off_m = write_optional_curves(lut.m_curves.as_ref(), &mut body)?;
    let off_clut = write_optional_clut(lut.clut.as_ref(), &mut body)?;
    let off_a = write_optional_curves(lut.a_curves.as_ref(), &mut body)?;
    write_mab(
        b"mBA ",
        lut.input_channels,
        lut.output_channels,
        [off_b, off_matrix, off_m, off_clut, off_a],
        &body,
        out,
    );
    Ok(())
}

/// Checks a required curve set against its declared channel count, failing with `mismatch`.
fn check_curve_count(
    curves: &[CurveOrParametric],
    channels: u8,
    mismatch: &'static str,
) -> Result<()> {
    if curves.len() != channels as usize {
        return Err(IccError::Malformed(mismatch));
    }
    Ok(())
}

/// Checks an optional curve set against its declared channel count, failing with `mismatch`.
fn check_optional_curve_count(
    curves: Option<&Vec<CurveOrParametric>>,
    channels: u8,
    mismatch: &'static str,
) -> Result<()> {
    match curves {
        Some(curves) => check_curve_count(curves, channels, mismatch),
        None => Ok(()),
    }
}

/// Checks an optional CLUT's grid dimensionality and output channels against the transform's
/// declared channel counts (the decoder derives both from the `mAB `/`mBA ` header).
fn check_clut_channels(clut: Option<&Clut>, input: u8, output: u8) -> Result<()> {
    let Some(clut) = clut else { return Ok(()) };
    if clut.grid_points.len() != input as usize {
        return Err(IccError::Malformed(
            "icc: CLUT grid dimensions do not match the transform's input channels",
        ));
    }
    if clut.output_channels != output {
        return Err(IccError::Malformed(
            "icc: CLUT output channels do not match the transform's output channels",
        ));
    }
    Ok(())
}

/// The byte length of the shared `mAB `/`mBA ` header (type+reserved, channels+reserved, five
/// offsets); the offset a body stage lands at, relative to the element start.
const MAB_HEADER_LEN: usize = 8 + 4 + 5 * 4;

fn stage_offset(body: &[u8]) -> u32 {
    (MAB_HEADER_LEN + body.len()) as u32
}

fn write_mab(
    type_sig: &[u8; 4],
    input_channels: u8,
    output_channels: u8,
    offsets: [u32; 5],
    body: &[u8],
    out: &mut Vec<u8>,
) {
    out.extend_from_slice(type_sig);
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&[input_channels, output_channels, 0, 0]);
    for offset in offsets {
        out.extend_from_slice(&offset.to_be_bytes());
    }
    out.extend_from_slice(body);
}

fn write_optional_matrix(matrix: Option<&Matrix3x4>, body: &mut Vec<u8>) -> u32 {
    let Some(matrix) = matrix else { return 0 };
    let offset = stage_offset(body);
    for &m in &matrix.matrix {
        push_s15fixed16(body, m);
    }
    for &o in &matrix.offset {
        push_s15fixed16(body, o);
    }
    pad_to_4(body);
    offset
}

fn write_optional_curves(
    curves: Option<&Vec<CurveOrParametric>>,
    body: &mut Vec<u8>,
) -> Result<u32> {
    let Some(curves) = curves else { return Ok(0) };
    let offset = stage_offset(body);
    for curve in curves {
        write_curve_element(curve, body)?;
    }
    Ok(offset)
}

/// Writes a CLUT stage, validating the model against the on-disk geometry: at most 16 grid
/// dimensions (the fixed field size), a sample count of `∏grid_points × output_channels` (what the
/// decoder will read back), and — for [`ClutPrecision::U8`] — samples that fit in a byte (anything
/// larger would otherwise truncate silently).
fn write_optional_clut(clut: Option<&Clut>, body: &mut Vec<u8>) -> Result<u32> {
    let Some(clut) = clut else { return Ok(0) };
    if clut.grid_points.len() > 16 {
        return Err(IccError::Malformed(
            "icc: CLUT has more than 16 grid dimensions",
        ));
    }
    let expected = grid_node_count(&clut.grid_points)?
        .checked_mul(clut.output_channels as usize)
        .ok_or(IccError::Malformed("icc: CLUT sample count overflow"))?;
    if clut.samples.len() != expected {
        return Err(IccError::Malformed(
            "icc: CLUT sample count does not match its grid and output channels",
        ));
    }
    if clut.precision == ClutPrecision::U8 && clut.samples.iter().any(|&s| s > 255) {
        return Err(IccError::Malformed("icc: 8-bit CLUT sample exceeds 255"));
    }
    let offset = stage_offset(body);
    let mut grid = [0u8; 16];
    grid[..clut.grid_points.len()].copy_from_slice(&clut.grid_points);
    body.extend_from_slice(&grid);
    body.push(match clut.precision {
        ClutPrecision::U8 => 1,
        ClutPrecision::U16 => 2,
    });
    body.extend_from_slice(&[0, 0, 0]); // reserved
    match clut.precision {
        ClutPrecision::U8 => {
            for &s in &clut.samples {
                body.push(s as u8);
            }
        }
        ClutPrecision::U16 => write_u16_slice(&clut.samples, body),
    }
    pad_to_4(body);
    Ok(offset)
}

fn write_matrix3x3(matrix: &Matrix3x3, out: &mut Vec<u8>) {
    for &element in &matrix.elements {
        push_s15fixed16(out, element);
    }
}

fn write_u16_slice(values: &[u16], out: &mut Vec<u8>) {
    for &value in values {
        out.extend_from_slice(&value.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{Curve, ParametricCurve};
    use crate::primitives::U8Fixed8;

    #[test]
    fn clut_precision_discriminants_are_the_bytes_per_sample() {
        assert_eq!(ClutPrecision::U8 as u8, 1);
        assert_eq!(ClutPrecision::U16 as u8, 2);
    }

    fn s15_be(v: f64) -> [u8; 4] {
        S15Fixed16::from_f64(v).0.to_be_bytes()
    }

    fn gamma_curve() -> CurveOrParametric {
        CurveOrParametric::Curve(Curve::Gamma(U8Fixed8(0x0200)))
    }

    #[test]
    fn decodes_lut8() {
        // 1 input channel, 1 output channel, 2 grid points.
        let mut e = b"mft1\x00\x00\x00\x00".to_vec();
        e.extend_from_slice(&[1, 1, 2, 0]); // i, o, grid, reserved
        for _ in 0..9 {
            e.extend_from_slice(&s15_be(0.0)); // matrix (identity content irrelevant here)
        }
        e.extend_from_slice(&[7u8; 256]); // input table (1 × 256)
        e.extend_from_slice(&[10, 20]); // CLUT (2^1 nodes × 1 output)
        e.extend_from_slice(&[9u8; 256]); // output table (1 × 256)

        let lut = decode_lut8(&e).unwrap();
        assert_eq!(lut.input_channels, 1);
        assert_eq!(lut.output_channels, 1);
        assert_eq!(lut.grid_points, 2);
        assert_eq!(lut.input_table.len(), 256);
        assert_eq!(lut.clut, vec![10, 20]);
        assert_eq!(lut.output_table.len(), 256);
    }

    #[test]
    fn decodes_lut16_clut_dimensions() {
        // 2 input channels, 3 output channels, 2 grid points → 2^2 × 3 = 12 CLUT samples.
        let mut e = b"mft2\x00\x00\x00\x00".to_vec();
        e.extend_from_slice(&[2, 3, 2, 0]);
        for _ in 0..9 {
            e.extend_from_slice(&s15_be(0.0));
        }
        e.extend_from_slice(&2u16.to_be_bytes()); // input table entries
        e.extend_from_slice(&2u16.to_be_bytes()); // output table entries
        for _ in 0..(2 * 2) {
            e.extend_from_slice(&0u16.to_be_bytes()); // input tables
        }
        for v in 0..12u16 {
            e.extend_from_slice(&v.to_be_bytes()); // CLUT
        }
        for _ in 0..(3 * 2) {
            e.extend_from_slice(&0u16.to_be_bytes()); // output tables
        }

        let lut = decode_lut16(&e).unwrap();
        assert_eq!(lut.input_channels, 2);
        assert_eq!(lut.output_channels, 3);
        assert_eq!(lut.clut.len(), 12);
        assert_eq!(lut.clut[11], 11);
    }

    #[test]
    fn decodes_lut_a_to_b_minimal() {
        // 3→3 with only the (required) B-curves: three identity curveType curves.
        let header_len = 8 + 4 + 5 * 4; // type+reserved, channels+reserved, five offsets
        let mut e = b"mAB \x00\x00\x00\x00".to_vec();
        e.extend_from_slice(&[3, 3, 0, 0]); // i, o, reserved
        e.extend_from_slice(&(header_len as u32).to_be_bytes()); // B-curves offset
        e.extend_from_slice(&0u32.to_be_bytes()); // matrix
        e.extend_from_slice(&0u32.to_be_bytes()); // M-curves
        e.extend_from_slice(&0u32.to_be_bytes()); // CLUT
        e.extend_from_slice(&0u32.to_be_bytes()); // A-curves
        for _ in 0..3 {
            e.extend_from_slice(b"curv\x00\x00\x00\x00"); // identity curve element
            e.extend_from_slice(&0u32.to_be_bytes()); // count 0
        }

        let lut = decode_lut_a_to_b(&e).unwrap();
        assert_eq!(lut.input_channels, 3);
        assert_eq!(lut.output_channels, 3);
        assert_eq!(lut.b_curves.len(), 3);
        assert!(matches!(
            lut.b_curves[0],
            CurveOrParametric::Curve(Curve::Identity)
        ));
        assert!(lut.a_curves.is_none());
        assert!(lut.clut.is_none());
        assert!(lut.m_curves.is_none());
        assert!(lut.matrix.is_none());
    }

    #[test]
    fn decodes_lut_a_to_b_with_clut_and_matrix() {
        // 3→3 with B-curves, a matrix, and a CLUT (2 grid points per axis, u8 precision).
        let mut e = b"mAB \x00\x00\x00\x00".to_vec();
        e.extend_from_slice(&[3, 3, 0, 0]);
        // Offsets filled in after laying out the body.
        let offsets_at = e.len();
        e.extend_from_slice(&[0u8; 20]);

        let off_b = e.len();
        for _ in 0..3 {
            e.extend_from_slice(b"curv\x00\x00\x00\x00");
            e.extend_from_slice(&0u32.to_be_bytes());
        }
        let off_matrix = e.len();
        for v in [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.1, 0.2, 0.3] {
            e.extend_from_slice(&s15_be(v));
        }
        let off_clut = e.len();
        e.extend_from_slice(&[2u8; 16]); // grid points per dimension
        e.push(1); // precision: u8
        e.extend_from_slice(&[0, 0, 0]); // reserved
        // 2^3 nodes × 3 outputs = 24 samples.
        e.extend_from_slice(&(0..24u8).collect::<Vec<_>>());

        let patch = |e: &mut Vec<u8>, at: usize, v: u32| {
            e[at..at + 4].copy_from_slice(&v.to_be_bytes());
        };
        patch(&mut e, offsets_at, off_b as u32);
        patch(&mut e, offsets_at + 4, off_matrix as u32);
        patch(&mut e, offsets_at + 12, off_clut as u32);

        let lut = decode_lut_a_to_b(&e).unwrap();
        let clut = lut.clut.expect("clut present");
        assert_eq!(clut.grid_points, vec![2, 2, 2]);
        assert_eq!(clut.output_channels, 3);
        assert_eq!(clut.precision, ClutPrecision::U8);
        assert_eq!(clut.samples.len(), 24);
        let matrix = lut.matrix.expect("matrix present");
        assert_eq!(matrix.offset[0], S15Fixed16::from_f64(0.1));
    }

    #[test]
    fn lut8_round_trips() {
        let lut = Lut8 {
            input_channels: 1,
            output_channels: 1,
            grid_points: 2,
            matrix: Matrix3x3 {
                elements: [S15Fixed16::from_f64(1.0); 9],
            },
            input_table: vec![7u8; 256],
            clut: vec![10, 20],
            output_table: vec![9u8; 256],
        };
        let mut out = Vec::new();
        encode_lut8(&lut, &mut out).unwrap();
        assert_eq!(decode_lut8(&out).unwrap(), lut);
    }

    #[test]
    fn lut16_round_trips() {
        let lut = Lut16 {
            input_channels: 2,
            output_channels: 3,
            grid_points: 2,
            matrix: Matrix3x3 {
                elements: [S15Fixed16(0); 9],
            },
            input_table_entries: 2,
            output_table_entries: 2,
            input_table: vec![0u16; 4],
            clut: (0..12u16).collect(),
            output_table: vec![0u16; 6],
        };
        let mut out = Vec::new();
        encode_lut16(&lut, &mut out).unwrap();
        assert_eq!(decode_lut16(&out).unwrap(), lut);
    }

    #[test]
    fn lut_a_to_b_round_trips_every_stage() {
        // All five stages present, with a 16-bit CLUT.
        let lut = LutAToB {
            input_channels: 3,
            output_channels: 3,
            a_curves: Some(vec![gamma_curve(), gamma_curve(), gamma_curve()]),
            clut: Some(Clut {
                grid_points: vec![2, 2, 2],
                output_channels: 3,
                precision: ClutPrecision::U16,
                samples: (0..24u16).collect(),
            }),
            m_curves: Some(vec![gamma_curve(), gamma_curve(), gamma_curve()]),
            matrix: Some(Matrix3x4 {
                matrix: [S15Fixed16::from_f64(1.0); 9],
                offset: [S15Fixed16::from_f64(0.5); 3],
            }),
            b_curves: vec![gamma_curve(), gamma_curve(), gamma_curve()],
        };
        let mut out = Vec::new();
        encode_lut_a_to_b(&lut, &mut out).unwrap();
        assert_eq!(decode_lut_a_to_b(&out).unwrap(), lut);
    }

    #[test]
    fn lut_b_to_a_round_trips_every_stage() {
        // 3 input (PCS) → 4 output (device) channels; an 8-bit CLUT and parametric curves.
        let param = || {
            CurveOrParametric::Parametric(ParametricCurve {
                function_type: 0,
                params: vec![S15Fixed16::from_f64(2.0)],
            })
        };
        let lut = LutBToA {
            input_channels: 3,
            output_channels: 4,
            b_curves: vec![param(), param(), param()],
            matrix: Some(Matrix3x4 {
                matrix: [S15Fixed16(0); 9],
                offset: [S15Fixed16(0); 3],
            }),
            m_curves: Some(vec![param(), param(), param()]),
            clut: Some(Clut {
                grid_points: vec![2, 2, 2],
                output_channels: 4,
                precision: ClutPrecision::U8,
                samples: (0..32u16).collect(), // 2^3 nodes × 4 outputs
            }),
            a_curves: Some(vec![param(), param(), param(), param()]),
        };
        let mut out = Vec::new();
        encode_lut_b_to_a(&lut, &mut out).unwrap();
        assert_eq!(decode_lut_b_to_a(&out).unwrap(), lut);
    }

    /// A structurally valid 1→1 [`Lut8`] the shape-rejection tests perturb one field of.
    fn valid_lut8() -> Lut8 {
        Lut8 {
            input_channels: 1,
            output_channels: 1,
            grid_points: 2,
            matrix: Matrix3x3 {
                elements: [S15Fixed16(0); 9],
            },
            input_table: vec![0u8; 256],
            clut: vec![0, 1],
            output_table: vec![0u8; 256],
        }
    }

    #[test]
    fn encode_lut8_rejects_mismatched_table_lengths() {
        let mut out = Vec::new();
        let mut bad = valid_lut8();
        bad.input_table.pop(); // 255 entries for one channel
        assert!(encode_lut8(&bad, &mut out).is_err());

        let mut bad = valid_lut8();
        bad.clut.push(9); // 3 samples for a 2-node × 1-output grid
        assert!(encode_lut8(&bad, &mut out).is_err());

        let mut bad = valid_lut8();
        bad.output_table.pop();
        assert!(encode_lut8(&bad, &mut out).is_err());
    }

    /// A structurally valid 2→3 [`Lut16`] the shape-rejection tests perturb one field of.
    fn valid_lut16() -> Lut16 {
        Lut16 {
            input_channels: 2,
            output_channels: 3,
            grid_points: 2,
            matrix: Matrix3x3 {
                elements: [S15Fixed16(0); 9],
            },
            input_table_entries: 2,
            output_table_entries: 2,
            input_table: vec![0u16; 4],
            clut: (0..12u16).collect(),
            output_table: vec![0u16; 6],
        }
    }

    #[test]
    fn encode_lut16_rejects_mismatched_table_lengths() {
        let mut out = Vec::new();
        let mut bad = valid_lut16();
        bad.input_table.pop(); // 3 entries for 2 channels × 2 per table
        assert!(encode_lut16(&bad, &mut out).is_err());

        let mut bad = valid_lut16();
        bad.clut.pop(); // 11 samples for 2² nodes × 3 outputs
        assert!(encode_lut16(&bad, &mut out).is_err());

        let mut bad = valid_lut16();
        bad.output_table.push(0); // 7 entries for 3 channels × 2 per table
        assert!(encode_lut16(&bad, &mut out).is_err());
    }

    /// A structurally valid 3→3 [`LutAToB`] with every stage present.
    fn valid_lut_a_to_b() -> LutAToB {
        LutAToB {
            input_channels: 3,
            output_channels: 3,
            a_curves: Some(vec![gamma_curve(), gamma_curve(), gamma_curve()]),
            clut: Some(Clut {
                grid_points: vec![2, 2, 2],
                output_channels: 3,
                precision: ClutPrecision::U16,
                samples: (0..24u16).collect(),
            }),
            m_curves: Some(vec![gamma_curve(), gamma_curve(), gamma_curve()]),
            matrix: None,
            b_curves: vec![gamma_curve(), gamma_curve(), gamma_curve()],
        }
    }

    #[test]
    fn encode_lut_a_to_b_rejects_mismatched_curve_counts() {
        let mut out = Vec::new();
        let mut bad = valid_lut_a_to_b();
        bad.b_curves.pop(); // 2 B-curves for 3 output channels
        assert!(encode_lut_a_to_b(&bad, &mut out).is_err());

        let mut bad = valid_lut_a_to_b();
        bad.m_curves = Some(vec![gamma_curve()]); // 1 M-curve for 3 output channels
        assert!(encode_lut_a_to_b(&bad, &mut out).is_err());

        let mut bad = valid_lut_a_to_b();
        bad.a_curves = Some(vec![gamma_curve()]); // 1 A-curve for 3 input channels
        assert!(encode_lut_a_to_b(&bad, &mut out).is_err());
    }

    #[test]
    fn encode_lut_a_to_b_rejects_mismatched_clut_shape() {
        let mut out = Vec::new();
        let mut bad = valid_lut_a_to_b();
        if let Some(clut) = &mut bad.clut {
            clut.grid_points = vec![2, 2]; // 2 grid dimensions for 3 input channels
            clut.samples = (0..12u16).collect(); // internally consistent, wrong dimensionality
        }
        assert!(encode_lut_a_to_b(&bad, &mut out).is_err());

        let mut bad = valid_lut_a_to_b();
        if let Some(clut) = &mut bad.clut {
            clut.output_channels = 2; // 2 CLUT outputs for 3 transform outputs
            clut.samples = (0..16u16).collect();
        }
        assert!(encode_lut_a_to_b(&bad, &mut out).is_err());
    }

    #[test]
    fn encode_lut_b_to_a_rejects_mismatched_curve_counts() {
        // 3 (PCS) → 4 (device): B- and M-curves follow the input count, A-curves the output count,
        // so each perturbation below also catches an input/output swap in the checks.
        let valid = || LutBToA {
            input_channels: 3,
            output_channels: 4,
            b_curves: vec![gamma_curve(), gamma_curve(), gamma_curve()],
            matrix: None,
            m_curves: Some(vec![gamma_curve(), gamma_curve(), gamma_curve()]),
            clut: Some(Clut {
                grid_points: vec![2, 2, 2],
                output_channels: 4,
                precision: ClutPrecision::U16,
                samples: (0..32u16).collect(),
            }),
            a_curves: Some(vec![
                gamma_curve(),
                gamma_curve(),
                gamma_curve(),
                gamma_curve(),
            ]),
        };
        let mut out = Vec::new();
        assert!(encode_lut_b_to_a(&valid(), &mut out).is_ok());

        let mut bad = valid();
        bad.b_curves.push(gamma_curve()); // 4 B-curves for 3 input channels
        assert!(encode_lut_b_to_a(&bad, &mut out).is_err());

        let mut bad = valid();
        bad.m_curves = Some(vec![gamma_curve(); 4]); // 4 M-curves for 3 input channels
        assert!(encode_lut_b_to_a(&bad, &mut out).is_err());

        let mut bad = valid();
        bad.a_curves = Some(vec![gamma_curve(); 3]); // 3 A-curves for 4 output channels
        assert!(encode_lut_b_to_a(&bad, &mut out).is_err());
    }

    #[test]
    fn write_clut_rejects_bad_geometry_and_overflowing_samples() {
        // More than 16 grid dimensions cannot fit the fixed field (17 input channels would also
        // be rejected upstream, but the CLUT check must hold on its own).
        let mut body = Vec::new();
        let too_many_dims = Clut {
            grid_points: vec![1; 17],
            output_channels: 1,
            precision: ClutPrecision::U8,
            samples: vec![0],
        };
        assert!(write_optional_clut(Some(&too_many_dims), &mut body).is_err());

        // Sample count disagreeing with the grid geometry.
        let wrong_count = Clut {
            grid_points: vec![2, 2],
            output_channels: 3,
            precision: ClutPrecision::U16,
            samples: vec![0; 11], // 2² × 3 = 12 expected
        };
        assert!(write_optional_clut(Some(&wrong_count), &mut body).is_err());

        // An 8-bit CLUT sample that does not fit a byte must error, not truncate.
        let overflowing = Clut {
            grid_points: vec![2],
            output_channels: 1,
            precision: ClutPrecision::U8,
            samples: vec![255, 256],
        };
        assert!(write_optional_clut(Some(&overflowing), &mut body).is_err());
        // The same samples at 16-bit precision are fine.
        let sixteen_bit = Clut {
            precision: ClutPrecision::U16,
            ..overflowing
        };
        assert!(write_optional_clut(Some(&sixteen_bit), &mut body).is_ok());

        // 255 is the largest valid 8-bit sample: the guard fires strictly above it, so a
        // boundary-valued CLUT must encode.
        let boundary = Clut {
            grid_points: vec![2],
            output_channels: 1,
            precision: ClutPrecision::U8,
            samples: vec![0, 255],
        };
        assert!(write_optional_clut(Some(&boundary), &mut body).is_ok());
    }

    #[test]
    fn rejects_invalid_clut_precision() {
        // An mAB whose CLUT precision byte is neither 1 nor 2.
        let mut e = b"mAB \x00\x00\x00\x00".to_vec();
        e.extend_from_slice(&[3, 3, 0, 0]);
        let offsets_at = e.len();
        e.extend_from_slice(&[0u8; 20]);
        let off_b = e.len();
        for _ in 0..3 {
            e.extend_from_slice(b"curv\x00\x00\x00\x00");
            e.extend_from_slice(&0u32.to_be_bytes());
        }
        let off_clut = e.len();
        e.extend_from_slice(&[2u8; 16]);
        e.push(9); // invalid precision
        e.extend_from_slice(&[0, 0, 0]);
        e.extend_from_slice(&[0u8; 24]);
        e[offsets_at..offsets_at + 4].copy_from_slice(&(off_b as u32).to_be_bytes());
        e[offsets_at + 12..offsets_at + 16].copy_from_slice(&(off_clut as u32).to_be_bytes());
        assert!(decode_lut_a_to_b(&e).is_err());
    }

    #[test]
    fn clut_precision_full_scale() {
        assert_eq!(ClutPrecision::U8.full_scale(), 255);
        assert_eq!(ClutPrecision::U16.full_scale(), 65535);
    }

    #[test]
    fn lut_b_to_a_round_trips_with_16_input_channels() {
        // An all-1 grid keeps the node count at 1, so a full 16-channel CLUT stays tiny — this
        // exercises the decoder's 16-channel upper boundary.
        let identity = || CurveOrParametric::Curve(Curve::Identity);
        let lut = LutBToA {
            input_channels: 16,
            output_channels: 2,
            b_curves: (0..16).map(|_| identity()).collect(),
            matrix: None,
            m_curves: None,
            clut: Some(Clut {
                grid_points: vec![1; 16],
                output_channels: 2,
                precision: ClutPrecision::U8,
                samples: vec![10, 20], // 1 node × 2 outputs
            }),
            a_curves: None,
        };
        let mut out = Vec::new();
        encode_lut_b_to_a(&lut, &mut out).unwrap();
        assert_eq!(decode_lut_b_to_a(&out).unwrap(), lut);
    }
}
