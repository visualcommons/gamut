//! The typed ISOBMFF/HEIF still-image box tree.
//!
//! These structs model the *structure* of a single-image ISOBMFF file — its `ftyp` brands and the
//! `meta` image items with their properties, references, and payloads — and never the coded
//! bitstream itself, which stays opaque (carried as [`PropertyKind::CodecConfiguration`] and
//! [`Item::payload`]). This is the codec-agnostic layer both AVIF (`av01`/`av1C`) and HEIC
//! (`hvc1`/`hvcC`) build on.
//!
//! [`IsoBmffImage::to_bytes`] serialises an [`IsoBmffImage`]; [`IsoBmffImage::parse`] parses one
//! back. The model is normalised so the two are inverse for files this crate writes: it stores each
//! item's resolved [`payload`](Item::payload) (not raw `iloc` offsets or `idat`/`mdat` placement),
//! its per-item [`properties`](Item::properties) list (not raw `ipco` indices), and its outgoing
//! [`references`](Item::references) (not the file-level `iref` table), so
//! `IsoBmffImage::parse(&img.to_bytes()?)? == img`.
//!
//! Top-level boxes the model does not otherwise own — a C2PA `uuid` box, a `free`, a vendor box —
//! are kept in [`IsoBmffImage::top_level_boxes`] as [`TopLevelBox`]es, each tagged with the
//! [`TopLevelPosition`] the writer places it at, so a file carrying one round-trips byte-identically
//! rather than losing the box. The list is grouped by position (every `AfterFtyp` box before every
//! `Trailing` one) — the order the file has and [`IsoBmffImage::parse`] produces;
//! [`IsoBmffImage::to_bytes`] refuses an interleaved list, and
//! [`IsoBmffImage::push_top_level_box`] appends without interleaving.

use gamut_core::{Cicp, Error, Result};

/// A parsed or constructed ISOBMFF still-image file: its `ftyp` brands, the id of the primary
/// (displayed) item, the image items, the entity groups, and any top-level boxes the model does not
/// otherwise own.
///
/// Non-exhaustive: construct it with [`IsoBmffImage::new`] and the `with_*` builders (or assign
/// the public fields), not a struct literal, so a field added in a future minor release does not
/// break callers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct IsoBmffImage {
    /// The `ftyp` major brand (e.g. `*b"avif"`, `*b"heic"`).
    pub major_brand: [u8; 4],
    /// The `ftyp` minor version (typically `0`).
    pub minor_version: u32,
    /// The `ftyp` compatible brands, in file order (e.g. `avif`/`mif1`/`miaf`/`MA1A`).
    pub compatible_brands: Vec<[u8; 4]>,
    /// The `pitm` primary item id — the image a reader displays. [`IsoBmffImage::to_bytes`] requires it to
    /// name one of [`items`](Self::items).
    pub primary_item_id: u32,
    /// The image items, in `iinf` order: the coded image(s) plus any auxiliaries (alpha plane,
    /// thumbnail, Exif/XMP metadata, derivation such as `grid`).
    pub items: Vec<Item>,
    /// The `grpl` entity groups (e.g. `altr` alternatives), in file order; usually empty. An empty
    /// list round-trips as an absent `grpl` box.
    pub groups: Vec<EntityGroup>,
    /// Top-level boxes the model does not otherwise own — anything but `ftyp`, `meta` and `mdat`
    /// (and a sequence track's `moov`/`trak`, which [`IsoBmffImage::parse`] skips, and an appended
    /// motion-photo stream, which it stops at) — in file order, each
    /// carrying the [`TopLevelPosition`] it is written at. File order means grouped by position:
    /// every [`AfterFtyp`](TopLevelPosition::AfterFtyp) box precedes every
    /// [`Trailing`](TopLevelPosition::Trailing) one, which is how [`IsoBmffImage::parse`] fills the list and
    /// what [`IsoBmffImage::to_bytes`] requires (an interleaved list cannot round-trip, so it is rejected as
    /// `InvalidInput`); [`push_top_level_box`](Self::push_top_level_box) appends without breaking
    /// the grouping. This is where a C2PA `ContentProvenanceBox` (a `uuid` box with the C2PA user
    /// type) lives; usually empty.
    pub top_level_boxes: Vec<TopLevelBox>,
}

