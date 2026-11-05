//! APPn metadata payload conventions: APP1 EXIF, APP1 XMP, and multi-segment APP2 ICC.
//!
//! T.81 defines only the `APPn` marker syntax (§B.2.4.6); the payload layouts come from the
//! metadata standards, pinned with the framing constants in
//! [`references/jpeg`](https://github.com/visualcommons/gamut/tree/master/references/jpeg):
//!
//! - **APP1 EXIF** (Exif 3.0, CIPA DC-008 §4.7.2): the signature `"Exif\0\0"` followed by a TIFF
//!   stream. A single segment.
//! - **APP1 XMP** (XMP Part 3 §1.1.3): the null-terminated namespace URI
//!   `"http://ns.adobe.com/xap/1.0/"` followed by one XMP `xpacket` of at most 65502 bytes (the
//!   spec-stated StandardXMP cap). The ExtendedXMP continuation scheme (§1.1.3.1, signature
//!   `"http://ns.adobe.com/xmp/extension/\0"`) is deferred — see `STATUS.md`.
//! - **APP2 ICC** (ICC.1:2001-04 Annex B.4): the signature `"ICC_PROFILE\0"` followed by a 1-based
//!   chunk index byte and a total chunk-count byte, the profile split across up to 255 chunks of at
//!   most 65519 bytes. T.81 does not guarantee segment order, so reassembly is by index.

use gamut_core::{Error, Result};

use crate::decoder::{expect_soi, read_marker, read_segment};
use crate::marker::{code, write_segment_header};

/// APP1 EXIF signature (Exif 3.0 §4.7.2.3): `"Exif"` plus two NUL padding bytes.
pub(crate) const EXIF_SIG: &[u8] = b"Exif\0\0";

/// APP1 XMP signature (XMP Part 3 §1.1.3): the null-terminated `xap/1.0` namespace URI. Note the
/// ExtendedXMP signature uses the distinct `xmp/extension/` URI and never matches this prefix.
pub(crate) const XMP_SIG: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

/// APP2 ICC signature (ICC.1:2001-04 Annex B.4), followed by the chunk index and count bytes.
pub(crate) const ICC_SIG: &[u8] = b"ICC_PROFILE\0";

/// Largest APPn payload: the two-byte segment length counts itself (T.81 §B.1.1.4).
const MAX_APP_PAYLOAD: usize = 65533;

/// Largest EXIF TIFF stream embeddable in the single APP1 segment.
pub(crate) const MAX_EXIF: usize = MAX_APP_PAYLOAD - EXIF_SIG.len();

/// Largest StandardXMP packet: XMP Part 3 §1.1.3 states 65502 explicitly (a stricter bound than
/// the segment arithmetic allows); packets beyond it need the deferred ExtendedXMP scheme.
pub(crate) const MAX_XMP: usize = 65502;

/// Profile bytes per ICC chunk: the APP2 payload less the signature, index, and count bytes.
pub(crate) const ICC_CHUNK: usize = MAX_APP_PAYLOAD - ICC_SIG.len() - 2;

/// Largest embeddable ICC profile: 255 chunks (the count is one byte) of [`ICC_CHUNK`] bytes,
/// i.e. 16 707 345 bytes (ICC.1:2001-04 Annex B.4).
pub(crate) const MAX_ICC: usize = 255 * ICC_CHUNK;

/// Appends the APP1 EXIF segment (Exif 3.0 §4.7.2): signature plus the TIFF stream. The caller
/// pre-validates `tiff.len() <= MAX_EXIF`.
pub(crate) fn write_app1_exif(out: &mut Vec<u8>, tiff: &[u8]) {
    write_segment_header(out, code::APP1, 2 + EXIF_SIG.len() + tiff.len());
    out.extend_from_slice(EXIF_SIG);
    out.extend_from_slice(tiff);
}

