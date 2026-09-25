//! Parses a single-still-image ISOBMFF file into an [`IsoBmffImage`].
//!
//! The structure is offset-driven (a parser-exploit surface), so every read is bounds-checked via
//! [`BoxReader`], counts are never trusted for allocation, the total resolved payload is capped at
//! the input size (a file cannot expand), and out-of-scope features (tracks, external data
//! references, `construction_method` 2, protected or `uri ` items) are rejected with a typed
//! error rather than mis-parsed. Unlike [`crate::write`] — which always normalises to the smallest
//! box versions — the reader also accepts what foreign encoders emit: `iloc` v0–v2 with
//! `mdat`/`idat` placement, multi-extent payloads (concatenated), `pitm` v0/v1, `infe` v2/v3,
//! `iref` v0/v1, and `ipma` v0/v1 with 8- or 16-bit indices. See `references/isobmff`.
//!
//! # Top-level boxes
//!
//! Every top-level box of the primary stream other than `ftyp`, `meta` and `mdat` is retained in
//! [`IsoBmffImage::top_level_boxes`] — a `uuid` box (C2PA's `ContentProvenanceBox` among them),
//! a `free`/`skip`, a vendor box — with its [`TopLevelPosition`] taken from where it was found:
//! [`Trailing`](TopLevelPosition::Trailing) once `mdat` has been seen, otherwise
//! [`AfterFtyp`](TopLevelPosition::AfterFtyp). A `uuid` box *inside* `meta` is not a top-level
//! box and is never promoted to one; [`crate::walk_meta_children`] surfaces it. `moov`/`trak`
//! remain [`Error::Unsupported`].
//!
//! # Motion-photo tolerance
//!
//! The semantic model intentionally covers only the *primary* still-image stream. Real-world
//! "motion photo" HEIC/AVIF files append a second, foreign stream after the still image — a Samsung
//! MP4 (a second top-level `ftyp` with its own `moov`), a Google `mpvd` box, or a trailing
//! non-ISOBMFF vendor blob (e.g. a Samsung SEF trailer). The top-level walk therefore stops cleanly
//! (a) at a *second* top-level `ftyp` — the first one wins, and everything from the second onward is
//! the appended stream — and (b) at any malformed/truncated box header or body encountered *after*
//! both `ftyp` and `meta` have already been parsed. Byte-level accounting of that remainder (mapping
//! it to boxes or explicit trailers) is a consumer's job — see the re-exported [`BoxReader`] /
//! [`crate::RawBox`] primitives and `gamut-heic`. Payload resolution keeps addressing the *full*
//! input buffer, so an `iloc` extent that points into the appended region still resolves.

use std::collections::{HashMap, HashSet};

use gamut_core::{Error, Result};

use crate::boxes::BoxReader;
use crate::model::{
    ColourInformation, EntityGroup, IsoBmffImage, Item, ItemReference, NclxColr, Property,
    PropertyKind, TopLevelBox, TopLevelPosition,
};

/// Per-item property associations parsed from `ipma`: each entry is `(item_id, associations)`
/// where an association is `(property_index, essential)`.
type ItemAssociations = Vec<(u32, Vec<(u16, bool)>)>;