impl IsoBmffImage {
    /// A still-image file with the given `ftyp` major brand and compatible brands, primary item id,
    /// and items; `minor_version` is `0` and there are no entity groups or top-level boxes. Adjust
    /// the rest with the `with_*` builders or by assigning the public fields.
    ///
    /// ```
    /// use gamut_isobmff::{IsoBmffImage, Item};
    ///
    /// let img = IsoBmffImage::new(*b"avif", vec![*b"avif", *b"mif1"], 1, Vec::<Item>::new());
    /// assert_eq!(img.minor_version, 0);
    /// assert!(img.groups.is_empty());
    /// assert!(img.top_level_boxes.is_empty());
    /// ```
    #[must_use]
    pub fn new(
        major_brand: [u8; 4],
        compatible_brands: Vec<[u8; 4]>,
        primary_item_id: u32,
        items: Vec<Item>,
    ) -> Self {
        Self {
            major_brand,
            minor_version: 0,
            compatible_brands,
            primary_item_id,
            items,
            groups: Vec::new(),
            top_level_boxes: Vec::new(),
        }
    }

    /// Sets the `ftyp` minor version.
    #[must_use]
    pub fn with_minor_version(mut self, minor_version: u32) -> Self {
        self.minor_version = minor_version;
        self
    }

    /// Sets the `grpl` entity groups.
    #[must_use]
    pub fn with_groups(mut self, groups: Vec<EntityGroup>) -> Self {
        self.groups = groups;
        self
    }

    /// Sets the top-level boxes the model does not otherwise own (see
    /// [`top_level_boxes`](Self::top_level_boxes)). The list must be grouped by position for
    /// [`IsoBmffImage::to_bytes`] to accept it; prefer [`push_top_level_box`](Self::push_top_level_box) when
    /// adding to an existing list.
    #[must_use]
    pub fn with_top_level_boxes(mut self, top_level_boxes: Vec<TopLevelBox>) -> Self {
        self.top_level_boxes = top_level_boxes;
        self
    }

    /// Appends `top` to [`top_level_boxes`](Self::top_level_boxes) at the end of its position
    /// group — an [`AfterFtyp`](TopLevelPosition::AfterFtyp) box goes after the last `AfterFtyp`
    /// box and before the first [`Trailing`](TopLevelPosition::Trailing) one, a `Trailing` box goes
    /// last — so appending to a parsed model that already carries trailing boxes never builds the
    /// interleaved list [`IsoBmffImage::to_bytes`] refuses. This is how a C2PA box is added to a file read
    /// from disk.
    ///
    /// ```
    /// use gamut_isobmff::{IsoBmffImage, Item, TopLevelBox, TopLevelPosition};
    ///
    /// let mut img = IsoBmffImage::new(*b"avif", vec![*b"mif1"], 1, Vec::<Item>::new())
    ///     .with_top_level_boxes(vec![
    ///         TopLevelBox::new(*b"free", vec![]).with_position(TopLevelPosition::Trailing),
    ///     ]);
    /// img.push_top_level_box(TopLevelBox::uuid([0xD8; 16], vec![1]));
    /// assert_eq!(img.top_level_boxes[0].ty, *b"uuid");
    /// assert_eq!(img.top_level_boxes[1].ty, *b"free");
    /// ```
    pub fn push_top_level_box(&mut self, top: TopLevelBox) {
        let index = match top.position {
            TopLevelPosition::AfterFtyp => self
                .top_level_boxes
                .iter()
                .position(|b| b.position == TopLevelPosition::Trailing)
                .unwrap_or(self.top_level_boxes.len()),
            TopLevelPosition::Trailing => self.top_level_boxes.len(),
        };
        self.top_level_boxes.insert(index, top);
    }
}

