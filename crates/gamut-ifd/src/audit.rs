//! The shared structural byte-completeness auditor (issue #263).
//!
//! [`audit`] walks a whole TIFF-family stream — header, the top-level IFD chain, every sub-IFD
//! reached through the spec's pointer tags, and every data extent the spec's tags locate — over
//! a [`Tracked`] source, collecting typed claims into a [`SegmentMap`] and finishing with the
//! dual-ledger cross-check. The result classifies **every byte** of the file, or reports
//! precisely what could not be classified.
//!
//! The walk is **lenient where the file is at fault** (an unparseable or cyclic sub-IFD becomes
//! an [`AuditFinding`], its bytes simply stay unclassified) and **strict where the parser is at
//! fault** (the dual-ledger invariants). It errors only where [`read`](crate::read) would: an
//! unreadable header or top-level chain — or on a transport failure.
//!
//! Codecs supply tag knowledge through [`AuditSpec`]; the [`StandardAuditSpec`] covers the
//! structural tags every TIFF-family file shares (strips, tiles, free ranges, embedded JPEG).

use gamut_core::{ErrorKind, Result};

use crate::reader::pointer_offsets;
use crate::segment::{Claim, DataLabel, SegmentMap, SegmentReport, SpanKind};
use crate::source::ReadAt;
use crate::stream::IfdReader;
use crate::track::Tracked;
use crate::{Ifd, TiffFile, tags};

/// An upper bound on sub-IFD nesting, guarding the recursive walk against hostile trees. The
/// deepest legitimate trees (a DNG raw sub-IFD, EXIF's Exif → Interop chain) are three levels.
const MAX_SUBIFD_DEPTH: usize = 16;

/// The codec-supplied tag knowledge an [`audit`] walk needs. Everything has a default, so a
/// plain-TIFF audit is [`StandardAuditSpec`]; a codec overrides to add its own pointer tags or
/// data carriers.
pub trait AuditSpec {
    /// The pointer tags followed as sub-IFD graphs (default:
    /// [`tags::STANDARD_POINTER_TAGS`]).
    fn pointer_tags(&self) -> &[u16] {
        tags::STANDARD_POINTER_TAGS
    }

    /// Reports every data extent `ifd`'s tag values locate, as `sink(offset, len, label)`
    /// calls. The default reports the standard structural carriers via
    /// [`standard_data_extents`]; an override that still wants those should call it too.
    fn data_extents(&self, ifd: &Ifd, sink: &mut dyn FnMut(u64, u64, DataLabel)) {
        standard_data_extents(ifd, sink);
    }
}

/// The default [`AuditSpec`]: standard pointer tags, standard data extents.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandardAuditSpec;

impl AuditSpec for StandardAuditSpec {}

/// Reports the data extents the **structural** tag pairs locate — strips, tiles, free ranges,
/// and the embedded-JPEG range — as `sink(offset, len, label)` calls, u64-native (BigTIFF
/// extents past 4 GiB coerce losslessly).
///
/// Offset/count arrays are paired index-wise; a length mismatch leaves the unpaired tail
/// unclaimed (so its bytes surface as unclassified — the codec's audit layer reports the
/// mismatch itself).
pub fn standard_data_extents(ifd: &Ifd, sink: &mut dyn FnMut(u64, u64, DataLabel)) {
    let pairs = [
        (
            tags::STRIP_OFFSETS,
            tags::STRIP_BYTE_COUNTS,
            DataLabel::Strip,
        ),
        (tags::TILE_OFFSETS, tags::TILE_BYTE_COUNTS, DataLabel::Tile),
        (
            tags::JPEG_INTERCHANGE_FORMAT,
            tags::JPEG_INTERCHANGE_FORMAT_LENGTH,
            DataLabel::JpegInterchange,
        ),
        (tags::FREE_OFFSETS, tags::FREE_BYTE_COUNTS, DataLabel::Free),
    ];
    for (off_tag, len_tag, label) in pairs {
        let (Some(offsets), Some(lens)) = (ifd.get_u64_vec(off_tag), ifd.get_u64_vec(len_tag))
        else {
            continue;
        };
        for (&offset, &len) in offsets.iter().zip(&lens) {
            sink(offset, len, label);
        }
    }
}