/// Appends the APP1 XMP segment (XMP Part 3 §1.1.3): the namespace URI plus the `xpacket`. The
/// caller pre-validates `xpacket.len() <= MAX_XMP`.
pub(crate) fn write_app1_xmp(out: &mut Vec<u8>, xpacket: &[u8]) {
    write_segment_header(out, code::APP1, 2 + XMP_SIG.len() + xpacket.len());
    out.extend_from_slice(XMP_SIG);
    out.extend_from_slice(xpacket);
}

/// Appends the profile as APP2 `ICC_PROFILE` segments (ICC.1:2001-04 Annex B.4): [`ICC_CHUNK`]-byte
/// chunks, each carrying its 1-based index and the shared total count. The caller pre-validates
/// `1 <= profile.len() <= MAX_ICC`, which bounds the count to the one-byte field.
pub(crate) fn write_app2_icc(out: &mut Vec<u8>, profile: &[u8]) {
    let count = profile.len().div_ceil(ICC_CHUNK) as u8;
    for (i, chunk) in profile.chunks(ICC_CHUNK).enumerate() {
        write_segment_header(out, code::APP2, 2 + ICC_SIG.len() + 2 + chunk.len());
        out.extend_from_slice(ICC_SIG);
        out.push(i as u8 + 1);
        out.push(count);
        out.extend_from_slice(chunk);
    }
}

/// The encoder's configured metadata, as handed to [`patch_stream`]: the already-cap-checked EXIF
/// TIFF stream, XMP `xpacket`, and ICC profile.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AppMetadata<'a> {
    /// The APP1 EXIF TIFF stream, signature stripped.
    pub(crate) exif: Option<&'a [u8]>,
    /// The APP1 XMP `xpacket`, namespace URI stripped.
    pub(crate) xmp: Option<&'a [u8]>,
    /// The ICC profile, to be split across APP2 `ICC_PROFILE` chunks.
    pub(crate) icc: Option<&'a [u8]>,
}

impl AppMetadata<'_> {
    /// Appends this metadata as APPn segments, in the order [`crate::JpegEncoder`]'s prologue uses
    /// (EXIF, XMP, ICC).
    fn write(self, out: &mut Vec<u8>) {
        if let Some(exif) = self.exif {
            write_app1_exif(out, exif);
        }
        if let Some(xmp) = self.xmp {
            write_app1_xmp(out, xmp);
        }
        if let Some(icc) = self.icc {
            write_app2_icc(out, icc);
        }
    }
}

/// Returns `true` for an APPn segment whose payload is one the crate owns — an APP1 EXIF, an APP1
/// XMP, or an APP2 `ICC_PROFILE` chunk. Any other APPn (JFIF APP0, Adobe APP14, ExtendedXMP, Exif
/// Flashpix APP2, …) belongs to the producer and is preserved.
fn is_crate_owned(marker: u8, payload: &[u8]) -> bool {
    match marker {
        code::APP1 => exif_payload(payload).is_some() || xmp_payload(payload).is_some(),
        code::APP2 => payload.starts_with(ICC_SIG),
        _ => false,
    }
}

/// Rewrites a backend-produced interchange stream so its crate-owned APPn metadata is exactly
/// `meta` — the encode-side half of the crate's metadata ownership (see [`crate::backend`]).
///
/// Every EXIF / XMP / `ICC_PROFILE` segment the producer emitted is **dropped**, and `meta` is
/// written once at the crate's canonical position: after the leading run of APP0 segments and before
/// everything else (matching `JpegEncoder::write_prologue`), so metadata is patched rather than
/// double-written. All other bytes — APP0, tables, the frame header, and every entropy-coded byte
/// from the first SOS onward — are copied verbatim.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if `stream` is
/// not a complete, parsable interchange stream: no
/// SOI, no trailing EOI, an unreadable marker or segment before the first scan, or a standalone
/// marker where a segment was expected.
pub(crate) fn patch_stream(stream: &[u8], meta: AppMetadata<'_>) -> Result<Vec<u8>> {
    expect_soi(stream)?;
    if stream.len() < 4 || stream[stream.len() - 2..] != [0xFF, code::EOI_CODE] {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "JPEG: backend stream does not end with EOI",
        ));
    }
    let mut out = Vec::with_capacity(stream.len());
    out.extend_from_slice(&stream[..2]);
    let mut pos = 2;
    let mut written = false;
    loop {
        let (marker, after) = read_marker(stream, pos)?;
        match marker {
            code::SOS | code::EOI_CODE => break,
            code::SOI | code::TEM | code::RST0..=code::RST7 => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "JPEG: backend stream has a standalone marker before the first scan",
                ));
            }
            _ => {}
        }
        if !written && marker != code::APP0 {
            meta.write(&mut out);
            written = true;
        }
        let (payload, next) = read_segment(stream, after)?;
        if !is_crate_owned(marker, payload) {
            out.extend_from_slice(&stream[pos..next]);
        }
        pos = next;
    }
    if !written {
        meta.write(&mut out);
    }
    // From the first SOS (or a scanless EOI) on, the stream is copied byte for byte: entropy-coded
    // data and its stuffing must not be reinterpreted.
    out.extend_from_slice(&stream[pos..]);
    Ok(out)
}