/// Parses `data` into an [`IsoBmffImage`].
///
/// The result is normalised: each item's payload is resolved (multi-extent payloads concatenated,
/// `idat`-stored data inlined), so writing it back yields an equivalent — not byte-identical —
/// file. `read(&`[`write`](crate::write)`(&img)?) == img` holds for anything `write` accepts.
/// Top-level boxes the model does not otherwise own are retained in
/// [`IsoBmffImage::top_level_boxes`] in file order — every `AfterFtyp` box before every
/// `Trailing` one, the grouping `write` requires (see [`IsoBmffImage::top_level_boxes`]).
///
/// The top-level walk models only the primary still-image stream and tolerates an appended foreign
/// "motion photo" stream — see the [module docs](self#motion-photo-tolerance): it stops at a second
/// top-level `ftyp` (the first wins) and at a malformed trailing box once `ftyp`+`meta` are seen.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if a box is truncated or overruns, a required box (`ftyp`,
/// `meta`, `hdlr`, `pitm`, `iinf`) is missing, an `iloc` extent overflows or points outside its
/// source, the items' extents sum to more than the file size, an `ipma` property index is out of
/// range, an `iref` names an unknown `from_item`, or a string is not UTF-8. A malformed top-level
/// box is only an error *before* both `ftyp` and `meta` have been parsed; after that it is treated
/// as the start of an appended vendor trailer and stops the walk cleanly.
/// Returns [`Error::Unsupported`] for structurally valid but out-of-scope features: image
/// sequences (`moov`/`trak`) in the primary stream, a non-`pict` handler, protected items, `uri `
/// items, external data references, `iloc` `construction_method` 2, and box versions beyond those
/// listed above.
pub fn read(data: &[u8]) -> Result<IsoBmffImage> {
    let mut top = BoxReader::new(data);
    let mut ftyp = None;
    let mut meta_body = None;
    let mut seen_mdat = false;
    let mut top_level_boxes = Vec::new();
    loop {
        let b = match top.next_box() {
            Ok(Some(b)) => b,
            Ok(None) => break, // clean end of the (primary) stream
            // A malformed top-level box after both required boxes are seen is the start of an
            // appended non-ISOBMFF vendor trailer (motion photo) — stop cleanly; the consumer's
            // accounting layer surfaces it. Before that, the file itself is malformed: propagate.
            Err(e) => {
                if ftyp.is_some() && meta_body.is_some() {
                    break;
                }
                return Err(e);
            }
        };
        match &b.ty {
            // First ftyp wins. A second top-level ftyp begins an appended foreign stream (e.g. a
            // Samsung motion-photo MP4, itself carrying a `moov`); stop the primary-stream walk so
            // that foreign stream — and any `moov` within it — is never mis-parsed as an image.
            b"ftyp" => {
                if ftyp.is_some() {
                    break;
                }
                ftyp = Some(parse_ftyp(b.body)?);
            }
            b"meta" => meta_body = Some(b.body),
            b"moov" | b"trak" => {
                return Err(Error::unsupported(
                    env!("CARGO_PKG_NAME"),
                    "ISOBMFF: image sequences (tracks) not supported",
                ));
            }
            // mdat is offset-addressed through iloc; its bytes are reached from the items.
            b"mdat" => seen_mdat = true,
            // Every other top-level box is retained verbatim, positioned by where it was found.
            _ => top_level_boxes.push(TopLevelBox {
                ty: b.ty,
                user_type: b.user_type,
                payload: b.payload().to_vec(),
                position: if seen_mdat {
                    TopLevelPosition::Trailing
                } else {
                    TopLevelPosition::AfterFtyp
                },
            }),
        }
    }

    let (major_brand, minor_version, compatible_brands) =
        ftyp.ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: missing ftyp"))?;
    let meta_body = meta_body
        .ok_or_else(|| Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: missing meta"))?;
    let meta = parse_meta(meta_body)?;

    // Index iloc/ipma/iref by item id up front (first iloc/ipma entry wins; references keep file
    // order) — id joins must not be quadratic on a hostile file with tens of thousands of entries.
    let mut iloc_by_id = HashMap::with_capacity(meta.iloc.len());
    for entry in &meta.iloc {
        iloc_by_id.entry(entry.id).or_insert(entry);
    }
    let mut ipma_by_id = HashMap::with_capacity(meta.ipma.len());
    for (id, row) in &meta.ipma {
        ipma_by_id.entry(*id).or_insert(row);
    }
    let mut iref_by_from: HashMap<u32, Vec<ItemReference>> = HashMap::new();
    for (from, reference) in meta.iref {
        iref_by_from.entry(from).or_default().push(reference);
    }
    let item_ids: HashSet<u32> = meta.infe.iter().map(|i| i.id).collect();
    if iref_by_from.keys().any(|from| !item_ids.contains(from)) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: iref from unknown item",
        ));
    }

    // Assemble items. `budget` caps the total resolved payload at the input size so overlapping
    // extents in a hostile file cannot amplify a small input into a huge allocation.
    let mut budget = data.len();
    let mut items = Vec::with_capacity(meta.infe.len());
    for infe in &meta.infe {
        let payload = match iloc_by_id.get(&infe.id) {
            // No iloc entry: an item with no data (e.g. a derived `iden` item).
            None => Vec::new(),
            Some(entry) => resolve_payload(entry, data, meta.idat, &mut budget)?,
        };

        // No ipma entry: an item with no properties (e.g. an Exif/XMP metadata item).
        let assoc = ipma_by_id.get(&infe.id).map_or(&[][..], |row| &row[..]);
        let mut properties = Vec::with_capacity(assoc.len());
        for &(index, essential) in assoc {
            let i = usize::from(index);
            if index == 0 || i > meta.ipco.len() {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "ISOBMFF: ipma property index out of range",
                ));
            }
            properties.push(Property {
                essential,
                kind: meta.ipco[i - 1].clone(),
            });
        }

        let references = iref_by_from.remove(&infe.id).unwrap_or_default();

        items.push(Item {
            id: infe.id,
            item_type: infe.item_type,
            name: infe.name.clone(),
            content_type: infe.content_type.clone(),
            content_encoding: infe.content_encoding.clone(),
            hidden: infe.hidden,
            references,
            properties,
            payload,
        });
    }
    Ok(IsoBmffImage {
        major_brand,
        minor_version,
        compatible_brands,
        primary_item_id: meta.primary_item_id,
        items,
        groups: meta.groups,
        top_level_boxes,
    })
}

