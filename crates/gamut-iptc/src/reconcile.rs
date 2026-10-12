//! Reconciliation between the legacy IIM and the modern XMP representations.
//!
//! An image may carry the same datum in legacy IIM, in IPTC-Core XMP, or in both with conflicting
//! values. This module is the crate's keystone engine: merging the two carriers into one coherent
//! [`PhotoMetadata`] view and projecting that view back to a consistent IIM block, driven by the
//! authoritative field table in [`crate::schema`]. It is surfaced through
//! [`crate::reader::IptcReader`] (merge, conflicts) and [`crate::writer::IptcWriter`] (project).

use crate::charset::IimCharset;
use crate::date;
use crate::error::{IptcError, Result};
use crate::iim::{IimBlock, IimDataSet, IimTagInfo};
use crate::photo_metadata::PhotoMetadata;
use crate::reader::{ConflictPolicy, FieldConflict};
use crate::schema::{FIELD_MAP, FieldMap, XmpShape};

/// Merges whichever carriers are present into one unified [`PhotoMetadata`] (XMP) view.
///
/// XMP-only properties are preserved. For each mapped field present in IIM, the value is adopted
/// when XMP lacks it, kept when both agree, and otherwise resolved by `policy`.
///
/// # Errors
///
/// Returns [`IptcError::Unsupported`] if the block's `1:90` designates a charset gamut does not
/// support — gamut never guess-decodes.
pub(crate) fn merge(
    policy: ConflictPolicy,
    iim: Option<&IimBlock>,
    xmp: Option<&PhotoMetadata>,
) -> Result<PhotoMetadata> {
    let mut out = xmp.cloned().unwrap_or_default();
    let Some(iim) = iim else {
        return Ok(out);
    };
    let charset = IimCharset::detect(iim)?;
    for row in FIELD_MAP {
        let iim_vals = read_iim_field(iim, row, charset);
        if iim_vals.is_empty() {
            continue;
        }
        let xmp_vals = out.get_field(&row.xmp);
        let adopt = xmp_vals.is_empty() // IIM-only: adopt it
            || (xmp_vals != iim_vals && policy == ConflictPolicy::IimWins); // conflict, IIM wins
        if adopt {
            let iim_refs: Vec<&str> = iim_vals.iter().map(String::as_str).collect();
            out.set_field(&row.xmp, &iim_refs);
        }
        // else: XMP present and (equal, or XmpWins) -> keep what's already in `out`.
    }
    Ok(out)
}

/// Projects a unified [`PhotoMetadata`] to an IIM block, encoding text with `charset`.
///
/// Emits the mandatory `2:00` Record Version and, for UTF-8, the `1:90` coded-character-set
/// escape, when any mapped field is present.
///
/// # Errors
///
/// Returns [`IptcError::Malformed`] if a value cannot be encoded in `charset`, exceeds the
/// dataset's maximum octet length, or (for `photoshop:DateCreated`) is not an IIM-expressible
/// ISO-8601 date-time — gamut never silently truncates or drops on write.
pub(crate) fn project(pm: &PhotoMetadata, charset: IimCharset) -> Result<IimBlock> {
    let mut fields = Vec::new();
    for row in FIELD_MAP {
        let vals = pm.get_field(&row.xmp);
        if vals.is_empty() {
            continue;
        }
        if row.xmp.shape == XmpShape::DateTime {
            let (date, time) = date::iso_to_iim(&vals[0]).ok_or(IptcError::Malformed(
                "IPTC IIM: DateCreated is not an IIM-expressible ISO-8601 date-time",
            ))?;
            push_dataset(&mut fields, 2, 55, date)?;
            if let Some(time) = time {
                push_dataset(&mut fields, 2, 60, time)?;
            }
        } else {
            let (record, dataset) = row.iim[0];
            for v in &vals {
                push_dataset(&mut fields, record, dataset, charset.encode(v)?)?;
            }
        }
    }

    let mut datasets = Vec::new();
    if !fields.is_empty() {
        if let Some(escape) = charset.escape_sequence() {
            datasets.push(IimDataSet {
                record: 1,
                dataset: 90,
                data: escape.to_vec(),
            });
        }
        datasets.push(IimDataSet {
            record: 2,
            dataset: 0,
            data: vec![0, 4],
        }); // Record Version 4
        datasets.extend(fields);
    }
    Ok(IimBlock { datasets })
}

/// Reports the mapped fields on which the two carriers disagree (both present, differing values).
///
/// # Errors
///
/// Returns [`IptcError::Unsupported`] if the block's `1:90` designates a charset gamut does not
/// support.
pub(crate) fn conflicts(iim: &IimBlock, xmp: &PhotoMetadata) -> Result<Vec<FieldConflict>> {
    let charset = IimCharset::detect(iim)?;
    let mut out = Vec::new();
    for row in FIELD_MAP {
        let iim_vals = read_iim_field(iim, row, charset);
        let xmp_vals = xmp.get_field(&row.xmp);
        if !iim_vals.is_empty() && !xmp_vals.is_empty() && iim_vals != xmp_vals {
            out.push(FieldConflict {
                field: row.xmp.name,
                iim: iim_vals,
                xmp: xmp_vals,
            });
        }
    }
    Ok(out)
}

