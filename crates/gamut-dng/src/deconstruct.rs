//! Strict "deconstruct" decoding: walk the whole DNG and prove every byte is classified.
//!
//! This is the archival counterpart to [`crate::decoder`], filed as issue #197 from the PR #161
//! review and rebuilt on the shared structural auditor for issue #263. Ordinary decoding reads
//! the raw image, profile, and metadata it needs and ignores the rest. [`deconstruct`] instead
//! drives [`gamut_ifd::audit`] over the entire IFD tree — IFD 0 and its chain, the `SubIFDs`
//! image sub-IFDs (including the full-resolution raw), the Exif/GPS/Interoperability metadata
//! sub-IFDs, and every strip/tile/free/embedded-JPEG extent — then layers DNG tag knowledge on
//! top. The resulting [`DeconstructReport`] carries the byte-level [`SegmentReport`](gamut_ifd::SegmentReport) (every byte
//! typed, or precisely what was not — including the dual-ledger parser cross-check) plus unknown
//! field types, unknown/private tags, out-of-spec codes, and malformed colour matrices.
//!
//! It is **collect-and-report, not fail-fast**: it errors only when the container itself is
//! unreadable, exactly as [`gamut_ifd::read`] would; everything else is reported for the caller
//! to judge.

use std::collections::BTreeMap;

use gamut_core::{Error, Result};
// The report vocabulary is gamut-ifd's, shared with gamut-tiff; re-exported so this module's
// public paths still name it.
pub use gamut_ifd::{Anomaly, DeconstructReport, Severity, UnknownFieldType, UnknownTag};
use gamut_ifd::{
    AuditFinding, Auditor, ByteOrder, Claim, DataLabel, Ifd, SkipReason, SpanKind,
    StandardAuditSpec, Value, Variant,
};

use crate::decoder::{DecodedDng, DngDecoder};
use crate::tags;
use crate::values::{Compression, PhotometricInterpretation};

/// The magic number of an embedded DNG camera-profile stream (the `.dcp` form): the two bytes
/// after the byte-order mark, `0x4352` (`"RC"` little-endian) where a TIFF header carries `42`.
const CAMERA_PROFILE_MAGIC: u16 = 0x4352;