/// A top-level box the model does not otherwise own, carried verbatim so it round-trips: its
/// four-character type, the 16-byte user type when it is a `uuid` box, its payload, and where the
/// writer places it.
///
/// The `uuid` split follows [`crate::RawBox`]: `user_type` is `Some` exactly when `ty` is `uuid`,
/// and `payload` is the body *after* the user type. [`IsoBmffImage::to_bytes`] rejects a box that breaks
/// that pairing, and a box typed `ftyp`/`meta`/`mdat` (the writer emits those from the model) or
/// `moov`/`trak` (an image sequence's track, whose chunk offsets address the source file's `mdat`
/// and so cannot be carried verbatim; [`IsoBmffImage::parse`] skips one rather than retaining it here).
///
/// The C2PA `ContentProvenanceBox` (C2PA 2.4 §A.5.1) is a `uuid` box with user type
/// `D8FEC3D6-1B0E-483C-9297-5828877EC481`; its payload — the `FullBox` version/flags, the
/// `box_purpose` string and the manifest store — is opaque here.
///
/// Non-exhaustive: build it with [`TopLevelBox::new`] / [`TopLevelBox::uuid`] and
/// [`with_position`](Self::with_position) (or assign the public fields), not a struct literal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TopLevelBox {
    /// The box type four-character code (e.g. `*b"uuid"`, `*b"free"`).
    pub ty: [u8; 4],
    /// The 16-byte extended type of a `uuid` box; `None` for every other type.
    pub user_type: Option<[u8; 16]>,
    /// The box body after the size/type header and, for a `uuid` box, after the user type.
    pub payload: Vec<u8>,
    /// Where [`IsoBmffImage::to_bytes`] places the box.
    pub position: TopLevelPosition,
}

impl TopLevelBox {
    /// A non-`uuid` box of type `ty` with the given payload, placed [`TopLevelPosition::AfterFtyp`].
    #[must_use]
    pub fn new(ty: [u8; 4], payload: Vec<u8>) -> Self {
        Self {
            ty,
            user_type: None,
            payload,
            position: TopLevelPosition::AfterFtyp,
        }
    }

    /// A `uuid` box with the given user type and payload, placed [`TopLevelPosition::AfterFtyp`] —
    /// the C2PA 2.4 §A.5.3 placement for a `ContentProvenanceBox`.
    #[must_use]
    pub fn uuid(user_type: [u8; 16], payload: Vec<u8>) -> Self {
        Self {
            ty: *b"uuid",
            user_type: Some(user_type),
            payload,
            position: TopLevelPosition::AfterFtyp,
        }
    }

    /// Sets where the writer places the box.
    #[must_use]
    pub fn with_position(mut self, position: TopLevelPosition) -> Self {
        self.position = position;
        self
    }
}

/// Where [`IsoBmffImage::to_bytes`] places a [`TopLevelBox`] within the `ftyp` + `meta` + `mdat` layout.
///
/// [`IsoBmffImage::parse`] assigns the position from where it found the box: before `mdat` (whether
/// before or after `meta`) is [`AfterFtyp`](Self::AfterFtyp), after `mdat` is
/// [`Trailing`](Self::Trailing). Because [`IsoBmffImage::to_bytes`] always emits `ftyp`, then the
/// `AfterFtyp` boxes, `meta`, `mdat`, then the `Trailing` boxes, a foreign file's box moves on
/// round-trip wherever its layout differs:
///
/// - a box between `meta` and `mdat` is written back between `ftyp` and `meta`. For a C2PA box
///   that is a move between two lawful positions: §A.5.3 requires only after `ftyp` and before
///   the first `mdat` (and any `moov`), which both satisfy;
/// - a box before `ftyp` is written back after `ftyp`;
/// - in a file with `mdat` before `meta`, a box between `mdat` and `meta` is `Trailing` and is
///   written back after `meta` and `mdat`, as is `mdat` itself after `meta`.
///
/// Files this crate writes use only that layout, so they round-trip byte-identically.
///
/// Non-exhaustive, with permanent append-only discriminants: a finer position (e.g. between
/// `meta` and `mdat`) may be added in a minor release, so match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum TopLevelPosition {
    /// After `ftyp` and before `meta` (so also before the first `mdat`): the C2PA 2.4 §A.5.3
    /// placement.
    AfterFtyp = 0,
    /// After `mdat`, at the end of the primary stream.
    Trailing = 1,
}

