//! fuzz · robustness — `gamut_metadata::MetadataExtractor`, the facade a container's located
//! metadata blocks reach.
//!
//! `docs/testing.md`'s per-crate table names `extract` as this crate's untrusted-input surface
//! (#311). Each carrier's parser has its own target — `exif_parse`, `xmp_parse`, `iptc_read_irb`,
//! `icc_parse` — so what this one searches is the layer **above** them, which none of those reach:
//!
//! - the IPTC reconciliation, where a parsed IIM block and a parsed XMP packet are merged under a
//!   [`ConflictPolicy`] into one graph ([`MetadataExtractor::extract`]), and the conflict report
//!   over the same pair ([`MetadataExtractor::conflicts`]) — `iptc_read_irb` drives the reader
//!   with no XMP at all;
//! - the EXIF ↔ XMP mapping ([`exif_to_xmp`], [`xmp_to_exif`]) over a parsed hostile model in both
//!   directions and both [`ExifConflictPolicy`] values;
//! - the IPTC Photo Metadata view ([`Metadata::iptc`]) and the re-embed a transcode performs on a
//!   model read from an untrusted file ([`Metadata::to_blocks`]).
//!
//! Input layout: byte 0's bit 0 picks the IPTC [`ConflictPolicy`] and bit 1 the
//! [`ExifConflictPolicy`]; bytes 1–2 and 3–4 are little-endian lengths of the EXIF and XMP
//! blocks; the EXIF block, the XMP block and an IIM block follow, each cut short by the end of the
//! input. An empty block is left out, so every subset of the three carriers is reachable. The ICC
//! and C2PA blocks are not driven: ICC has no cross-carrier logic here beyond `icc_parse`'s, and
//! C2PA is copied verbatim, unparsed.
//!
//! The crate is `#![forbid(unsafe_code)]`, so hostile blocks must end in a typed error, never a
//! panic, a hang, or an allocation past libFuzzer's malloc limit. Like the per-carrier targets,
//! this one lists **no check beyond the crash oracle**; a crash is minimised and promoted into a
//! named deterministic case in the crate that owns the defect.

#![no_main]

use gamut_metadata::exif::{ByteOrder, Exif};
use gamut_metadata::iptc::ConflictPolicy;
use gamut_metadata::{
    ExifConflictPolicy, Metadata, MetadataBlock, MetadataExtractor, exif_to_xmp, xmp_to_exif,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&flags, rest)) = data.split_first() else {
        return;
    };
    let (exif_len, rest) = take_len(rest);
    let (xmp_len, rest) = take_len(rest);
    let (exif, rest) = rest.split_at(exif_len.min(rest.len()));
    let (xmp, iim) = rest.split_at(xmp_len.min(rest.len()));

    let mut blocks = Vec::with_capacity(3);
    if !exif.is_empty() {
        blocks.push(MetadataBlock::Exif(exif));
    }
    if !xmp.is_empty() {
        blocks.push(MetadataBlock::Xmp(xmp));
    }
    if !iim.is_empty() {
        blocks.push(MetadataBlock::IptcIim(iim));
    }

    let iptc_policy = if flags & 1 == 0 {
        ConflictPolicy::XmpWins
    } else {
        ConflictPolicy::IimWins
    };
    let exif_policy = if flags & 2 == 0 {
        ExifConflictPolicy::XmpWins
    } else {
        ExifConflictPolicy::ExifWins
    };

    let extractor = MetadataExtractor::new().with_policy(iptc_policy);
    let _ = extractor.conflicts(&blocks);
    let Ok(meta) = extractor.extract(&blocks) else {
        return;
    };
    let _ = meta.iptc();
    let _ = meta.provenance();
    map_both_ways(&meta, exif_policy);
    let _ = meta.to_blocks();
});

/// Splits a little-endian `u16` length off `data`, reading a missing byte as zero.
fn take_len(data: &[u8]) -> (usize, &[u8]) {
    let (head, rest) = data.split_at(data.len().min(2));
    let low = head.first().copied().unwrap_or(0);
    let high = head.get(1).copied().unwrap_or(0);
    (usize::from(u16::from_le_bytes([low, high])), rest)
}

/// Maps the parsed EXIF into the parsed XMP and back, each onto a copy, so both directions see the
/// carriers exactly as the file delivered them. A missing carrier is mapped from or into an empty
/// one, as a caller adding the other carrier to a file would.
fn map_both_ways(meta: &Metadata, policy: ExifConflictPolicy) {
    let exif = meta
        .exif
        .clone()
        .unwrap_or_else(|| Exif::new(ByteOrder::LittleEndian));
    let xmp = meta.xmp.clone().unwrap_or_default();

    let mut xmp_out = xmp.clone();
    exif_to_xmp(&exif, &mut xmp_out, policy);

    let mut exif_out = exif;
    xmp_to_exif(&xmp, &mut exif_out, policy);
}
