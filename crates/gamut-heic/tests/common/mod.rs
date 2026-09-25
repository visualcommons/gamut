//! Shared HEIF test fixtures: model builders that drive `gamut_isobmff::write`, plus hand-authored
//! box byte-builders (mirroring `gamut-isobmff/tests/common`) for the accounting/meta cases that a
//! normalising writer cannot express.
#![allow(dead_code)] // each integration-test binary uses a different subset

use gamut_isobmff::{IsoBmffImage, Item, ItemReference, Property, PropertyKind, write};

// ---- model builders (write-based fixtures) -------------------------------------------------

/// A bare item: no name, no mime info, no references, no properties.
pub fn item(id: u32, item_type: [u8; 4], payload: Vec<u8>) -> Item {
    Item {
        id,
        item_type,
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden: false,
        references: vec![],
        properties: vec![],
        payload,
    }
}

/// An `hvcC` codec-configuration property (essential), with the given opaque record bytes.
pub fn hvcc(data: Vec<u8>) -> Property {
    Property {
        essential: true,
        kind: PropertyKind::CodecConfiguration {
            kind: *b"hvcC",
            data,
        },
    }
}

/// A non-essential `ispe` property.
pub fn ispe(width: u32, height: u32) -> Property {
    Property {
        essential: false,
        kind: PropertyKind::ImageSpatialExtents { width, height },
    }
}

/// A non-essential `auxC` property with the given aux-type URN.
pub fn auxc(aux_type: &str) -> Property {
    Property {
        essential: false,
        kind: PropertyKind::AuxiliaryType {
            aux_type: aux_type.to_string(),
            aux_subtype: vec![],
        },
    }
}

/// An outgoing item reference.
pub fn iref(reference_type: &[u8; 4], to: &[u32]) -> ItemReference {
    ItemReference {
        reference_type: *reference_type,
        to_item_ids: to.to_vec(),
    }
}

/// A canonical HEVC coded-image item: `hvc1`, one `hvcC` + one `ispe`, with the given payload.
pub fn hvc1_item(id: u32, payload: Vec<u8>) -> Item {
    Item {
        properties: vec![hvcc(vec![1, 2, 3, 4]), ispe(64, 48)],
        ..item(id, *b"hvc1", payload)
    }
}

/// Wraps items into a complete `heic` file whose primary is the item with `primary_id`.
pub fn heic_image(primary_id: u32, items: Vec<Item>) -> IsoBmffImage {
    IsoBmffImage::new(*b"heic", vec![*b"heic", *b"mif1"], primary_id, items)
}

/// Serialises a model to a clean HEIF file (`ftyp` + `meta` + `mdat`).
pub fn clean_file(primary_id: u32, items: Vec<Item>) -> Vec<u8> {
    write(&heic_image(primary_id, items)).expect("valid model")
}

// ---- hand-authored box byte-builders -------------------------------------------------------

/// One complete box: 32-bit size + type + body.
pub fn bx(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
    out.extend_from_slice(ty);
    out.extend_from_slice(body);
    out
}

/// One complete FullBox: box header + 1-byte version + 3-byte flags + body.
pub fn full(ty: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(4 + body.len());
    inner.push(version);
    inner.extend_from_slice(&flags.to_be_bytes()[1..]);
    inner.extend_from_slice(body);
    bx(ty, &inner)
}

/// Concatenates byte chunks.
pub fn cat<T: AsRef<[u8]>>(parts: &[T]) -> Vec<u8> {
    parts.iter().flat_map(|p| p.as_ref().to_vec()).collect()
}

/// A minimal `ftyp` with the given major brand, minor version 0, no compatible brands.
pub fn ftyp(major: &[u8; 4]) -> Vec<u8> {
    bx(b"ftyp", &cat(&[&major[..], &[0, 0, 0, 0]]))
}