/// `ftyp`: major brand, minor version, and 4-byte compatible brands.
fn parse_ftyp(body: &[u8]) -> Result<([u8; 4], u32, Vec<[u8; 4]>)> {
    let mut r = BoxReader::new(body);
    let major = r.fourcc()?;
    let minor = r.u32()?;
    let mut compatible = Vec::with_capacity(r.remaining() / 4);
    while r.remaining() >= 4 {
        compatible.push(r.fourcc()?);
    }
    if r.remaining() != 0 {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: ftyp has trailing bytes",
        ));
    }
    Ok((major, minor, compatible))
}

/// The structural pieces parsed out of `meta`, before payloads are resolved.
struct Meta<'a> {
    primary_item_id: u32,
    iloc: Vec<IlocEntry>,
    infe: Vec<InfeEntry>,
    ipco: Vec<PropertyKind>,
    ipma: ItemAssociations,
    iref: Vec<(u32, ItemReference)>,
    idat: Option<&'a [u8]>,
    groups: Vec<EntityGroup>,
}

/// One `iloc` item: its id, payload source (`construction_method` 0 = file, 1 = `idat`), and
/// extents as `(offset, length)` relative to `base_offset`.
struct IlocEntry {
    id: u32,
    construction_method: u8,
    base_offset: u64,
    extents: Vec<(u64, u64)>,
}

/// One `infe` entry: the item identity fields (see [`Item`]).
struct InfeEntry {
    id: u32,
    item_type: [u8; 4],
    name: String,
    content_type: Option<String>,
    content_encoding: Option<String>,
    hidden: bool,
}

/// Parses the `meta` (FullBox) body and its child boxes.
fn parse_meta(body: &[u8]) -> Result<Meta<'_>> {
    let mut r = BoxReader::new(body);
    full_box_header(&mut r)?; // meta is a FullBox

    let mut hdlr_seen = false;
    let mut primary_item_id = None;
    let mut iloc = None;
    let mut infe = None;
    let mut iprp = None;
    let mut iref = None;
    let mut idat = None;
    let mut groups = None;
    while let Some(b) = r.next_box()? {
        match &b.ty {
            b"hdlr" => {
                parse_hdlr(b.body)?;
                hdlr_seen = true;
            }
            b"pitm" => primary_item_id = Some(parse_pitm(b.body)?),
            b"iloc" => iloc = Some(parse_iloc(b.body)?),
            b"iinf" => infe = Some(parse_iinf(b.body)?),
            b"iprp" => iprp = Some(parse_iprp(b.body)?),
            b"iref" => iref = Some(parse_iref(b.body)?),
            b"idat" => idat = Some(b.body),
            b"grpl" => groups = Some(parse_grpl(b.body)?),
            _ => {} // tolerate unknown meta children (dinf, uuid, …)
        }
    }

    if !hdlr_seen {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: meta missing hdlr",
        ));
    }
    let (ipco, ipma) = iprp.unwrap_or_default();
    Ok(Meta {
        primary_item_id: primary_item_id.ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: meta missing pitm")
        })?,
        iloc: iloc.unwrap_or_default(),
        infe: infe.ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: meta missing iinf")
        })?,
        ipco,
        ipma,
        iref: iref.unwrap_or_default(),
        idat,
        groups: groups.unwrap_or_default(),
    })
}