/// One image item: its `infe` identity, the properties associated with it, its outgoing item
/// references, and its payload bytes.
///
/// Non-exhaustive: construct it with [`Item::new`] and the `with_*` builders (or assign the
/// public fields), not a struct literal, so a field added in a future minor release does not
/// break callers.
///
/// ```
/// use gamut_isobmff::{Item, ItemReference};
///
/// let exif = Item::new(2, *b"Exif")
///     .with_references(vec![ItemReference::new(*b"cdsc", vec![1])])
///     .with_payload(b"\0\0\0\0II*\0".to_vec());
/// assert_eq!(exif.exif_tiff_stream().unwrap(), b"II*\0");
/// assert!(exif.name.is_empty() && !exif.hidden && exif.properties.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Item {
    /// The item id, unique within the file and referenced by `pitm`/`iloc`/`iinf`/`ipma`/`iref`.
    /// [`IsoBmffImage::parse`] accepts the full 32-bit range (`infe` v3 / `iloc` v2); [`IsoBmffImage::to_bytes`]
    /// normalises to the 16-bit box versions and rejects ids above `u16::MAX` — a still image
    /// never approaches that.
    pub id: u32,
    /// The item type four-character code (e.g. `*b"av01"` for an AV1 image, `*b"hvc1"` for HEVC,
    /// `*b"Exif"` for Exif metadata, `*b"mime"` for XMP, `*b"grid"` for a derived grid).
    pub item_type: [u8; 4],
    /// The item name (`infe` `item_name`), usually empty. Must have no interior NUL.
    pub name: String,
    /// The `infe` MIME content type — required iff [`item_type`](Self::item_type) is `mime`
    /// (e.g. `application/rdf+xml` for XMP).
    pub content_type: Option<String>,
    /// The `infe` MIME content encoding (e.g. `deflate`), only meaningful with
    /// [`content_type`](Self::content_type). `None` and an absent field are equivalent; an empty
    /// string does not round-trip.
    pub content_encoding: Option<String>,
    /// The `infe` hidden flag (`flags & 1`): the item is not intended for standalone display
    /// (e.g. `grid` tiles).
    pub hidden: bool,
    /// The item's outgoing `iref` references (this item is the `from_item`), in file order — e.g.
    /// `auxl` from an alpha item to its master, `cdsc` from Exif/XMP to the described image,
    /// `dimg` from a `grid` item to its tiles, `thmb` from a thumbnail to its master.
    pub references: Vec<ItemReference>,
    /// The item's properties, in `ipma` association order. The codec configuration is
    /// conventionally first and `essential`.
    pub properties: Vec<Property>,
    /// The item's payload with `iloc` placement resolved: the coded bitstream for an image item
    /// (e.g. the AV1 temporal unit), the [`ImageGrid`](crate::ImageGrid) struct for a `grid` item,
    /// the Exif block for an `Exif` item. Opaque to this crate. [`IsoBmffImage::parse`] concatenates multi-extent payloads
    /// and resolves `idat`-stored data; [`IsoBmffImage::to_bytes`] always places payloads in `mdat` as a
    /// single extent.
    pub payload: Vec<u8>,
}

impl Item {
    /// An item with the given id and type and nothing else: an empty name, no content type or
    /// encoding, not hidden, and no references, properties or payload. Fill the rest with the
    /// `with_*` builders or by assigning the public fields.
    #[must_use]
    pub fn new(id: u32, item_type: [u8; 4]) -> Self {
        Self {
            id,
            item_type,
            name: String::new(),
            content_type: None,
            content_encoding: None,
            hidden: false,
            references: Vec::new(),
            properties: Vec::new(),
            payload: Vec::new(),
        }
    }

    /// Sets the `infe` item name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Sets the `infe` MIME content type (required for, and only for, a `mime` item).
    #[must_use]
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Sets the `infe` MIME content encoding (e.g. `deflate`).
    #[must_use]
    pub fn with_content_encoding(mut self, content_encoding: impl Into<String>) -> Self {
        self.content_encoding = Some(content_encoding.into());
        self
    }

    /// Sets the `infe` hidden flag.
    #[must_use]
    pub fn with_hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    /// Sets the item's outgoing `iref` references.
    #[must_use]
    pub fn with_references(mut self, references: Vec<ItemReference>) -> Self {
        self.references = references;
        self
    }

    /// Sets the item's properties, in `ipma` association order.
    #[must_use]
    pub fn with_properties(mut self, properties: Vec<Property>) -> Self {
        self.properties = properties;
        self
    }

