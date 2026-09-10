//! Strict "deconstruct" decoding: walk the whole TIFF and prove every byte is classified.
//!
//! Ordinary decoding reads the strips/tiles and tags it needs and ignores the rest. For archival
//! / critical use [`deconstruct`] drives the shared structural auditor ([`gamut_ifd::audit`],
//! issue #263) over the entire container — the header, every page IFD and image sub-IFD, every
//! strip/tile/free/embedded-JPEG byte range, and the Exif/GPS metadata sub-IFDs — then layers
//! TIFF tag knowledge on top. The resulting [`DeconstructReport`] carries the byte-level
//! [`SegmentReport`] (every byte typed, or precisely what was not — including the dual-ledger
//! parser cross-check) plus unknown field types, unknown/private tags, and out-of-spec codes.
//!
//! It is **collect-and-report, not fail-fast**: it errors only when the container itself is
//! unreadable (a malformed header or a truncated top-level chain), exactly as
//! [`gamut_ifd::read`] would.

use std::collections::BTreeMap;

use gamut_core::{ImageBuf, Result, Rgb8};
use gamut_ifd::{
    AuditFinding, Ifd, IfdReader, SegmentReport, SkipReason, SpanKind, StandardAuditSpec, Value,
    audit as ifd_audit,
};

use crate::compression::Compression;
use crate::decoder::TiffDecoder;
use crate::ifd::{PhotometricInterpretation, Predictor};
use crate::tags;

/// How serious a reported [`Anomaly`] is.
///
/// Non-exhaustive: further severities (e.g. an informational level) may be added without a
/// breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Severity {
    /// Out of spec but often benign (e.g. an image IFD carrying no pixel data).
    Warning,
    /// A structural defect (e.g. a strip-offset/byte-count mismatch or a sub-IFD cycle).
    Error,
}

/// A tag a valid TIFF would not be expected to carry — recognised structurally but not part of the
/// TIFF 6.0 tag set (see [`tags::is_known_tag`]).
///
/// Non-exhaustive: fields may be added as the deconstruct grows more precise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnknownTag {
    /// The page (top-level IFD index) the tag was found in.
    pub page: usize,
    /// The tag number.
    pub tag: u16,
    /// The tag's on-disk field-type code. Kept as the raw `u16` (not [`FieldType`](crate::FieldType))
    /// so that field types this crate does not recognise are still representable.
    pub field_type: u16,
    /// The tag's value count.
    pub count: u64,
}

/// An IFD entry whose field-type code is unrecognised. The entry is **preserved verbatim** in
/// the parse (as a [`gamut_ifd::Value::Unknown`]) — reported here because its out-of-line
/// payload, if any, is unsizable and therefore unclassifiable.
///
/// Non-exhaustive: fields may be added as the deconstruct grows more precise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnknownFieldType {
    /// The page (top-level IFD index) the entry was found in.
    pub page: usize,
    /// The entry's tag.
    pub tag: u16,
    /// The unrecognised on-disk field-type code.
    pub type_code: u16,
    /// The entry's declared value count (untrusted — the element size is unknown).
    pub count: u64,
}

