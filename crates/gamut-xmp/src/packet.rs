//! The XMP packet wrapper (Adobe XMP Part 1 §7.3.2).
//!
//! An embedded XMP packet is the RDF/XML body bracketed by `<?xpacket?>` processing instructions:
//! `<?xpacket begin="…" id="…"?>` … body … optional whitespace padding … `<?xpacket end='r'|'w'?>`.
//! [`XmpPacket::scan`] recovers the body and the `writable`/`padding` details from raw bytes, and
//! [`XmpPacket::parse`] turns the body into a graph — [`crate::XmpMeta::from_packet`] is exactly
//! that composition. Scanning first is how an in-place editor honors the envelope: parse, modify,
//! then re-serialize with the original `writable`/`padding` on a [`crate::XmpWriter`].

use crate::error::{Result, XmpError};

/// The UTF-8 byte-order mark (`U+FEFF`). Tolerated as a leading prefix on read; never emitted.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// The serialized form of an XMP packet — the RDF/XML body inside its `<?xpacket?>` wrapper.
///
/// XMP is embedded as an `<?xpacket begin=… id=…?>` processing instruction, the `x:xmpmeta` /
/// `rdf:RDF` body, then `<?xpacket end='r'|'w'?>`. A writable (`'w'`) packet carries trailing
/// whitespace padding so it can be edited in place without rewriting the whole file. Build one from
/// bytes with [`XmpPacket::scan`], parse its body into a graph with [`XmpPacket::parse`], and
/// produce packet bytes from a graph with [`crate::XmpMeta::to_packet`] or a configured
/// [`crate::XmpWriter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmpPacket {
    /// The RDF/XML body between the opening and closing `xpacket` instructions, with surrounding
    /// whitespace trimmed.
    pub body: String,
    /// Whether the packet is writable in place (`end='w'`) versus read-only (`end='r'`).
    pub writable: bool,
    /// The number of trailing padding bytes reserved for in-place edits.
    pub padding: usize,
}

impl XmpPacket {
    /// Recovers the packet structure from raw bytes.
    ///
    /// Strips a leading UTF-8 byte-order mark, locates the `<?xpacket?>` wrapper (if present), and
    /// records whether the packet is writable and how much trailing padding it carries. If there is
    /// no wrapper the whole input is taken as the body (`writable = false`, `padding = 0`).
    ///
    /// The input is **one** packet. Anything after the first packet's trailer is ignored, except
    /// another `<?xpacket` instruction: bytes that hold several packets end to end are refused,
    /// never read as their first packet with the rest silently dropped (issue #562).
    ///
    /// # Errors
    ///
    /// Returns [`XmpError::Encoding`] if the bytes begin with a UTF-16/UTF-32 byte-order mark or are
    /// not valid UTF-8 — only UTF-8 packets are supported (Part 1 §7.1) — and
    /// [`XmpError::MultiplePackets`], carrying the offset at which the next one begins, if a
    /// `<?xpacket` instruction follows the first packet's trailer.
    pub fn scan(bytes: &[u8]) -> Result<XmpPacket> {
        let split = split_packet(bytes)?;
        let body = core::str::from_utf8(split.inner)
            .map_err(|_| XmpError::Encoding("packet body is not valid UTF-8"))?
            .trim()
            .to_owned();
        Ok(XmpPacket {
            body,
            writable: split.writable,
            padding: split.padding,
        })
    }
}

/// The result of locating the body inside a packet's wrapper.
struct SplitPacket<'a> {
    /// The RDF/XML body bytes (between the wrapper instructions, or the whole input if unwrapped),
    /// still including any surrounding whitespace — the XML parser ignores it.
    inner: &'a [u8],
    /// Whether the trailer requested in-place writability (`end='w'`).
    writable: bool,
    /// Count of trailing whitespace bytes in `inner` (the in-place edit padding).
    padding: usize,
}