/// Resolves one item's payload: each extent addressed against the file (`construction_method` 0)
/// or the `idat` body (1), concatenated in extent order. `budget` is the remaining total payload
/// allowance (see [`read`]).
fn resolve_payload(
    entry: &IlocEntry,
    data: &[u8],
    idat: Option<&[u8]>,
    budget: &mut usize,
) -> Result<Vec<u8>> {
    let source = match entry.construction_method {
        0 => data,
        _ => idat.ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: iloc references idat but meta has none",
            )
        })?,
    };
    let mut payload = Vec::new();
    for &(offset, length) in &entry.extents {
        let start = entry.base_offset.checked_add(offset).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: iloc extent overflow")
        })?;
        let end = start.checked_add(length).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: iloc extent overflow")
        })?;
        let range = usize::try_from(start).ok().zip(usize::try_from(end).ok());
        let slice = range
            .and_then(|(start, end)| source.get(start..end))
            .ok_or_else(|| {
                Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: iloc extent out of bounds")
            })?;
        *budget = budget.checked_sub(slice.len()).ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: extents exceed the file size",
            )
        })?;
        payload.extend_from_slice(slice);
    }
    Ok(payload)
}

/// Reads a `FullBox` header, returning `(version, flags)`.
fn full_box_header(r: &mut BoxReader) -> Result<(u8, u32)> {
    let version = r.u8()?;
    let f = r.take(3)?;
    Ok((version, u32::from_be_bytes([0, f[0], f[1], f[2]])))
}

/// `hdlr`: require `handler_type == "pict"` (HEIF image).
fn parse_hdlr(body: &[u8]) -> Result<()> {
    let mut r = BoxReader::new(body);
    full_box_header(&mut r)?;
    let _pre_defined = r.u32()?;
    let handler = r.fourcc()?;
    if &handler != b"pict" {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: non-picture handler",
        ));
    }
    Ok(())
}

/// `pitm` v0 (16-bit id) / v1 (32-bit id): the primary item id.
fn parse_pitm(body: &[u8]) -> Result<u32> {
    let mut r = BoxReader::new(body);
    match full_box_header(&mut r)?.0 {
        0 => Ok(u32::from(r.u16()?)),
        1 => r.u32(),
        _ => Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: pitm version above 1",
        )),
    }
}

/// Reads an `iloc` offset/length/base/index field of `size` ∈ {0, 4, 8} bytes.
fn read_sized(r: &mut BoxReader, size: u8) -> Result<u64> {
    match size {
        0 => Ok(0),
        4 => Ok(u64::from(r.u32()?)),
        _ => r.u64(),
    }
}

