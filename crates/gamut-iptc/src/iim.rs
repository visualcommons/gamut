//! The legacy IPTC-IIM 4.2 record/dataset model and its binary codec.
//!
//! IIM is a flat stream of *datasets*. Each dataset is introduced by a `0x1C` tag marker, then a
//! one-octet record number, a one-octet dataset number, a length, and the value octets (IPTC-IIM
//! 4.2 §1.4–1.5). A dataset may repeat (e.g. multiple keywords). Lengths come in two forms: the
//! *standard* form (a 16-bit count, with the high bit of the first octet clear, for values up to
//! 32767 octets) and the *extended* form (used for larger values), distinguished by the high bit of
//! octet 4.
//!
//! [`IimBlock::parse`] / [`IimBlock::encode`] are the codec; charset decoding of text values lives
//! in [`crate::charset`]. Values are kept as raw octets in [`IimDataSet::data`] so that any dataset
//! — known or not, text or binary — round-trips losslessly (IPTC-IIM 4.2 Ch. 4 §1.3).

use crate::error::{IptcError, Result};

/// The IIM tag marker octet (`0x1C`) that introduces every dataset (IPTC-IIM 4.2 §1.5(b)(ii)).
const TAG_MARKER: u8 = 0x1C;

/// Bit 7 of octet 4 — set to flag the extended length form (IPTC-IIM 4.2 §1.5(c)).
const EXTENDED_FLAG: u8 = 0x80;

/// The largest value length expressible in the standard (16-bit) length form (IPTC-IIM 4.2 §1.4(d)).
const STANDARD_MAX_LEN: usize = 0x7FFF;

/// The kind of value a known IIM dataset carries, which decides how its octets are interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IimFieldKind {
    /// Graphic characters — human-readable text decoded with the stream's [`crate::charset`].
    Graphic,
    /// Binary data — raw octets that are not text (e.g. the two-octet record-version number).
    Binary,
    /// A date in the eight-octet `CCYYMMDD` form (e.g. IPTC-IIM 4.2 dataset 2:55 Date Created).
    Date,
    /// A time in the eleven-octet `HHMMSS±HHMM` form (e.g. IPTC-IIM 4.2 dataset 2:60 Time Created).
    Time,
}

/// Static metadata for a known IIM dataset: its name and the wire constraints from the spec.
///
/// Look one up with [`IimTagInfo::lookup`]. Unknown datasets have no entry and still round-trip as
/// raw [`IimDataSet`]s. The constraints (`repeatable`, `max_octets`) are taken from the IPTC-IIM 4.2
/// dataset definitions; `max_octets` is the value-field octet maximum, not a character count
/// (IPTC-IIM 4.2 Ch. 2 §1.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IimTagInfo {
    /// The record number the dataset lives in.
    pub record: u8,
    /// The dataset number within the record.
    pub dataset: u8,
    /// The dataset's human-readable name (IPTC-IIM 4.2 dataset name).
    pub name: &'static str,
    /// Whether the dataset may legitimately appear more than once (IPTC-IIM 4.2 Ch. 4 §1.4).
    pub repeatable: bool,
    /// The maximum length of the value field in octets.
    pub max_octets: u16,
    /// How the value octets are interpreted.
    pub kind: IimFieldKind,
}

use IimFieldKind::{Binary, Date, Graphic, Time};

