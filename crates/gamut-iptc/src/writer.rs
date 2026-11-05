//! The IPTC writer — the write-side entry point: project the unified view to the legacy carrier.

use crate::charset::IimCharset;
use crate::error::Result;
use crate::iim::IimBlock;
use crate::irb::PhotoshopIrb;
use crate::photo_metadata::PhotoMetadata;
use crate::reconcile;

/// Writer for IPTC metadata.
///
/// Projects a [`PhotoMetadata`] view to the legacy IIM carrier, encoding text with the writer's
/// [`IimCharset`]:
///
/// - [`IptcWriter::to_iim_block`] produces the IIM datasets (with the mandatory `2:00` Record Version
///   and, for UTF-8, the `1:90` coded-character-set escape).
/// - [`IptcWriter::write_irb`] additionally wraps them in a Photoshop image-resource (`8BIM`)
///   stream holding a single `0x0404` resource — the inverse of
///   [`IptcReader::read_irb`](crate::reader::IptcReader::read_irb).
///
/// The modern carrier needs no writer: [`PhotoMetadata::to_xmp`] hands the properties back as an
/// XMP graph for [`gamut_xmp`] to serialize (issue #34). To serialize a hand-built [`IimBlock`]
/// instead of a view, use the primitives directly:
/// `PhotoshopIrb::with_iptc(block.encode()?).encode()`.
///
/// The default charset is UTF-8: every value is encodable, at the cost of the `1:90` escape.
/// Choose [`IimCharset::Latin1`] for maximum legacy-consumer compatibility; the writer then
/// rejects values outside Latin-1 rather than mis-encode them.
#[derive(Debug, Clone, Copy)]
pub struct IptcWriter {
    charset: IimCharset,
}

impl Default for IptcWriter {
    fn default() -> Self {
        Self {
            charset: IimCharset::Utf8,
        }
    }
}

impl IptcWriter {
    /// Creates a writer with the default charset ([`IimCharset::Utf8`]).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the charset used to encode IIM text values and returns the writer.
    #[must_use]
    pub fn with_charset(mut self, charset: IimCharset) -> Self {
        self.charset = charset;
        self
    }

    /// Projects the view's mapped fields to an IIM block.
    ///
    /// Emits the mandatory `2:00` Record Version and, for UTF-8, the `1:90` coded-character-set
    /// escape, when any mapped field is present; an empty view yields an empty block.
    ///
    /// # Errors
    ///
    /// Returns [`crate::IptcError::Malformed`] if a value cannot be encoded in the writer's
    /// charset or exceeds the dataset's maximum octet length — gamut never silently truncates.
    pub fn to_iim_block(&self, pm: &PhotoMetadata) -> Result<IimBlock> {
        reconcile::project(pm, self.charset)
    }

