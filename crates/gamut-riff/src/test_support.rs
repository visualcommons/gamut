//! Fixture builders the `vp8x`, `layout` and `c2pa` unit tests share. Test-only.

use core::ops::Range;

use crate::c2pa::c2pa_span;
use crate::chunk::Chunk;
use crate::fourcc::FourCc;
use crate::reader::RiffReader;
use crate::vp8x::{VP8X_PAYLOAD_LEN, Vp8xHeader};
use crate::writer::RiffWriter;

/// A borrowed chunk, for the writers' `&[Chunk]` arguments.
pub(crate) const fn chunk(fourcc: FourCc, payload: &[u8]) -> Chunk<'_> {
    Chunk { fourcc, payload }
}

/// The chunk FourCCs of `file`, in file order.
pub(crate) fn chunk_ids(file: &[u8]) -> Vec<FourCc> {
    RiffReader::new(file)
        .unwrap()
        .map(|c| c.unwrap().fourcc)
        .collect()
}

/// The parsed `VP8X` feature header of an extended `file` (its first chunk).
pub(crate) fn vp8x_header_of(file: &[u8]) -> Vp8xHeader {
    let first = RiffReader::new(file).unwrap().next().unwrap().unwrap();
    assert_eq!(
        first.fourcc,
        FourCc::VP8X,
        "an extended file opens with VP8X"
    );
    Vp8xHeader::parse(first.payload).unwrap()
}

/// [`c2pa_span`] of a well-formed `file` as a `usize` range, ready to index the file with.
pub(crate) fn span_in(file: &[u8]) -> Option<Range<usize>> {
    c2pa_span(file)
        .unwrap()
        .map(|span| usize::try_from(span.start).unwrap()..usize::try_from(span.end).unwrap())
}

/// Assembles a file from raw chunks, bypassing the ordering the writers impose — the only way
/// to build the malformed inputs `WebpLayout::parse` must reject.
pub(crate) fn raw_file(chunks: &[(FourCc, &[u8])]) -> Vec<u8> {
    let mut w = RiffWriter::new();
    for (fourcc, payload) in chunks {
        w.write_chunk(*fourcc, payload).unwrap();
    }
    w.finish().unwrap()
}

/// A `VP8X` payload for a small canvas with the given feature flags left at their defaults.
pub(crate) fn vp8x(alpha: bool) -> [u8; VP8X_PAYLOAD_LEN] {
    Vp8xHeader {
        alpha,
        canvas_width: 16,
        canvas_height: 16,
        ..Default::default()
    }
    .to_bytes()
    .unwrap()
}