/// Strips a leading byte-order mark, rejecting non-UTF-8 ones, then locates the packet body.
///
/// # Errors
///
/// Returns [`XmpError::Encoding`] for a UTF-16/UTF-32 byte-order mark (Part 1 §7.1: only UTF-8 is
/// implemented), and [`XmpError::MultiplePackets`] when another `<?xpacket` instruction follows
/// the trailer.
fn split_packet(input: &[u8]) -> Result<SplitPacket<'_>> {
    let bytes = strip_bom(input)?;
    // Offsets below are into `bytes`; the error reports one into the caller's `input`.
    let bom_len = input.len() - bytes.len();

    let Some(header) = find(bytes, b"<?xpacket") else {
        // Bare RDF/XML (no wrapper) — e.g. some WebP/AVIF payloads.
        return Ok(SplitPacket {
            inner: bytes,
            writable: false,
            padding: trailing_whitespace(bytes),
        });
    };

    // Body starts after the header PI's closing "?>".
    let Some(rel) = find(&bytes[header..], b"?>") else {
        return Err(XmpError::Encoding("unterminated <?xpacket?> header"));
    };
    let body_start = header + rel + 2;

    // The trailer is the next "<?xpacket" after the body; the packet is writable only when its
    // `end` attribute says so (`end="w"` in either quote style, Part 1 §7.3.2) — an unrelated `w`
    // byte elsewhere in the instruction must not count.
    let (body_end, writable) = match find(&bytes[body_start..], b"<?xpacket") {
        Some(rel) => {
            let trailer = body_start + rel;
            let trailer_end = find(&bytes[trailer..], b"?>").map_or(bytes.len(), |e| trailer + e);
            // A second packet after this one's trailer is a catenation (Part 3, "External storage
            // of metadata"). Reading only the first would drop the rest without a signal, and
            // merging needs a conflict rule no part of the specification supplies — so refuse.
            if let Some(next) = find(&bytes[trailer_end..], b"<?xpacket") {
                return Err(XmpError::MultiplePackets(bom_len + trailer_end + next));
            }
            let pi = &bytes[trailer..trailer_end];
            let writable = find(pi, b"end=\"w\"").is_some() || find(pi, b"end='w'").is_some();
            (trailer, writable)
        }
        None => (bytes.len(), false),
    };

    let inner = &bytes[body_start..body_end];
    Ok(SplitPacket {
        inner,
        writable,
        padding: trailing_whitespace(inner),
    })
}

/// Removes a leading UTF-8 byte-order mark; errors on a UTF-16/32 mark.
fn strip_bom(bytes: &[u8]) -> Result<&[u8]> {
    if let Some(rest) = bytes.strip_prefix(&UTF8_BOM) {
        return Ok(rest);
    }
    // Reject a UTF-16/32 byte-order mark: UTF-16 BE (FE FF), UTF-16 LE / UTF-32 LE (FF FE …),
    // UTF-32 BE (00 00 FE FF). Only UTF-8 is supported.
    let non_utf8_boms: [&[u8]; 3] = [&[0xFE, 0xFF], &[0xFF, 0xFE], &[0x00, 0x00, 0xFE, 0xFF]];
    if non_utf8_boms.iter().any(|bom| bytes.starts_with(bom)) {
        return Err(XmpError::Encoding(
            "only UTF-8 packets are supported (UTF-16/32 byte-order mark found)",
        ));
    }
    Ok(bytes)
}

/// The number of trailing ASCII-whitespace bytes in `bytes`.
fn trailing_whitespace(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .rev()
        .take_while(|b| b.is_ascii_whitespace())
        .count()
}