/// A recognised but out-of-spec or unparsable element a deconstruct flags.
///
/// Non-exhaustive (as are its variants): the diagnostic taxonomy grows with the deconstruct.
/// The `detail` strings are human-readable diagnostics; their exact wording is not part of the
/// API contract — match on the variant and its typed fields instead.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Anomaly {
    /// A tag whose value uses a code this crate does not recognise (e.g. an unknown `Compression`).
    #[non_exhaustive]
    UnknownCode {
        /// The page the tag was found in.
        page: usize,
        /// The tag carrying the code.
        tag: u16,
        /// The unrecognised code.
        code: u32,
        /// A human-readable description (wording not contractual).
        detail: &'static str,
    },
    /// A known tag whose value could not be interpreted (wrong type, count, or range).
    #[non_exhaustive]
    UnparsableTag {
        /// The page the tag was found in.
        page: usize,
        /// The tag.
        tag: u16,
        /// A human-readable description (wording not contractual).
        detail: &'static str,
    },
    /// An out-of-spec or unexpected structural condition.
    #[non_exhaustive]
    Structure {
        /// The page the condition relates to.
        page: usize,
        /// A human-readable description (wording not contractual).
        detail: &'static str,
        /// How serious the condition is.
        severity: Severity,
    },
    /// A directory carrying more than one entry under the same tag.
    ///
    /// TIFF 6.0 §2 gives a directory one entry per tag, in ascending order. A repeated tag is
    /// therefore a directory no reader can resolve without choosing — and readers choose
    /// differently: this crate's model keeps the **last** occurrence, while libtiff marks every
    /// occurrence after the **first** to be ignored (`tif_dirread.c`, "Mark duplicates of any tag
    /// to be ignored") and warns that the directory is not sorted in ascending order. Which entry
    /// survives is therefore a property of the reader, not of the file.
    ///
    /// Named by **offset** rather than by page, because the directory may be a metadata sub-IFD
    /// several levels below one: the offset is the identity
    /// [`SpanKind::IfdBody`](gamut_ifd::SpanKind::IfdBody) already uses.
    #[non_exhaustive]
    DuplicateTag {
        /// The file offset of the directory holding the repeated tag.
        ifd: u64,
        /// The repeated tag.
        tag: u16,
        /// How many entries that directory carries under it.
        entries: usize,
        /// How serious the condition is — always [`Severity::Error`], as for every other
        /// structural defect: TIFF 6.0 §2 gives a directory one entry per tag, so a field the
        /// caller set is lost by at least one reader. Carried as a field rather than left
        /// implicit so that a caller triaging a report reads severity the same way off every
        /// variant that has one, and so that a later condition graded `Warning` needs no
        /// breaking change to say so.
        severity: Severity,
    },
}

/// The result of a strict deconstruct: byte-level classification plus TIFF-specific findings.
///
/// Non-exhaustive: report categories may be added without a breaking change.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DeconstructReport {
    /// The byte-level verdict: every byte of the file typed ([`SegmentReport::segments`]), or
    /// precisely what was not — unclassified ranges, conflicts, out-of-bounds claims, and the
    /// dual-ledger parser cross-check.
    pub segments: SegmentReport,
    /// IFD entries whose field-type code was unrecognised (preserved verbatim in the parse).
    pub unknown_fields: Vec<UnknownFieldType>,
    /// Tags not part of the recognised TIFF tag set.
    pub unknown_tags: Vec<UnknownTag>,
    /// Recognised-but-out-of-spec codes, unparsable tags, and structural problems.
    pub anomalies: Vec<Anomaly>,
}

impl DeconstructReport {
    /// Whether every byte of the file maps to exactly one typed segment (delegates to
    /// [`SegmentReport::is_fully_classified`] — zero tolerance, alignment padding included).
    /// Independent of whether every tag/code was recognised.
    #[must_use]
    pub fn is_fully_classified(&self) -> bool {
        self.segments.is_fully_classified()
    }

    /// Whether the file is pristine for archival: fully classified, with no unknown field
    /// types, no unknown tags, and no anomalies.
    #[must_use]
    pub fn is_fully_accounted(&self) -> bool {
        self.segments.is_fully_classified()
            && self.unknown_fields.is_empty()
            && self.unknown_tags.is_empty()
            && self.anomalies.is_empty()
    }
}

/// Walks `data` and returns a [`DeconstructReport`] classifying every byte.
///
/// # Errors
///
/// Returns [`gamut_core::Error::InvalidInput`] only when the container is unreadable (bad header,
/// truncated/looping IFD chain) — the same conditions under which [`gamut_ifd::read`] fails.
/// Unknown tags, unknown field types, out-of-spec codes, and unclassified bytes are reported, not
/// errored.
pub fn deconstruct(data: &[u8]) -> Result<DeconstructReport> {
    // The standard audit spec covers everything TIFF 6.0 locates structurally: the pointer tags
    // (SubIFDs/Exif/GPS/Interop) and the data extents (strips, tiles, free ranges, embedded
    // JPEG) — all u64-native.
    let audit = ifd_audit(data, &StandardAuditSpec)?;
    let mut findings = Findings::default();
    for (page, ifd) in audit.file.ifds.iter().enumerate() {
        findings.check_image_ifd(ifd, page);
    }
    findings.map_audit_findings(&audit.findings);
    findings.check_duplicate_tags(data, &audit.report);
    Ok(DeconstructReport {
        segments: audit.report,
        unknown_fields: findings.unknown_fields,
        unknown_tags: findings.unknown_tags,
        anomalies: findings.anomalies,
    })
}

