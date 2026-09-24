//! The typed ISOBMFF/HEIF still-image box tree.
//!
//! These structs model the *structure* of a single-image ISOBMFF file — its `ftyp` brands and the
//! `meta` image items with their properties, references, and payloads — and never the coded
//! bitstream itself, which stays opaque (carried as [`PropertyKind::CodecConfiguration`] and
//! [`Item::payload`]). This is the codec-agnostic layer both AVIF (`av01`/`av1C`) and HEIC
//! (`hvc1`/`hvcC`) build on.
//!
//! [`crate::write`] serialises an [`IsoBmffImage`]; [`crate::read`] parses one back. The model is
//! normalised so the two are inverse for files this crate writes: it stores each item's resolved
//! [`payload`](Item::payload) (not raw `iloc` offsets or `idat`/`mdat` placement), its per-item
//! [`properties`](Item::properties) list (not raw `ipco` indices), and its outgoing
//! [`references`](Item::references) (not the file-level `iref` table), so
//! `read(&write(&img)?) == img`.
//!
//! Top-level boxes the model does not otherwise own — a C2PA `uuid` box, a `free`, a vendor box —
//! are kept in [`IsoBmffImage::top_level_boxes`] as [`TopLevelBox`]es, each tagged with the
//! [`TopLevelPosition`] the writer places it at, so a file carrying one round-trips byte-identically
//! rather than losing the box. The list is grouped by position (every `AfterFtyp` box before every
//! `Trailing` one) — the order the file has and [`crate::read`] produces; [`crate::write`] refuses
//! an interleaved list, and [`IsoBmffImage::push_top_level_box`] appends without interleaving.

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
    /// The `pitm` primary item id — the image a reader displays. [`crate::write`] requires it to
    /// name one of [`items`](Self::items).
    pub primary_item_id: u32,
    /// The image items, in `iinf` order: the coded image(s) plus any auxiliaries (alpha plane,
    /// thumbnail, Exif/XMP metadata, derivation such as `grid`).
    pub items: Vec<Item>,
    /// The `grpl` entity groups (e.g. `altr` alternatives), in file order; usually empty. An empty
    /// list round-trips as an absent `grpl` box.
    pub groups: Vec<EntityGroup>,
    /// Top-level boxes the model does not otherwise own — anything but `ftyp`, `meta` and `mdat`
    /// (and an appended motion-photo stream, which [`crate::read`] stops at) — in file order, each
    /// carrying the [`TopLevelPosition`] it is written at. File order means grouped by position:
    /// every [`AfterFtyp`](TopLevelPosition::AfterFtyp) box precedes every
    /// [`Trailing`](TopLevelPosition::Trailing) one, which is how [`crate::read`] fills the list and
    /// what [`crate::write`] requires (an interleaved list cannot round-trip, so it is rejected as
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
    /// [`crate::write`] to accept it; prefer [`push_top_level_box`](Self::push_top_level_box) when
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
    /// interleaved list [`crate::write`] refuses. This is how a C2PA box is added to a file read
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
/// and `payload` is the body *after* the user type. [`crate::write`] rejects a box that breaks
/// that pairing, and a box typed `ftyp`/`meta`/`mdat` (the writer emits those from the model) or
/// `moov`/`trak` (image sequences, which [`crate::read`] rejects).
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
    /// Where [`crate::write`] places the box.
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

/// Where [`crate::write`] places a [`TopLevelBox`] within the `ftyp` + `meta` + `mdat` layout.
///
/// [`crate::read`] assigns the position from where it found the box: before `mdat` (whether
/// before or after `meta`) is [`AfterFtyp`](Self::AfterFtyp), after `mdat` is
/// [`Trailing`](Self::Trailing). Because [`crate::write`] always emits `ftyp`, then the
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The item id, unique within the file and referenced by `pitm`/`iloc`/`iinf`/`ipma`/`iref`.
    /// [`crate::read`] accepts the full 32-bit range (`infe` v3 / `iloc` v2); [`crate::write`]
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
    /// the Exif block for an `Exif` item. Opaque to this crate. [`crate::read`] concatenates multi-extent payloads
    /// and resolves `idat`-stored data; [`crate::write`] always places payloads in `mdat` as a
    /// single extent.
    pub payload: Vec<u8>,
}

/// One `iref` entry: a typed reference from the owning [`Item`] to other items
/// (ISO/IEC 14496-12 `SingleItemTypeReferenceBox`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemReference {
    /// The reference type four-character code (`auxl`, `cdsc`, `dimg`, `thmb`, `prem`, …).
    pub reference_type: [u8; 4],
    /// The referenced item ids, in order. Not resolved or validated against
    /// [`IsoBmffImage::items`] — dangling ids are the consumer's concern.
    pub to_item_ids: Vec<u32>,
}

