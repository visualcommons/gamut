//! XMP sidecar files — a packet stored *beside* the image instead of inside it (Adobe XMP Part 3,
//! Introduction, "External storage of metadata").
//!
//! A sidecar is the standard interchange for RAW workflows: a camera's proprietary raw file is not
//! extensible, so the metadata travels in `<image>.xmp` next to it. Part 3 asks that such a file be
//! "a complete, well-formed XML document, including the leading XML declaration", written "as though
//! it were embedded and then had the XMP packets extracted and catenated by a postprocessor", with
//! the `.xmp` extension and the same base name as the image; applications find it by looking in the
//! image's directory.
//!
//! [`XmpSidecar::read`] and [`XmpSidecar::write`] are the bytes-in / bytes-out pair. This crate has
//! no filesystem API and the naming convention is documented, not enforced: the caller owns the
//! path, exactly as the format crates own the embedded packet's location in their containers.
//!
//! **One packet, not a catenation.** That "catenated by a postprocessor" phrase describes a file
//! holding *several* `<?xpacket?>` packets end to end — a shape [`XmpSidecar::write`] never
//! produces (it emits exactly one) and [`XmpSidecar::read`] does not read: it takes the first
//! packet and silently discards the rest. See [`XmpSidecar::read`] for what that costs the caller,
//! and issue [#562](https://github.com/visualcommons/gamut/issues/562) for the open decision.

use quick_xml::NsReader;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

use crate::error::{Result, XmpError};
use crate::model::XmpMeta;
use crate::namespace::XMPMETA_NAMESPACE;
use crate::packet::XmpPacket;
use crate::writer::XmpWriter;

/// The XMP sidecar file format: an XMP packet as a standalone `.xmp` file.
///
/// The one thing this crate requires of a sidecar beyond an embedded packet is the `x:xmpmeta`
/// element — **a gamut rule, stricter than the specification rather than derived from it**. Part 1
/// §7.3.3 says an "optional" `x:xmpmeta` element "may be placed around the rdf:RDF element" and that
/// a processor "should tolerate" one: permission, never a requirement. Part 3's "External storage
/// of metadata" bullets never mention the element at all. A wrapper-less `.xmp` file is therefore
/// spec-conformant, and [`XmpSidecar::read`] rejects it anyway.
///
/// What §7.3.3 does supply is the element's purpose — "to identify XMP metadata within general XML
/// text that might contain other non-XMP uses of RDF" — and a standalone `.xmp` file is exactly
/// that general XML text: without the element nothing in the bytes says the RDF is XMP. exiv2 draws
/// the line elsewhere again, accepting `<?xpacket` **or** `<x:xmpmeta`, so an xpacket-wrapped bare
/// `rdf:RDF` is a sidecar to exiv2 and not to this crate. [`XmpSidecar::write`] always emits the
/// element.
///
/// ```
/// use gamut_xmp::{WellKnownNs, XmpMeta, XmpSidecar};
///
/// let mut meta = XmpMeta::new();
/// meta.set_text(WellKnownNs::Xmp.uri(), "Rating", "5");
///
/// // The bytes of `photo.xmp`, ready to be written beside `photo.dng`...
/// let file = XmpSidecar::write(&meta);
/// // ...and read back from it.
/// let parsed = XmpSidecar::read(&file)?;
/// assert_eq!(parsed.get_text(WellKnownNs::Xmp.uri(), "Rating"), Some("5"));
/// # Ok::<(), gamut_xmp::XmpError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct XmpSidecar;

impl XmpSidecar {
    /// Parses the bytes of a `.xmp` sidecar file into a property graph.
    ///
    /// Accepts a UTF-8 file with or without the leading XML declaration, with or without the
    /// `<?xpacket?>` wrapper, tolerating a leading byte-order mark — everything
    /// [`XmpMeta::from_packet`] accepts — **provided** the document element is `x:xmpmeta`
    /// (namespace `adobe:ns:meta/`, Part 1 §7.3.3). Trailing padding inside the packet wrapper is
    /// ignored.
    ///
    /// # A catenated file loses everything after its first packet
    ///
    /// Part 3 asks that external metadata be written "as though it were embedded and then had the
    /// XMP packets extracted **and catenated by a postprocessor**", so a conforming producer may
    /// hand over a `.xmp` file holding several `<?xpacket?>` packets end to end. This reads the
    /// first one and **silently discards the rest**: no error, and no way to tell the result from
    /// a genuine single-packet file. Two [`XmpSidecar::write`] outputs concatenated read back as
    /// the properties of the first alone. Adobe XMPCore rejects those same bytes.
    ///
    /// Read such a file only if you know it holds one packet, or split it on `<?xpacket` yourself
    /// and read each part. Which of rejecting, merging or keeping this is right is open as issue
    /// [#562](https://github.com/visualcommons/gamut/issues/562); until it is settled the
    /// behaviour is documented rather than changed, so it will not shift under a patch release.
    ///
    /// # Errors
    ///
    /// Returns [`XmpError::MissingXmpMeta`] naming the document element when it is not `x:xmpmeta`
    /// — a bare `rdf:RDF` document is a packet body, valid XMP that [`XmpMeta::from_packet`]
    /// accepts, but not one this crate reads as a sidecar. Otherwise the same errors as
    /// [`XmpMeta::from_packet`]:
    /// [`XmpError::Encoding`] for non-UTF-8 input, [`XmpError::Xml`] for malformed XML,
    /// [`XmpError::MissingRdf`] when no `rdf:RDF` is found, and the RDF/XML-for-XMP errors for
    /// constructs XMP does not permit.
    ///
    /// Both exiv2 and the specification are more permissive here. exiv2's sidecar sniffer
    /// (`isXmpType`) accepts a file that starts with `<?xpacket` **or** `<x:xmpmeta`, so it reads
    /// as a sidecar both a wrapper-less packet and an xpacket-wrapped bare `rdf:RDF` that this
    /// rejects; and §7.3.3 calls the element optional, so the rejection is gamut's own choice and
    /// not a spec requirement (see the type's documentation for why it is made). A caller who wants
    /// that latitude uses [`XmpMeta::from_packet`].
    pub fn read(bytes: &[u8]) -> Result<XmpMeta> {
        let packet = XmpPacket::scan(bytes)?;
        // Parse first so a malformed file reports its malformation, not a missing wrapper.
        let meta = packet.parse()?;
        if let Some(root) = document_element_unless_xmpmeta(&packet.body)? {
            return Err(XmpError::MissingXmpMeta(root));
        }
        Ok(meta)
    }