/// Accumulates the TIFF-level findings over the audited tree.
#[derive(Default)]
struct Findings {
    unknown_fields: Vec<UnknownFieldType>,
    unknown_tags: Vec<UnknownTag>,
    anomalies: Vec<Anomaly>,
}

impl Findings {
    /// Checks an image IFD (a page or an image sub-IFD): its tags, codes, and pixel-structure
    /// coherence, then its image sub-IFDs. Metadata sub-IFDs (Exif/GPS/Interop) belong to a
    /// separate tag namespace, so they are followed (their bytes are already classified by the
    /// audit) but not tag-checked.
    fn check_image_ifd(&mut self, ifd: &Ifd, page: usize) {
        self.check_tags(ifd, page);
        self.check_codes(ifd, page);
        self.check_pixel_structure(ifd, page);
        for group in ifd.sub_ifds() {
            if group.tag == tags::SUB_IFDS {
                for child in &group.ifds {
                    self.check_image_ifd(child, page);
                }
            }
        }
    }

    /// Flags unknown-type entries (preserved verbatim by the parse) and every field whose tag
    /// is not part of the recognised TIFF tag set.
    fn check_tags(&mut self, ifd: &Ifd, page: usize) {
        for field in ifd.fields() {
            if let Value::Unknown(u) = &field.value {
                self.unknown_fields.push(UnknownFieldType {
                    page,
                    tag: field.tag,
                    type_code: u.type_code(),
                    count: u.count(),
                });
            } else if !tags::is_known_tag(field.tag) {
                self.unknown_tags.push(UnknownTag {
                    page,
                    tag: field.tag,
                    field_type: field.value.type_code(),
                    count: field.value.count(),
                });
            }
        }
    }

    /// Flags `Compression`, `PhotometricInterpretation`, and `Predictor` values whose code this
    /// crate does not recognise.
    fn check_codes(&mut self, ifd: &Ifd, page: usize) {
        if let Some(code) = ifd.get_u32(tags::COMPRESSION)
            && Compression::try_from(code).is_err()
        {
            self.anomalies.push(Anomaly::UnknownCode {
                page,
                tag: tags::COMPRESSION,
                code,
                detail: "TIFF: unrecognised Compression code",
            });
        }
        if let Some(code) = ifd.get_u32(tags::PHOTOMETRIC_INTERPRETATION)
            && PhotometricInterpretation::try_from(code).is_err()
        {
            self.anomalies.push(Anomaly::UnknownCode {
                page,
                tag: tags::PHOTOMETRIC_INTERPRETATION,
                code,
                detail: "TIFF: unrecognised PhotometricInterpretation code",
            });
        }
        if let Some(code) = ifd.get_u32(tags::PREDICTOR)
            && Predictor::try_from(code).is_err()
        {
            self.anomalies.push(Anomaly::UnknownCode {
                page,
                tag: tags::PREDICTOR,
                code,
                detail: "TIFF: unrecognised Predictor code",
            });
        }
    }

    /// Checks the pixel-data structure: the strip/tile extents themselves are claimed by the
    /// audit; this validates their *coherence* (pair present, lengths matching) and flags an
    /// image IFD with no pixel data at all.
    fn check_pixel_structure(&mut self, ifd: &Ifd, page: usize) {
        if ifd.get(tags::TILE_WIDTH).is_some() || ifd.get(tags::TILE_OFFSETS).is_some() {
            self.check_pair(
                ifd,
                page,
                tags::TILE_OFFSETS,
                tags::TILE_BYTE_COUNTS,
                "TIFF: TileOffsets/TileByteCounts length mismatch",
                "TIFF: tiled image missing TileOffsets/TileByteCounts",
            );
        } else if ifd.get(tags::STRIP_OFFSETS).is_some() {
            self.check_pair(
                ifd,
                page,
                tags::STRIP_OFFSETS,
                tags::STRIP_BYTE_COUNTS,
                "TIFF: StripOffsets/StripByteCounts length mismatch",
                "TIFF: image missing StripOffsets/StripByteCounts",
            );
        } else if ifd.get_u32(tags::IMAGE_WIDTH).is_some() {
            self.anomalies.push(Anomaly::Structure {
                page,
                detail: "TIFF: image IFD has no strip or tile data",
                severity: Severity::Warning,
            });
        }
    }

