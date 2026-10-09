//! Serialises an [`IsoBmffImage`] into a single-still-image ISOBMFF file.
//!
//! The layout is `ftyp` + `meta` + `mdat`, with the model's [`TopLevelBox`]es interleaved at their
//! [`TopLevelPosition`]: [`AfterFtyp`](TopLevelPosition::AfterFtyp) boxes between `ftyp` and `meta`
//! (so before the first `mdat`, where C2PA 2.4 §A.5.3 wants a `ContentProvenanceBox`), and
//! [`Trailing`](TopLevelPosition::Trailing) boxes after `mdat`. The one keystone is the back-patch: each item's `iloc`
//! `extent_offset` can only be filled once the `mdat` payload positions are known, so the writer
//! reserves those 4-byte slots while emitting `meta` and patches them after `mdat` is placed (the
//! analogue of `gamut-ifd`'s two-pass offset layout). Box byte layouts follow ISO/IEC 14496-12
//! (ISOBMFF) and ISO/IEC 23008-12 (HEIF); see `references/isobmff`.
//!
//! The writer *normalises*: it always emits the smallest still-image box versions (`pitm` v0,
//! `iloc` v0 single-extent into `mdat`, `infe` v2, `iref` v0), which is why [`write`] validates up
//! front that the model fits them — see [`validate`].

use gamut_core::{Error, Result};

use crate::boxes::BoxBuilder;
use crate::model::{
    ColourInformation, EntityGroup, IsoBmffImage, Item, PropertyKind, TopLevelBox, TopLevelPosition,
};

/// Serialises `image` into a complete ISOBMFF file (`ftyp` + `meta` + `mdat`, with the model's
/// [`TopLevelBox`]es placed after `ftyp` or after `mdat` per their [`TopLevelPosition`]).
///
/// [`read`](crate::read)`(&write(&image)?)` reproduces `image` for any value this function
/// accepts.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] for a model that cannot round-trip: a `primary_item_id` naming
/// no item, duplicate item ids, an interior NUL in a name/content-type/encoding/aux-type string,
/// an empty `content_encoding`, a `content_type` on a non-`mime` item (or a `mime` item without
/// one), an out-of-range `Rotation`/`Mirror` value, a top-level box whose type the writer
/// emits from the model itself (`ftyp`/`meta`/`mdat`) or whose `user_type` does not pair with a
/// `uuid` type, or a `top_level_boxes` list that interleaves positions (an `AfterFtyp` box after
/// a `Trailing` one — the file cannot record that order, so `read` could not reproduce it; use
/// [`IsoBmffImage::push_top_level_box`] to append). Returns [`Error::Unsupported`] for a model
/// that does not fit the still-image box
/// versions this crate writes: item ids above `u16::MAX`, `uri ` items, more than 255 properties or
/// 65 535 reference targets per item, more than 32 767 distinct properties, more than 65 535
/// items, a `moov`/`trak` top-level box (image sequences), or a payload/box/file at 4 GiB or
/// beyond.
#[must_use = "the serialised file is returned, not written anywhere"]
pub fn write(image: &IsoBmffImage) -> Result<Vec<u8>> {
    validate(image)?;
    let mut bb = BoxBuilder::new();
    write_ftyp(&mut bb, image);
    write_top_level(&mut bb, &image.top_level_boxes, TopLevelPosition::AfterFtyp);
    let extent_slots = write_meta(&mut bb, image)?;

    let mdat_start = bb.begin_box(b"mdat");
    let mut payload_positions = Vec::with_capacity(image.items.len());
    for item in &image.items {
        payload_positions.push(bb.len());
        bb.bytes(&item.payload);
    }
    bb.end_box(mdat_start);
    write_top_level(&mut bb, &image.top_level_boxes, TopLevelPosition::Trailing);

    for (slot, pos) in extent_slots.into_iter().zip(payload_positions) {
        let pos = u32::try_from(pos).map_err(|_| {
            Error::unsupported(env!("CARGO_PKG_NAME"), "ISOBMFF: file at or beyond 4 GiB")
        })?;
        bb.patch_u32(slot, pos);
    }
    Ok(bb.into_vec())
}