    /// Sets the item's payload bytes.
    #[must_use]
    pub fn with_payload(mut self, payload: Vec<u8>) -> Self {
        self.payload = payload;
        self
    }

    /// For an `Exif` item, the TIFF stream (starting `II`/`MM`) behind the payload's 4-byte
    /// big-endian `exif_tiff_header_offset` — the `ExifDataBlock` of ISO/IEC 23008-12 §A.2.1,
    /// whose offset counts bytes from the end of the field to the TIFF header. This is the form
    /// `gamut-exif` parses; the raw payload, offset included, stays in
    /// [`payload`](Self::payload). The framing is HEIF's, not a codec's, so `gamut-heic` and
    /// `gamut-avif` both read their `Exif` items through this one lens.
    ///
    /// An offset that lands exactly at the payload's end yields an empty stream, not an error.
    ///
    /// # Errors
    ///
    /// Returns [`gamut_core::ErrorKind::InvalidInput`] if [`item_type`](Self::item_type) is not
    /// `Exif`, if the payload is shorter than the offset field, or if the offset points past the
    /// payload's end.
    pub fn exif_tiff_stream(&self) -> Result<&[u8]> {
        if self.item_type != *b"Exif" {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: item is not an Exif item",
            ));
        }
        let [o0, o1, o2, o3, rest @ ..] = self.payload.as_slice() else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "ISOBMFF: Exif item payload is shorter than its tiff-header offset field",
            ));
        };
        usize::try_from(u32::from_be_bytes([*o0, *o1, *o2, *o3]))
            .ok()
            .and_then(|offset| rest.get(offset..))
            .ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "ISOBMFF: Exif item tiff-header offset out of range",
                )
            })
    }
}

/// One `iref` entry: a typed reference from the owning [`Item`] to other items
/// (ISO/IEC 14496-12 `SingleItemTypeReferenceBox`).
///
/// Non-exhaustive: construct it with [`ItemReference::new`], not a struct literal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ItemReference {
    /// The reference type four-character code (`auxl`, `cdsc`, `dimg`, `thmb`, `prem`, …).
    pub reference_type: [u8; 4],
    /// The referenced item ids, in order. Not resolved or validated against
    /// [`IsoBmffImage::items`] — dangling ids are the consumer's concern.
    pub to_item_ids: Vec<u32>,
}

impl ItemReference {
    /// A reference of type `reference_type` from the owning item to `to_item_ids`, in order.
    #[must_use]
    pub fn new(reference_type: [u8; 4], to_item_ids: Vec<u32>) -> Self {
        Self {
            reference_type,
            to_item_ids,
        }
    }
}

/// One `grpl` entity group (ISO/IEC 14496-12 `EntityToGroupBox`), e.g. `altr` alternatives.
///
/// Non-exhaustive: construct it with [`EntityGroup::new`], not a struct literal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EntityGroup {
    /// The grouping type four-character code (e.g. `*b"altr"`).
    pub group_type: [u8; 4],
    /// The group id — shares the id space with item ids per the spec (not policed here).
    pub group_id: u32,
    /// The grouped entity (item) ids, in order.
    pub entity_ids: Vec<u32>,
}

impl EntityGroup {
    /// A group of type `group_type` with id `group_id` over `entity_ids`, in order.
    #[must_use]
    pub fn new(group_type: [u8; 4], group_id: u32, entity_ids: Vec<u32>) -> Self {
        Self {
            group_type,
            group_id,
            entity_ids,
        }
    }
}

/// An item property together with whether a reader must understand it to render the item
/// (`essential`, MIAF §7.3.6 / ISO/IEC 23008-12 §9.3.1). Transformative properties and the codec
/// configuration are essential; descriptive ones (`ispe`/`pixi`/`colr`) are not.
///
/// Non-exhaustive: construct it with [`Property::new`], not a struct literal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Property {
    /// Whether the property is marked essential in `ipma` (the high bit of the association entry).
    pub essential: bool,
    /// The property itself.
    pub kind: PropertyKind,
}

impl Property {
    /// The property `kind`, marked essential in `ipma` when `essential` is `true`.
    #[must_use]
    pub fn new(essential: bool, kind: PropertyKind) -> Self {
        Self { essential, kind }
    }
}