/// Why a sub-IFD pointer target was skipped.
///
/// `#[non_exhaustive]`: further reasons can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// The target could not be parsed as a directory.
    Unparseable,
    /// The target offset was already visited — a pointer cycle.
    Cycle,
    /// The target lies beyond the nesting depth guard.
    TooDeep,
}

/// A lenient observation the audit walk made about the *file* (as opposed to the byte-level
/// verdicts in the [`SegmentReport`]).
///
/// `#[non_exhaustive]`: further findings can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditFinding {
    /// A sub-IFD pointer target that could not be followed; its bytes stay unclassified.
    SkippedSubIfd {
        /// The top-level page the pointer was reached from.
        page: usize,
        /// The pointer tag that named the target.
        tag: u16,
        /// The target offset.
        offset: u64,
        /// Why the target was skipped.
        reason: SkipReason,
    },
    /// A followed sub-IFD carried a non-zero next-IFD link — out of spec for the standard
    /// pointer tags. The chained directory was followed, accounted, and appended to the same
    /// pointer group.
    ChainedSubIfd {
        /// The top-level page the pointer was reached from.
        page: usize,
        /// The pointer tag whose target carried the chain.
        tag: u16,
        /// The directory whose next-IFD link was non-zero.
        offset: u64,
    },
    /// A directory carried more than one entry under `tag`, and this entry is one the decoded
    /// [`Ifd`] does **not** hold: a duplicate tag keeps its *last* occurrence (see
    /// [`IfdReader::decode_ifd`]), so every earlier one is shadowed — one finding per shadowed
    /// entry, so a tag written three times yields two.
    ///
    /// TIFF 6.0 §2 gives a directory one entry per tag, so the file is out of spec, and which
    /// entry survives is a property of the reader rather than of the file (libtiff keeps the
    /// first). Only the *field* is lost: an out-of-line value the shadowed entry pointed at was
    /// still read and claimed as a [`SpanKind::Value`] of its directory, so its bytes stay
    /// classified.
    ///
    /// **Round trip.** The [`Ifd`] holds one value per tag by construction, so writing it back
    /// emits only the survivor: `write(read(x))` drops every shadowed entry, and this finding is
    /// the only record that it existed. Nothing re-emits a duplicate — the writer cannot.
    ShadowedEntry {
        /// The offset of the directory carrying the duplicate — the identity
        /// [`SpanKind::IfdBody`] uses, so a finding in a metadata sub-IFD several levels below a
        /// page is named exactly.
        ifd: u64,
        /// The repeated tag.
        tag: u16,
        /// The offset of the shadowed entry's own record within that directory
        /// ([`RawEntry::offset`](crate::RawEntry::offset)).
        offset: u64,
    },
}

/// A completed structural audit: the parsed tree, the byte-level report, and the walk's
/// lenient findings.
#[derive(Debug, Clone)]
pub struct Audit {
    /// The parsed stream, with the spec's pointer tags resolved into sub-IFD groups (like
    /// [`read_tree`](crate::read_tree); a skipped target leaves its group incomplete — see
    /// [`AuditFinding::SkippedSubIfd`]).
    pub file: TiffFile,
    /// The byte-level verdict: every byte classified, or precisely what was not.
    pub report: SegmentReport,
    /// Lenient file-level observations made during the walk.
    pub findings: Vec<AuditFinding>,
}