/// Rejects models that cannot round-trip or that do not fit the normalised still-image box
/// versions [`write`] emits (see the [`write`] error docs for the full list).
fn validate(image: &IsoBmffImage) -> Result<()> {
    if !image.items.iter().any(|i| i.id == image.primary_item_id) {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: primary_item_id names no item",
        ));
    }
    if u16::try_from(image.items.len()).is_err() {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: more than 65535 items",
        ));
    }
    for (n, item) in image.items.iter().enumerate() {
        if image.items[..n].iter().any(|prev| prev.id == item.id) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: duplicate item id",
            ));
        }
        validate_item(item)?;
    }
    // The file holds every AfterFtyp box before every Trailing one, so a model whose list
    // interleaves the two would come back re-grouped from `read`: refuse it rather than silently
    // reorder.
    let mut seen_trailing = false;
    for top in &image.top_level_boxes {
        validate_top_level(top)?;
        match top.position {
            TopLevelPosition::Trailing => seen_trailing = true,
            TopLevelPosition::AfterFtyp if seen_trailing => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "ISOBMFF: top_level_boxes interleave positions (an AfterFtyp box follows a \
                     Trailing one); order them AfterFtyp then Trailing, or append with \
                     IsoBmffImage::push_top_level_box",
                ));
            }
            TopLevelPosition::AfterFtyp => {}
        }
    }
    Ok(())
}

/// The largest header a top-level box carries: the 8-byte size/type plus a `uuid` box's 16-byte
/// user type. The 4 GiB bound below applies it to every box type, so a non-`uuid` box is refused
/// 16 bytes early — a deliberate simplification at an edge no still image approaches.
const TOP_LEVEL_HEADER_MAX: usize = 24;

/// A top-level box must be one the model does not already emit, must pair its `user_type` with the
/// `uuid` type exactly as [`crate::RawBox`] does, and its complete box (header included) must fit
/// the 32-bit size field.
fn validate_top_level(top: &TopLevelBox) -> Result<()> {
    match &top.ty {
        b"ftyp" | b"meta" | b"mdat" => {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: top-level box type is owned by the model (ftyp/meta/mdat)",
            ));
        }
        b"moov" | b"trak" => {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: image sequences (tracks) not supported",
            ));
        }
        _ => {}
    }
    if top.user_type.is_some() != (top.ty == *b"uuid") {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: user_type is required for uuid boxes and forbidden otherwise",
        ));
    }
    if u32::try_from(top.payload.len().saturating_add(TOP_LEVEL_HEADER_MAX)).is_err() {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: top-level box at or beyond 4 GiB",
        ));
    }
    Ok(())
}

fn validate_item(item: &Item) -> Result<()> {
    if u16::try_from(item.id).is_err() {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: item id above u16::MAX (still-image writer emits 16-bit boxes)",
        ));
    }
    if item.item_type == *b"uri " {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: uri items not modelled",
        ));
    }
    if (item.item_type == *b"mime") != item.content_type.is_some() {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: content_type is required for mime items and forbidden otherwise",
        ));
    }
    if item.content_encoding.is_some() && item.content_type.is_none() {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: content_encoding requires a content_type",
        ));
    }
    if item.content_encoding.as_deref() == Some("") {
        return Err(Error::invalid_input(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: empty content_encoding does not round-trip (use None)",
        ));
    }
    for s in [
        Some(item.name.as_str()),
        item.content_type.as_deref(),
        item.content_encoding.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if s.as_bytes().contains(&0) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: interior NUL in item string",
            ));
        }
    }
    if u32::try_from(item.payload.len()).is_err() {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: payload at or beyond 4 GiB",
        ));
    }
    if u8::try_from(item.properties.len()).is_err() {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: more than 255 properties on one item",
        ));
    }
    for property in &item.properties {
        match &property.kind {
            PropertyKind::Rotation(angle) if *angle > 3 => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "ISOBMFF: irot angle above 3",
                ));
            }
            PropertyKind::Mirror(axis) if *axis > 1 => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "ISOBMFF: imir axis above 1",
                ));
            }
            PropertyKind::AuxiliaryType { aux_type, .. } if aux_type.as_bytes().contains(&0) => {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "ISOBMFF: interior NUL in auxC type",
                ));
            }
            _ => {}
        }
    }
    for reference in &item.references {
        if u16::try_from(reference.to_item_ids.len()).is_err() {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: more than 65535 targets in one reference",
            ));
        }
        if reference
            .to_item_ids
            .iter()
            .any(|&id| u16::try_from(id).is_err())
        {
            return Err(Error::unsupported(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: referenced item id above u16::MAX",
            ));
        }
    }
    Ok(())
}