    /// Serializes a property graph as the bytes of a `.xmp` sidecar file.
    ///
    /// The file is the XML declaration Part 3 asks for, then a read-only (`end="r"`, no padding)
    /// `<?xpacket?>` packet whose body is the canonical RDF/XML (Part 1 §7) inside `x:xmpmeta`.
    /// UTF-8, no byte-order mark. Being canonical, it is byte-stable for a given graph, so two
    /// sidecars of the same metadata diff clean.
    #[must_use]
    pub fn write(meta: &XmpMeta) -> Vec<u8> {
        let mut out = XML_DECLARATION.as_bytes().to_vec();
        out.extend_from_slice(
            &XmpWriter::new()
                .wrap_xmpmeta(true)
                .writable(false)
                .serialize(meta),
        );
        out
    }
}

/// The XML declaration a sidecar leads with (Part 3, "External storage of metadata").
const XML_DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";

/// The name of the document element of `xml` when it is **not** `x:xmpmeta`, or `None` when it is.
///
/// Walks past the prolog (declaration, comments, processing instructions, whitespace) to the first
/// start tag and resolves its namespace, so the check is on the element's identity rather than on
/// the literal `x:` prefix a writer happened to choose.
///
/// **Reachability.** Called from [`XmpSidecar::read`] the answer can only ever be `None` or
/// `Some("RDF")`: `read` parses the body first, and the reader admits exactly one document element,
/// `rdf:RDF` or `x:xmpmeta` (`reader.rs`'s `find_rdf`, over a tree `build_tree` has already
/// rejected multiple roots in). So on that path the namespace comparison, the `Empty` arm and the
/// end-of-input error are all unreachable, and the second lex is a bounded extra pass over bytes
/// already parsed once. It is kept as a total function over any XML — unit-tested directly for the
/// cases `read` cannot produce — rather than threading the root element out of `find_rdf`, which
/// would widen the blast radius in `reader.rs` for a one-off cost.
///
/// # Errors
///
/// Returns [`XmpError::Xml`] if the prolog cannot be lexed or the document has no element at all.
/// [`XmpSidecar::read`] parses the body before calling this, so on that path neither happens.
fn document_element_unless_xmpmeta(xml: &str) -> Result<Option<String>> {
    let mut reader = NsReader::from_str(xml);
    loop {
        let event = reader
            .read_event()
            .map_err(|err| XmpError::Xml(err.to_string()))?;
        // Checked before the match (as `reader.rs` does) so the loop's exit does not depend on a
        // match arm: end of input is always terminal.
        if matches!(event, Event::Eof) {
            return Err(XmpError::Xml("sidecar has no document element".into()));
        }
        if let Event::Start(start) | Event::Empty(start) = event {
            let (resolved, local) = reader.resolver().resolve_element(start.name());
            let local = String::from_utf8_lossy(local.as_ref()).into_owned();
            let is_xmpmeta = matches!(resolved, ResolveResult::Bound(ns)
                if ns.as_ref() == XMPMETA_NAMESPACE.as_bytes())
                && local == "xmpmeta";
            return Ok((!is_xmpmeta).then_some(local));
        }
        // Declaration, comments, processing instructions, DOCTYPE and whitespace: prolog, skipped.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespace::WellKnownNs;

    const RDF_ONLY: &str = "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
                            <rdf:Description rdf:about=\"\"/></rdf:RDF>";

    /// `RDF_ONLY` inside the wrapper Part 1 §7.3.3 defines.
    fn wrapped(body: &str) -> String {
        format!("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">{body}</x:xmpmeta>")
    }

    #[test]
    fn write_emits_the_exact_sidecar_for_an_empty_graph() {
        // Byte-exact: the XML declaration, the read-only packet wrapper with no padding, the
        // x:xmpmeta wrapper, and the canonical body — the file is meant to diff clean.
        let file = XmpSidecar::write(&XmpMeta::new());
        assert_eq!(
            core::str::from_utf8(&file).unwrap(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n \
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
             <rdf:Description rdf:about=\"\"/>\n \
             </rdf:RDF>\n\
             </x:xmpmeta>\n\
             <?xpacket end=\"r\"?>"
        );
    }

    #[test]
    fn read_accepts_what_write_produces() {
        // The two halves agree on the wrapper requirement: a written sidecar reads back.
        let mut meta = XmpMeta::new();
        meta.set_text(WellKnownNs::Xmp.uri(), "CreatorTool", "gamut");
        let parsed = XmpSidecar::read(&XmpSidecar::write(&meta)).unwrap();
        assert_eq!(parsed, meta);
    }

    #[test]
    fn read_rejects_a_bare_rdf_document() {
        // A packet body without x:xmpmeta is valid embedded XMP but not a sidecar; the error names
        // the element found so the caller knows which wrapper is missing. It is deliberately not
        // `Prohibited`: Part 1 §7.3.3 permits the wrapper-less form, and `XmpMeta::from_packet`
        // reads exactly these bytes.
        let err = XmpSidecar::read(RDF_ONLY.as_bytes()).unwrap_err();
        assert!(
            matches!(&err, XmpError::MissingXmpMeta(root) if root == "RDF"),
            "got {err:?}"
        );
        assert!(
            XmpMeta::from_packet(RDF_ONLY.as_bytes()).is_ok(),
            "the same bytes must stay valid as an embedded packet"
        );
    }

    #[test]
    fn read_rejects_a_bare_rdf_document_even_inside_the_packet_wrapper() {
        // The xpacket wrapper alone does not make a sidecar: the document element must still be
        // x:xmpmeta.
        let file = format!(
            "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>{RDF_ONLY}<?xpacket end=\"r\"?>"
        );
        let err = XmpSidecar::read(file.as_bytes()).unwrap_err();
        assert!(
            matches!(&err, XmpError::MissingXmpMeta(root) if root == "RDF"),
            "got {err:?}"
        );
    }

    #[test]
    fn read_accepts_a_wrapped_document_without_the_packet_wrapper() {
        // Part 1 §7.3.3: x:xmpmeta may stand without the xpacket instructions.
        let meta = XmpSidecar::read(wrapped(RDF_ONLY).as_bytes()).unwrap();
        assert!(meta.properties.is_empty());
    }

    #[test]
    fn read_skips_bom_declaration_comment_and_whitespace_before_the_document_element() {
        // Part 3 asks for the leading XML declaration; a BOM and a comment are legal XML prolog
        // too. None of them is the document element.
        let file = format!(
            "\u{FEFF}<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- exported -->\n  {}",
            wrapped(RDF_ONLY)
        );
        let meta = XmpSidecar::read(file.as_bytes()).unwrap();
        assert!(meta.properties.is_empty());
    }

    #[test]
    fn read_reports_malformed_xml_before_the_missing_wrapper() {
        // Parse errors take precedence: an ill-formed bare document (mismatched end tag) is
        // "malformed XML", not "not a sidecar", so the diagnosis points at the real defect.
        let err = XmpSidecar::read(
            b"<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"></oops>",
        )
        .unwrap_err();
        assert!(matches!(err, XmpError::Xml(_)), "got {err:?}");
    }

    #[test]
    fn document_element_check_resolves_the_namespace_not_the_prefix() {
        // An `xmpmeta` element in the wrong namespace is not the wrapper; the wrapper under
        // another prefix is. Identity is the URI (Part 1 §6.2).
        assert_eq!(
            document_element_unless_xmpmeta(
                "<x:xmpmeta xmlns:x=\"urn:not-adobe\"><rdf:RDF/></x:xmpmeta>"
            )
            .unwrap(),
            Some("xmpmeta".to_owned())
        );
        assert_eq!(
            document_element_unless_xmpmeta(
                "<meta:xmpmeta xmlns:meta=\"adobe:ns:meta/\"><rdf:RDF/></meta:xmpmeta>"
            )
            .unwrap(),
            None
        );
        // An empty-element wrapper is still the wrapper (quick-xml reports it as `Empty`).
        assert_eq!(
            document_element_unless_xmpmeta("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>").unwrap(),
            None
        );
    }

    #[test]
    fn document_element_check_reports_an_element_less_document() {
        // Only reachable outside `read` (which has already parsed an rdf:RDF); pinned so the
        // loop's EOF arm is a typed error, not a hang or a panic.
        let err = document_element_unless_xmpmeta("<?xml version=\"1.0\"?>\n<!-- nothing -->")
            .unwrap_err();
        assert!(
            matches!(&err, XmpError::Xml(m) if m.contains("no document element")),
            "got {err:?}"
        );
    }
}
