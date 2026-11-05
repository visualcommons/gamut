//! The marker layer (T.81 Annex B): the SOI check (§B.2.1), marker codes past fill bytes
//! (§B.1.1.2), length-prefixed segment payloads (§B.1.1.4), and the walk to the frame header that
//! [`JpegDecoder::info`] and the backend seam read without entropy decoding.

use gamut_core::{Error, Result};

#[cfg(doc)]
use super::JpegDecoder;
use super::JpegProcess;
use crate::marker::code;

/// Walks the marker layer to the frame header, returning its process and raw SOFn payload without
/// entropy decoding. Shared by [`JpegDecoder::info`] and [`crate::JpegStreamInfo::parse`], which read different
/// field subsets out of the same payload.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the stream
/// is malformed or has no frame header, or an
/// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error if the frame uses a process
/// other than baseline / extended-sequential / progressive DCT.
pub(crate) fn frame_header(data: &[u8]) -> Result<(JpegProcess, &[u8])> {
    expect_soi(data)?;
    let mut pos = 2;
    loop {
        let (marker, after) = read_marker(data, pos)?;
        let process = match marker {
            code::SOF0 => JpegProcess::Baseline,
            code::SOF1 => JpegProcess::ExtendedSequential,
            code::SOF2 => JpegProcess::Progressive,
            code::SOF3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: unsupported process",
                ));
            }
            code::SOS | code::EOI_CODE => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: no frame header before scan/end",
                ));
            }
            code::SOI | code::TEM | code::RST0..=code::RST7 => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: unexpected standalone marker",
                ));
            }
            _ => {
                // Any other segment (tables, application data, comment): skip by length.
                let (_, next) = read_segment(data, after)?;
                pos = next;
                continue;
            }
        };
        let (payload, _) = read_segment(data, after)?;
        return Ok((process, payload));
    }
}

/// Shared truncated-frame-header error.
pub(crate) fn trunc_sof() -> Error {
    Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: truncated frame header")
}

/// Verifies the two-byte SOI that opens every JPEG stream (§B.2.1).
pub(crate) fn expect_soi(data: &[u8]) -> Result<()> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != code::SOI {
        return Err(
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: missing SOI marker")
                .with_byte_offset(0),
        );
    }
    Ok(())
}

/// Reads the next marker at or after `pos` (skipping fill `0xFF` bytes, §B.1.1.2), returning its
/// code and the offset just past it.
pub(crate) fn read_marker(data: &[u8], pos: usize) -> Result<(u8, usize)> {
    if data.get(pos) != Some(&0xFF) {
        return Err(
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: expected a marker")
                .with_byte_offset(pos as u64),
        );
    }
    // The marker code is the first non-fill byte: skip any run of fill 0xFF bytes (§B.1.1.2).
    let code_pos = pos + data[pos..].iter().take_while(|&&b| b == 0xFF).count();
    let code = *data.get(code_pos).ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: truncated marker")
            .with_byte_offset(code_pos as u64)
    })?;
    Ok((code, code_pos + 1))
}

/// Reads a marker segment's payload at `pos` (the offset just past the marker code), returning the
/// payload bytes and the offset just past the segment. The two-byte length counts itself (§B.1.1.4).
pub(crate) fn read_segment(data: &[u8], pos: usize) -> Result<(&[u8], usize)> {
    let truncated = || trunc_seg().with_byte_offset(pos as u64);
    let hi = *data.get(pos).ok_or_else(truncated)?;
    let lo = *data.get(pos.saturating_add(1)).ok_or_else(truncated)?;
    let len = usize::from(u16::from_be_bytes([hi, lo]));
    if len < 2 {
        return Err(
            Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: segment length < 2")
                .with_byte_offset(pos as u64),
        );
    }
    let end = pos.checked_add(len).ok_or_else(truncated)?;
    let payload_start = pos.checked_add(2).ok_or_else(truncated)?;
    let payload = data.get(payload_start..end).ok_or_else(truncated)?;
    Ok((payload, end))
}

/// Shared truncated-segment error.
fn trunc_seg() -> Error {
    Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: truncated segment")
}