/// Audits `source` for byte completeness: parses the whole tree under a [`Tracked`] read
/// ledger, claims every structure and declared data extent, classifies padding from the actual
/// bytes, and cross-checks the dual-ledger invariants.
///
/// This is the one-shot form of [`Auditor`] — use the latter when a codec must also account
/// structures the generic walk cannot know (embedded IFD streams, codec-private blocks).
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] only when the container is unreadable (bad header,
/// unreadable top-level chain) — the same conditions as [`read`](crate::read) — or
/// [`Error::Io`] on a transport failure. File defects inside sub-IFDs are findings, not errors.
pub fn audit<S: ReadAt>(source: S, spec: &dyn AuditSpec) -> Result<Audit> {
    let mut auditor = Auditor::new(source)?;
    let file = auditor.walk(spec)?;
    let (report, findings) = auditor.finish()?;
    Ok(Audit {
        file,
        report,
        findings,
    })
}

/// An in-progress byte-completeness audit, for codecs that must account structures the generic
/// walk cannot know.
///
/// The one-shot [`audit`] covers TIFF-family structure; a codec with **embedded IFD streams**
/// (a DNG camera profile, a vendor maker note) or private binary blocks drives the pieces
/// itself: [`walk`](Self::walk) the main structure, [`claim`](Self::claim) extra ranges or
/// [`walk_embedded`](Self::walk_embedded) rebased directory chains, then
/// [`finish`](Self::finish) into the dual-ledger-checked report.
#[derive(Debug)]
pub struct Auditor<S> {
    tracked: Tracked<S>,
    map: SegmentMap,
    findings: Vec<AuditFinding>,
}

impl<S: ReadAt> Auditor<S> {
    /// Starts an audit over `source`, wrapping it in a [`Tracked`] read ledger.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the source's length cannot be determined.
    pub fn new(source: S) -> Result<Self> {
        let mut tracked = Tracked::new(source);
        let file_len = tracked.len()?;
        Ok(Self {
            tracked,
            map: SegmentMap::new(file_len),
            findings: Vec::new(),
        })
    }

    /// Walks the main structure — header, top-level chain, the spec's sub-IFD graph, and its
    /// declared data extents — returning the resolved tree. Every directory it decodes reports
    /// its shadowed duplicate-tag entries as [`AuditFinding::ShadowedEntry`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] when the container is unreadable, like
    /// [`read`](crate::read); sub-IFD defects become findings.
    pub fn walk(&mut self, spec: &dyn AuditSpec) -> Result<TiffFile> {
        let mut reader = IfdReader::open(&mut self.tracked)?;
        let mut file = reader.read_chain(Some(&mut self.map), Some(&mut self.findings))?;
        let mut visited: Vec<u64> = Vec::new();
        for (page, ifd) in file.ifds.iter_mut().enumerate() {
            declare_extents(spec, ifd, &mut self.map);
            follow_subifds(
                &mut reader,
                &mut self.map,
                spec,
                ifd,
                &mut visited,
                page,
                1,
                &mut self.findings,
            )?;
        }
        Ok(file)
    }

    /// Reads bytes through the tracked source — how a codec sniffs an embedded structure's
    /// header. The read lands in the ledger, so it must be matched by a [`claim`](Self::claim).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the range lies outside the source, or [`Error::Io`]
    /// on a transport failure.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.tracked.read_exact_at(offset, buf)
    }

    /// Claims an extra byte range the codec knows structurally (a private header, a
    /// codec-specific block).
    pub fn claim(&mut self, start: u64, len: u64, kind: SpanKind, provenance: Claim) {
        self.map.claim(start, len, kind, provenance);
    }

    /// Records a lenient file-level finding alongside the walk's own.
    pub fn record(&mut self, finding: AuditFinding) {
        self.findings.push(finding);
    }

    /// Walks a **bare directory chain embedded at `base`** — a stream whose internal offsets
    /// are relative to its own start (a DNG camera profile after its 8-byte header, a vendor
    /// maker-note mini-IFD) — claiming every directory body and value at its physical position.
    /// `first_ifd` is the chain's first directory offset *within* the stream. The chain's
    /// [`AuditFinding::ShadowedEntry`] findings are recorded at physical positions too, and — like
    /// its claims — only when the whole chain parses.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the embedded chain is unreadable, loops, or runs away
    /// — the caller decides whether that is a finding or an error.
    pub fn walk_embedded(
        &mut self,
        base: u64,
        first_ifd: u64,
        order: crate::ByteOrder,
        variant: crate::Variant,
    ) -> Result<Vec<Ifd>> {
        let mut view = (&mut self.tracked).rebased(base);
        let logical_len = view.len()?;
        let mut sub = SegmentMap::new(logical_len);
        let mut reader = IfdReader::with_layout(&mut view, order, variant);
        let mut guard = crate::reader::ChainGuard::new();
        let mut ifds = Vec::new();
        let mut shadowed = Vec::new();
        let mut offset = first_ifd;
        while offset != 0 {
            guard.admit(offset)?;
            let (ifd, next) =
                reader.read_ifd_at_reporting(offset, &mut sub, &mut shadowed, base)?;
            ifds.push(ifd);
            offset = next;
        }
        // Like the claims, the chain's findings land only once the whole chain parsed — and they
        // were recorded at physical offsets, since a stream-relative offset names nothing in the
        // file being audited.
        self.map.merge_shifted(sub, base);
        self.findings.append(&mut shadowed);
        Ok(ifds)
    }

    /// Finishes the audit: cross-checks the read ledger against the claims, classifies padding
    /// from the actual bytes, names whatever bytes the file's own structures still do not
    /// account for by position, and returns the report with the accumulated findings.
    ///
    /// The two classification passes run in that order so all-zero word padding keeps its
    /// specific kind and only the genuinely unaccounted remainder becomes a preamble,
    /// interstitial or trailer — see [`SegmentReport::classify_unclaimed`]. Real camera files
    /// routinely carry all three; the dual-ledger invariants still fail the verdict on an actual
    /// parser defect.
    ///
    /// The naming pass is **skipped entirely** when the walk recorded an
    /// [`AuditFinding::SkippedSubIfd`]: bytes the walker admits it never reached are the
    /// walker's failure, not the file's, and must stay unclassified so the verdict says so.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the source fails while padding candidates are inspected.
    pub fn finish(mut self) -> Result<(SegmentReport, Vec<AuditFinding>)> {
        let mut report = self.map.finish(Some(self.tracked.ledger()));
        report.classify_padding(&mut self.tracked)?;
        let walked_everything = !self
            .findings
            .iter()
            .any(|f| matches!(f, AuditFinding::SkippedSubIfd { .. }));
        if walked_everything {
            report.classify_unclaimed();
        }
        Ok((report, self.findings))
    }
}