/// Walks `data` (a DNG file) and returns a [`DeconstructReport`] classifying every byte — without
/// decoding the raw image.
///
/// # Errors
///
/// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error only when the
/// container is unreadable (bad header,
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
    Ok(DeconstructReport::new(
        segments,
        findings.unknown_fields,
        findings.unknown_tags,
        findings.anomalies,
    ))
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
                self.unknown_fields.push(UnknownFieldType::new(
                    page,
                    field.tag,
                    u.type_code(),
                    u.count(),
                ));
            } else if !tags::is_known_tag(field.tag) {
                self.unknown_tags.push(UnknownTag::new(
                    page,
                    field.tag,
                    field.value.type_code(),
                    field.value.count(),
                ));
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
            self.anomalies.push(Anomaly::unknown_code(
                page,
                tags::COMPRESSION,
                code,
                "DNG: unrecognised Compression code",
            ));
        }
        if let Some(code) = ifd.get_u32(tags::PHOTOMETRIC_INTERPRETATION)
            && u16::try_from(code)
                .ok()
                .and_then(PhotometricInterpretation::from_code)
                .is_none()
        {
            self.anomalies.push(Anomaly::unknown_code(
                page,
                tags::PHOTOMETRIC_INTERPRETATION,
                code,
                "DNG: unrecognised PhotometricInterpretation code",
            ));
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
                self.anomalies.push(Anomaly::unparsable_tag(
                    page,
                    tag,
                    "DNG: 3x3 matrix tag is not nine rationals",
                ));
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
            self.anomalies.push(Anomaly::structure(
                page,
                "DNG: image IFD has no strip or tile data",
                Severity::Warning,
            ));
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
                Err(_) => self.anomalies.push(Anomaly::structure(
                    page,
                    "DNG: extra camera profile could not be parsed",
                    Severity::Error,
                )),
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
            self.anomalies
                .push(Anomaly::structure(page, missing, Severity::Error));
            return;
        };
        if offsets.len() != counts.len() {
            self.anomalies
                .push(Anomaly::structure(page, mismatch, Severity::Error));
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
                    let detail = match reason {
                        SkipReason::Cycle => "DNG: sub-IFD offset revisited (possible cycle)",
                        SkipReason::TooDeep => "DNG: sub-IFD nesting too deep",
                        SkipReason::OverBudget => "DNG: sub-IFD budget exhausted",
                        _ => "DNG: sub-IFD could not be parsed",
                    };
                    self.anomalies
                        .push(Anomaly::structure(page, detail, Severity::Error));
                }
                AuditFinding::ChainedSubIfd { page, .. } => {
                    self.anomalies.push(Anomaly::structure(
                        page,
                        "DNG: sub-IFD carries a next-IFD chain",
                        Severity::Warning,
                    ));
                }
                AuditFinding::ShadowedEntry { ifd, tag, .. } => {
                    *shadowed.entry((ifd, tag)).or_default() += 1;
                }
                // The finding taxonomy may grow; unrecognised findings are not anomalies.
                _ => {}
            }
        }
        for ((ifd, tag), lost) in shadowed {
            self.anomalies
                .push(Anomaly::duplicate_tag(ifd, tag, lost + 1));
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

    /// A sub-IFD target the audit skipped because the walk had spent its directory budget is
    /// reported as budget exhaustion, not as a target that could not be parsed: it was never
    /// attempted.
    ///
    /// Driven through `map_audit_findings` directly rather than a file, because reaching the
    /// budget end to end takes 65,536 parsed sub-IFDs, and `gamut-ifd`'s own audit test already
    /// pins that the walk emits `SkipReason::OverBudget` there; what this crate owns is only the
    /// mapping. Deleting the `OverBudget` arm falls through to "could not be parsed".
    #[test]
    fn map_audit_findings_reports_an_over_budget_sub_ifd_as_budget_exhausted() {
        let mut findings = Findings::default();
        findings.map_audit_findings(&[AuditFinding::SkippedSubIfd {
            page: 0,
            tag: 330,
            offset: 26,
            reason: SkipReason::OverBudget,
        }]);
        assert_eq!(
            findings.anomalies,
            [Anomaly::structure(
                0,
                "DNG: sub-IFD budget exhausted",
                Severity::Error,
            )]
        );
    }

    /// An unknown `PhotometricInterpretation` (262) code is flagged — including one past `u16`
    /// that would alias `BlackIsZero` (1) if truncated — and a recognised one is not.
    #[test]
    fn check_codes_flags_an_unknown_photometric_interpretation_only() {
        for code in [99, 0x1_0001] {
            let mut ifd = Ifd::new();
            ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Long(vec![code]));
            let mut findings = Findings::default();
            findings.check_codes(&ifd, 3);
            assert_eq!(
                findings.anomalies,
                [Anomaly::unknown_code(
                    3,
                    tags::PHOTOMETRIC_INTERPRETATION,
                    code,
                    "DNG: unrecognised PhotometricInterpretation code",
                )]
            );
        }

        let mut ifd = Ifd::new();
        ifd.set(tags::PHOTOMETRIC_INTERPRETATION, Value::Short(vec![32803]));
        let mut findings = Findings::default();
        findings.check_codes(&ifd, 3);
        assert_eq!(findings.anomalies, []);
    }

    /// Unlike `gamut-tiff`'s deconstruct, this one does not check `Predictor` (317): an unknown
    /// code there raises no anomaly. Pinned as the current difference between the two crates.
    #[test]
    fn check_codes_does_not_check_predictor() {
        let mut ifd = Ifd::new();
        ifd.set(tags::PREDICTOR, Value::Short(vec![99]));
        let mut findings = Findings::default();
        findings.check_codes(&ifd, 3);
        assert_eq!(findings.anomalies, []);
    }

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
            [&Anomaly::duplicate_tag(8, 256, 2)],
            "{report:?}"
        );
    }
}