    /// Validates an `(offsets, byte counts)` tag pair, u64-native.
    fn check_pair(
        &mut self,
        ifd: &Ifd,
        page: usize,
        off_tag: u16,
        cnt_tag: u16,
        mismatch: &'static str,
        missing: &'static str,
    ) {
        let (Some(offsets), Some(counts)) = (ifd.get_u64_vec(off_tag), ifd.get_u64_vec(cnt_tag))
        else {
            self.anomalies.push(Anomaly::Structure {
                page,
                detail: missing,
                severity: Severity::Error,
            });
            return;
        };
        if offsets.len() != counts.len() {
            self.anomalies.push(Anomaly::Structure {
                page,
                detail: mismatch,
                severity: Severity::Error,
            });
        }
    }

    /// Maps the audit walk's lenient findings onto this crate's anomaly taxonomy.
    /// Flags every directory that carries two entries under one tag.
    ///
    /// This re-reads the **raw** entry records rather than consulting the parsed tree, and that is
    /// the whole point: [`Ifd`] is a directory model, so by the time the tree exists a duplicated
    /// tag has already collapsed to its last occurrence and the defect is invisible there. It was
    /// invisible here too, and that is why a round trip through this crate could not see a writer
    /// that emitted a field and a sub-IFD group under one tag — the report graded such a file
    /// clean.
    ///
    /// The directories to re-read are the ones the audit says it walked
    /// ([`SpanKind::IfdBody`](gamut_ifd::SpanKind::IfdBody)), so this is a second pass over bytes
    /// already claimed rather than a second walk of the pointer graph. A directory the audit
    /// reached parses again by construction; one that does not is already reported by the audit's
    /// own finding, so it is skipped here rather than reported twice.
    fn check_duplicate_tags(&mut self, data: &[u8], report: &SegmentReport) {
        let Ok(mut reader) = IfdReader::open(data) else {
            return;
        };
        for segment in &report.segments {
            let SpanKind::IfdBody { ifd } = segment.kind else {
                continue;
            };
            let Ok(raw) = reader.read_ifd(ifd) else {
                continue;
            };
            let mut counts: BTreeMap<u16, usize> = BTreeMap::new();
            for entry in &raw.entries {
                *counts.entry(entry.tag).or_default() += 1;
            }
            for (tag, entries) in counts {
                if entries > 1 {
                    self.anomalies.push(Anomaly::DuplicateTag {
                        ifd,
                        tag,
                        entries,
                        severity: Severity::Error,
                    });
                }
            }
        }
    }

    fn map_audit_findings(&mut self, findings: &[AuditFinding]) {
        for finding in findings {
            match *finding {
                AuditFinding::SkippedSubIfd { page, reason, .. } => {
                    self.anomalies.push(Anomaly::Structure {
                        page,
                        detail: match reason {
                            SkipReason::Cycle => "TIFF: sub-IFD offset revisited (possible cycle)",
                            SkipReason::TooDeep => "TIFF: sub-IFD nesting too deep",
                            _ => "TIFF: sub-IFD could not be parsed",
                        },
                        severity: Severity::Error,
                    });
                }
                AuditFinding::ChainedSubIfd { page, .. } => {
                    self.anomalies.push(Anomaly::Structure {
                        page,
                        detail: "TIFF: sub-IFD carries a next-IFD chain",
                        severity: Severity::Warning,
                    });
                }
                // The finding taxonomy may grow; unrecognised findings are not anomalies.
                _ => {}
            }
        }
    }
}

