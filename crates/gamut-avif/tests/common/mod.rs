//! Shared AVIF test fixtures: model builders that drive `gamut_isobmff::write`, plus hand-authored
//! box byte-builders (mirroring `gamut-isobmff/tests/common`) for the accounting/meta cases that a
//! normalising writer cannot express.
#![allow(dead_code)] // each integration-test binary uses a different subset

use gamut_isobmff::{IsoBmffImage, Item, ItemReference, Property, PropertyKind, write};

/// A minimal valid `av1C` record: High profile (4:4:4), level 0, 8-bit, empty `configOBUs`.
pub const AV1C_444_8BIT: [u8; 4] = [0x81, 0x20, 0x00, 0x00];

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

/// An `av1C` codec-configuration property (essential), with the given opaque record bytes.
pub fn av1c(data: Vec<u8>) -> Property {
    Property {
        essential: true,
        kind: PropertyKind::CodecConfiguration {
            kind: *b"av1C",
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

/// A canonical AV1 coded-image item: `av01`, one `av1C` + one `ispe`, with the given payload.
pub fn av01_item(id: u32, payload: Vec<u8>) -> Item {
    Item {
        properties: vec![av1c(AV1C_444_8BIT.to_vec()), ispe(64, 48)],
        ..item(id, *b"av01", payload)
    }
}

/// Wraps items into a complete `avif` file whose primary is the item with `primary_id`.
pub fn avif_image(primary_id: u32, items: Vec<Item>) -> IsoBmffImage {
    IsoBmffImage::new(
        *b"avif",
        vec![*b"avif", *b"mif1", *b"miaf"],
        primary_id,
        items,
    )
}

/// Serialises a model to a clean AVIF file (`ftyp` + `meta` + `mdat`).
pub fn clean_file(primary_id: u32, items: Vec<Item>) -> Vec<u8> {
    write(&avif_image(primary_id, items)).expect("valid model")
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