/// Every IIM dataset gamut names: the complete Envelope (record 1) and Application (record 2)
/// dataset sets of IPTC-IIM 4.2, chapters 5 and 6.
///
/// Sourced from the IPTC-IIM 4.2 dataset definitions (`references/iptc/iim-4.2.pdf`); the subset
/// IPTC Photo Metadata maps to XMP is additionally cross-checked against the machine-readable
/// technical reference (`references/iptc/iptc-pmd-techreference_2025.1.json`) by
/// `tests/techreference.rs`.
///
/// Datasets the spec gives no determinate octet maximum for are deliberately absent, because
/// [`IimTagInfo::max_octets`] can only state one: `2:202` ObjectData Preview Data (256000 octets,
/// beyond `u16`) and every dataset of records 7–9 (`7:20`, `7:90`, `7:95`, `8:10`, `9:10`), whose
/// value is "a binary number" of unstated width. Records 3 (a separate publication), 4 and 5 (not
/// allocated) and 6 (IIM 4.2 Appendix F defines method identifiers, not datasets) carry no dataset
/// definitions in the vendored spec at all. Every unmodeled dataset in any record still round-trips
/// byte-exact.
#[rustfmt::skip]
const KNOWN_TAGS: &[IimTagInfo] = &[
    // Envelope record (1) — IIM 4.2 Chapter 5.
    IimTagInfo { record: 1, dataset: 0, name: "Model Version", repeatable: false, max_octets: 2, kind: Binary },
    IimTagInfo { record: 1, dataset: 5, name: "Destination", repeatable: true, max_octets: 1024, kind: Graphic },
    IimTagInfo { record: 1, dataset: 20, name: "File Format", repeatable: false, max_octets: 2, kind: Binary },
    IimTagInfo { record: 1, dataset: 22, name: "File Format Version", repeatable: false, max_octets: 2, kind: Binary },
    IimTagInfo { record: 1, dataset: 30, name: "Service Identifier", repeatable: false, max_octets: 10, kind: Graphic },
    IimTagInfo { record: 1, dataset: 40, name: "Envelope Number", repeatable: false, max_octets: 8, kind: Graphic },
    IimTagInfo { record: 1, dataset: 50, name: "Product I.D.", repeatable: true, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 1, dataset: 60, name: "Envelope Priority", repeatable: false, max_octets: 1, kind: Graphic },
    IimTagInfo { record: 1, dataset: 70, name: "Date Sent", repeatable: false, max_octets: 8, kind: Date },
    IimTagInfo { record: 1, dataset: 80, name: "Time Sent", repeatable: false, max_octets: 11, kind: Time },
    IimTagInfo { record: 1, dataset: 90, name: "Coded Character Set", repeatable: false, max_octets: 32, kind: Binary },
    IimTagInfo { record: 1, dataset: 100, name: "UNO", repeatable: false, max_octets: 80, kind: Graphic },
    IimTagInfo { record: 1, dataset: 120, name: "ARM Identifier", repeatable: false, max_octets: 2, kind: Binary },
    IimTagInfo { record: 1, dataset: 122, name: "ARM Version", repeatable: false, max_octets: 2, kind: Binary },
    // Application record (2) — IIM 4.2 Chapter 6.
    IimTagInfo { record: 2, dataset: 0, name: "Record Version", repeatable: false, max_octets: 2, kind: Binary },
    IimTagInfo { record: 2, dataset: 3, name: "Object Type Reference", repeatable: false, max_octets: 67, kind: Graphic },
    // 68 per the IIM 4.2 wire form (3-digit reference number + ':' + up to 64 octets of text); the
    // PMD tech-reference JSON's IIMmaxbytes records 64 (text only) — see tests/techreference.rs.
    IimTagInfo { record: 2, dataset: 4, name: "Object Attribute Reference", repeatable: true, max_octets: 68, kind: Graphic },
    IimTagInfo { record: 2, dataset: 5, name: "Object Name", repeatable: false, max_octets: 64, kind: Graphic },
    IimTagInfo { record: 2, dataset: 7, name: "Edit Status", repeatable: false, max_octets: 64, kind: Graphic },
    IimTagInfo { record: 2, dataset: 8, name: "Editorial Update", repeatable: false, max_octets: 2, kind: Graphic },
    IimTagInfo { record: 2, dataset: 10, name: "Urgency", repeatable: false, max_octets: 1, kind: Graphic },
    IimTagInfo { record: 2, dataset: 12, name: "Subject Reference", repeatable: true, max_octets: 236, kind: Graphic },
    IimTagInfo { record: 2, dataset: 15, name: "Category", repeatable: false, max_octets: 3, kind: Graphic },
    IimTagInfo { record: 2, dataset: 20, name: "Supplemental Category", repeatable: true, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 22, name: "Fixture Identifier", repeatable: false, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 25, name: "Keywords", repeatable: true, max_octets: 64, kind: Graphic },
    IimTagInfo { record: 2, dataset: 26, name: "Content Location Code", repeatable: true, max_octets: 3, kind: Graphic },
    IimTagInfo { record: 2, dataset: 27, name: "Content Location Name", repeatable: true, max_octets: 64, kind: Graphic },
    IimTagInfo { record: 2, dataset: 30, name: "Release Date", repeatable: false, max_octets: 8, kind: Date },
    IimTagInfo { record: 2, dataset: 35, name: "Release Time", repeatable: false, max_octets: 11, kind: Time },
    IimTagInfo { record: 2, dataset: 37, name: "Expiration Date", repeatable: false, max_octets: 8, kind: Date },
    IimTagInfo { record: 2, dataset: 38, name: "Expiration Time", repeatable: false, max_octets: 11, kind: Time },
    IimTagInfo { record: 2, dataset: 40, name: "Special Instructions", repeatable: false, max_octets: 256, kind: Graphic },
    IimTagInfo { record: 2, dataset: 42, name: "Action Advised", repeatable: false, max_octets: 2, kind: Graphic },
    // 2:45, 2:47 and 2:50 take the formats of 1:30, 1:70 and 1:40 respectively (IIM 4.2 Ch. 6).
    IimTagInfo { record: 2, dataset: 45, name: "Reference Service", repeatable: true, max_octets: 10, kind: Graphic },
    IimTagInfo { record: 2, dataset: 47, name: "Reference Date", repeatable: true, max_octets: 8, kind: Date },
    IimTagInfo { record: 2, dataset: 50, name: "Reference Number", repeatable: true, max_octets: 8, kind: Graphic },
    IimTagInfo { record: 2, dataset: 55, name: "Date Created", repeatable: false, max_octets: 8, kind: Date },
    IimTagInfo { record: 2, dataset: 60, name: "Time Created", repeatable: false, max_octets: 11, kind: Time },
    IimTagInfo { record: 2, dataset: 62, name: "Digital Creation Date", repeatable: false, max_octets: 8, kind: Date },
    IimTagInfo { record: 2, dataset: 63, name: "Digital Creation Time", repeatable: false, max_octets: 11, kind: Time },
    IimTagInfo { record: 2, dataset: 65, name: "Originating Program", repeatable: false, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 70, name: "Program Version", repeatable: false, max_octets: 10, kind: Graphic },
    IimTagInfo { record: 2, dataset: 75, name: "Object Cycle", repeatable: false, max_octets: 1, kind: Graphic },
    IimTagInfo { record: 2, dataset: 80, name: "By-line", repeatable: true, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 85, name: "By-line Title", repeatable: true, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 90, name: "City", repeatable: false, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 92, name: "Sub-location", repeatable: false, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 95, name: "Province/State", repeatable: false, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 100, name: "Country/Primary Location Code", repeatable: false, max_octets: 3, kind: Graphic },
    IimTagInfo { record: 2, dataset: 101, name: "Country/Primary Location Name", repeatable: false, max_octets: 64, kind: Graphic },
    IimTagInfo { record: 2, dataset: 103, name: "Original Transmission Reference", repeatable: false, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 105, name: "Headline", repeatable: false, max_octets: 256, kind: Graphic },
    IimTagInfo { record: 2, dataset: 110, name: "Credit", repeatable: false, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 115, name: "Source", repeatable: false, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 116, name: "Copyright Notice", repeatable: false, max_octets: 128, kind: Graphic },
    IimTagInfo { record: 2, dataset: 118, name: "Contact", repeatable: true, max_octets: 128, kind: Graphic },
    IimTagInfo { record: 2, dataset: 120, name: "Caption/Abstract", repeatable: false, max_octets: 2000, kind: Graphic },
    IimTagInfo { record: 2, dataset: 122, name: "Writer/Editor", repeatable: true, max_octets: 32, kind: Graphic },
    IimTagInfo { record: 2, dataset: 125, name: "Rasterized Caption", repeatable: false, max_octets: 7360, kind: Binary },
    IimTagInfo { record: 2, dataset: 130, name: "Image Type", repeatable: false, max_octets: 2, kind: Graphic },
    IimTagInfo { record: 2, dataset: 131, name: "Image Orientation", repeatable: false, max_octets: 1, kind: Graphic },
    IimTagInfo { record: 2, dataset: 135, name: "Language Identifier", repeatable: false, max_octets: 3, kind: Graphic },
    IimTagInfo { record: 2, dataset: 150, name: "Audio Type", repeatable: false, max_octets: 2, kind: Graphic },
    IimTagInfo { record: 2, dataset: 151, name: "Audio Sampling Rate", repeatable: false, max_octets: 6, kind: Graphic },
    IimTagInfo { record: 2, dataset: 152, name: "Audio Sampling Resolution", repeatable: false, max_octets: 2, kind: Graphic },
    IimTagInfo { record: 2, dataset: 153, name: "Audio Duration", repeatable: false, max_octets: 6, kind: Graphic },
    IimTagInfo { record: 2, dataset: 154, name: "Audio Outcue", repeatable: false, max_octets: 64, kind: Graphic },
    IimTagInfo { record: 2, dataset: 200, name: "ObjectData Preview File Format", repeatable: false, max_octets: 2, kind: Binary },
    IimTagInfo { record: 2, dataset: 201, name: "ObjectData Preview File Format Version", repeatable: false, max_octets: 2, kind: Binary },
];

