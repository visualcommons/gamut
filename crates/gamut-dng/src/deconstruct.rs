//! Strict "deconstruct" decoding: walk the whole DNG and prove every byte is classified.
//!
//! This is the archival counterpart to [`crate::decoder`], filed as issue #197 from the PR #161
//! review and rebuilt on the shared structural auditor for issue #263. Ordinary decoding reads
//! the raw image, profile, and metadata it needs and ignores the rest. [`deconstruct`] instead
//! drives [`gamut_ifd::audit`] over the entire IFD tree — IFD 0 and its chain, the `SubIFDs`
//! image sub-IFDs (including the full-resolution raw), the Exif/GPS/Interoperability metadata
//! sub-IFDs, and every strip/tile/free/embedded-JPEG extent — then layers DNG tag knowledge on
//! top. The resulting [`DeconstructReport`] carries the byte-level [`SegmentReport`] (every byte
//! typed, or precisely what was not — including the dual-ledger parser cross-check) plus unknown
//! field types, unknown/private tags, out-of-spec codes, and malformed colour matrices.
//!
//! It is **collect-and-report, not fail-fast**: it errors only when the container itself is
//! unreadable, exactly as [`gamut_ifd::read`] would; everything else is reported for the caller
//! to judge.

use std::collections::BTreeMap;

use gamut_core::{Error, Result};
use gamut_ifd::{
    AuditFinding, Auditor, ByteOrder, Claim, DataLabel, Ifd, SegmentReport, SkipReason, SpanKind,
    StandardAuditSpec, Value, Variant,
};

use crate::decoder::{DecodedDng, DngDecoder};
use crate::tags;
use crate::values::{Compression, PhotometricInterpretation};

/// The magic number of an embedded DNG camera-profile stream (the `.dcp` form): the two bytes
/// after the byte-order mark, `0x4352` (`"RC"` little-endian) where a TIFF header carries `42`.
const CAMERA_PROFILE_MAGIC: u16 = 0x4352;

/// How serious a reported [`Anomaly`] is.
///
/// Non-exhaustive: further severities may be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Severity {
    /// Out of spec but often benign.
    Warning,
    /// A structural defect (e.g. a strip mismatch, a malformed matrix, or a sub-IFD cycle).
    Error,
}

/// A tag a valid DNG would not be expected to carry — recognised structurally but not part of the
/// DNG 1.7.1 tag set (see [`tags::is_known_tag`]).
///
/// Non-exhaustive: fields may be added as the deconstruct grows more precise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnknownTag {
    /// The page (top-level IFD index) the tag was found in.
    pub page: usize,
    /// The tag number.
    pub tag: u16,
    /// The tag's on-disk field-type code.
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
    /// TIFF 6.0 §2 — which DNG 1.7.1 builds on — gives a directory one entry per tag. A repeated
    /// tag is a directory no reader resolves without choosing, and readers choose differently:
    /// the parse keeps the **last** occurrence, libtiff the first. Which entry survives is a
    /// property of the reader, not of the file.
    ///
    /// Named by **offset** rather than by page, because the directory may be a metadata sub-IFD
    /// or an embedded camera-profile directory rather than a page: the offset is the identity
    /// [`SpanKind::IfdBody`] already uses.
    #[non_exhaustive]
    DuplicateTag {
        /// The file offset of the directory holding the repeated tag.
        ifd: u64,
        /// The repeated tag.
        tag: u16,
        /// How many entries that directory carries under it.
        entries: usize,
        /// How serious the condition is — always [`Severity::Error`]: a field the writer set is
        /// lost by at least one reader.
        severity: Severity,
    },
}

/// The result of a strict deconstruct: byte-level classification plus DNG-specific findings.
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
    /// Tags not part of the recognised DNG tag set.
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

/// Walks `data` (a DNG file) and returns a [`DeconstructReport`] classifying every byte — without
/// decoding the raw image.
///
/// # Errors
///
/// Returns [`gamut_core::Error::InvalidInput`] only when the container is unreadable (bad header,
/// truncated/looping IFD chain) — the same conditions under which [`gamut_ifd::read`] fails.
/// Unknown tags, unknown field types, out-of-spec codes, malformed matrices, and unclassified
/// bytes are reported, not errored.
pub fn deconstruct(data: &[u8]) -> Result<DeconstructReport> {
    // The standard audit spec covers everything DNG locates through plain TIFF structure: the
    // pointer tags (SubIFDs/Exif/GPS/Interop) and the data extents (strips, tiles, free ranges,
    // embedded JPEG) — u64-native, so BigTIFF DNGs past 4 GiB account correctly. DNG's one
    // extra structural carrier — `ExtraCameraProfiles`, pointing at *embedded camera-profile
    // streams* rather than plain sub-IFDs — is walked below.
    let mut auditor = Auditor::new(data)?;
    let file = auditor.walk(&StandardAuditSpec)?;
    let mut findings = Findings::default();
    for (page, ifd) in file.ifds.iter().enumerate() {
        findings.check_image_ifd(ifd, page);
        findings.audit_camera_profiles(&mut auditor, ifd, page);
    }
    let (segments, audit_findings) = auditor.finish()?;
    findings.map_audit_findings(&audit_findings);
    Ok(DeconstructReport {
        segments,
        unknown_fields: findings.unknown_fields,
        unknown_tags: findings.unknown_tags,
        anomalies: findings.anomalies,
    })
}