/// An item property box (`ipco` child). The HEIF still-image properties are modelled structurally;
/// any other property box (including a codec configuration) is carried verbatim so it round-trips.
///
/// Non-exhaustive: a property currently carried as [`Other`](Self::Other) (e.g. `cclv`, `amve`)
/// may gain a typed variant in a future minor release, so match with a wildcard arm and do not
/// rely on a specific box type staying inside `Other`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PropertyKind {
    /// `ispe` image spatial extents — the stored image dimensions (ISO/IEC 23008-12 §6.5.3).
    ImageSpatialExtents {
        /// Image width in pixels.
        width: u32,
        /// Image height in pixels.
        height: u32,
    },
    /// `pixi` pixel information — the bit depth of each channel, in order (ISO/IEC 23008-12 §6.5.6).
    /// The length is the channel count (3 for colour, 1 for monochrome).
    PixelInformation {
        /// Bits per channel, one entry per channel.
        bits_per_channel: Vec<u8>,
    },
    /// `colr` colour information (ISOBMFF `ColourInformationBox`).
    Colour(ColourInformation),
    /// `irot` image rotation — anti-clockwise quarter turns, `0..=3` (ISO/IEC 23008-12 §6.5.10).
    Rotation(u8),
    /// `imir` image mirror — axis `0` exchanges the top and bottom parts, axis `1` the left and
    /// right parts (ISO/IEC 23008-12:2022 §6.5.12, the semantics libheif and libavif implement).
    Mirror(u8),
    /// `clap` clean aperture — the displayed crop as fractional width/height/centre-offsets
    /// (ISO/IEC 14496-12 §12.1.4).
    CleanAperture(CleanAperture),
    /// `pasp` pixel aspect ratio — `h_spacing:v_spacing` (ISO/IEC 14496-12 §12.1.4).
    PixelAspectRatio(PixelAspectRatio),
    /// `auxC` auxiliary type — what an auxiliary (non-displayed) image is, e.g.
    /// `urn:mpeg:mpegB:cicp:systems:auxiliary:alpha` for an alpha plane
    /// (ISO/IEC 23008-12 §6.5.8; MIAF §7.3.5).
    AuxiliaryType {
        /// The aux type URN. Must have no interior NUL.
        aux_type: String,
        /// Format-specific subtype bytes following the URN, usually empty.
        aux_subtype: Vec<u8>,
    },
    /// `clli` content light level — HDR `MaxCLL`/`MaxPALL` in cd/m² (ISO/IEC 14496-12).
    ContentLightLevel(ContentLightLevel),
    /// `mdcv` mastering display colour volume — the SMPTE ST 2086 mastering-display metadata, as
    /// stored (ISO/IEC 14496-12 `MasteringDisplayColourVolumeBox`). Exactly 24 bytes on the wire;
    /// a reader rejects any other length as malformed.
    MasteringDisplayColourVolume(MasteringDisplay),
    /// A codec configuration property (e.g. `av1C`, `hvcC`) carried as opaque bytes — the container
    /// never interprets the coded-format record. `kind` is the box type; `data` is its body.
    CodecConfiguration {
        /// The property box type (e.g. `*b"av1C"`).
        kind: [u8; 4],
        /// The property box body, verbatim.
        data: Vec<u8>,
    },
    /// Any other (unrecognised) property box, preserved verbatim for round-tripping. `kind` is the
    /// box type; `data` is its body.
    Other {
        /// The property box type.
        kind: [u8; 4],
        /// The property box body, verbatim.
        data: Vec<u8>,
    },
}

/// The contents of a `colr` box (ISO/IEC 14496-12 §12.1.5): CICP code points (`nclx`) or an
/// embedded ICC profile (`rICC`/`prof`). An unknown `colour_type` round-trips as
/// [`PropertyKind::Other`].
///
/// Non-exhaustive: future `colour_type`s may gain variants in a minor release.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColourInformation {
    /// `nclx` on-screen colour: the CICP code points plus the full-range flag, as the workspace's
    /// one [`Cicp`] type. For an AV1 image `matrix_coefficients` and `full_range` must match the
    /// sequence header.
    Nclx(Cicp),
    /// `rICC` — a restricted ICC profile (input or display class, ISO 15076-1), carried opaquely;
    /// parse it with `gamut-icc`.
    RestrictedIcc(Vec<u8>),
    /// `prof` — an unrestricted ICC profile, carried opaquely; parse it with `gamut-icc`.
    UnrestrictedIcc(Vec<u8>),
}