impl IimTagInfo {
    /// The static metadata for the dataset `record:dataset`, or `None` if gamut does not model it.
    ///
    /// ```
    /// use gamut_iptc::IimTagInfo;
    ///
    /// let info = IimTagInfo::lookup(2, 25).expect("Keywords is a known dataset");
    /// assert_eq!(info.name, "Keywords");
    /// assert!(info.repeatable);
    /// ```
    #[must_use]
    pub fn lookup(record: u8, dataset: u8) -> Option<&'static IimTagInfo> {
        KNOWN_TAGS
            .iter()
            .find(|t| t.record == record && t.dataset == dataset)
    }
}

/// One IIM dataset: a `(record, dataset)` tag and its raw value octets (IPTC-IIM 4.2 §1.4).
///
/// The value is kept as raw octets; decode text values with [`crate::charset`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IimDataSet {
    /// The record number this dataset belongs to (IPTC-IIM 4.2 §1.1: 1 = Envelope, 2 = Application;
    /// records 3–9 round-trip as raw datasets).
    pub record: u8,
    /// The dataset number within the record.
    pub dataset: u8,
    /// The raw value octets, exactly as they appear on the wire.
    pub data: Vec<u8>,
}

impl IimDataSet {
    /// The static metadata for this dataset, or `None` if gamut does not model it.
    #[must_use]
    pub fn info(&self) -> Option<&'static IimTagInfo> {
        IimTagInfo::lookup(self.record, self.dataset)
    }

    /// The dataset's human-readable name, or `None` if gamut does not model it.
    #[must_use]
    pub fn name(&self) -> Option<&'static str> {
        self.info().map(|i| i.name)
    }
}

