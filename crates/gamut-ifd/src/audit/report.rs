//! The strict-deconstruct report a TIFF-family codec builds on top of an [`audit`](crate::audit).
//!
//! The audit walk classifies bytes; a codec then layers its own tag knowledge on top — which tags
//! and codes it recognises, which colour matrices are well formed — and reports what it found in
//! these types. They are shared so that every TIFF-based codec (gamut-tiff, gamut-dng) reports in
//! one vocabulary and a consumer renders them once. Which checks run stays the codec's own
//! business: TIFF checks `Predictor` codes and DNG does not, DNG checks colour matrices and TIFF
//! does not.

use crate::segment::SegmentReport;

/// How serious a reported [`Anomaly`] is.
///
/// Non-exhaustive: further severities (e.g. an informational level) may be added without a
/// breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Severity {
    /// Out of spec but often benign (e.g. an image IFD carrying no pixel data).
    Warning,
    /// A structural defect (e.g. a strip-offset/byte-count mismatch, a malformed matrix, or a
    /// sub-IFD cycle).
    Error,
}

/// A tag a valid file would not be expected to carry — recognised structurally but not part of
/// the reporting codec's tag set.
///
/// Non-exhaustive: fields may be added as the deconstruct grows more precise; build one with
/// [`UnknownTag::new`].
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

impl UnknownTag {
    /// An unknown `tag` found on `page`, carrying `count` values of on-disk type `field_type`.
    #[must_use]
    pub fn new(page: usize, tag: u16, field_type: u16, count: u64) -> Self {
        Self {
            page,
            tag,
            field_type,
            count,
        }
    }
}

/// An IFD entry whose field-type code is unrecognised. The entry is **preserved verbatim** in
/// the parse (as a [`Value::Unknown`](crate::Value::Unknown)) — reported here because its
/// out-of-line payload, if any, is unsizable and therefore unclassifiable.
///
/// Non-exhaustive: fields may be added as the deconstruct grows more precise; build one with
/// [`UnknownFieldType::new`].
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

impl UnknownFieldType {
    /// An entry under `tag` on `page` whose field-type code `type_code` is unrecognised, declaring
    /// `count` values.
    #[must_use]
    pub fn new(page: usize, tag: u16, type_code: u16, count: u64) -> Self {
        Self {
            page,
            tag,
            type_code,
            count,
        }
    }
}

/// A recognised but out-of-spec or unparsable element a deconstruct flags.
///
/// Non-exhaustive (as are its variants): the diagnostic taxonomy grows with the deconstruct, and
/// a codec builds each variant through its constructor ([`Anomaly::unknown_code`],
/// [`Anomaly::unparsable_tag`], [`Anomaly::structure`], [`Anomaly::duplicate_tag`]). The `detail`
/// strings are human-readable diagnostics; their exact wording is not part of the API contract —
/// match on the variant and its typed fields instead.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Anomaly {
    /// A tag whose value uses a code the codec does not recognise (e.g. an unknown `Compression`).
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
    /// TIFF 6.0 §2 — which every TIFF-family format builds on — gives a directory one entry per
    /// tag, in ascending order. A repeated tag is therefore a directory no reader can resolve
    /// without choosing — and readers choose differently: this crate's model keeps the **last**
    /// occurrence, while libtiff marks every occurrence after the **first** to be ignored
    /// (`tif_dirread.c`, "Mark duplicates of any tag to be ignored") and warns that the directory
    /// is not sorted in ascending order. Which entry survives is therefore a property of the
    /// reader, not of the file.
    ///
    /// Named by **offset** rather than by page, because the directory may be a metadata sub-IFD
    /// several levels below one, or an embedded camera-profile directory: the offset is the
    /// identity [`SpanKind::IfdBody`](crate::SpanKind::IfdBody) already uses.
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

impl Anomaly {
    /// An [`Anomaly::UnknownCode`]: `tag` on `page` carries the unrecognised `code`.
    #[must_use]
    pub fn unknown_code(page: usize, tag: u16, code: u32, detail: &'static str) -> Self {
        Self::UnknownCode {
            page,
            tag,
            code,
            detail,
        }
    }

    /// An [`Anomaly::UnparsableTag`]: `tag` on `page` could not be interpreted.
    #[must_use]
    pub fn unparsable_tag(page: usize, tag: u16, detail: &'static str) -> Self {
        Self::UnparsableTag { page, tag, detail }
    }

    /// An [`Anomaly::Structure`] of `severity` relating to `page`.
    #[must_use]
    pub fn structure(page: usize, detail: &'static str, severity: Severity) -> Self {
        Self::Structure {
            page,
            detail,
            severity,
        }
    }

    /// An [`Anomaly::DuplicateTag`]: the directory at offset `ifd` carries `entries` entries under
    /// `tag`. Its severity is [`Severity::Error`], as the variant documents.
    #[must_use]
    pub fn duplicate_tag(ifd: u64, tag: u16, entries: usize) -> Self {
        Self::DuplicateTag {
            ifd,
            tag,
            entries,
            severity: Severity::Error,
        }
    }
}