/// One `grpl` entity group (ISO/IEC 14496-12 `EntityToGroupBox`), e.g. `altr` alternatives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityGroup {
    /// The grouping type four-character code (e.g. `*b"altr"`).
    pub group_type: [u8; 4],
    /// The group id — shares the id space with item ids per the spec (not policed here).
    pub group_id: u32,
    /// The grouped entity (item) ids, in order.
    pub entity_ids: Vec<u32>,
}

/// An item property together with whether a reader must understand it to render the item
/// (`essential`, MIAF §7.3.6 / ISO/IEC 23008-12 §9.3.1). Transformative properties and the codec
/// configuration are essential; descriptive ones (`ispe`/`pixi`/`colr`) are not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// Whether the property is marked essential in `ipma` (the high bit of the association entry).
    pub essential: bool,
    /// The property itself.
    pub kind: PropertyKind,
}

/// An item property box (`ipco` child). The HEIF still-image properties are modelled structurally;
/// any other property box (including a codec configuration) is carried verbatim so it round-trips.
///
/// Non-exhaustive: a property currently carried as [`Other`](Self::Other) (e.g. `mdcv`, `cclv`,
/// `amve`) may gain a typed variant in a future minor release, so match with a wildcard arm and do
/// not rely on a specific box type staying inside `Other`.
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
    /// (ISO/IEC 14496-12 §12.1.4). Fields are the raw unsigned 32-bit box values; the offset
    /// numerators are interpreted as two's-complement signed by consumers (MIAF §7.3.6.7).
    CleanAperture {
        /// Clean-aperture width numerator.
        width_n: u32,
        /// Clean-aperture width denominator.
        width_d: u32,
        /// Clean-aperture height numerator.
        height_n: u32,
        /// Clean-aperture height denominator.
        height_d: u32,
        /// Horizontal centre-offset numerator (signed, stored as the raw two's-complement bits).
        horiz_off_n: u32,
        /// Horizontal centre-offset denominator.
        horiz_off_d: u32,
        /// Vertical centre-offset numerator (signed, stored as the raw two's-complement bits).
        vert_off_n: u32,
        /// Vertical centre-offset denominator.
        vert_off_d: u32,
    },
    /// `pasp` pixel aspect ratio — `h_spacing:v_spacing` (ISO/IEC 14496-12 §12.1.4).
    PixelAspectRatio {
        /// Relative horizontal pixel spacing.
        h_spacing: u32,
        /// Relative vertical pixel spacing.
        v_spacing: u32,
    },
    /// `auxC` auxiliary type — what an auxiliary (non-displayed) image is, e.g.
    /// `urn:mpeg:mpegB:cicp:systems:auxiliary:alpha` for an alpha plane
    /// (ISO/IEC 23008-12 §6.5.8; MIAF §7.3.5).
    AuxiliaryType {
        /// The aux type URN. Must have no interior NUL.
        aux_type: String,
        /// Format-specific subtype bytes following the URN, usually empty.
        aux_subtype: Vec<u8>,
    },
    /// `clli` content light level — HDR `MaxCLL`/`MaxPALL` in cd/m² (ISO/IEC 14496-12;
    /// semantics from the HEVC SEI, ISO/IEC 23008-2 §D.3.35).
    ContentLightLevel {
        /// Maximum content light level (`MaxCLL`).
        max_content_light_level: u16,
        /// Maximum picture average light level (`MaxPALL`).
        max_pic_average_light_level: u16,
    },
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
    /// `nclx` on-screen colour: CICP code points plus the full-range flag.
    Nclx(NclxColr),
    /// `rICC` — a restricted ICC profile (input or display class, ISO 15076-1), carried opaquely;
    /// parse it with `gamut-icc`.
    RestrictedIcc(Vec<u8>),
    /// `prof` — an unrestricted ICC profile, carried opaquely; parse it with `gamut-icc`.
    UnrestrictedIcc(Vec<u8>),
}

/// The `nclx` colour information written into a `colr` box (CICP code points, ITU-T H.273). For an
/// AV1 image `matrix_coefficients` and `full_range` must match the sequence header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NclxColr {
    /// CICP colour primaries.
    pub colour_primaries: u16,
    /// CICP transfer characteristics.
    pub transfer_characteristics: u16,
    /// CICP matrix coefficients.
    pub matrix_coefficients: u16,
    /// Full-range (vs limited-range) flag.
    pub full_range: bool,
}