/// `ftyp`: major brand, minor version, and the compatible-brand list.
fn write_ftyp(bb: &mut BoxBuilder, image: &IsoBmffImage) {
    let start = bb.begin_box(b"ftyp");
    bb.bytes(&image.major_brand);
    bb.u32(image.minor_version);
    for brand in &image.compatible_brands {
        bb.bytes(brand);
    }
    bb.end_box(start);
}

/// The model's top-level boxes at `position`, in model order: size + type header, the 16-byte user
/// type for a `uuid` box, then the payload verbatim.
fn write_top_level(bb: &mut BoxBuilder, boxes: &[TopLevelBox], position: TopLevelPosition) {
    for top in boxes.iter().filter(|b| b.position == position) {
        let start = bb.begin_box(&top.ty);
        if let Some(user_type) = &top.user_type {
            bb.bytes(user_type);
        }
        bb.bytes(&top.payload);
        bb.end_box(start);
    }
}

/// `meta` (FullBox v0) and its children; returns each item's reserved `iloc` `extent_offset` slot
/// in item order.
fn write_meta(bb: &mut BoxBuilder, image: &IsoBmffImage) -> Result<Vec<usize>> {
    let start = bb.begin_box(b"meta");
    bb.full_box(0, 0);
    write_hdlr(bb);
    write_pitm(bb, image.primary_item_id as u16);
    let extent_slots = write_iloc(bb, &image.items);
    write_iinf(bb, &image.items);
    write_iref(bb, &image.items);
    write_iprp(bb, &image.items)?;
    write_grpl(bb, &image.groups);
    bb.end_box(start);
    Ok(extent_slots)
}

/// `hdlr`: handler_type `pict` (HEIF image-item handler).
fn write_hdlr(bb: &mut BoxBuilder) {
    let start = bb.begin_box(b"hdlr");
    bb.full_box(0, 0);
    bb.u32(0); // pre_defined
    bb.bytes(b"pict"); // handler_type
    bb.u32(0); // reserved[0]
    bb.u32(0); // reserved[1]
    bb.u32(0); // reserved[2]
    bb.u8(0); // name: empty, null-terminated
    bb.end_box(start);
}

/// `pitm` v0: the primary item id.
fn write_pitm(bb: &mut BoxBuilder, primary_item_id: u16) {
    let start = bb.begin_box(b"pitm");
    bb.full_box(0, 0);
    bb.u16(primary_item_id);
    bb.end_box(start);
}

/// `iloc` v0: one extent per item, `construction_method` 0 (file offset). Reserves and returns the
/// per-item 4-byte `extent_offset` slots (patched once `mdat` is placed).
fn write_iloc(bb: &mut BoxBuilder, items: &[Item]) -> Vec<usize> {
    let start = bb.begin_box(b"iloc");
    bb.full_box(0, 0);
    bb.u8(0x44); // offset_size = 4, length_size = 4
    bb.u8(0x00); // base_offset_size = 0, reserved = 0
    bb.u16(items.len() as u16); // item_count
    let mut slots = Vec::with_capacity(items.len());
    for item in items {
        bb.u16(item.id as u16); // item_ID
        bb.u16(0); // data_reference_index (0 = this file)
        // base_offset: 0 bytes (base_offset_size == 0)
        bb.u16(1); // extent_count
        slots.push(bb.reserve_u32()); // extent_offset (patched after mdat is placed)
        bb.u32(item.payload.len() as u32); // extent_length
    }
    bb.end_box(start);
    slots
}

/// `iinf` v0 + one `infe` v2 per item.
fn write_iinf(bb: &mut BoxBuilder, items: &[Item]) {
    let start = bb.begin_box(b"iinf");
    bb.full_box(0, 0);
    bb.u16(items.len() as u16); // entry_count
    for item in items {
        let infe = bb.begin_box(b"infe");
        bb.full_box(2, u32::from(item.hidden)); // version 2; flags & 1 = hidden
        bb.u16(item.id as u16); // item_ID
        bb.u16(0); // item_protection_index
        bb.bytes(&item.item_type); // item_type
        bb.bytes(item.name.as_bytes()); // item_name
        bb.u8(0); // item_name null terminator
        if let Some(content_type) = &item.content_type {
            bb.bytes(content_type.as_bytes());
            bb.u8(0);
            if let Some(content_encoding) = &item.content_encoding {
                bb.bytes(content_encoding.as_bytes());
                bb.u8(0);
            }
        }
        bb.end_box(infe);
    }
    bb.end_box(start);
}