/// The result of a strict deconstruct: byte-level classification plus the codec's own findings.
///
/// Non-exhaustive: report categories may be added without a breaking change; build one with
/// [`DeconstructReport::new`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DeconstructReport {
    /// The byte-level verdict: every byte of the file typed ([`SegmentReport::segments`]), or
    /// precisely what was not — unclassified ranges, conflicts, out-of-bounds claims, and the
    /// dual-ledger parser cross-check.
    pub segments: SegmentReport,
    /// IFD entries whose field-type code was unrecognised (preserved verbatim in the parse).
    pub unknown_fields: Vec<UnknownFieldType>,
    /// Tags not part of the reporting codec's recognised tag set.
    pub unknown_tags: Vec<UnknownTag>,
    /// Recognised-but-out-of-spec codes, unparsable tags, and structural problems.
    pub anomalies: Vec<Anomaly>,
}

impl DeconstructReport {
    /// A report over the byte-level verdict `segments` and the codec's findings.
    #[must_use]
    pub fn new(
        segments: SegmentReport,
        unknown_fields: Vec<UnknownFieldType>,
        unknown_tags: Vec<UnknownTag>,
        anomalies: Vec<Anomaly>,
    ) -> Self {
        Self {
            segments,
            unknown_fields,
            unknown_tags,
            anomalies,
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A byte-level verdict over an empty file: nothing claimed, nothing left over.
    fn clean() -> SegmentReport {
        SegmentReport {
            file_len: 0,
            segments: Vec::new(),
            unclassified: Vec::new(),
            conflicts: Vec::new(),
            shared: Vec::new(),
            out_of_bounds: Vec::new(),
            unclaimed_reads: Vec::new(),
            unread_claims: Vec::new(),
        }
    }

    /// The same verdict with one unclassified byte.
    fn unclassified() -> SegmentReport {
        SegmentReport {
            file_len: 1,
            unclassified: core::iter::once(0..1).collect(),
            ..clean()
        }
    }

    #[test]
    fn classification_follows_the_byte_level_verdict_alone() {
        let report = DeconstructReport::new(clean(), Vec::new(), Vec::new(), Vec::new());
        assert!(report.is_fully_classified());
        let report = DeconstructReport::new(unclassified(), Vec::new(), Vec::new(), Vec::new());
        assert!(!report.is_fully_classified());
        // Findings above the byte level do not move it.
        let report = DeconstructReport::new(
            clean(),
            vec![UnknownFieldType::new(0, 1, 0xF0, 1)],
            vec![UnknownTag::new(0, 2, 3, 1)],
            vec![Anomaly::unparsable_tag(0, 3, "x")],
        );
        assert!(report.is_fully_classified());
    }

    /// Each of the four conditions of the archival verdict fails it on its own.
    #[test]
    fn accounting_needs_every_category_empty() {
        let pristine = DeconstructReport::new(clean(), Vec::new(), Vec::new(), Vec::new());
        assert!(pristine.is_fully_accounted());
        let cases = [
            DeconstructReport::new(unclassified(), Vec::new(), Vec::new(), Vec::new()),
            DeconstructReport::new(
                clean(),
                vec![UnknownFieldType::new(0, 1, 0xF0, 1)],
                Vec::new(),
                Vec::new(),
            ),
            DeconstructReport::new(
                clean(),
                Vec::new(),
                vec![UnknownTag::new(0, 2, 3, 1)],
                Vec::new(),
            ),
            DeconstructReport::new(
                clean(),
                Vec::new(),
                Vec::new(),
                vec![Anomaly::structure(0, "x", Severity::Warning)],
            ),
        ];
        for report in cases {
            assert!(!report.is_fully_accounted(), "{report:?}");
        }
    }

    /// The constructors place every argument in its own field.
    #[test]
    fn constructors_fill_their_named_fields() {
        let tag = UnknownTag::new(1, 2, 3, 4);
        assert_eq!((tag.page, tag.tag, tag.field_type, tag.count), (1, 2, 3, 4));
        let field = UnknownFieldType::new(5, 6, 7, 8);
        assert_eq!(
            (field.page, field.tag, field.type_code, field.count),
            (5, 6, 7, 8)
        );
        assert_eq!(
            Anomaly::unknown_code(1, 259, 999, "d"),
            Anomaly::UnknownCode {
                page: 1,
                tag: 259,
                code: 999,
                detail: "d"
            }
        );
        assert_eq!(
            Anomaly::unparsable_tag(2, 50721, "m"),
            Anomaly::UnparsableTag {
                page: 2,
                tag: 50721,
                detail: "m"
            }
        );
        assert_eq!(
            Anomaly::structure(3, "s", Severity::Warning),
            Anomaly::Structure {
                page: 3,
                detail: "s",
                severity: Severity::Warning
            }
        );
        assert_eq!(
            Anomaly::duplicate_tag(8, 330, 2),
            Anomaly::DuplicateTag {
                ifd: 8,
                tag: 330,
                entries: 2,
                severity: Severity::Error
            }
        );
    }
}