/// Reads a mapped field's value(s) from the IIM block, decoded with `charset`.
///
/// Per-value leniency: an individual dataset value that fails to decode in the (supported)
/// charset is treated as absent, so one corrupt value cannot destroy access to the rest.
fn read_iim_field(iim: &IimBlock, row: &FieldMap, charset: IimCharset) -> Vec<String> {
    if row.xmp.shape == XmpShape::DateTime {
        let find = |dataset| {
            iim.datasets
                .iter()
                .find(|d| d.record == 2 && d.dataset == dataset)
        };
        return match find(55) {
            Some(d) => date::iim_to_iso(&d.data, find(60).map(|t| t.data.as_slice()))
                .map(|s| vec![s])
                .unwrap_or_default(),
            None => Vec::new(),
        };
    }
    let (record, dataset) = row.iim[0];
    let decoded: Vec<String> = iim
        .datasets
        .iter()
        .filter(|d| d.record == record && d.dataset == dataset)
        .filter_map(|d| charset.decode(&d.data).ok())
        .collect();
    match row.xmp.shape {
        XmpShape::Bag | XmpShape::Seq => decoded,
        _ => decoded.into_iter().take(1).collect(),
    }
}

/// Appends a dataset, rejecting a value that exceeds the dataset's maximum octet length.
///
/// A dataset gamut does not name, or one IPTC-IIM 4.2 states no maximum for, is not checked. A
/// maximum beyond `usize` cannot be exceeded by a value held in memory.
fn push_dataset(out: &mut Vec<IimDataSet>, record: u8, dataset: u8, data: Vec<u8>) -> Result<()> {
    if let Some(max) = IimTagInfo::lookup(record, dataset).and_then(|info| info.max_octets)
        && usize::try_from(max).is_ok_and(|max| data.len() > max)
    {
        return Err(IptcError::Malformed(
            "IPTC IIM: value exceeds the dataset's maximum length",
        ));
    }
    out.push(IimDataSet {
        record,
        dataset,
        data,
    });
    Ok(())
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
    fn merge_only_iim_promotes_to_xmp() {
        let iim = IimBlock {
            datasets: vec![ds(2, 90, b"Paris"), ds(2, 25, b"sky"), ds(2, 25, b"sea")],
        };
        let pm = merge(ConflictPolicy::default(), Some(&iim), None).unwrap();
        assert_eq!(pm.city(), Some("Paris"));
        assert_eq!(pm.keywords(), vec!["sky", "sea"]);
    }

    #[test]
    fn merge_only_xmp_is_unchanged() {
        let mut xmp = PhotoMetadata::new();
        xmp.set_city("Berlin");
        xmp.set_usage_terms("CC-BY"); // XMP-only field is preserved
        let pm = merge(ConflictPolicy::default(), None, Some(&xmp)).unwrap();
        assert_eq!(pm, xmp);
    }

    #[test]
    fn merge_conflict_respects_policy() {
        let iim = IimBlock {
            datasets: vec![ds(2, 90, b"Lyon")],
        };
        let mut xmp = PhotoMetadata::new();
        xmp.set_city("Paris");

        let xmp_wins = merge(ConflictPolicy::XmpWins, Some(&iim), Some(&xmp)).unwrap();
        assert_eq!(xmp_wins.city(), Some("Paris"));

        let iim_wins = merge(ConflictPolicy::IimWins, Some(&iim), Some(&xmp)).unwrap();
        assert_eq!(iim_wins.city(), Some("Lyon"));
    }

    #[test]
    fn merge_agreeing_carriers_keep_value() {
        let iim = IimBlock {
            datasets: vec![ds(2, 90, b"Oslo")],
        };
        let mut xmp = PhotoMetadata::new();
        xmp.set_city("Oslo");
        let pm = merge(ConflictPolicy::IimWins, Some(&iim), Some(&xmp)).unwrap();
        assert_eq!(pm.city(), Some("Oslo"));
        assert_eq!(pm.xmp.properties.len(), 1);
    }

    #[test]
    fn datetime_splits_and_joins() {
        let mut pm = PhotoMetadata::new();
        pm.set_simple(
            crate::schema::ns::PHOTOSHOP,
            "DateCreated",
            "1990-01-27T13:30:15+01:00",
        );
        let block = project(&pm, IimCharset::Latin1).unwrap();
        assert!(block.datasets.contains(&ds(2, 55, b"19900127")));
        assert!(block.datasets.contains(&ds(2, 60, b"133015+0100")));
        // ...and back the other way.
        let merged = merge(ConflictPolicy::default(), Some(&block), None).unwrap();
        assert_eq!(
            merged.simple(crate::schema::ns::PHOTOSHOP, "DateCreated"),
            Some("1990-01-27T13:30:15+01:00")
        );
    }

    #[test]
    fn push_dataset_accepts_exactly_the_maximum_and_rejects_one_octet_more() {
        // 2:25 Keywords: 64 octets (IIM 4.2 Ch. 6).
        let mut out = Vec::new();
        push_dataset(&mut out, 2, 25, vec![b'k'; 64]).unwrap();
        assert_eq!(out, [ds(2, 25, &[b'k'; 64])]);
        assert!(matches!(
            push_dataset(&mut out, 2, 25, vec![b'k'; 65]),
            Err(IptcError::Malformed(_))
        ));
        assert_eq!(out.len(), 1, "a rejected value is not appended");
    }

    #[test]
    fn push_dataset_skips_the_check_where_no_maximum_is_stated() {
        // 8:10 Subfile has no spec-stated maximum; a value past every `u16` bound is accepted.
        let mut out = Vec::new();
        push_dataset(&mut out, 8, 10, vec![0; 70_000]).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn conflicts_lists_only_disagreements() {
        let iim = IimBlock {
            datasets: vec![ds(2, 90, b"Lyon"), ds(2, 101, b"France")],
        };
        let mut xmp = PhotoMetadata::new();
        xmp.set_city("Paris"); // conflicts
        xmp.set_country("France"); // agrees
        let found = conflicts(&iim, &xmp).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].field, "City");
        assert_eq!(found[0].iim, vec!["Lyon"]);
        assert_eq!(found[0].xmp, vec!["Paris"]);
    }
}