/// `iref` v0: one `SingleItemTypeReferenceBox` per item reference, in item then reference order.
/// Omitted entirely when no item has references (an empty `iref` does not round-trip).
fn write_iref(bb: &mut BoxBuilder, items: &[Item]) {
    if items.iter().all(|i| i.references.is_empty()) {
        return;
    }
    let start = bb.begin_box(b"iref");
    bb.full_box(0, 0);
    for item in items {
        for reference in &item.references {
            let single = bb.begin_box(&reference.reference_type);
            bb.u16(item.id as u16); // from_item_ID
            bb.u16(reference.to_item_ids.len() as u16); // reference_count
            for &to in &reference.to_item_ids {
                bb.u16(to as u16);
            }
            bb.end_box(single);
        }
    }
    bb.end_box(start);
}

/// `grpl`: one `EntityToGroupBox` (FullBox v0, box type = grouping type) per group. Omitted when
/// there are no groups.
fn write_grpl(bb: &mut BoxBuilder, groups: &[EntityGroup]) {
    if groups.is_empty() {
        return;
    }
    let start = bb.begin_box(b"grpl");
    for group in groups {
        let entity = bb.begin_box(&group.group_type);
        bb.full_box(0, 0);
        bb.u32(group.group_id);
        bb.u32(group.entity_ids.len() as u32); // num_entities_in_group
        for &id in &group.entity_ids {
            bb.u32(id);
        }
        bb.end_box(entity);
    }
    bb.end_box(start);
}

/// `iprp` = a shared `ipco` (deduplicated property boxes) + `ipma` associating them with each item.
fn write_iprp(bb: &mut BoxBuilder, items: &[Item]) -> Result<()> {
    // Build the shared ipco pool, deduplicating by serialized bytes. The essential flag is an ipma
    // concern (it is not part of the property box), so two items may share a property at different
    // essentiality. `assoc[i]` is item i's associations as `(1-based pool index, essential)`.
    let mut pool: Vec<Vec<u8>> = Vec::new();
    let mut assoc: Vec<Vec<(usize, bool)>> = Vec::with_capacity(items.len());
    for item in items {
        let mut row = Vec::with_capacity(item.properties.len());
        for property in &item.properties {
            let bytes = serialize_property(&property.kind);
            let index = match pool.iter().position(|p| *p == bytes) {
                Some(i) => i + 1,
                None => {
                    pool.push(bytes);
                    pool.len()
                }
            };
            row.push((index, property.essential));
        }
        assoc.push(row);
    }
    // The widest ipma association form has a 15-bit index, i.e. i16::MAX slots.
    if i16::try_from(pool.len()).is_err() {
        return Err(Error::unsupported(
            env!("CARGO_PKG_NAME"),
            "ISOBMFF: more than 32767 distinct properties",
        ));
    }

    let start = bb.begin_box(b"iprp");
    let ipco = bb.begin_box(b"ipco");
    for property in &pool {
        bb.bytes(property);
    }
    bb.end_box(ipco);
    write_ipma(bb, items, &assoc);
    bb.end_box(start);
    Ok(())
}

/// `ipma` v0: each item id → its `(property_index, essential)` associations, in association order.
///
/// While every pool index fits 7 bits (`≤ 127`, the common case) each association is a single byte
/// `essential(1) | index(7)` with `flags = 0`; otherwise `flags = 1` selects the two-byte
/// `essential(1) | index(15)` form. The pool size is pre-validated to fit 15 bits.
fn write_ipma(bb: &mut BoxBuilder, items: &[Item], assoc: &[Vec<(usize, bool)>]) {
    let wide = assoc.iter().flatten().any(|&(index, _)| index > 0x7f);
    let start = bb.begin_box(b"ipma");
    bb.full_box(0, u32::from(wide)); // flags & 1 selects 16-bit property indices
    bb.u32(items.len() as u32); // entry_count
    for (item, row) in items.iter().zip(assoc) {
        bb.u16(item.id as u16);
        bb.u8(row.len() as u8); // association_count (≤ 255, pre-validated)
        for &(index, essential) in row {
            // The essential flag is the top bit; the index occupies the rest. Written as an
            // addition rather than `|` so the operator is mutation-observable (OR/XOR/ADD all
            // coincide for the disjoint top bit, which would otherwise leave an equivalent mutant).
            if wide {
                let word = index as u16;
                bb.u16(if essential { word + 0x8000 } else { word });
            } else {
                let byte = index as u8;
                bb.u8(if essential { byte + 0x80 } else { byte });
            }
        }
    }
    bb.end_box(start);
}