/// A parsed IIM dataset stream: an ordered list of datasets, with repeats preserved.
///
/// This is the payload of a Photoshop `0x0404` image resource (see [`crate::irb`]) and the unit of
/// [`crate::reader::IptcReader`]/[`crate::writer::IptcWriter`] for the legacy carrier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IimBlock {
    /// The datasets, in stream order. Order and repeats are significant and preserved.
    pub datasets: Vec<IimDataSet>,
}

impl IimBlock {
    /// Parses an IIM dataset stream (the `0x0404` resource payload) into its datasets.
    ///
    /// Every offset is bounds-checked, and the value length — never a search for the next marker —
    /// drives parsing, so embedded `0x1C` octets in a value are handled correctly (IPTC-IIM 4.2
    /// Ch. 4 §1.2). Unknown datasets are retained rather than rejected (§1.3).
    ///
    /// # Errors
    ///
    /// Returns [`IptcError::Malformed`] if the stream is truncated or a dataset is not introduced by
    /// the `0x1C` marker, and [`IptcError::Unsupported`] for an extended length too large to address.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut datasets = Vec::new();
        let mut pos = 0usize;
        while pos < data.len() {
            if data[pos] != TAG_MARKER {
                return Err(IptcError::Malformed("IPTC IIM: expected 0x1C tag marker"));
            }
            let record = *data
                .get(pos + 1)
                .ok_or(IptcError::Malformed("IPTC IIM: truncated dataset tag"))?;
            let dataset = *data
                .get(pos + 2)
                .ok_or(IptcError::Malformed("IPTC IIM: truncated dataset tag"))?;
            let len_hi = *data
                .get(pos + 3)
                .ok_or(IptcError::Malformed("IPTC IIM: truncated dataset tag"))?;
            let len_lo = *data
                .get(pos + 4)
                .ok_or(IptcError::Malformed("IPTC IIM: truncated dataset tag"))?;

            let (value_start, value_len): (usize, u64) = if len_hi & EXTENDED_FLAG == 0 {
                // Standard form: octets 4–5 are the value length (≤ 32767).
                (pos + 5, (u64::from(len_hi) << 8) | u64::from(len_lo))
            } else {
                // Extended form: the low 15 bits of octets 4–5 give the width `k` of the
                // big-endian value-length field that follows.
                let k = (usize::from(len_hi & !EXTENDED_FLAG) << 8) | usize::from(len_lo);
                if k == 0 || k > 8 {
                    return Err(IptcError::Unsupported(
                        "IPTC IIM: unsupported extended length descriptor size",
                    ));
                }
                let count_end = pos + 5 + k;
                let count = data
                    .get(pos + 5..count_end)
                    .ok_or(IptcError::Malformed("IPTC IIM: truncated extended length"))?;
                let mut len = 0u64;
                for &b in count {
                    len = (len << 8) | u64::from(b);
                }
                (count_end, len)
            };

            // Resolve the value end in u64 so a huge declared length can never overflow `usize`;
            // both overflow and a length past the buffer fail here.
            let value_end = (value_start as u64)
                .checked_add(value_len)
                .filter(|&end| end <= data.len() as u64)
                .ok_or(IptcError::Malformed("IPTC IIM: truncated dataset value"))?
                as usize;
            datasets.push(IimDataSet {
                record,
                dataset,
                data: data[value_start..value_end].to_vec(),
            });
            pos = value_end;
        }
        Ok(Self { datasets })
    }

    /// Serializes the datasets back to an IIM dataset stream.
    ///
    /// Values up to 32767 octets use the standard length form; larger ones use the extended form
    /// with a four-octet count field. The result is byte-for-byte identical to a freshly parsed
    /// stream's input for standard-length datasets, so `parse(encode(b)) == b` always holds.
    ///
    /// # Errors
    ///
    /// Returns [`IptcError::Unsupported`] if a value exceeds the 4 GiB the extended form can encode.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for ds in &self.datasets {
            out.push(TAG_MARKER);
            out.push(ds.record);
            out.push(ds.dataset);
            let len = ds.data.len();
            if len <= STANDARD_MAX_LEN {
                out.push((len >> 8) as u8);
                out.push((len & 0xFF) as u8);
            } else {
                let len = u32::try_from(len).map_err(|_| {
                    IptcError::Unsupported("IPTC IIM: dataset value too large to serialize")
                })?;
                // Extended form with a four-octet count: octets 4–5 encode k = 4 with the flag set.
                out.push(EXTENDED_FLAG);
                out.push(4);
                out.extend_from_slice(&len.to_be_bytes());
            }
            out.extend_from_slice(&ds.data);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(record: u8, dataset: u8, data: &[u8]) -> IimDataSet {
        IimDataSet {
            record,
            dataset,
            data: data.to_vec(),
        }
    }

    #[test]
    fn tag_info_lookup_and_dataset_helpers() {
        let kw = IimTagInfo::lookup(2, 25).unwrap();
        assert_eq!(kw.name, "Keywords");
        assert!(kw.repeatable);
        assert_eq!(kw.max_octets, 64);
        assert_eq!(kw.kind, IimFieldKind::Graphic);

        assert_eq!(IimTagInfo::lookup(2, 0).unwrap().kind, IimFieldKind::Binary);
        assert_eq!(IimTagInfo::lookup(2, 55).unwrap().kind, IimFieldKind::Date);
        assert_eq!(IimTagInfo::lookup(2, 60).unwrap().kind, IimFieldKind::Time);
        assert!(!IimTagInfo::lookup(2, 90).unwrap().repeatable);
        // 2:04 maps to a single XMP property but is repeatable on the IIM wire.
        assert!(IimTagInfo::lookup(2, 4).unwrap().repeatable);
        assert_eq!(IimTagInfo::lookup(2, 4).unwrap().max_octets, 68);
        assert!(IimTagInfo::lookup(9, 99).is_none());

        let set = ds(2, 25, b"sky");
        assert_eq!(set.name(), Some("Keywords"));
        assert_eq!(set.info().unwrap().max_octets, 64);
        assert_eq!(ds(2, 99, b"x").name(), None);
    }

    #[test]
    fn known_tags_are_well_formed() {
        for t in KNOWN_TAGS {
            assert!(
                t.record == 1 || t.record == 2,
                "unexpected record {}",
                t.record
            );
            assert!(!t.name.is_empty());
            assert!(t.max_octets > 0);
            // No duplicate (record, dataset) entries, and no duplicate names.
            let count = KNOWN_TAGS
                .iter()
                .filter(|o| o.record == t.record && o.dataset == t.dataset)
                .count();
            assert_eq!(count, 1, "duplicate tag {}:{}", t.record, t.dataset);
            let named = KNOWN_TAGS.iter().filter(|o| o.name == t.name).count();
            assert_eq!(named, 1, "duplicate name {}", t.name);
        }
        // The table is ordered by (record, dataset), so a transcription inserted in the wrong
        // place is visible rather than merely unsorted.
        assert!(
            KNOWN_TAGS
                .windows(2)
                .all(|w| (w[0].record, w[0].dataset) < (w[1].record, w[1].dataset)),
            "KNOWN_TAGS must be ordered by (record, dataset)"
        );
    }

    #[test]
    fn date_and_time_datasets_carry_their_fixed_iim_form_lengths() {
        // IIM 4.2 fixes both forms: a date is CCYYMMDD (8 octets), a time HHMMSS±HHMM (11). A
        // mis-transcribed length on any of the nine date/time datasets shows up here.
        for t in KNOWN_TAGS {
            match t.kind {
                Date => assert_eq!(t.max_octets, 8, "{}:{} {}", t.record, t.dataset, t.name),
                Time => assert_eq!(t.max_octets, 11, "{}:{} {}", t.record, t.dataset, t.name),
                Binary | Graphic => {}
            }
        }
        // Both kinds are actually present, so the loop above is not vacuous.
        assert!(KNOWN_TAGS.iter().any(|t| t.kind == Date));
        assert!(KNOWN_TAGS.iter().any(|t| t.kind == Time));
    }

    #[test]
    fn datasets_without_a_stated_maximum_stay_unmodeled() {
        // `max_octets` can only state a determinate maximum, so the datasets IIM 4.2 gives none
        // for are deliberately absent: `2:202` ObjectData Preview Data (256000 octets, beyond
        // `u16`) and the records 7-9 datasets ("a binary number" of unstated width).
        for (record, dataset) in [
            (2, 202),
            (7, 10),
            (7, 20),
            (7, 90),
            (7, 95),
            (8, 10),
            (9, 10),
        ] {
            assert!(
                IimTagInfo::lookup(record, dataset).is_none(),
                "{record}:{dataset} is modelled but has no stated octet maximum"
            );
        }
        // The rest of IIM 4.2 chapters 5 and 6 is named: 14 Envelope + 56 Application datasets.
        assert_eq!(KNOWN_TAGS.iter().filter(|t| t.record == 1).count(), 14);
        assert_eq!(KNOWN_TAGS.iter().filter(|t| t.record == 2).count(), 56);
    }

    #[test]
    fn parse_single_standard_dataset() {
        // 0x1C, record 2, dataset 25, len 0x0003, "sky"
        let bytes = [0x1C, 0x02, 0x19, 0x00, 0x03, b's', b'k', b'y'];
        let block = IimBlock::parse(&bytes).unwrap();
        assert_eq!(block.datasets, vec![ds(2, 25, b"sky")]);
    }

    #[test]
    fn parse_preserves_order_and_repeats() {
        let bytes = [
            0x1C, 0x02, 0x19, 0x00, 0x01, b'a', // 2:25 = "a"
            0x1C, 0x02, 0x19, 0x00, 0x01, b'b', // 2:25 = "b" (repeat)
            0x1C, 0x02, 0x05, 0x00, 0x02, b'h', b'i', // 2:5 = "hi"
        ];
        let block = IimBlock::parse(&bytes).unwrap();
        assert_eq!(
            block.datasets,
            vec![ds(2, 25, b"a"), ds(2, 25, b"b"), ds(2, 5, b"hi")]
        );
    }

    #[test]
    fn parse_value_may_contain_marker_octet() {
        // A value byte equal to 0x1C must not be mistaken for a new dataset.
        let bytes = [0x1C, 0x02, 0x19, 0x00, 0x02, 0x1C, 0x1C];
        let block = IimBlock::parse(&bytes).unwrap();
        assert_eq!(block.datasets, vec![ds(2, 25, &[0x1C, 0x1C])]);
    }

    #[test]
    fn parse_empty_is_empty_block() {
        assert_eq!(IimBlock::parse(&[]).unwrap(), IimBlock::default());
    }

    #[test]
    fn parse_rejects_bad_marker_and_truncation() {
        assert!(IimBlock::parse(&[0x00, 0x02, 0x19, 0x00, 0x00]).is_err());
        // Truncated tag (only four octets).
        assert!(IimBlock::parse(&[0x1C, 0x02, 0x19, 0x00]).is_err());
        // Declared length runs past the buffer.
        assert!(IimBlock::parse(&[0x1C, 0x02, 0x19, 0x00, 0x05, b'a']).is_err());
    }

    #[test]
    fn encode_standard_length_is_big_endian() {
        // A 256-octet value exercises both length octets (0x0100) so the byte order is pinned.
        let block = IimBlock {
            datasets: vec![ds(2, 120, &vec![b'x'; 256])],
        };
        let bytes = block.encode().unwrap();
        assert_eq!(&bytes[..5], &[0x1C, 0x02, 0x78, 0x01, 0x00]);
        assert_eq!(IimBlock::parse(&bytes).unwrap(), block);
    }

    #[test]
    fn standard_extended_boundary_is_pinned() {
        // Exactly 32767 octets uses the standard form: length 0x7FFF.
        let std = IimBlock {
            datasets: vec![ds(2, 120, &vec![b'x'; STANDARD_MAX_LEN])],
        };
        let std_bytes = std.encode().unwrap();
        assert_eq!(&std_bytes[..5], &[0x1C, 0x02, 0x78, 0x7F, 0xFF]);
        assert_eq!(IimBlock::parse(&std_bytes).unwrap(), std);

        // 32768 octets forces the extended form: k = 4 (0x80 0x04) then the 4-octet BE length.
        let ext = IimBlock {
            datasets: vec![ds(2, 120, &vec![b'x'; STANDARD_MAX_LEN + 1])],
        };
        let ext_bytes = ext.encode().unwrap();
        assert_eq!(
            &ext_bytes[..9],
            &[0x1C, 0x02, 0x78, 0x80, 0x04, 0x00, 0x00, 0x80, 0x00]
        );
        assert_eq!(IimBlock::parse(&ext_bytes).unwrap(), ext);
    }

    #[test]
    fn parse_handcrafted_extended_length() {
        // Extended: k = 4 (0x80 0x04), value length 0x00000003, then "abc".
        let bytes = [
            0x1C, 0x02, 0x78, 0x80, 0x04, 0x00, 0x00, 0x00, 0x03, b'a', b'b', b'c',
        ];
        let block = IimBlock::parse(&bytes).unwrap();
        assert_eq!(block.datasets, vec![ds(2, 120, b"abc")]);
    }

    #[test]
    fn parse_accepts_max_extended_descriptor_width() {
        // k = 8 is the widest count field gamut accepts; with a small length it parses normally.
        let bytes = [0x1C, 0x02, 0x78, 0x80, 0x08, 0, 0, 0, 0, 0, 0, 0, 1, b'z'];
        assert_eq!(
            IimBlock::parse(&bytes).unwrap().datasets,
            vec![ds(2, 120, b"z")]
        );
    }

    #[test]
    fn parse_rejects_bad_extended_descriptor() {
        // k = 0 (no count field) is rejected.
        assert!(IimBlock::parse(&[0x1C, 0x02, 0x78, 0x80, 0x00]).is_err());
        // k = 9 is wider than gamut supports — rejected even when nine count octets are present.
        let k9 = [
            0x1C, 0x02, 0x78, 0x80, 0x09, 0, 0, 0, 0, 0, 0, 0, 0, 1, b'z',
        ];
        assert!(IimBlock::parse(&k9).is_err());
        // A descriptor with its high octet set (k = 0x0104 = 260) is rejected; this pins the
        // shift in the width calculation (a `>> 8` would read k = 4 and wrongly accept it).
        let wide = [0x1C, 0x02, 0x78, 0x81, 0x04, 0, 0, 0, 1, b'z'];
        assert!(IimBlock::parse(&wide).is_err());
    }

    #[test]
    fn parse_rejects_extended_length_overflow() {
        // k = 8 with the maximum u64 length cannot be addressed, so it is rejected, not panicked.
        let bytes = [
            0x1C, 0x02, 0x78, 0x80, 0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        assert!(IimBlock::parse(&bytes).is_err());
    }

    #[test]
    fn encode_roundtrips_via_parse() {
        let block = IimBlock {
            datasets: vec![
                ds(2, 0, &[0x00, 0x04]),
                ds(2, 80, b"Jane Doe"),
                ds(2, 25, b"a"),
                ds(2, 25, b"b"),
            ],
        };
        let bytes = block.encode().unwrap();
        assert_eq!(IimBlock::parse(&bytes).unwrap(), block);
        // Standard-length output is exactly the wire form.
        assert_eq!(&bytes[..5], &[0x1C, 0x02, 0x00, 0x00, 0x02]);
    }
}