/// A spec `hdlr` with handler type `pict`.
pub fn hdlr() -> Vec<u8> {
    full(
        b"hdlr",
        0,
        0,
        &cat(&[&[0u8; 4][..], b"pict", &[0u8; 12], &[0u8]]),
    )
}

/// A `pitm` v0 naming `id`.
pub fn pitm_v0(id: u16) -> Vec<u8> {
    full(b"pitm", 0, 0, &id.to_be_bytes())
}

/// An `infe` v2 for an unprotected item; `flags & 1` marks it hidden.
pub fn infe_v2(id: u16, item_type: &[u8; 4], hidden: bool) -> Vec<u8> {
    full(
        b"infe",
        2,
        u32::from(hidden),
        &cat(&[&id.to_be_bytes()[..], &[0, 0], item_type, &[0]]),
    )
}

/// An `iinf` v0 wrapping the given `infe` boxes.
pub fn iinf_v0(infes: &[Vec<u8>]) -> Vec<u8> {
    let mut body = (infes.len() as u16).to_be_bytes().to_vec();
    for infe in infes {
        body.extend_from_slice(infe);
    }
    full(b"iinf", 0, 0, &body)
}

/// A `meta` FullBox with the given children.
pub fn meta(children: &[Vec<u8>]) -> Vec<u8> {
    full(b"meta", 0, 0, &cat(children))
}

// ---- C2PA box builders (C2PA 2.4 §A.5.1) ---------------------------------------------------

/// The C2PA `ContentProvenanceBox` extended (user) type — C2PA 2.4 §A.5.1.1.
pub const C2PA_UUID: [u8; 16] = [
    0xD8, 0xFE, 0xC3, 0xD6, 0x1B, 0x0E, 0x48, 0x3C, 0x92, 0x97, 0x58, 0x28, 0x87, 0x7E, 0xC4, 0x81,
];

/// A top-level `uuid` box with an explicit user type, `FullBox` version/flags, null-terminated
/// `box_purpose` and raw `data` — every field independently settable so malformed variants are
/// expressible (§A.5.1.2).
pub fn uuid_box(
    user_type: &[u8; 16],
    version: u8,
    flags: u32,
    purpose: &str,
    data: &[u8],
) -> Vec<u8> {
    bx(
        b"uuid",
        &cat(&[
            &user_type[..],
            &[version],
            &flags.to_be_bytes()[1..],
            purpose.as_bytes(),
            &[0],
            data,
        ]),
    )
}

/// A well-formed C2PA `ContentProvenanceBox`: the C2PA user type, version 0/flags 0, the given
/// `box_purpose`, and `data` laid out as `merkle_offset` (8 big-endian bytes when `Some`, omitted
/// entirely when `None`), then `store`, then `padding`.
///
/// The offset is an explicit parameter rather than something derived from `purpose`, so a fixture
/// cannot silently re-encode the reader's own assumption about which purposes carry it. §A.5.3
/// mandates it for `manifest` and `original`; for `update` the specification is silent and both
/// layouts occur, so update fixtures must state which one they are.
pub fn c2pa_box(
    purpose: &str,
    merkle_offset: Option<u64>,
    store: &[u8],
    padding: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();
    if let Some(offset) = merkle_offset {
        data.extend_from_slice(&offset.to_be_bytes());
    }
    data.extend_from_slice(store);
    data.extend_from_slice(padding);
    uuid_box(&C2PA_UUID, 0, 0, purpose, &data)
}

/// A JUMBF-shaped manifest store: a 4-byte big-endian `LBox` covering the whole box, the `jumb`
/// `TBox`, then opaque contents. §8.4.2.3 gives the `LBox`/`TBox` width and endianness (while
/// defining the `c2sh` salt box, whose own `TBox` differs); §A.3.9 names the superbox `jumb`.
pub fn jumbf_store(contents: &[u8]) -> Vec<u8> {
    let mut out = ((8 + contents.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(b"jumb");
    out.extend_from_slice(contents);
    out
}