/// Serialises one property as a complete box (size + type + body). The `essential` flag is *not*
/// encoded here — it lives in `ipma`.
fn serialize_property(kind: &PropertyKind) -> Vec<u8> {
    let mut bb = BoxBuilder::new();
    match kind {
        PropertyKind::ImageSpatialExtents { width, height } => {
            let start = bb.begin_box(b"ispe");
            bb.full_box(0, 0);
            bb.u32(*width);
            bb.u32(*height);
            bb.end_box(start);
        }
        PropertyKind::PixelInformation { bits_per_channel } => {
            let start = bb.begin_box(b"pixi");
            bb.full_box(0, 0);
            bb.u8(bits_per_channel.len() as u8);
            for &bits in bits_per_channel {
                bb.u8(bits);
            }
            bb.end_box(start);
        }
        PropertyKind::Colour(ColourInformation::Nclx(c)) => {
            let start = bb.begin_box(b"colr");
            bb.bytes(b"nclx");
            bb.u16(c.colour_primaries);
            bb.u16(c.transfer_characteristics);
            bb.u16(c.matrix_coefficients);
            bb.u8(u8::from(c.full_range) << 7); // full_range_flag in bit 7, reserved = 0
            bb.end_box(start);
        }
        PropertyKind::Colour(ColourInformation::RestrictedIcc(profile)) => {
            let start = bb.begin_box(b"colr");
            bb.bytes(b"rICC");
            bb.bytes(profile);
            bb.end_box(start);
        }
        PropertyKind::Colour(ColourInformation::UnrestrictedIcc(profile)) => {
            let start = bb.begin_box(b"colr");
            bb.bytes(b"prof");
            bb.bytes(profile);
            bb.end_box(start);
        }
        PropertyKind::Rotation(angle) => {
            let start = bb.begin_box(b"irot");
            bb.u8(*angle); // reserved(6) | angle(2); range pre-validated
            bb.end_box(start);
        }
        PropertyKind::Mirror(axis) => {
            let start = bb.begin_box(b"imir");
            bb.u8(*axis); // reserved(7) | axis(1); range pre-validated
            bb.end_box(start);
        }
        PropertyKind::CleanAperture {
            width_n,
            width_d,
            height_n,
            height_d,
            horiz_off_n,
            horiz_off_d,
            vert_off_n,
            vert_off_d,
        } => {
            let start = bb.begin_box(b"clap");
            for value in [
                width_n,
                width_d,
                height_n,
                height_d,
                horiz_off_n,
                horiz_off_d,
                vert_off_n,
                vert_off_d,
            ] {
                bb.u32(*value);
            }
            bb.end_box(start);
        }
        PropertyKind::PixelAspectRatio {
            h_spacing,
            v_spacing,
        } => {
            let start = bb.begin_box(b"pasp");
            bb.u32(*h_spacing);
            bb.u32(*v_spacing);
            bb.end_box(start);
        }
        PropertyKind::AuxiliaryType {
            aux_type,
            aux_subtype,
        } => {
            let start = bb.begin_box(b"auxC");
            bb.full_box(0, 0);
            bb.bytes(aux_type.as_bytes());
            bb.u8(0); // aux_type null terminator
            bb.bytes(aux_subtype);
            bb.end_box(start);
        }
        PropertyKind::ContentLightLevel {
            max_content_light_level,
            max_pic_average_light_level,
        } => {
            let start = bb.begin_box(b"clli");
            bb.u16(*max_content_light_level);
            bb.u16(*max_pic_average_light_level);
            bb.end_box(start);
        }
        PropertyKind::CodecConfiguration { kind, data } | PropertyKind::Other { kind, data } => {
            let start = bb.begin_box(kind);
            bb.bytes(data);
            bb.end_box(start);
        }
    }
    bb.into_vec()
}