/// The `clap` clean-aperture crop as fractional width/height and centre offsets (ISO/IEC 14496-12
/// §12.1.4), carried by [`PropertyKind::CleanAperture`]. Fields are the raw unsigned 32-bit box
/// values; the offset numerators are the raw two's-complement bits, interpreted as signed by
/// consumers (MIAF §7.3.6.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanAperture {
    /// Clean-aperture width numerator.
    pub width_n: u32,
    /// Clean-aperture width denominator.
    pub width_d: u32,
    /// Clean-aperture height numerator.
    pub height_n: u32,
    /// Clean-aperture height denominator.
    pub height_d: u32,
    /// Horizontal centre-offset numerator (signed, stored as the raw two's-complement bits).
    pub horiz_off_n: u32,
    /// Horizontal centre-offset denominator.
    pub horiz_off_d: u32,
    /// Vertical centre-offset numerator (signed, stored as the raw two's-complement bits).
    pub vert_off_n: u32,
    /// Vertical centre-offset denominator.
    pub vert_off_d: u32,
}

/// The `pasp` pixel aspect ratio `h_spacing : v_spacing` (ISO/IEC 14496-12 §12.1.4), carried by
/// [`PropertyKind::PixelAspectRatio`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelAspectRatio {
    /// Relative horizontal pixel spacing.
    pub h_spacing: u32,
    /// Relative vertical pixel spacing.
    pub v_spacing: u32,
}

/// The `clli` content light level `MaxCLL`/`MaxPALL`, in cd/m² (ISO/IEC 14496-12; semantics from
/// the HEVC SEI, ISO/IEC 23008-2 §D.3.35), carried by [`PropertyKind::ContentLightLevel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentLightLevel {
    /// Maximum content light level (`MaxCLL`).
    pub max_content_light_level: u16,
    /// Maximum picture average light level (`MaxPALL`).
    pub max_pic_average_light_level: u16,
}

/// The `mdcv` mastering display colour volume — SMPTE ST 2086 metadata describing the display the
/// content was mastered on (ISO/IEC 14496-12 `MasteringDisplayColourVolumeBox`, a plain `Box` with
/// the `SmDm` field layout per AV1-ISOBMFF §2.3.4; semantics from the HEVC mastering display
/// colour volume SEI, ISO/IEC 23008-2 Annex D), carried by
/// [`PropertyKind::MasteringDisplayColourVolume`].
///
/// Every field is the stored integer code point, not a converted float, so a value read from one
/// file and written to another is bit-exact. Neither the reader nor the writer range-checks them:
/// the box does not, and a reader rejecting out-of-range SEI values would refuse files the world
/// holds.
///
/// Non-exhaustive: construct it with [`MasteringDisplay::new`], not a struct literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MasteringDisplay {
    /// The three display primaries as `[x, y]` CIE 1931 chromaticity pairs in units of 0.00002,
    /// in stored order. The HEVC convention orders them green, blue, red; the box does not
    /// enforce an order, so they are kept exactly as stored.
    pub display_primaries: [[u16; 2]; 3],
    /// The white point as an `[x, y]` CIE 1931 chromaticity pair in units of 0.00002.
    pub white_point: [u16; 2],
    /// Nominal maximum display luminance, in units of 0.0001 cd/m².
    pub max_display_mastering_luminance: u32,
    /// Nominal minimum display luminance, in units of 0.0001 cd/m².
    pub min_display_mastering_luminance: u32,
}

impl MasteringDisplay {
    /// The stored `mdcv` code points: the three `[x, y]` display primaries in stored order, the
    /// `[x, y]` white point (both in units of 0.00002), and the maximum and minimum mastering
    /// luminance (in units of 0.0001 cd/m²).
    #[must_use]
    pub fn new(
        display_primaries: [[u16; 2]; 3],
        white_point: [u16; 2],
        max_display_mastering_luminance: u32,
        min_display_mastering_luminance: u32,
    ) -> Self {
        Self {
            display_primaries,
            white_point,
            max_display_mastering_luminance,
            min_display_mastering_luminance,
        }
    }
}

