//! Coded character set handling for IPTC-IIM text values (IPTC-IIM 4.2 §1.6, dataset 1:90).
//!
//! IIM text is interpreted according to dataset 1:90 (Coded Character Set), a sequence of ISO 2022
//! control functions. In practice photo metadata uses one of two encodings: **UTF-8**, designated
//! by the escape `ESC % G` (`1B 25 47`), or — when 1:90 is absent — the **default** set, which the
//! spec defines as ISO 646 IRV / ISO 4873 DV (IPTC-IIM 4.2 §1.6(a) and dataset 1:90). gamut decodes
//! that default as ISO-8859-1 (Latin-1): Latin-1 is the de-facto reading used by exiv2/exiftool, and
//! it is a strict superset of ISO 646 IRV, so it never rejects a valid default-set value.
//!
//! Any other (exotic) ISO 2022 designation is reported as [`IptcError::Unsupported`] rather than
//! silently mis-decoded. The UTF-8 escape octets are documented in the IPTC-NAA Code Library
//! (IPTC-IIM 4.2 Appendix C); gamut matches the canonical `ESC % G` sequence.

use crate::error::{IptcError, Result};
use crate::iim::IimBlock;

/// The coded character set an IIM text value is encoded in.
///
/// Marked `#[non_exhaustive]`: supporting a further ISO 2022 designation later must not be a
/// breaking change. `#[repr(u8)]` with permanent, append-only discriminants, so the value crosses
/// a C boundary unchanged; a new charset takes the next free discriminant.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IimCharset {
    /// ISO-8859-1 (Latin-1) — gamut's reading of the spec default when dataset 1:90 is absent.
    Latin1 = 0,
    /// UTF-8 — designated by the `ESC % G` escape in dataset 1:90.
    Utf8 = 1,
}

impl IimCharset {
    /// The ISO 2022 escape (`ESC % G` = `1B 25 47`) that designates UTF-8 in dataset 1:90.
    pub const UTF8_ESCAPE: [u8; 3] = [0x1B, 0x25, 0x47];

    /// Determines the charset of a parsed block from its dataset 1:90 (Coded Character Set).
    ///
    /// Absent or empty 1:90 ⇒ [`IimCharset::Latin1`]; the exact `ESC % G` escape ⇒
    /// [`IimCharset::Utf8`].
    ///
    /// # Errors
    ///
    /// Returns [`IptcError::Unsupported`] if 1:90 designates any other ISO 2022 character set.
    pub fn detect(block: &IimBlock) -> Result<Self> {
        match block
            .datasets
            .iter()
            .find(|d| d.record == 1 && d.dataset == 90)
        {
            None => Ok(IimCharset::Latin1),
            Some(d) if d.data.is_empty() => Ok(IimCharset::Latin1),
            Some(d) if d.data.as_slice() == Self::UTF8_ESCAPE => Ok(IimCharset::Utf8),
            Some(_) => Err(IptcError::Unsupported(
                "IPTC IIM: unsupported coded character set in 1:90",
            )),
        }
    }

    /// Decodes value octets into a [`String`] using this charset.
    ///
    /// # Errors
    ///
    /// Returns [`IptcError::Malformed`] for [`IimCharset::Utf8`] octets that are not valid UTF-8.
    /// Latin-1 decoding is infallible.
    pub fn decode(self, bytes: &[u8]) -> Result<String> {
        match self {
            IimCharset::Latin1 => Ok(decode_latin1(bytes)),
            IimCharset::Utf8 => core::str::from_utf8(bytes)
                .map(str::to_owned)
                .map_err(|_| IptcError::Malformed("IPTC IIM: invalid UTF-8 text value")),
        }
    }

    /// Encodes text into value octets using this charset.
    ///
    /// # Errors
    ///
    /// Returns [`IptcError::Malformed`] if encoding as [`IimCharset::Latin1`] and the text contains a
    /// character beyond U+00FF (callers should select [`IimCharset::Utf8`] for such text).
    pub fn encode(self, text: &str) -> Result<Vec<u8>> {
        match self {
            IimCharset::Utf8 => Ok(text.as_bytes().to_vec()),
            IimCharset::Latin1 => encode_latin1(text),
        }
    }