/// Returns the TIFF stream of an APP1 EXIF payload, or `None` if the signature does not match.
pub(crate) fn exif_payload(payload: &[u8]) -> Option<&[u8]> {
    payload.strip_prefix(EXIF_SIG)
}

/// Returns the `xpacket` of an APP1 XMP payload, or `None` if the signature does not match
/// (including ExtendedXMP segments, whose URI differs).
pub(crate) fn xmp_payload(payload: &[u8]) -> Option<&[u8]> {
    payload.strip_prefix(XMP_SIG)
}

/// Reassembles an ICC profile from its APP2 `ICC_PROFILE` chunks (ICC.1:2001-04 Annex B.4).
///
/// Chunks may arrive in any order; each carries a 1-based index and the shared total count, and
/// every chunk must agree on that count. Feed each APP2 payload to [`IccAssembler::add`] and take
/// the profile from [`IccAssembler::finish`].
#[derive(Debug, Default)]
pub(crate) struct IccAssembler {
    /// Chunk slots, allocated to the declared count when the first ICC chunk arrives; `None` slots
    /// are chunks not yet seen. Empty until then.
    chunks: Vec<Option<Vec<u8>>>,
}

impl IccAssembler {
    /// Records one APP2 payload. Payloads without the `ICC_PROFILE` signature are ignored (APP2 is
    /// also used by e.g. Exif Flashpix extensions).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error on a
    /// truncated chunk header, a zero index or count, an index beyond the count, a count that disagrees with earlier chunks, or a repeated index.
    pub(crate) fn add(&mut self, payload: &[u8]) -> Result<()> {
        let Some(rest) = payload.strip_prefix(ICC_SIG) else {
            return Ok(());
        };
        let [index, count, data @ ..] = rest else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: truncated ICC_PROFILE chunk",
            ));
        };
        let (index, count) = (usize::from(*index), usize::from(*count));
        if index == 0 || count == 0 || index > count {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: ICC_PROFILE chunk index out of range",
            ));
        }
        if self.chunks.is_empty() {
            self.chunks.resize(count, None);
        } else if self.chunks.len() != count {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: ICC_PROFILE chunk count mismatch",
            ));
        }
        let slot = &mut self.chunks[index - 1];
        if slot.is_some() {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "JPEG: duplicate ICC_PROFILE chunk",
            ));
        }
        *slot = Some(data.to_vec());
        Ok(())
    }

    /// Concatenates the chunks in index order, or `None` if no ICC chunk was seen.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if any
    /// chunk of the declared count is missing.
    pub(crate) fn finish(self) -> Result<Option<Vec<u8>>> {
        if self.chunks.is_empty() {
            return Ok(None);
        }
        let mut profile = Vec::new();
        for chunk in self.chunks {
            let chunk = chunk.ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "JPEG: missing ICC_PROFILE chunk")
            })?;
            profile.extend_from_slice(&chunk);
        }
        Ok(Some(profile))
    }
}