    /// Projects the view to a Photoshop image-resource (`8BIM`) stream holding a single `0x0404`
    /// resource.
    ///
    /// Returns `Ok(None)` when no mapped field is present — there is nothing to embed, and an
    /// empty `0x0404` resource would be noise.
    ///
    /// # Errors
    ///
    /// As [`IptcWriter::to_iim_block`].
    pub fn write_irb(&self, pm: &PhotoMetadata) -> Result<Option<Vec<u8>>> {
        let block = self.to_iim_block(pm)?;
        if block.datasets.is_empty() {
            return Ok(None);
        }
        PhotoshopIrb::with_iptc(block.encode()?).encode().map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iim::IimDataSet;
    use crate::reader::{IptcReader, Sources};

    fn ds(record: u8, dataset: u8, data: &[u8]) -> IimDataSet {
        IimDataSet {
            record,
            dataset,
            data: data.to_vec(),
        }
    }

    #[test]
    fn to_iim_block_emits_version_and_fields() {
        let mut pm = PhotoMetadata::new();
        pm.set_city("Paris");
        pm.set_keywords(&["sky", "sea"]);
        let block = IptcWriter::new()
            .with_charset(IimCharset::Latin1)
            .to_iim_block(&pm)
            .unwrap();
        assert!(block.datasets.contains(&ds(2, 0, &[0, 4]))); // Record Version
        assert!(block.datasets.contains(&ds(2, 90, b"Paris")));
        assert_eq!(
            block.datasets.iter().filter(|d| d.dataset == 25).count(),
            2 // one dataset per keyword
        );
    }

    #[test]
    fn to_iim_block_utf8_escape_only_when_needed() {
        let mut pm = PhotoMetadata::new();
        pm.set_city("Köln");
        // The default charset is UTF-8, so the 1:90 escape leads the block.
        let utf8 = IptcWriter::new().to_iim_block(&pm).unwrap();
        assert_eq!(utf8.datasets[0], ds(1, 90, &IimCharset::UTF8_ESCAPE));
        // An empty view emits nothing at all — not even the version/escape.
        let empty = IptcWriter::new()
            .to_iim_block(&PhotoMetadata::new())
            .unwrap();
        assert!(empty.datasets.is_empty());
    }

    #[test]
    fn to_iim_block_rejects_overlong_and_unencodable_values() {
        let mut too_long = PhotoMetadata::new();
        too_long.set_city(&"x".repeat(33)); // City max is 32 octets
        assert!(
            IptcWriter::new()
                .with_charset(IimCharset::Latin1)
                .to_iim_block(&too_long)
                .is_err()
        );

        let mut non_latin1 = PhotoMetadata::new();
        non_latin1.set_headline("€"); // U+20AC is not Latin-1
        assert!(
            IptcWriter::new()
                .with_charset(IimCharset::Latin1)
                .to_iim_block(&non_latin1)
                .is_err()
        );
        // ...but the default UTF-8 charset encodes it fine.
        assert!(IptcWriter::new().to_iim_block(&non_latin1).is_ok());
    }

    #[test]
    fn write_irb_roundtrips_through_the_reader() {
        let mut pm = PhotoMetadata::new();
        pm.set_city("Lyon");
        pm.set_keywords(&["sky", "sea"]);
        let bytes = IptcWriter::new().write_irb(&pm).unwrap().unwrap();
        let block = IptcReader::new().read_irb(&bytes).unwrap().unwrap();
        let read = IptcReader::new()
            .read(Sources {
                iim: Some(&block),
                xmp: None,
            })
            .unwrap();
        assert_eq!(read.city(), Some("Lyon"));
        assert_eq!(read.keywords(), vec!["sky", "sea"]);
    }

    #[test]
    fn to_iim_block_rejects_unparseable_date_created() {
        // Strict write: an un-projectable DateCreated is an error, never a silent drop.
        let mut pm = PhotoMetadata::new();
        pm.set_simple(
            gamut_xmp::WellKnownNs::Photoshop.uri(),
            "DateCreated",
            "yesterday",
        );
        assert!(IptcWriter::new().to_iim_block(&pm).is_err());

        // A missing-seconds time is not IIM-expressible either (2:60 is HHMMSS±HHMM).
        pm.set_simple(
            gamut_xmp::WellKnownNs::Photoshop.uri(),
            "DateCreated",
            "2024-06-15T12:00",
        );
        assert!(IptcWriter::new().to_iim_block(&pm).is_err());

        // ...while a plain date, a partial date, and a full date-time all project fine.
        for ok in ["2024-06-15", "2024", "2024-06-15T12:00:00Z"] {
            pm.set_simple(gamut_xmp::WellKnownNs::Photoshop.uri(), "DateCreated", ok);
            assert!(IptcWriter::new().to_iim_block(&pm).is_ok(), "{ok}");
        }
    }

    #[test]
    fn write_irb_empty_view_is_none() {
        assert_eq!(
            IptcWriter::new().write_irb(&PhotoMetadata::new()).unwrap(),
            None
        );
    }
}