    /// The dataset 1:90 escape sequence that designates this charset, if one is required.
    ///
    /// [`IimCharset::Utf8`] returns its escape; [`IimCharset::Latin1`] is the default and returns
    /// `None` (no 1:90 dataset need be written).
    #[must_use]
    pub fn escape_sequence(self) -> Option<[u8; 3]> {
        match self {
            IimCharset::Utf8 => Some(Self::UTF8_ESCAPE),
            IimCharset::Latin1 => None,
        }
    }
}

/// Decodes ISO-8859-1 (Latin-1) octets: each octet maps one-to-one to U+0000..=U+00FF.
pub(crate) fn decode_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// Encodes text as ISO-8859-1 (Latin-1), erroring on any character beyond U+00FF.
pub(crate) fn encode_latin1(text: &str) -> Result<Vec<u8>> {
    text.chars()
        .map(|c| {
            u8::try_from(c as u32)
                .map_err(|_| IptcError::Malformed("IPTC: text not representable in Latin-1"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iim::IimDataSet;

    #[test]
    fn discriminants_are_the_frozen_c_abi_values() {
        assert_eq!(IimCharset::Latin1 as u8, 0);
        assert_eq!(IimCharset::Utf8 as u8, 1);
    }

    fn block_with_190(data: &[u8]) -> IimBlock {
        IimBlock {
            datasets: vec![IimDataSet {
                record: 1,
                dataset: 90,
                data: data.to_vec(),
            }],
        }
    }

    #[test]
    fn detect_defaults_to_latin1_when_absent_or_empty() {
        assert_eq!(
            IimCharset::detect(&IimBlock::default()).unwrap(),
            IimCharset::Latin1
        );
        assert_eq!(
            IimCharset::detect(&block_with_190(&[])).unwrap(),
            IimCharset::Latin1
        );
    }

    #[test]
    fn detect_utf8_escape() {
        let block = block_with_190(&IimCharset::UTF8_ESCAPE);
        assert_eq!(IimCharset::detect(&block).unwrap(), IimCharset::Utf8);
    }

    #[test]
    fn detect_ignores_datasets_that_are_not_1_90() {
        // Datasets that share only the record (1:00) or only the dataset number (2:90) of 1:90 must
        // not be mistaken for the coded character set; with no real 1:90 the default is Latin-1.
        let block = IimBlock {
            datasets: vec![
                IimDataSet {
                    record: 1,
                    dataset: 0,
                    data: vec![0x00, 0x04],
                },
                IimDataSet {
                    record: 2,
                    dataset: 90,
                    data: b"London".to_vec(),
                },
            ],
        };
        assert_eq!(IimCharset::detect(&block).unwrap(), IimCharset::Latin1);
    }

    #[test]
    fn detect_rejects_other_charset() {
        // ESC ( B (designate ASCII into G0) is a different ISO 2022 function gamut does not handle.
        assert!(IimCharset::detect(&block_with_190(&[0x1B, 0x28, 0x42])).is_err());
    }

    #[test]
    fn latin1_roundtrip_including_high_bytes() {
        // 0xE9 is 'é' in Latin-1.
        assert_eq!(IimCharset::Latin1.decode(&[b'A', 0xE9]).unwrap(), "Aé");
        assert_eq!(IimCharset::Latin1.encode("Aé").unwrap(), vec![b'A', 0xE9]);
    }

    #[test]
    fn latin1_encode_rejects_out_of_range() {
        // U+20AC (€) is beyond Latin-1.
        assert!(IimCharset::Latin1.encode("€").is_err());
    }

    #[test]
    fn utf8_decode_and_encode() {
        let bytes = "café €".as_bytes();
        assert_eq!(IimCharset::Utf8.decode(bytes).unwrap(), "café €");
        assert_eq!(IimCharset::Utf8.encode("café €").unwrap(), bytes);
        assert!(IimCharset::Utf8.decode(&[0xFF, 0xFE]).is_err());
    }

    #[test]
    fn escape_sequence_is_present_only_for_utf8() {
        assert_eq!(
            IimCharset::Utf8.escape_sequence(),
            Some(IimCharset::UTF8_ESCAPE)
        );
        assert_eq!(IimCharset::Latin1.escape_sequence(), None);
    }
}