/// `iloc` v0/v1/v2: per-item payload location. `construction_method` 0 (file) and 1 (`idat`) are
/// accepted; 2 (item offsets) and external data references are not.
fn parse_iloc(body: &[u8]) -> Result<Vec<IlocEntry>> {
    let mut r = BoxReader::new(body);
    let version = full_box_header(&mut r)?.0;
    if version > 2 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: iloc version above 2",
        ));
    }
    let b0 = r.u8()?;
    let (offset_size, length_size) = (b0 >> 4, b0 & 0xf);
    let b1 = r.u8()?;
    let base_offset_size = b1 >> 4;
    let index_size = if version >= 1 { b1 & 0xf } else { 0 };
    for size in [offset_size, length_size, base_offset_size, index_size] {
        if !matches!(size, 0 | 4 | 8) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: iloc field size not 0/4/8",
            ));
        }
    }
    let item_count = if version == 2 {
        r.u32()?
    } else {
        u32::from(r.u16()?)
    };
    // Counts are untrusted; do not pre-allocate from them — the bounded reads below fail on
    // truncation, so a malformed count errors after a bounded number of iterations.
    let mut entries = Vec::new();
    for _ in 0..item_count {
        let id = if version == 2 {
            r.u32()?
        } else {
            u32::from(r.u16()?)
        };
        let construction_method = if version >= 1 {
            (r.u16()? & 0xf) as u8 // reserved(12) | construction_method(4)
        } else {
            0
        };
        if construction_method > 1 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: iloc construction_method 2 (item offsets)",
            ));
        }
        if r.u16()? != 0 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: external data reference",
            ));
        }
        let base_offset = read_sized(&mut r, base_offset_size)?;
        let extent_count = r.u16()?;
        let mut extents = Vec::new();
        for _ in 0..extent_count {
            let _extent_index = read_sized(&mut r, index_size)?; // no-op when index_size == 0
            let offset = read_sized(&mut r, offset_size)?;
            let length = read_sized(&mut r, length_size)?;
            extents.push((offset, length));
        }
        entries.push(IlocEntry {
            id,
            construction_method,
            base_offset,
            extents,
        });
    }
    Ok(entries)
}

/// `iinf` v0 (16-bit count) / v1 (32-bit count) + `infe` children.
fn parse_iinf(body: &[u8]) -> Result<Vec<InfeEntry>> {
    let mut r = BoxReader::new(body);
    let entry_count = match full_box_header(&mut r)?.0 {
        0 => u32::from(r.u16()?),
        1 => r.u32()?,
        _ => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: iinf version above 1",
            ));
        }
    };
    let mut entries = Vec::new();
    for _ in 0..entry_count {
        let b = r.next_box()?.ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: iinf truncated")
        })?;
        if &b.ty != b"infe" {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: iinf child is not infe",
            ));
        }
        entries.push(parse_infe(b.body)?);
    }
    Ok(entries)
}

/// `infe` v2 (16-bit id) / v3 (32-bit id): item identity. Protected and `uri ` items are
/// out of scope.
fn parse_infe(body: &[u8]) -> Result<InfeEntry> {
    let mut r = BoxReader::new(body);
    let (version, flags) = full_box_header(&mut r)?;
    let id = match version {
        2 => u32::from(r.u16()?),
        3 => r.u32()?,
        _ => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: infe version (only v2/v3)",
            ));
        }
    };
    if r.u16()? != 0 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: protected item",
        ));
    }
    let item_type = r.fourcc()?;
    if &item_type == b"uri " {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: uri items not modelled",
        ));
    }
    let name = read_c_string(&mut r)?;
    let mut content_type = None;
    let mut content_encoding = None;
    if &item_type == b"mime" {
        if r.remaining() == 0 {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: mime infe missing content_type",
            ));
        }
        content_type = Some(read_c_string(&mut r)?);
        if r.remaining() != 0 {
            // Optional; an explicit empty string is normalised to absent.
            content_encoding = Some(read_c_string(&mut r)?).filter(|s| !s.is_empty());
        }
    }
    Ok(InfeEntry {
        id,
        item_type,
        name,
        content_type,
        content_encoding,
        hidden: flags & 1 == 1,
    })
}