/// Accumulates the DNG-level findings over the audited tree.
#[derive(Default)]
struct Findings {
    unknown_fields: Vec<UnknownFieldType>,
    unknown_tags: Vec<UnknownTag>,
    anomalies: Vec<Anomaly>,
}

impl Findings {
    /// Checks an image IFD (IFD 0, the raw sub-IFD, or a preview/image sub-IFD): its tags,
    /// codes, colour matrices, and pixel-structure coherence, then its image sub-IFDs. Metadata
    /// sub-IFDs (Exif/GPS/Interop) belong to a separate tag namespace, so they are followed
    /// (their bytes are already classified by the audit) but not tag-checked.
    fn check_image_ifd(&mut self, ifd: &Ifd, page: usize) {
        self.check_tags(ifd, page);
        self.check_codes(ifd, page);
        self.check_matrices(ifd, page);
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
    /// is not part of the recognised DNG tag set.
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

    /// Flags `Compression` and `PhotometricInterpretation` values whose code this crate does not
    /// recognise.
    fn check_codes(&mut self, ifd: &Ifd, page: usize) {
        if let Some(code) = ifd.get_u32(tags::COMPRESSION)
            && u16::try_from(code)
                .ok()
                .and_then(Compression::from_code)
                .is_none()
        {
            self.anomalies.push(Anomaly::UnknownCode {
                page,
                tag: tags::COMPRESSION,
                code,
                detail: "DNG: unrecognised Compression code",
            });
        }
        if let Some(code) = ifd.get_u32(tags::PHOTOMETRIC_INTERPRETATION)
            && u16::try_from(code)
                .ok()
                .and_then(PhotometricInterpretation::from_code)
                .is_none()
        {
            self.anomalies.push(Anomaly::UnknownCode {
                page,
                tag: tags::PHOTOMETRIC_INTERPRETATION,
                code,
                detail: "DNG: unrecognised PhotometricInterpretation code",
            });
        }
    }

    /// Flags 3×3 colour-calibration matrix tags that are present but not nine `(S)RATIONAL`s.
    fn check_matrices(&mut self, ifd: &Ifd, page: usize) {
        for &tag in tags::MATRIX_3X3_TAGS {
            let Some(value) = ifd.get(tag) else {
                continue;
            };
            let ok = matches!(value, Value::Rational(v) if v.len() == 9)
                || matches!(value, Value::SRational(v) if v.len() == 9);
            if !ok {
                self.anomalies.push(Anomaly::UnparsableTag {
                    page,
                    tag,
                    detail: "DNG: 3x3 matrix tag is not nine rationals",
                });
            }
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
                "DNG: TileOffsets/TileByteCounts length mismatch",
                "DNG: tiled image missing TileOffsets/TileByteCounts",
            );
        } else if ifd.get(tags::STRIP_OFFSETS).is_some() {
            self.check_pair(
                ifd,
                page,
                tags::STRIP_OFFSETS,
                tags::STRIP_BYTE_COUNTS,
                "DNG: StripOffsets/StripByteCounts length mismatch",
                "DNG: image missing StripOffsets/StripByteCounts",
            );
        } else if ifd.get_u32(tags::IMAGE_WIDTH).is_some() {
            self.anomalies.push(Anomaly::Structure {
                page,
                detail: "DNG: image IFD has no strip or tile data",
                severity: Severity::Warning,
            });
        }
    }