/// The index of the first occurrence of `needle` in `haystack`, if any.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRAPPED: &str = concat!(
        "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>",
        "<rdf:RDF/>",
        "   \n  ", // 3 spaces + newline + 2 spaces = 6 padding bytes
        "<?xpacket end=\"w\"?>",
    );

    #[test]
    fn scans_writable_wrapper_with_exact_padding() {
        let pkt = XmpPacket::scan(WRAPPED.as_bytes()).unwrap();
        assert_eq!(pkt.body, "<rdf:RDF/>");
        assert!(pkt.writable);
        assert_eq!(pkt.padding, 6);
    }

    #[test]
    fn read_only_trailer_is_not_writable() {
        let s = "<?xpacket begin=\"\" id=\"x\"?><rdf:RDF/><?xpacket end=\"r\"?>";
        let pkt = XmpPacket::scan(s.as_bytes()).unwrap();
        assert!(!pkt.writable);
        assert_eq!(pkt.padding, 0);
    }

    #[test]
    fn bare_rdf_without_wrapper_is_accepted() {
        let pkt = XmpPacket::scan(b"<rdf:RDF/>").unwrap();
        assert_eq!(pkt.body, "<rdf:RDF/>");
        assert!(!pkt.writable);
    }

    #[test]
    fn strips_utf8_bom() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(b"<rdf:RDF/>");
        let pkt = XmpPacket::scan(&bytes).unwrap();
        assert_eq!(pkt.body, "<rdf:RDF/>");
    }

    #[test]
    fn rejects_utf16_bom() {
        // The BOM must be rejected by the BOM check specifically — not fall through to the generic
        // "not valid UTF-8" path — so the message names the byte-order mark.
        let err = XmpPacket::scan(&[0xFE, 0xFF, 0x00, 0x3C]).unwrap_err();
        assert!(
            matches!(&err, XmpError::Encoding(m) if m.contains("byte-order mark")),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_utf16_le_and_utf32_be_boms() {
        // Part 1 §7.1 also permits UTF-16 LE and UTF-32; every non-UTF-8 mark takes the
        // BOM-specific rejection.
        for bom in [&[0xFF, 0xFE][..], &[0x00, 0x00, 0xFE, 0xFF][..]] {
            let mut bytes = bom.to_vec();
            bytes.extend_from_slice(b"<rdf:RDF/>");
            let err = XmpPacket::scan(&bytes).unwrap_err();
            assert!(
                matches!(&err, XmpError::Encoding(m) if m.contains("byte-order mark")),
                "BOM {bom:02X?} must be rejected as a byte-order mark, got {err:?}"
            );
        }
    }

    #[test]
    fn utf32_le_bom_is_rejected_via_its_utf16_le_prefix() {
        // UTF-32 LE (FF FE 00 00) begins with the UTF-16 LE mark, so the two-byte check
        // catches it — pinned so the four-byte mark never slips through as "valid UTF-8".
        let err = XmpPacket::scan(&[0xFF, 0xFE, 0x00, 0x00, b'<']).unwrap_err();
        assert!(
            matches!(&err, XmpError::Encoding(m) if m.contains("byte-order mark")),
            "got {err:?}"
        );
    }

    #[test]
    fn single_quoted_trailer_is_detected() {
        // §7.3.2 examples use double quotes, but XML permits either style.
        let writable = "<?xpacket begin='' id='x'?><rdf:RDF/><?xpacket end='w'?>";
        assert!(XmpPacket::scan(writable.as_bytes()).unwrap().writable);
        let read_only = "<?xpacket begin='' id='x'?><rdf:RDF/><?xpacket end='r'?>";
        assert!(!XmpPacket::scan(read_only.as_bytes()).unwrap().writable);
    }

    #[test]
    fn unrelated_w_byte_in_the_trailer_is_not_writable() {
        // Writability comes from the end attribute alone — a stray `w` elsewhere in the
        // instruction (here in a nonstandard extra attribute) must not flip it.
        let s = "<?xpacket begin=\"\" id=\"x\"?><rdf:RDF/><?xpacket end=\"r\" note=\"w\"?>";
        assert!(!XmpPacket::scan(s.as_bytes()).unwrap().writable);
    }

    #[test]
    fn a_second_packet_after_the_trailer_is_refused_at_its_offset() {
        // Issue #562: two packets end to end are refused, not read as the first. The offset is
        // where the second `<?xpacket` begins — past the separator, not at the trailer's end.
        let catenated = format!("{WRAPPED}\n{WRAPPED}");
        assert_eq!(
            XmpPacket::scan(catenated.as_bytes()),
            Err(XmpError::MultiplePackets(WRAPPED.len() + 1))
        );
    }

    #[test]
    fn multiple_packets_offset_counts_a_stripped_bom() {
        // The offset indexes the caller's input, so the three BOM bytes `scan` strips are counted.
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(format!("{WRAPPED}{WRAPPED}").as_bytes());
        assert_eq!(
            XmpPacket::scan(&bytes),
            Err(XmpError::MultiplePackets(UTF8_BOM.len() + WRAPPED.len()))
        );
    }

    #[test]
    fn header_without_trailer_is_read_only() {
        // A truncated wrapper (header but no trailer) still yields the body, as read-only.
        let pkt = XmpPacket::scan(b"<?xpacket begin=\"\" id=\"x\"?><rdf:RDF/>").unwrap();
        assert_eq!(pkt.body, "<rdf:RDF/>");
        assert!(!pkt.writable);
    }

    #[test]
    fn accepts_bom_character_in_the_begin_attribute() {
        // §7.3.2 recommends begin="\u{FEFF}" — packets from Adobe tools carry the BOM character
        // inside the attribute value, which must not confuse body extraction.
        let s = "<?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
                 <rdf:RDF/><?xpacket end=\"r\"?>";
        let pkt = XmpPacket::scan(s.as_bytes()).unwrap();
        assert_eq!(pkt.body, "<rdf:RDF/>");
        assert!(!pkt.writable);
    }

    #[test]
    fn find_handles_edges() {
        assert_eq!(find(b"xy<?xpacket", b"<?xpacket"), Some(2));
        // An exact-length match must be found (guards the `>` length check).
        assert_eq!(find(b"abc", b"abc"), Some(0));
        // A needle longer than the haystack is absent.
        assert_eq!(find(b"ab", b"abc"), None);
        // An empty needle is never found (and must not panic on `windows(0)`).
        assert_eq!(find(b"abc", b""), None);
    }

    #[test]
    fn rejects_non_utf8_body() {
        let mut bytes = b"<rdf:RDF>".to_vec();
        bytes.push(0xFF); // invalid UTF-8 in the body
        assert!(matches!(
            XmpPacket::scan(&bytes),
            Err(XmpError::Encoding(_))
        ));
    }
}
