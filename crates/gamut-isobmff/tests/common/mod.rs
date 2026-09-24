//! Shared test fixtures: canonical model values, plus hand-authored box builders so reader tests
//! can assert against spec-conformant bytes built *independently* of this crate's writer.
#![allow(dead_code)] // each integration-test binary uses a different subset

use gamut_isobmff::{ColourInformation, IsoBmffImage, Item, NclxColr, Property, PropertyKind};

// ---- model fixtures ------------------------------------------------------------------------

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

/// A colour image item carrying the canonical AVIF property set (essential `av1C`, then
/// `ispe`/`pixi`/`colr`).
pub fn av01_item(id: u32, payload: Vec<u8>) -> Item {
    Item {
        properties: vec![
            Property {
                essential: true,
                kind: PropertyKind::CodecConfiguration {
                    kind: *b"av1C",
                    data: vec![0x81, 0x20, 0x0c, 0x00],
                },
            },
            Property {
                essential: false,
                kind: PropertyKind::ImageSpatialExtents {
                    width: 48,
                    height: 32,
                },
            },
            Property {
                essential: false,
                kind: PropertyKind::PixelInformation {
                    bits_per_channel: vec![8, 8, 8],
                },
            },
            Property {
                essential: false,
                kind: PropertyKind::Colour(ColourInformation::Nclx(NclxColr {
                    colour_primaries: 1,
                    transfer_characteristics: 13,
                    matrix_coefficients: 0,
                    full_range: true,
                })),
            },
        ],
        ..item(id, *b"av01", payload)
    }
}

/// Wraps items in a complete AVIF-style file whose primary item is the first one.
pub fn image(items: Vec<Item>) -> IsoBmffImage {
    let primary_item_id = items[0].id;
    IsoBmffImage::new(
        *b"avif",
        vec![*b"avif", *b"mif1", *b"miaf", *b"MA1A"],
        primary_item_id,
        items,
    )
}

// ---- spec-byte builders --------------------------------------------------------------------

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

/// Concatenates byte chunks (box bodies are assembled from typed pieces).
pub fn cat<T: AsRef<[u8]>>(parts: &[T]) -> Vec<u8> {
    parts.iter().flat_map(|p| p.as_ref().to_vec()).collect()
}

/// A minimal `ftyp` (major `avif`, minor 0, no compatible brands).
pub fn ftyp() -> Vec<u8> {
    bx(b"ftyp", b"avif\x00\x00\x00\x00")
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

/// An `infe` v2 for an unprotected item with an empty name.
pub fn infe_v2(id: u16, item_type: &[u8; 4]) -> Vec<u8> {
    full(
        b"infe",
        2,
        0,
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

/// An empty `iprp` (no properties, no associations).
pub fn empty_iprp() -> Vec<u8> {
    bx(
        b"iprp",
        &cat(&[bx(b"ipco", &[]), full(b"ipma", 0, 0, &0u32.to_be_bytes())]),
    )
}

/// A `meta` FullBox with the given children.
pub fn meta(children: &[Vec<u8>]) -> Vec<u8> {
    full(b"meta", 0, 0, &cat(children))
}