/// The `exif_tiff_header_offset` arithmetic of [`Item::exif_tiff_stream`] (ISO/IEC 23008-12
/// §A.2.1), moved here from `gamut-heic` and `gamut-avif`, which both delegate to it.
#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;

    use super::*;

    /// A bare item of `item_type` carrying `payload`.
    fn item(item_type: [u8; 4], payload: &[u8]) -> Item {
        Item::new(1, item_type).with_payload(payload.to_vec())
    }

    #[test]
    fn item_new_is_empty_and_each_builder_sets_only_its_field() {
        let bare = Item::new(7, *b"mime");
        assert_eq!(
            bare,
            Item {
                id: 7,
                item_type: *b"mime",
                name: String::new(),
                content_type: None,
                content_encoding: None,
                hidden: false,
                references: vec![],
                properties: vec![],
                payload: vec![],
            }
        );
        let reference = ItemReference::new(*b"cdsc", vec![1]);
        let property = Property::new(true, PropertyKind::Rotation(1));
        let built = bare
            .clone()
            .with_name("xmp")
            .with_content_type("application/rdf+xml")
            .with_content_encoding("deflate")
            .with_hidden(true)
            .with_references(vec![reference.clone()])
            .with_properties(vec![property.clone()])
            .with_payload(vec![9]);
        assert_eq!(
            built,
            Item {
                name: "xmp".into(),
                content_type: Some("application/rdf+xml".into()),
                content_encoding: Some("deflate".into()),
                hidden: true,
                references: vec![reference],
                properties: vec![property],
                payload: vec![9],
                ..bare
            }
        );
    }

    #[test]
    fn the_leaf_constructors_place_each_argument_in_its_field() {
        let reference = ItemReference::new(*b"dimg", vec![2, 3]);
        assert_eq!(reference.reference_type, *b"dimg");
        assert_eq!(reference.to_item_ids, [2, 3]);
        let group = EntityGroup::new(*b"altr", 9, vec![1, 2]);
        assert_eq!(
            (group.group_type, group.group_id, group.entity_ids),
            (*b"altr", 9, vec![1, 2])
        );
        let property = Property::new(true, PropertyKind::Mirror(1));
        assert!(property.essential);
        assert_eq!(property.kind, PropertyKind::Mirror(1));
        let mdcv = MasteringDisplay::new([[1, 2], [3, 4], [5, 6]], [7, 8], 9, 10);
        assert_eq!(mdcv.display_primaries, [[1, 2], [3, 4], [5, 6]]);
        assert_eq!(mdcv.white_point, [7, 8]);
        assert_eq!(mdcv.max_display_mastering_luminance, 9);
        assert_eq!(mdcv.min_display_mastering_luminance, 10);
    }

    #[test]
    fn exif_tiff_stream_skips_the_offset_field_and_the_offset() {
        for (payload, expected) in [
            // Offset 0, the usual case.
            (&b"\0\0\0\0II*\0"[..], &b"II*\0"[..]),
            // A non-zero offset: a lens that ignored it would return the filler bytes.
            (&b"\0\0\0\x02\xEE\xEEMM\0*"[..], &b"MM\0*"[..]),
            // An offset landing exactly at the end is an empty stream, not an error.
            (&b"\0\0\0\x01\xEE"[..], &b""[..]),
        ] {
            assert_eq!(
                item(*b"Exif", payload).exif_tiff_stream().unwrap(),
                expected,
                "{payload:?}"
            );
        }
    }

    #[test]
    fn exif_tiff_stream_refuses_the_named_faults() {
        for (item_type, payload, message) in [
            (
                *b"mime",
                &b"\0\0\0\0II*\0"[..],
                "ISOBMFF: item is not an Exif item",
            ),
            (
                *b"Exif",
                &b"\0\0\0"[..],
                "ISOBMFF: Exif item payload is shorter than its tiff-header offset field",
            ),
            (
                *b"Exif",
                &b"\0\0\0\x05II*\0"[..],
                "ISOBMFF: Exif item tiff-header offset out of range",
            ),
        ] {
            let err = item(item_type, payload).exif_tiff_stream().unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{message}");
            assert_eq!(err.static_message(), Some(message));
        }
    }
}