impl TiffDecoder {
    /// Strictly deconstructs `data`, returning a [`DeconstructReport`] that classifies every
    /// byte and flags anything unrecognised — without decoding pixels.
    ///
    /// # Errors
    ///
    /// Returns [`gamut_core::Error`] only when the container itself is unreadable (see
    /// [`deconstruct`]).
    pub fn deconstruct(&self, data: &[u8]) -> Result<DeconstructReport> {
        deconstruct(data)
    }

    /// Decodes page `page` to [`Rgb8`] *and* returns a whole-file [`DeconstructReport`].
    ///
    /// The report covers the entire file (every page), independent of which page is decoded.
    ///
    /// # Errors
    ///
    /// Returns [`gamut_core::Error`] if the container is unreadable or the requested page cannot be
    /// decoded (e.g. an unsupported pixel mode) — a strict superset of [`TiffDecoder::decode_page`].
    pub fn deconstruct_page(
        &self,
        data: &[u8],
        page: usize,
    ) -> Result<(ImageBuf<Rgb8>, DeconstructReport)> {
        let report = deconstruct(data)?;
        let image = self.decode_page(data, page)?;
        Ok((image, report))
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::{Dimensions, EncodeImage, Gray8, ImageRef};
    use gamut_ifd::{ByteOrder, Value, Variant};

    use super::*;
    use crate::encoder::TiffEncoder;
    use crate::writer::{write_image, write_multipage};

    /// Asserts the structural walk classified the whole file — zero tolerance: alignment
    /// padding must come back as typed `Padding` segments, not tolerated gaps.
    fn assert_clean_coverage(r: &DeconstructReport) {
        // Through the public wrapper, not `r.segments` directly. Reaching past it left
        // `DeconstructReport::is_fully_classified` asserted by nothing, so it could be replaced
        // with either constant and the suite stayed green (#110).
        assert!(r.is_fully_classified(), "not fully classified: {r:?}");
        assert!(r.unknown_fields.is_empty(), "unknown fields: {r:?}");
    }

    /// A minimal 2×2 8-bit BlackIsZero grayscale image IFD (the strip tags are filled in by the
    /// writer).
    fn image_ifd() -> Ifd {
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![2]));
        ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![2]));
        ifd.set(tags::BITS_PER_SAMPLE, Value::Short(vec![8]));
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![1]));
        ifd.set(tags::SAMPLES_PER_PIXEL, Value::Short(vec![1]));
        ifd.set(tags::ROWS_PER_STRIP, Value::Short(vec![2]));
        ifd
    }

    #[test]
    fn encoded_gray_strip_image_is_accounted() {
        for order in [ByteOrder::LittleEndian, ByteOrder::BigEndian] {
            let dims = Dimensions {
                width: 5,
                height: 3,
            };
            let pixels: Vec<u8> = (0..15).collect();
            let mut tiff = Vec::new();
            TiffEncoder::new()
                .with_byte_order(order)
                .encode_image(ImageRef::<Gray8>::new(&pixels, dims).unwrap(), &mut tiff)
                .expect("encode");
            let report = deconstruct(&tiff).expect("deconstruct");
            assert_clean_coverage(&report);
            assert!(report.unknown_tags.is_empty(), "{report:?}");
            assert!(report.anomalies.is_empty(), "{report:?}");
            assert!(report.is_fully_accounted(), "{report:?}");
        }
    }

    #[test]
    fn flags_a_directory_that_repeats_a_tag() {
        // The report is this crate's own judge, and it graded a duplicated tag clean — which is
        // why a round trip could not see a writer that emitted a field and a sub-IFD group under
        // one tag. `Ifd` collapses a repeated tag to its last occurrence, so the defect exists
        // only in the raw entry records; libtiff keeps the *first* instead, so which entry
        // survives is a property of the reader rather than of the file.
        //
        // Both sides are asserted, because the anomaly must not fire on a conforming directory:
        // the same image without the extra entry is graded fully accounted.
        let mut child = Ifd::new();
        child.set(1, Value::Ascii("R98".into()));
        let mut plain = image_ifd();
        plain.set_sub_ifd(tags::SUB_IFDS, vec![child.clone()]);
        let clean = write_image(
            ByteOrder::LittleEndian,
            Variant::Classic,
            &plain,
            &[vec![0u8; 4]],
        )
        .expect("write");
        let report = deconstruct(&clean).expect("deconstruct");
        assert!(
            !report
                .anomalies
                .iter()
                .any(|a| matches!(a, Anomaly::DuplicateTag { .. })),
            "one entry per tag is a conforming directory: {report:?}"
        );

        let mut repeated = image_ifd();
        repeated.set(tags::SUB_IFDS, Value::Short(vec![7]));
        repeated.set_sub_ifd(tags::SUB_IFDS, vec![child]);
        let duplicated = write_image(
            ByteOrder::LittleEndian,
            Variant::Classic,
            &repeated,
            &[vec![0u8; 4]],
        )
        .expect("write");
        let report = deconstruct(&duplicated).expect("deconstruct");
        assert!(
            report.anomalies.iter().any(|a| matches!(
                a,
                Anomaly::DuplicateTag { tag, entries, severity: Severity::Error, .. }
                    if *tag == tags::SUB_IFDS && *entries == 2
            )),
            "two entries under one tag must be graded: {report:?}"
        );
        assert!(!report.is_fully_accounted(), "{report:?}");
    }

    #[test]
    fn flags_unknown_private_tag() {
        let mut ifd = image_ifd();
        ifd.set(0x9999, Value::Long(vec![42])); // private/unknown tag, inline LONG
        let bytes = write_image(
            ByteOrder::LittleEndian,
            Variant::Classic,
            &ifd,
            &[vec![0u8; 4]],
        )
        .expect("write");
        let report = deconstruct(&bytes).expect("deconstruct");
        assert_clean_coverage(&report); // the unknown tag's bytes are still classified
        assert!(
            report
                .unknown_tags
                .iter()
                .any(|u| u.tag == 0x9999 && u.page == 0),
            "{report:?}"
        );
        // Unknown tags fail the strict verdict but not the byte-classification one.
        assert!(!report.is_fully_accounted());
    }

    #[test]
    fn flags_unknown_field_type_entry() {
        // An entry with an unrecognised field-type code (0xF0): preserved verbatim by the parse,
        // reported here — and, its (hypothetical) out-of-line payload being unsizable, the file
        // still classifies fully because this word is inline-plausible garbage pointing nowhere.
        let mut bytes = write_image(
            ByteOrder::LittleEndian,
            Variant::Classic,
            &image_ifd(),
            &[vec![0u8; 4]],
        )
        .expect("write");
        // Patch the first entry's type code (bytes 10..12 hold IFD0's first entry tag; type at
        // +2). Locate the first entry: header(8) + count(2) => entry at 10, type at 12.
        bytes[12] = 0xF0;
        bytes[13] = 0x00;
        let report = deconstruct(&bytes).expect("deconstruct");
        assert_eq!(report.unknown_fields.len(), 1, "{report:?}");
        assert_eq!(report.unknown_fields[0].type_code, 0xF0);
        assert!(!report.is_fully_accounted());
    }

    #[test]
    fn flags_unknown_compression_code() {
        let mut ifd = image_ifd();
        ifd.set(tags::COMPRESSION, Value::Short(vec![999])); // not a recognised Compression code
        let bytes = write_image(
            ByteOrder::LittleEndian,
            Variant::Classic,
            &ifd,
            &[vec![0u8; 4]],
        )
        .expect("write");
        let report = deconstruct(&bytes).expect("deconstruct");
        assert!(
            report.anomalies.iter().any(|a| matches!(
                a,
                Anomaly::UnknownCode { tag, code, .. }
                    if *tag == tags::COMPRESSION && *code == 999
            )),
            "{report:?}"
        );
    }

    /// An IFD that claims to be tiled but carries no tile data is diagnosed *as tiled*.
    ///
    /// `check_pixel_structure` enters the tile branch on `TileWidth` **or** `TileOffsets`, and the
    /// `||` is the whole point: a half-present pair is exactly the malformation the audit exists to
    /// name. Mutated to `&&`, an IFD with only `TileWidth` falls through to the strip branch and
    /// then to the generic "no strip or tile data" warning -- a different, vaguer diagnosis of the
    /// same file, and a `Warning` where the truth is an `Error`.
    ///
    /// Every tile fixture in the suite was well-formed, so nothing distinguished the two (#110).
    #[test]
    fn flags_a_tiled_ifd_that_carries_no_tile_data() {
        let mut ifd = image_ifd();
        ifd.set(tags::TILE_WIDTH, Value::Short(vec![16]));
        // Deliberately no TileOffsets/TileByteCounts, and no strip offsets either: the IFD says
        // "tiled" and then provides nothing.
        let bytes = gamut_ifd::write(&gamut_ifd::TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        })
        .expect("write");

        let report = deconstruct(&bytes).expect("deconstruct");
        assert!(
            report.anomalies.iter().any(|a| matches!(
                a,
                Anomaly::Structure {
                    detail: "TIFF: tiled image missing TileOffsets/TileByteCounts",
                    severity: Severity::Error,
                    ..
                }
            )),
            "expected the tiled diagnosis, got {report:?}"
        );
    }

    /// A self-pointing sub-IFD is diagnosed as a cycle, not merely as unparseable.
    ///
    /// `map_audit_findings` translates the audit walk's findings into this crate's anomaly
    /// taxonomy, and until these three tests nothing produced an `AuditFinding` at all -- the
    /// whole function could be replaced with `()` and the suite stayed green (#110, #490). The
    /// detail string is asserted rather than the variant, because deleting the `Cycle` arm falls
    /// through to the generic "could not be parsed" and stays an `Anomaly::Structure` either way.
    #[test]
    fn diagnoses_a_sub_ifd_cycle() {
        // Root @8 points at the child @26, whose own SubIFDs pointer aims back at 26.
        let data: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let report = deconstruct(data).expect("deconstruct");
        assert!(
            report.anomalies.iter().any(|a| matches!(
                a,
                Anomaly::Structure { detail, severity: Severity::Error, .. }
                    if detail.contains("possible cycle")
            )),
            "{report:?}"
        );
    }

    /// A sub-IFD carrying a next-IFD chain is out of spec, and only a warning.
    #[test]
    fn diagnoses_a_chained_sub_ifd() {
        let data: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, IFD0 @ 8
            0x01, 0x00, //
            0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00,
            0x00, // 330 -> 26
            0x00, 0x00, 0x00, 0x00, //
            0x01, 0x00, // child A @ 26
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, //
            0x2c, 0x00, 0x00, 0x00, // next = 44, out of spec for a sub-IFD
            0x01, 0x00, // child B @ 44
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00,
        ];
        let report = deconstruct(data).expect("deconstruct");
        assert!(
            report.anomalies.iter().any(|a| matches!(
                a,
                Anomaly::Structure { detail, severity: Severity::Warning, .. }
                    if detail.contains("next-IFD chain")
            )),
            "{report:?}"
        );
    }

    /// Nesting past the audit's depth guard is diagnosed as too deep, not as a parse failure.
    #[test]
    fn diagnoses_sub_ifd_nesting_that_is_too_deep() {
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![1]));
        for _ in 0..20 {
            let mut parent = Ifd::new();
            parent.set_sub_ifd(tags::SUB_IFDS, vec![ifd]);
            ifd = parent;
        }
        let bytes = gamut_ifd::write(&gamut_ifd::TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        })
        .expect("write");
        let report = deconstruct(&bytes).expect("deconstruct");
        assert!(
            report.anomalies.iter().any(|a| matches!(
                a,
                Anomaly::Structure { detail, severity: Severity::Error, .. }
                    if detail.contains("too deep")
            )),
            "{report:?}"
        );
    }

    #[test]
    fn flags_strip_offset_count_mismatch() {
        // Two offsets but one byte count: a structural defect the deconstruct must surface.
        let mut ifd = image_ifd();
        ifd.set(tags::STRIP_OFFSETS, Value::Long(vec![1000, 2000]));
        ifd.set(tags::STRIP_BYTE_COUNTS, Value::Long(vec![4]));
        // Write with no real strips so the writer does not overwrite our deliberate mismatch.
        let bytes = gamut_ifd::write(&gamut_ifd::TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        })
        .expect("write");
        let report = deconstruct(&bytes).expect("deconstruct");
        assert!(
            report.anomalies.iter().any(|a| matches!(
                a,
                Anomaly::Structure {
                    severity: Severity::Error,
                    ..
                }
            )),
            "{report:?}"
        );
        // The other half of the verdict, and the only fixture in the suite that reaches it: the
        // strips are declared at offset 1000 in a file far shorter than that, so the walk records
        // an out-of-bounds segment and the archival claim is false. Without this the *negative*
        // side of `is_fully_classified` was never observed at all.
        assert!(
            !report.is_fully_classified(),
            "a strip past EOF must fail the archival verdict: {report:?}"
        );
        assert_eq!(report.segments.out_of_bounds.len(), 1, "{report:?}");
    }

    /// Two entries legitimately sharing one out-of-line value (TIFF permits it) is informational
    /// sharing, not an overlap conflict.
    #[test]
    fn shared_out_of_line_value_is_not_a_conflict() {
        // Hand-build: two LONG[2] entries whose value offsets both point at the same 8 bytes.
        let data: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, IFD0 @ 8
            0x02, 0x00, // 2 entries
            0x10, 0x27, 0x04, 0x00, 0x02, 0x00, 0x00, 0x00, 0x26, 0x00, 0x00,
            0x00, // tag 10000 LONG[2] @ 38
            0x11, 0x27, 0x04, 0x00, 0x02, 0x00, 0x00, 0x00, 0x26, 0x00, 0x00,
            0x00, // tag 10001 LONG[2] @ 38 (shared)
            0x00, 0x00, 0x00, 0x00, // next = 0
            0x2a, 0x00, 0x00, 0x00, 0x2b, 0x00, 0x00, 0x00, // the shared value @ 38
        ];
        let report = deconstruct(data).expect("deconstruct");
        assert!(report.segments.conflicts.is_empty(), "{report:?}");
        assert_eq!(report.segments.shared.len(), 1, "{report:?}");
        assert!(report.segments.is_fully_classified(), "{report:?}");
    }

    #[test]
    fn multipage_is_accounted() {
        let pages = [
            (image_ifd(), vec![vec![0u8; 4]]),
            (image_ifd(), vec![vec![1u8; 4]]),
        ];
        let bytes =
            write_multipage(ByteOrder::LittleEndian, Variant::Classic, &pages).expect("write");
        let report = deconstruct(&bytes).expect("deconstruct");
        assert_clean_coverage(&report);
        assert!(report.unknown_tags.is_empty(), "{report:?}");
        assert!(report.anomalies.is_empty(), "{report:?}");
    }

    #[test]
    fn follows_and_accounts_an_exif_subifd() {
        // The writer lays out an Exif sub-IFD reached through the ExifIFD pointer; the deconstruct
        // must follow it and account its bytes, and must NOT mis-flag its EXIF-namespace tag
        // (ExposureTime, 33434) as an unknown TIFF tag.
        let mut exif = Ifd::new();
        exif.set(33434, Value::Rational(vec![(1, 100)])); // ExposureTime — EXIF namespace
        let mut ifd = image_ifd();
        ifd.set_sub_ifd(tags::EXIF_IFD, vec![exif]);
        let bytes = write_image(
            ByteOrder::LittleEndian,
            Variant::Classic,
            &ifd,
            &[vec![0u8; 4]],
        )
        .expect("write");
        let report = deconstruct(&bytes).expect("deconstruct");
        assert_clean_coverage(&report);
        assert!(report.unknown_tags.is_empty(), "{report:?}");
        assert!(report.anomalies.is_empty(), "{report:?}");
    }

    #[test]
    fn image_ifd_without_pixel_data_warns() {
        // An image IFD (has ImageWidth) with neither strips nor tiles is flagged as a warning.
        let mut ifd = Ifd::new();
        ifd.set(tags::IMAGE_WIDTH, Value::Short(vec![2]));
        ifd.set(tags::IMAGE_LENGTH, Value::Short(vec![2]));
        let bytes = gamut_ifd::write(&gamut_ifd::TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![ifd],
        })
        .expect("write");
        let report = deconstruct(&bytes).expect("deconstruct");
        assert!(
            report.anomalies.iter().any(|a| matches!(
                a,
                Anomaly::Structure {
                    severity: Severity::Warning,
                    ..
                }
            )),
            "{report:?}"
        );
    }
}