/// Reads a NUL-terminated UTF-8 string (without the terminator), tolerating a missing terminator
/// at end of box.
fn read_c_string(r: &mut BoxReader) -> Result<String> {
    let mut bytes = Vec::new();
    while r.remaining() != 0 {
        let b = r.u8()?;
        if b == 0 {
            break;
        }
        bytes.push(b);
    }
    String::from_utf8(bytes)
        .map_err(|_| Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: string not UTF-8"))
}

/// `iref` v0 (16-bit ids) / v1 (32-bit ids): each child box is one typed reference set,
/// returned as `(from_item_id, reference)` in file order.
fn parse_iref(body: &[u8]) -> Result<Vec<(u32, ItemReference)>> {
    let mut r = BoxReader::new(body);
    let version = full_box_header(&mut r)?.0;
    if version > 1 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: iref version above 1",
        ));
    }
    let mut references = Vec::new();
    while let Some(b) = r.next_box()? {
        let mut r = BoxReader::new(b.body);
        let from = if version == 0 {
            u32::from(r.u16()?)
        } else {
            r.u32()?
        };
        let reference_count = r.u16()?;
        let mut to_item_ids = Vec::new();
        for _ in 0..reference_count {
            to_item_ids.push(if version == 0 {
                u32::from(r.u16()?)
            } else {
                r.u32()?
            });
        }
        references.push((
            from,
            ItemReference {
                reference_type: b.ty,
                to_item_ids,
            },
        ));
    }
    Ok(references)
}

/// `grpl`: each child is one `EntityToGroupBox` (FullBox v0, box type = grouping type).
fn parse_grpl(body: &[u8]) -> Result<Vec<EntityGroup>> {
    let mut r = BoxReader::new(body);
    let mut groups = Vec::new();
    while let Some(b) = r.next_box()? {
        let mut r = BoxReader::new(b.body);
        if full_box_header(&mut r)?.0 != 0 {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: EntityToGroupBox version above 0",
            ));
        }
        let group_id = r.u32()?;
        let num_entities = r.u32()?;
        let mut entity_ids = Vec::new();
        for _ in 0..num_entities {
            entity_ids.push(r.u32()?);
        }
        groups.push(EntityGroup {
            group_type: b.ty,
            group_id,
            entity_ids,
        });
    }
    Ok(groups)
}

/// `iprp`: the `ipco` property list (1-based) and the `ipma` per-item associations.
fn parse_iprp(body: &[u8]) -> Result<(Vec<PropertyKind>, ItemAssociations)> {
    let mut r = BoxReader::new(body);
    let mut ipco = None;
    let mut ipma = None;
    while let Some(b) = r.next_box()? {
        match &b.ty {
            b"ipco" => ipco = Some(parse_ipco(b.body)?),
            b"ipma" => ipma = Some(parse_ipma(b.body)?),
            _ => {}
        }
    }
    let ipco = ipco.ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: iprp missing ipco")
    })?;
    let ipma = ipma.ok_or_else(|| {
        Error::invalid_input(env!("CARGO_PKG_NAME"), "ISOBMFF: iprp missing ipma")
    })?;
    Ok((ipco, ipma))
}

/// `ipco`: the ordered property container; each child box becomes one [`PropertyKind`].
fn parse_ipco(body: &[u8]) -> Result<Vec<PropertyKind>> {
    let mut r = BoxReader::new(body);
    let mut props = Vec::new();
    while let Some(b) = r.next_box()? {
        props.push(parse_property(b.ty, b.body)?);
    }
    Ok(props)
}