    /// Walks the embedded camera-profile streams `ExtraCameraProfiles` (50933) points at.
    ///
    /// Each target is a `.dcp`-form stream — a byte-order mark, the profile magic `0x4352`
    /// where a TIFF header carries `42`, and a first-IFD offset, followed by a directory whose
    /// internal offsets are relative to the **stream start** — so it is audited as an embedded
    /// rebased chain, its bytes claimed at their physical positions. An unreadable target is an
    /// anomaly; its bytes stay unclassified (the honest verdict).
    fn audit_camera_profiles(&mut self, auditor: &mut Auditor<&[u8]>, ifd: &Ifd, page: usize) {
        let Some(offsets) = ifd.get_u64_vec(tags::EXTRA_CAMERA_PROFILES) else {
            return;
        };
        for base in offsets {
            match audit_camera_profile(auditor, base) {
                Ok(profile_ifds) => {
                    // Profile IFDs carry DNG profile tags (ColorMatrix1, ProfileName, …): run
                    // the tag and matrix checks, but no pixel-structure expectations.
                    for profile in &profile_ifds {
                        self.check_tags(profile, page);
                        self.check_matrices(profile, page);
                    }
                }
                Err(_) => self.anomalies.push(Anomaly::Structure {
                    page,
                    detail: "DNG: extra camera profile could not be parsed",
                    severity: Severity::Error,
                }),
            }
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
    ///
    /// A directory's [`AuditFinding::ShadowedEntry`] findings — one per entry its parsed [`Ifd`]
    /// lost to a later entry under the same tag — fold into one [`Anomaly::DuplicateTag`] per
    /// `(directory, tag)`, counting the survivor too, in directory-offset then tag order.
    fn map_audit_findings(&mut self, findings: &[AuditFinding]) {
        let mut shadowed: BTreeMap<(u64, u16), usize> = BTreeMap::new();
        for finding in findings {
            match *finding {
                AuditFinding::SkippedSubIfd { page, reason, .. } => {
                    self.anomalies.push(Anomaly::Structure {
                        page,
                        detail: match reason {
                            SkipReason::Cycle => "DNG: sub-IFD offset revisited (possible cycle)",
                            SkipReason::TooDeep => "DNG: sub-IFD nesting too deep",
                            _ => "DNG: sub-IFD could not be parsed",
                        },
                        severity: Severity::Error,
                    });
                }
                AuditFinding::ChainedSubIfd { page, .. } => {
                    self.anomalies.push(Anomaly::Structure {
                        page,
                        detail: "DNG: sub-IFD carries a next-IFD chain",
                        severity: Severity::Warning,
                    });
                }
                AuditFinding::ShadowedEntry { ifd, tag, .. } => {
                    *shadowed.entry((ifd, tag)).or_default() += 1;
                }
                // The finding taxonomy may grow; unrecognised findings are not anomalies.
                _ => {}
            }
        }
        for ((ifd, tag), lost) in shadowed {
            self.anomalies.push(Anomaly::DuplicateTag {
                ifd,
                tag,
                entries: lost + 1,
                severity: Severity::Error,
            });
        }
    }
}

/// Parses (and claims) one embedded camera-profile stream at `base`: its 8-byte header, then
/// its rebased directory chain.
fn audit_camera_profile(auditor: &mut Auditor<&[u8]>, base: u64) -> Result<Vec<Ifd>> {
    let mut head = [0u8; 8];
    auditor.read_at(base, &mut head)?;
    let order = match [head[0], head[1]] {
        [0x49, 0x49] => ByteOrder::LittleEndian,
        [0x4D, 0x4D] => ByteOrder::BigEndian,
        _ => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "DNG: bad camera-profile byte-order mark",
            ));
        }
    };
    if order.u16([head[2], head[3]]) != CAMERA_PROFILE_MAGIC {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "DNG: bad camera-profile magic",
        ));
    }
    let first = u64::from(order.u32([head[4], head[5], head[6], head[7]]));
    auditor.claim(
        base,
        8,
        SpanKind::Data(DataLabel::Other("camera-profile header")),
        Claim::Parsed,
    );
    auditor.walk_embedded(base, first, order, Variant::Classic)
}

impl DngDecoder {
    /// Decodes `data` to its [`DecodedDng`] *and* returns a whole-file [`DeconstructReport`] that
    /// classifies every byte and flags anything unrecognised.
    ///
    /// # Errors
    ///
    /// Returns [`gamut_core::Error`] if the container is unreadable or the raw image cannot be
    /// decoded (e.g. an out-of-scope compression) — a strict superset of [`DngDecoder::decode`].
    /// Use the free [`deconstruct`] function for a report without decoding.
    pub fn deconstruct(&self, data: &[u8]) -> Result<(DecodedDng, DeconstructReport)> {
        let report = deconstruct(data)?;
        let decoded = self.decode(data)?;
        Ok((decoded, report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that repeats a tag is graded: the audit's shadowed-entry finding folds into one
    /// `DuplicateTag` naming the directory by offset and counting both entries.
    #[test]
    fn flags_a_directory_that_repeats_a_tag() {
        let data: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, IFD0 @ 8
            0x02, 0x00, // 2 entries
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // 256 = 1
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // 256 = 2
            0x00, 0x00, 0x00, 0x00, // next = 0
        ];
        let report = deconstruct(data).expect("deconstruct");
        let duplicates: Vec<_> = report
            .anomalies
            .iter()
            .filter(|a| matches!(a, Anomaly::DuplicateTag { .. }))
            .collect();
        assert_eq!(
            duplicates,
            [&Anomaly::DuplicateTag {
                ifd: 8,
                tag: 256,
                entries: 2,
                severity: Severity::Error,
            }],
            "{report:?}"
        );
    }
}