/// Claims every data extent the spec locates in `ifd`, as [`Claim::Declared`].
fn declare_extents(spec: &dyn AuditSpec, ifd: &Ifd, map: &mut SegmentMap) {
    spec.data_extents(ifd, &mut |offset, len, label| {
        map.claim(offset, len, SpanKind::Data(label), Claim::Declared);
    });
}

/// Follows `ifd`'s pointer tags leniently: each parseable target becomes a sub-IFD child
/// (its extents declared, its own pointers followed recursively); an unparseable, cyclic, or
/// too-deep target becomes a [`AuditFinding::SkippedSubIfd`]; a non-zero next-IFD link on a
/// target is followed as [`AuditFinding::ChainedSubIfd`].
#[expect(
    clippy::too_many_arguments,
    reason = "internal recursive walk threading its full context"
)]
fn follow_subifds<S: ReadAt>(
    reader: &mut IfdReader<S>,
    map: &mut SegmentMap,
    spec: &dyn AuditSpec,
    ifd: &mut Ifd,
    visited: &mut Vec<u64>,
    page: usize,
    depth: usize,
    findings: &mut Vec<AuditFinding>,
) -> Result<()> {
    for &tag in spec.pointer_tags() {
        let Some(offsets) = ifd.get(tag).and_then(pointer_offsets) else {
            continue;
        };
        let mut children: Vec<Ifd> = Vec::with_capacity(offsets.len());
        for head in offsets {
            let mut offset = head;
            while offset != 0 {
                let refusal = if depth + 1 > MAX_SUBIFD_DEPTH {
                    Some(SkipReason::TooDeep)
                } else if visited.contains(&offset) {
                    Some(SkipReason::Cycle)
                } else {
                    None
                };
                if let Some(reason) = refusal {
                    findings.push(AuditFinding::SkippedSubIfd {
                        page,
                        tag,
                        offset,
                        reason,
                    });
                    break;
                }
                visited.push(offset);
                let (mut child, next) = match reader.read_ifd_at_reporting(offset, map, findings, 0)
                {
                    Ok(parsed) => parsed,
                    // Transport failures abort the audit; file defects are findings.
                    Err(error) if error.kind() == ErrorKind::Io => return Err(error),
                    Err(_) => {
                        findings.push(AuditFinding::SkippedSubIfd {
                            page,
                            tag,
                            offset,
                            reason: SkipReason::Unparseable,
                        });
                        break;
                    }
                };
                declare_extents(spec, &child, map);
                follow_subifds(
                    reader,
                    map,
                    spec,
                    &mut child,
                    visited,
                    page,
                    depth + 1,
                    findings,
                )?;
                children.push(child);
                if next != 0 {
                    findings.push(AuditFinding::ChainedSubIfd { page, tag, offset });
                }
                offset = next;
            }
        }
        if !children.is_empty() {
            ifd.remove(tag);
            ifd.set_sub_ifd(tag, children);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ByteOrder, Value, Variant, align_word, read_tree, write};

    fn classic_le(ifds: Vec<Ifd>) -> Vec<u8> {
        write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds,
        })
        .expect("write")
    }

    /// A written tree with appended strip data audits to full classification: structure via
    /// Parsed claims, strips via Declared extents, alignment via padding classification.
    #[test]
    fn audit_classifies_a_tree_with_strip_data() {
        let strip = [9u8, 8, 7, 6, 5];
        let build = |strip_at: u64| {
            let mut exif = Ifd::new();
            exif.set(33434, Value::Rational(vec![(1, 200)]));
            let mut root = Ifd::new();
            root.set(256, Value::Short(vec![640]));
            root.set(270, Value::Ascii("odd".to_owned())); // odd length: forces padding
            root.set(273, Value::Long(vec![strip_at as u32]));
            root.set(279, Value::Long(vec![strip.len() as u32]));
            root.set_sub_ifd(tags::EXIF_IFD, vec![exif]);
            root
        };
        // Structural determinism: measure with a placeholder, then write the real offset.
        let probe = classic_le(vec![build(0)]);
        let strip_at = align_word(probe.len() as u64);
        let mut bytes = classic_le(vec![build(strip_at)]);
        bytes.resize(strip_at as usize, 0);
        bytes.extend_from_slice(&strip);

        let out = audit(&bytes[..], &StandardAuditSpec).expect("audit");
        assert!(out.findings.is_empty(), "findings: {:?}", out.findings);
        assert!(out.report.is_fully_classified(), "report: {:?}", out.report);
        // The tree matches read_tree, and the strip extent is a Declared Data segment.
        assert_eq!(
            out.file,
            read_tree(&bytes, &[tags::EXIF_IFD]).expect("tree")
        );
        assert!(out.report.segments.iter().any(|s| {
            s.kind == SpanKind::Data(DataLabel::Strip)
                && s.range
                    == crate::Range {
                        start: strip_at,
                        len: strip.len() as u64,
                    }
        }));
    }

    /// An unparseable sub-IFD target is a lenient finding — the audit completes and the
    /// target's bytes stay unclassified.
    #[test]
    fn unparseable_subifd_is_a_finding_not_an_error() {
        let mut root = Ifd::new();
        root.set(256, Value::Short(vec![640]));
        // A pointer aimed just past the end of the file.
        root.set(tags::SUB_IFDS, Value::Long(vec![0xFFFF]));
        let bytes = classic_le(vec![root]);
        let out = audit(&bytes[..], &StandardAuditSpec).expect("audit");
        assert_eq!(
            out.findings,
            vec![AuditFinding::SkippedSubIfd {
                page: 0,
                tag: tags::SUB_IFDS,
                offset: 0xFFFF,
                reason: SkipReason::Unparseable,
            }]
        );
        // The stale pointer stays in place as a field (nothing was resolved).
        assert!(out.file.ifds[0].get(tags::SUB_IFDS).is_some());
        assert!(
            out.report.is_fully_classified(),
            "the file bytes themselves are all classified"
        );
    }

    #[test]
    fn subifd_transport_failure_is_propagated() {
        struct FailsAt {
            bytes: Vec<u8>,
            offset: u64,
        }

        impl ReadAt for FailsAt {
            fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
                if offset == self.offset {
                    return Err(gamut_core::Error::Io(std::io::Error::other(
                        "injected transport failure",
                    )));
                }
                let mut bytes = self.bytes.as_slice();
                bytes.read_exact_at(offset, buf)
            }

            fn len(&mut self) -> Result<u64> {
                Ok(self.bytes.len() as u64)
            }
        }

        let bytes = vec![
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, IFD0 @ 8
            0x01, 0x00, 0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00,
            0x00, // SubIFDs -> 26
            0x00, 0x00, 0x00, 0x00, // next = 0
            0x01, 0x00, 0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
            0x00, // child ImageWidth = 1
            0x00, 0x00, 0x00, 0x00, // next = 0
        ];
        let error = audit(FailsAt { bytes, offset: 26 }, &StandardAuditSpec)
            .expect_err("transport failure");
        assert_eq!(error.kind(), ErrorKind::Io);
    }

    /// A self-pointing sub-IFD terminates as a cycle finding, not a hang.
    #[test]
    fn cyclic_subifd_is_a_finding() {
        // Root @8 points at the child @26, whose own pointer aims back at 26.
        let data: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let out = audit(data, &StandardAuditSpec).expect("audit");
        assert_eq!(
            out.findings,
            vec![AuditFinding::SkippedSubIfd {
                page: 0,
                tag: tags::SUB_IFDS,
                offset: 26,
                reason: SkipReason::Cycle,
            }]
        );
        // The child itself was parsed and accounted before the cycle was refused.
        assert_eq!(out.file.ifds[0].sub_ifds().len(), 1);
        assert!(out.report.is_fully_classified(), "report: {:?}", out.report);
    }

    /// A sub-IFD carrying an out-of-spec next-IFD chain link: the chained directory is
    /// followed, accounted, and appended to the same group, with a finding.
    #[test]
    fn chained_subifd_is_followed_and_flagged() {
        let data: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, IFD0 @ 8
            0x01, 0x00, // IFD0: 1 entry
            0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00,
            0x00, // 330 -> 26
            0x00, 0x00, 0x00, 0x00, // next = 0
            0x01, 0x00, // child A @ 26: 1 entry
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // 256 = 1
            0x2c, 0x00, 0x00, 0x00, // next = 44 (out of spec for SubIFDs)
            0x01, 0x00, // child B @ 44: 1 entry
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // 256 = 2
            0x00, 0x00, 0x00, 0x00, // next = 0
        ];
        let out = audit(data, &StandardAuditSpec).expect("audit");
        assert_eq!(
            out.findings,
            vec![AuditFinding::ChainedSubIfd {
                page: 0,
                tag: tags::SUB_IFDS,
                offset: 26,
            }]
        );
        let group = &out.file.ifds[0].sub_ifds()[0];
        assert_eq!(group.ifds.len(), 2, "the chained directory joins the group");
        assert_eq!(group.ifds[0].get_u32(256), Some(1));
        assert_eq!(group.ifds[1].get_u32(256), Some(2));
        assert!(out.report.is_fully_classified(), "report: {:?}", out.report);
    }

    /// A directory with two entries under one tag, as a bare classic stream: header, then the
    /// directory @ 8 with entry records @ 10 (`256 = 1`, shadowed) and @ 22 (`256 = 2`).
    const SHADOWING_STREAM: &[u8] = &[
        b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, IFD0 @ 8
        0x02, 0x00, // 2 entries
        0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // 256 = 1
        0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // 256 = 2
        0x00, 0x00, 0x00, 0x00, // next = 0
    ];

    /// A top-level directory's shadowed duplicate is a finding; the bytes stay classified.
    #[test]
    fn shadowed_top_level_entry_is_a_finding() {
        let out = audit(SHADOWING_STREAM, &StandardAuditSpec).expect("audit");
        assert_eq!(
            out.findings,
            vec![AuditFinding::ShadowedEntry {
                ifd: 8,
                tag: 256,
                offset: 10,
            }]
        );
        assert_eq!(
            out.file.ifds[0].get_u32(256),
            Some(2),
            "last occurrence wins"
        );
        assert!(out.report.is_fully_classified(), "report: {:?}", out.report);
    }

    /// A followed sub-IFD's shadowed duplicate is named by the sub-IFD's own offset.
    #[test]
    fn shadowed_subifd_entry_is_a_finding() {
        let data: &[u8] = &[
            b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, // header, IFD0 @ 8
            0x01, 0x00, // IFD0: 1 entry
            0x4a, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00,
            0x00, // 330 -> 26
            0x00, 0x00, 0x00, 0x00, // next = 0
            0x02, 0x00, // child @ 26: 2 entries
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // @28
            0x00, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // @40
            0x00, 0x00, 0x00, 0x00, // next = 0
        ];
        let out = audit(data, &StandardAuditSpec).expect("audit");
        assert_eq!(
            out.findings,
            vec![AuditFinding::ShadowedEntry {
                ifd: 26,
                tag: 256,
                offset: 28,
            }]
        );
        assert!(out.report.is_fully_classified(), "report: {:?}", out.report);
    }

    /// An embedded chain's shadowed duplicate is reported at its **physical** offsets.
    #[test]
    fn walk_embedded_reports_shadowed_entries_at_physical_offsets() {
        let base = 1000u64;
        let mut outer = vec![0u8; base as usize];
        outer.extend_from_slice(SHADOWING_STREAM);
        let mut auditor = Auditor::new(&outer[..]).expect("new");
        auditor
            .walk_embedded(base, 8, ByteOrder::LittleEndian, Variant::Classic)
            .expect("walk_embedded");
        let (_, findings) = auditor.finish().expect("finish");
        assert_eq!(
            findings,
            vec![AuditFinding::ShadowedEntry {
                ifd: base + 8,
                tag: 256,
                offset: base + 10,
            }]
        );
    }

    /// Nesting past the depth guard terminates with skip findings, not a stack overflow.
    #[test]
    fn depth_bomb_is_skipped_leniently() {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Short(vec![1]));
        for _ in 0..20 {
            let mut parent = Ifd::new();
            parent.set_sub_ifd(tags::SUB_IFDS, vec![ifd]);
            ifd = parent;
        }
        let bytes = classic_le(vec![ifd]);
        let out = audit(&bytes[..], &StandardAuditSpec).expect("audit");
        assert!(
            out.findings
                .iter()
                .any(|f| matches!(f, AuditFinding::SkippedSubIfd { .. })),
            "the too-deep tail is skipped"
        );
        // The skipped tail's directory bytes are unclassified — the honest verdict.
        assert!(!out.report.is_fully_classified());
    }

    /// An unreadable header is still a hard error, exactly like `read`.
    #[test]
    fn unreadable_container_errors() {
        assert!(audit(&b"not a tiff"[..], &StandardAuditSpec).is_err());
        assert!(audit(&b""[..], &StandardAuditSpec).is_err());
    }

    /// The composable pieces: `read_at` returns the real bytes (and its read must be matched by
    /// a claim), `claim` classifies a codec-private block, and `record` lands in the findings.
    #[test]
    fn auditor_pieces_compose_for_codec_known_structures() {
        let mut ifd = Ifd::new();
        ifd.set(256, Value::Short(vec![640]));
        let mut bytes = classic_le(vec![ifd]);
        let block_at = bytes.len() as u64;
        bytes.extend_from_slice(&[0xAB; 6]);

        let mut auditor = Auditor::new(&bytes[..]).expect("new");
        auditor.walk(&StandardAuditSpec).expect("walk");
        let mut buf = [0u8; 6];
        auditor.read_at(block_at, &mut buf).expect("read_at");
        assert_eq!(buf, [0xAB; 6], "read_at returns the physical bytes");
        auditor.claim(
            block_at,
            6,
            SpanKind::Data(DataLabel::Other("private block")),
            crate::Claim::Parsed,
        );
        let noted = AuditFinding::ChainedSubIfd {
            page: 0,
            tag: 1,
            offset: 2,
        };
        auditor.record(noted);
        let (report, findings) = auditor.finish().expect("finish");
        assert!(report.is_fully_classified(), "report: {report:?}");
        assert_eq!(findings, vec![noted]);
    }

    /// `walk_embedded` parses a rebased chain (both directories, real values) and its claims
    /// land at **physical** offsets in the shared map.
    #[test]
    fn walk_embedded_claims_at_physical_offsets() {
        let mut a = Ifd::new();
        a.set(256, Value::Short(vec![7]));
        a.set(270, Value::Ascii("vendor mode".to_owned())); // out of line within the stream
        let mut b = Ifd::new();
        b.set(256, Value::Short(vec![9]));
        let note = write(&TiffFile {
            order: ByteOrder::LittleEndian,
            variant: Variant::Classic,
            ifds: vec![a, b],
        })
        .expect("write note");
        let base = 1000u64;
        let mut outer = vec![0u8; base as usize];
        outer.extend_from_slice(&note);

        let mut auditor = Auditor::new(&outer[..]).expect("new");
        let ifds = auditor
            .walk_embedded(base, 8, ByteOrder::LittleEndian, Variant::Classic)
            .expect("walk_embedded");
        assert_eq!(ifds.len(), 2, "the whole chain is walked");
        assert_eq!(ifds[0].get_u32(256), Some(7));
        assert_eq!(ifds[1].get_u32(256), Some(9));
        // Cover the wrapper bytes so the classification verdict isolates the embedded claims.
        auditor.claim(
            0,
            base,
            SpanKind::Data(DataLabel::Other("prefix")),
            crate::Claim::Declared,
        );
        auditor.claim(
            base,
            8,
            SpanKind::Data(DataLabel::Other("embedded header")),
            crate::Claim::Declared,
        );
        let (report, _) = auditor.finish().expect("finish");
        assert!(report.is_fully_classified(), "report: {report:?}");
        // The embedded directory bodies sit at base + 8 (and chained after), physically —
        // and the out-of-line value claim is shifted the same way, its owning-IFD diagnostic
        // included.
        assert!(
            report
                .segments
                .iter()
                .any(|s| s.kind == SpanKind::IfdBody { ifd: base + 8 }
                    && s.range.start == base + 8),
            "physical claim positions: {report:?}"
        );
        assert!(
            report.segments.iter().any(|s| s.kind
                == SpanKind::Value {
                    ifd: base + 8,
                    tag: 270,
                }
                && s.range.start > base),
            "physical value claim: {report:?}"
        );
    }

    /// The sub-IFD depth guard's boundary is exact: a 16-level tree walks clean, a 17-level
    /// tree skips exactly its deepest directory.
    #[test]
    fn depth_guard_boundary_is_exact() {
        let nested = |levels: usize| {
            let mut ifd = Ifd::new();
            ifd.set(256, Value::Short(vec![1]));
            for _ in 0..levels - 1 {
                let mut parent = Ifd::new();
                parent.set_sub_ifd(tags::SUB_IFDS, vec![ifd]);
                ifd = parent;
            }
            classic_le(vec![ifd])
        };
        let at_cap = audit(&nested(16)[..], &StandardAuditSpec).expect("audit");
        assert!(at_cap.findings.is_empty(), "{:?}", at_cap.findings);
        assert!(at_cap.report.is_fully_classified());

        let over = audit(&nested(17)[..], &StandardAuditSpec).expect("audit");
        assert_eq!(over.findings.len(), 1, "{:?}", over.findings);
        assert!(matches!(
            over.findings[0],
            AuditFinding::SkippedSubIfd {
                reason: SkipReason::TooDeep,
                ..
            }
        ));
    }
}