/// Maps one `ipco` child box to a [`PropertyKind`]. Unrecognised boxes are preserved verbatim.
fn parse_property(ty: [u8; 4], body: &[u8]) -> Result<PropertyKind> {
    match &ty {
        b"ispe" => {
            let mut r = BoxReader::new(body);
            full_box_header(&mut r)?;
            let width = r.u32()?;
            let height = r.u32()?;
            Ok(PropertyKind::ImageSpatialExtents { width, height })
        }
        b"pixi" => {
            let mut r = BoxReader::new(body);
            full_box_header(&mut r)?;
            let count = r.u8()?;
            let mut bits_per_channel = Vec::new();
            for _ in 0..count {
                bits_per_channel.push(r.u8()?);
            }
            Ok(PropertyKind::PixelInformation { bits_per_channel })
        }
        b"colr" => {
            let mut r = BoxReader::new(body);
            let colour_type = r.fourcc()?;
            match &colour_type {
                b"nclx" => {
                    let colour_primaries = r.u16()?;
                    let transfer_characteristics = r.u16()?;
                    let matrix_coefficients = r.u16()?;
                    let full_range = (r.u8()? >> 7) & 1 == 1;
                    Ok(PropertyKind::Colour(ColourInformation::Nclx(NclxColr {
                        colour_primaries,
                        transfer_characteristics,
                        matrix_coefficients,
                        full_range,
                    })))
                }
                b"rICC" => Ok(PropertyKind::Colour(ColourInformation::RestrictedIcc(
                    r.take(r.remaining())?.to_vec(),
                ))),
                b"prof" => Ok(PropertyKind::Colour(ColourInformation::UnrestrictedIcc(
                    r.take(r.remaining())?.to_vec(),
                ))),
                // Unknown colour types: preserve verbatim.
                _ => Ok(PropertyKind::Other {
                    kind: ty,
                    data: body.to_vec(),
                }),
            }
        }
        b"irot" => {
            let mut r = BoxReader::new(body);
            Ok(PropertyKind::Rotation(r.u8()? & 0x03))
        }
        b"imir" => {
            let mut r = BoxReader::new(body);
            Ok(PropertyKind::Mirror(r.u8()? & 0x01))
        }
        b"clap" => {
            let mut r = BoxReader::new(body);
            Ok(PropertyKind::CleanAperture {
                width_n: r.u32()?,
                width_d: r.u32()?,
                height_n: r.u32()?,
                height_d: r.u32()?,
                horiz_off_n: r.u32()?,
                horiz_off_d: r.u32()?,
                vert_off_n: r.u32()?,
                vert_off_d: r.u32()?,
            })
        }
        b"pasp" => {
            let mut r = BoxReader::new(body);
            Ok(PropertyKind::PixelAspectRatio {
                h_spacing: r.u32()?,
                v_spacing: r.u32()?,
            })
        }
        b"auxC" => {
            let mut r = BoxReader::new(body);
            full_box_header(&mut r)?;
            let aux_type = read_c_string(&mut r)?;
            let aux_subtype = r.take(r.remaining())?.to_vec();
            Ok(PropertyKind::AuxiliaryType {
                aux_type,
                aux_subtype,
            })
        }
        b"clli" => {
            let mut r = BoxReader::new(body);
            Ok(PropertyKind::ContentLightLevel {
                max_content_light_level: r.u16()?,
                max_pic_average_light_level: r.u16()?,
            })
        }
        // The codec-configuration records this crate's consumers stamp (AV1, HEVC, VVC).
        b"av1C" | b"hvcC" | b"vvcC" => Ok(PropertyKind::CodecConfiguration {
            kind: ty,
            data: body.to_vec(),
        }),
        _ => Ok(PropertyKind::Other {
            kind: ty,
            data: body.to_vec(),
        }),
    }
}

/// `ipma` v0 (16-bit item ids) / v1 (32-bit item ids), with 8-bit (`flags & 1 == 0`) or 16-bit
/// (`flags & 1 == 1`) `essential | property_index` associations.
fn parse_ipma(body: &[u8]) -> Result<ItemAssociations> {
    let mut r = BoxReader::new(body);
    let (version, flags) = full_box_header(&mut r)?;
    if version > 1 {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: ipma version above 1",
        ));
    }
    let wide = flags & 1 == 1;
    // `entry_count`/`association_count` are untrusted; do not pre-allocate from them — the bounded
    // reads below fail on truncation after a bounded number of iterations.
    let entry_count = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..entry_count {
        let item_id = if version == 0 {
            u32::from(r.u16()?)
        } else {
            r.u32()?
        };
        let assoc_count = r.u8()?;
        let mut row = Vec::new();
        for _ in 0..assoc_count {
            row.push(if wide {
                let word = r.u16()?;
                (word & 0x7fff, word & 0x8000 != 0)
            } else {
                let byte = r.u8()?;
                (u16::from(byte & 0x7f), byte & 0x80 != 0)
            });
        }
        out.push((item_id, row));
    }
    Ok(out)
}
