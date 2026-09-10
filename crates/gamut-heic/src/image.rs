//! The role-typed semantic view over a parsed HEIF still image ([`HeifImage`] / [`HeifItem`]).
//!
//! This layer owns a [`gamut_isobmff::IsoBmffImage`] (the codec-agnostic box-tree model AVIF and
//! HEIC share) and reads *roles* off it — which item is the primary image, which is its alpha plane,
//! its thumbnail, its Exif/XMP metadata, its grid tiles — without duplicating any state. Following
//! the unified-model rule, every datum lives once in the underlying [`IsoBmffImage`]; the accessors
//! here are computed lenses over its items, properties, and references (ISO/IEC 23008-12; see
//! `references/heif`). The typed HEVC configuration record (`hvcC`) and the coded NAL stream stay
//! opaque — parsing them is a later slice (issue #238); this layer stops at the container.

use gamut_core::{Dimensions, Error, Result};
use gamut_isobmff::{
    ColourInformation, EntityGroup, ImageGrid, ImageOverlay, IsoBmffImage, Item, PropertyKind,
};

use crate::hvcc::HevcConfig;

/// The `aux_type` URNs that mark an auxiliary image as an **alpha** plane (ISO/IEC 23008-12 §6.5.8;
/// MIAF §7.3.5 / CICP). See `references/heif` §6.
const ALPHA_AUX_URNS: [&str; 2] = [
    "urn:mpeg:hevc:2015:auxid:1",
    "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha",
];

/// The `aux_type` URNs that mark an auxiliary image as a **depth** map. See `references/heif` §6.
const DEPTH_AUX_URNS: [&str; 2] = [
    "urn:mpeg:hevc:2015:auxid:2",
    "urn:mpeg:mpegB:cicp:systems:auxiliary:depth",
];

/// The four-character HEIF brands that denote an HEVC still image (`references/heif` §7). `heim`/
/// `heis` are layered-HEVC brands, read as their single (base) layer.
const HEVC_BRANDS: [[u8; 4]; 4] = [*b"heic", *b"heix", *b"heim", *b"heis"];

/// A role-typed view of a parsed HEIF still image: the brands, the validated primary item, and
/// relationship lenses (thumbnails, auxiliaries, metadata, derivations, groups) over the items.
///
/// It owns the underlying [`IsoBmffImage`]; [`as_isobmff`](Self::as_isobmff) exposes it for callers
/// that want the raw box-tree model. Every accessor is a computed lens — no role is cached into a
/// peer field — so the model stays the single source of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeifImage {
    inner: IsoBmffImage,
    /// Index of the primary item within `inner.items`, resolved and validated at construction.
    primary_index: usize,
}

impl HeifImage {
    /// Wraps a parsed [`IsoBmffImage`], validating the primary item.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the `pitm` primary-item id names no item, or if that item
    /// is hidden (ISO/IEC 23008-12: the primary item shall not be hidden).
    pub(crate) fn new(inner: IsoBmffImage) -> Result<Self> {
        let primary_index = inner
            .items
            .iter()
            .position(|item| item.id == inner.primary_item_id)
            .ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "HEIF: primary item id names no item",
                )
            })?;
        if inner.items[primary_index].hidden {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIF: primary item is hidden",
            ));
        }
        Ok(Self {
            inner,
            primary_index,
        })
    }

    /// The underlying codec-agnostic box-tree model.
    #[must_use]
    pub fn as_isobmff(&self) -> &IsoBmffImage {
        &self.inner
    }

    /// The `ftyp` major brand (e.g. `*b"heic"`).
    #[must_use]
    pub fn major_brand(&self) -> [u8; 4] {
        self.inner.major_brand
    }

    /// The `ftyp` compatible brands, in file order.
    #[must_use]
    pub fn compatible_brands(&self) -> &[[u8; 4]] {
        &self.inner.compatible_brands
    }

    /// Whether this file is an HEVC still image.
    ///
    /// True when the major brand or any compatible brand is one of `heic`/`heix`/`heim`/`heis`
    /// (`references/heif` §7), or — for the generic structural brand `mif1` — when the primary item
    /// carries an `hvcC` HEVC configuration. Image-*sequence* brands (`msf1`/`hevc`/`hevx`) are out
    /// of scope and are not treated as stills here, but a valid still-image `meta` is not rejected
    /// for carrying one alongside `mif1` ([`gamut_isobmff::read`] already rejects track files).
    #[must_use]
    pub fn is_hevc_still(&self) -> bool {
        let has_hevc_brand = core::iter::once(&self.inner.major_brand)
            .chain(&self.inner.compatible_brands)
            .any(|brand| HEVC_BRANDS.contains(brand));
        if has_hevc_brand {
            return true;
        }
        let mif1 = core::iter::once(&self.inner.major_brand)
            .chain(&self.inner.compatible_brands)
            .any(|brand| brand == b"mif1");
        mif1 && self.primary_item().kind().is_hevc()
    }

    /// The primary (displayed) item — validated at parse time to exist and not be hidden.
    #[must_use]
    pub fn primary_item(&self) -> HeifItem<'_> {
        HeifItem {
            inner: &self.inner.items[self.primary_index],
        }
    }

    /// All image items, in `iinf` order.
    pub fn items(&self) -> impl Iterator<Item = HeifItem<'_>> + '_ {
        self.inner.items.iter().map(|inner| HeifItem { inner })
    }

    /// The item with `id`, if present.
    #[must_use]
    pub fn item(&self, id: u32) -> Option<HeifItem<'_>> {
        self.inner
            .items
            .iter()
            .find(|item| item.id == id)
            .map(|inner| HeifItem { inner })
    }

    /// The `grpl` entity groups, in file order.
    #[must_use]
    pub fn groups(&self) -> &[EntityGroup] {
        &self.inner.groups
    }

    /// The `altr` alternative-item groups (each lists interchangeable items in preference order).
    #[must_use]
    pub fn alternatives(&self) -> Vec<&EntityGroup> {
        self.inner
            .groups
            .iter()
            .filter(|g| &g.group_type == b"altr")
            .collect()
    }

    /// The thumbnail items *of* `item_id`: items carrying a `thmb` reference to it.
    #[must_use]
    pub fn thumbnails_of(&self, item_id: u32) -> Vec<HeifItem<'_>> {
        self.items_referencing(b"thmb", item_id)
    }

    /// The auxiliary items *of* `item_id`: items carrying an `auxl` reference to it (an `auxl`
    /// reference runs auxiliary → master).
    #[must_use]
    pub fn auxiliaries_of(&self, item_id: u32) -> Vec<HeifItem<'_>> {
        self.items_referencing(b"auxl", item_id)
    }

    /// The alpha-plane auxiliary of `item_id`, if any: an auxiliary whose `auxC` `aux_type` is an
    /// alpha URN (`references/heif` §6).
    #[must_use]
    pub fn alpha_auxiliary_of(&self, item_id: u32) -> Option<HeifItem<'_>> {
        self.auxiliaries_of(item_id)
            .into_iter()
            .find(|aux| aux.auxiliary_type().is_some_and(is_alpha_urn))
    }

    /// The depth-map auxiliary of `item_id`, if any: an auxiliary whose `auxC` `aux_type` is a depth
    /// URN (`references/heif` §6).
    #[must_use]
    pub fn depth_auxiliary_of(&self, item_id: u32) -> Option<HeifItem<'_>> {
        self.auxiliaries_of(item_id)
            .into_iter()
            .find(|aux| aux.auxiliary_type().is_some_and(is_depth_urn))
    }

    /// Whether `item_id`'s colour values are premultiplied by its associated alpha auxiliary.
    ///
    /// A premultiplied colour image carries an outgoing `prem` item reference to its alpha auxiliary
    /// (ISO/IEC 23008-12 §6; the direction used by the crate's libheif/libavif oracles — the colour
    /// image is the reference *source*). This returns whether `item_id` is the source of such a
    /// reference.
    #[must_use]
    pub fn is_premultiplied(&self, item_id: u32) -> bool {
        self.item(item_id)
            .is_some_and(|item| item.reference_targets(b"prem").is_some())
    }

    /// The metadata items describing `item_id`: `Exif`/`mime` items carrying a `cdsc` reference to
    /// it (a `cdsc` reference runs metadata → described image, `references/heif` §9).
    #[must_use]
    pub fn metadata_of(&self, item_id: u32) -> Vec<HeifItem<'_>> {
        self.items_referencing(b"cdsc", item_id)
    }

    /// The Exif metadata item describing the primary item, if any.
    #[must_use]
    pub fn exif(&self) -> Option<HeifItem<'_>> {
        self.metadata_of(self.inner.primary_item_id)
            .into_iter()
            .find(|item| matches!(item.kind(), ItemKind::Exif))
    }

    /// The XMP metadata item describing the primary item, if any (a `mime` item whose
    /// `content_type` is `application/rdf+xml`, `references/heif` §9).
    #[must_use]
    pub fn xmp(&self) -> Option<HeifItem<'_>> {
        self.metadata_of(self.inner.primary_item_id)
            .into_iter()
            .find(|item| {
                item.as_isobmff_item().content_type.as_deref() == Some("application/rdf+xml")
            })
    }

    /// The derivation sources of `item_id`: the items its `dimg` reference lists, in order (the
    /// tiles of a `grid`, the inputs of an `iovl`, the single source of an `iden`).
    #[must_use]
    pub fn derivation_sources(&self, item_id: u32) -> Vec<HeifItem<'_>> {
        self.item(item_id)
            .and_then(|item| item.reference_targets(b"dimg"))
            .map(|ids| ids.iter().filter_map(|&id| self.item(id)).collect())
            .unwrap_or_default()
    }

    /// Parses the `ImageGrid` payload of a `grid` item and validates that its `dimg` reference count
    /// equals `rows * columns`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `item_id` names no item or the tile count does not match
    /// `rows * columns`, and propagates [`ImageGrid::parse`] errors for a malformed payload.
    pub fn grid(&self, item_id: u32) -> Result<ImageGrid> {
        let item = self.item(item_id).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "HEIF: grid item not found")
        })?;
        let grid = ImageGrid::parse(&item.as_isobmff_item().payload)?;
        let tiles = usize::from(grid.rows) * usize::from(grid.columns);
        if item.derivation_target_ids().len() != tiles {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIF: grid dimg count does not equal rows * columns",
            ));
        }
        Ok(grid)
    }

    /// Parses the `ImageOverlay` payload of an `iovl` item, using the item's `dimg` reference count
    /// (the overlay stores one offset pair per referenced input, `references/heif` §4).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `item_id` names no item, and propagates
    /// [`ImageOverlay::parse`] errors for a payload that does not hold exactly that many offset pairs.
    pub fn overlay(&self, item_id: u32) -> Result<ImageOverlay> {
        let item = self.item(item_id).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "HEIF: overlay item not found")
        })?;
        ImageOverlay::parse(
            &item.as_isobmff_item().payload,
            item.derivation_target_ids().len(),
        )
    }

    /// Items carrying an outgoing reference of type `ref_type` whose target list contains `target`.
    fn items_referencing(&self, ref_type: &[u8; 4], target: u32) -> Vec<HeifItem<'_>> {
        self.inner
            .items
            .iter()
            .filter(|item| {
                item.references
                    .iter()
                    .any(|r| &r.reference_type == ref_type && r.to_item_ids.contains(&target))
            })
            .map(|inner| HeifItem { inner })
            .collect()
    }
}

#[cfg(feature = "metadata")]
impl HeifImage {
    /// The primary item's located metadata payloads as
    /// [`MetadataBlock`](gamut_metadata::MetadataBlock)s, ready for
    /// [`Metadata::from_blocks`](gamut_metadata::Metadata::from_blocks) or a
    /// [`MetadataExtractor`](gamut_metadata::MetadataExtractor) with a chosen
    /// [`ConflictPolicy`](gamut_metadata::ConflictPolicy): the Exif item's TIFF stream
    /// ([`HeifItem::exif_tiff_stream`]), the XMP `mime` item's packet ([`xmp`](Self::xmp)) and the
    /// primary item's `colr` ICC profile ([`HeifItem::icc_profile`]), each present only when the
    /// file carries it.
    ///
    /// HEIF has no IPTC-IIM item type, so no `IptcIim` block is produced. A C2PA manifest store
    /// lives in a top-level `uuid` box outside the item model, so it is not produced here either:
    /// [`HeifContainer::c2pa`](crate::HeifContainer::c2pa) locates it, and a caller wanting it in
    /// the same model appends a [`MetadataBlock::C2pa`](gamut_metadata::MetadataBlock::C2pa).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the Exif item's payload is malformed — shorter than its
    /// 4-byte `exif_tiff_header_offset`, or with the offset past the payload's end.
    pub fn blocks(&self) -> Result<Vec<gamut_metadata::MetadataBlock<'_>>> {
        use gamut_metadata::MetadataBlock;
        let mut blocks = Vec::new();
        if let Some(exif) = self.exif() {
            blocks.push(MetadataBlock::Exif(exif.exif_tiff_stream()?));
        }
        if let Some(xmp) = self.xmp() {
            blocks.push(MetadataBlock::Xmp(&xmp.as_isobmff_item().payload));
        }
        if let Some(icc) = self.primary_item().icc_profile() {
            blocks.push(MetadataBlock::Icc(icc));
        }
        Ok(blocks)
    }

    /// Parses the primary item's located metadata into the unified
    /// [`Metadata`](gamut_metadata::Metadata) model —
    /// [`Metadata::from_blocks`](gamut_metadata::Metadata::from_blocks) over
    /// [`blocks`](Self::blocks).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] as [`blocks`](Self::blocks) does, or when a located payload
    /// does not parse — the facade's [`MetadataError`](gamut_metadata::MetadataError) message,
    /// naming the carrier, is carried as [`Error::detail`].
    pub fn metadata(&self) -> Result<gamut_metadata::Metadata> {
        gamut_metadata::Metadata::from_blocks(&self.blocks()?).map_err(|e| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIF: embedded metadata does not parse",
            )
            .with_detail(e.to_string())
        })
    }
}

/// A single HEIF item, viewed by role. A zero-cost borrow of the underlying [`gamut_isobmff::Item`];
/// [`as_isobmff_item`](Self::as_isobmff_item) exposes it. Per-item accessors read the item's type
/// and properties; cross-item relationships live on [`HeifImage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeifItem<'a> {
    inner: &'a Item,
}

impl<'a> HeifItem<'a> {
    /// The underlying box-tree item.
    #[must_use]
    pub fn as_isobmff_item(&self) -> &'a Item {
        self.inner
    }

    /// For an `Exif` item, the TIFF stream (starting `II`/`MM`) behind the payload's 4-byte
    /// big-endian `exif_tiff_header_offset` — the `ExifDataBlock` of ISO/IEC 23008-12 §A.2.1,
    /// whose offset counts bytes from the end of the field to the TIFF header (`references/heif`
    /// §9). This is the form `gamut-exif` parses; the raw payload, offset included, stays
    /// available through [`as_isobmff_item`](Self::as_isobmff_item).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the item is not an `Exif` item, if the payload is shorter
    /// than the offset field, or if the offset points past the payload's end.
    pub fn exif_tiff_stream(&self) -> Result<&'a [u8]> {
        if !matches!(self.kind(), ItemKind::Exif) {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIF: item is not an Exif item",
            ));
        }
        let [o0, o1, o2, o3, rest @ ..] = self.inner.payload.as_slice() else {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "HEIF: Exif item payload is shorter than its tiff-header offset field",
            ));
        };
        usize::try_from(u32::from_be_bytes([*o0, *o1, *o2, *o3]))
            .ok()
            .and_then(|offset| rest.get(offset..))
            .ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "HEIF: Exif item tiff-header offset out of range",
                )
            })
    }

    /// The ICC profile carried by the item's first `colr` property of ICC type (`rICC` or `prof`),
    /// if any — the bytes `gamut-icc` parses. An item may carry both an `nclx` and an ICC `colr`
    /// (MIAF allows the pair); [`colour`](Self::colour) returns whichever comes first, this lens
    /// the profile regardless of order.
    #[must_use]
    pub fn icc_profile(&self) -> Option<&'a [u8]> {
        self.inner.properties.iter().find_map(|p| match &p.kind {
            PropertyKind::Colour(
                ColourInformation::RestrictedIcc(profile)
                | ColourInformation::UnrestrictedIcc(profile),
            ) => Some(profile.as_slice()),
            _ => None,
        })
    }

    /// The item's id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.inner.id
    }

    /// The item's semantic kind, derived from its `item_type`.
    #[must_use]
    pub fn kind(&self) -> ItemKind {
        match &self.inner.item_type {
            b"grid" => ItemKind::Grid,
            b"iovl" => ItemKind::Overlay,
            b"iden" => ItemKind::Identity,
            b"Exif" => ItemKind::Exif,
            b"mime" => ItemKind::Mime,
            // Any coded-image item (`hvc1`/`hev1` for HEVC, but also `av01`, `vvc1`, …): a HEIF file
            // may legitimately carry a non-HEVC codec item. `is_hevc`/`is_coded_image` classify it.
            ty if is_coded_image_type(ty) => ItemKind::CodedImage { codec: *ty },
            ty => ItemKind::Unknown(*ty),
        }
    }

    /// For an HEVC coded-image item, whether parameter sets (VPS/SPS/PPS) may appear **inband** in
    /// the coded payload: `true` for `hev1`, `false` for `hvc1` (which keeps them only in `hvcC`).
    /// `None` for any non-HEVC item. See `references/heif` §2.
    #[must_use]
    pub fn hevc_inband_parameter_sets_allowed(&self) -> Option<bool> {
        match &self.inner.item_type {
            b"hev1" => Some(true),
            b"hvc1" => Some(false),
            _ => None,
        }
    }

    /// The stored image dimensions from the `ispe` property, if present and non-degenerate.
    #[must_use]
    pub fn dimensions(&self) -> Option<Dimensions> {
        self.find_property(|kind| match *kind {
            PropertyKind::ImageSpatialExtents { width, height } => {
                Dimensions::new(width, height).ok()
            }
            _ => None,
        })
    }

    /// The `irot` rotation as anti-clockwise quarter turns (`0..=3`), if present.
    #[must_use]
    pub fn rotation(&self) -> Option<u8> {
        self.find_property(|kind| match *kind {
            PropertyKind::Rotation(turns) => Some(turns),
            _ => None,
        })
    }

    /// The `imir` mirror axis (`0` exchanges top and bottom, `1` exchanges left and right), if
    /// present.
    #[must_use]
    pub fn mirror(&self) -> Option<u8> {
        self.find_property(|kind| match *kind {
            PropertyKind::Mirror(axis) => Some(axis),
            _ => None,
        })
    }

    /// The `clap` clean-aperture crop, if present.
    #[must_use]
    pub fn clean_aperture(&self) -> Option<CleanAperture> {
        self.find_property(|kind| match *kind {
            PropertyKind::CleanAperture {
                width_n,
                width_d,
                height_n,
                height_d,
                horiz_off_n,
                horiz_off_d,
                vert_off_n,
                vert_off_d,
            } => Some(CleanAperture {
                width_n,
                width_d,
                height_n,
                height_d,
                horiz_off_n,
                horiz_off_d,
                vert_off_n,
                vert_off_d,
            }),
            _ => None,
        })
    }

    /// The `pasp` pixel aspect ratio (`h_spacing`, `v_spacing`), if present.
    #[must_use]
    pub fn pixel_aspect_ratio(&self) -> Option<PixelAspectRatio> {
        self.find_property(|kind| match *kind {
            PropertyKind::PixelAspectRatio {
                h_spacing,
                v_spacing,
            } => Some(PixelAspectRatio {
                h_spacing,
                v_spacing,
            }),
            _ => None,
        })
    }

    /// The per-channel bit depths from the `pixi` property, if present (3 entries for colour, 1 for
    /// monochrome).
    #[must_use]
    pub fn bits_per_channel(&self) -> Option<&'a [u8]> {
        self.inner.properties.iter().find_map(|p| match &p.kind {
            PropertyKind::PixelInformation { bits_per_channel } => {
                Some(bits_per_channel.as_slice())
            }
            _ => None,
        })
    }

    /// The `colr` colour information (CICP `nclx` or an embedded ICC profile), if present.
    #[must_use]
    pub fn colour(&self) -> Option<&'a ColourInformation> {
        self.inner.properties.iter().find_map(|p| match &p.kind {
            PropertyKind::Colour(info) => Some(info),
            _ => None,
        })
    }

    /// The colour property suitable for YCbCr-to-RGB presentation: prefer `nclx` regardless of
    /// association order, then retain the first-colour fallback used by [`Self::colour`].
    pub(crate) fn colour_for_presentation(&self) -> Option<&'a ColourInformation> {
        let mut first = None;
        for property in &self.inner.properties {
            if let PropertyKind::Colour(info) = &property.kind {
                first.get_or_insert(info);
                if matches!(info, ColourInformation::Nclx(_)) {
                    return Some(info);
                }
            }
        }
        first
    }

    /// The `clli` content light level (`MaxCLL`/`MaxPALL`), if present.
    #[must_use]
    pub fn content_light_level(&self) -> Option<ContentLightLevel> {
        self.find_property(|kind| match *kind {
            PropertyKind::ContentLightLevel {
                max_content_light_level,
                max_pic_average_light_level,
            } => Some(ContentLightLevel {
                max_content_light_level,
                max_pic_average_light_level,
            }),
            _ => None,
        })
    }

    /// The raw codec-configuration record as `(box type, body)` — the `hvcC`/`av1C` bytes, kept
    /// opaque. `None` if the item has no codec configuration. For the typed HEVC record use
    /// [`hevc_config`](Self::hevc_config).
    #[must_use]
    pub fn codec_configuration(&self) -> Option<(&'a [u8; 4], &'a [u8])> {
        self.inner.properties.iter().find_map(|p| match &p.kind {
            PropertyKind::CodecConfiguration { kind, data } => Some((kind, data.as_slice())),
            _ => None,
        })
    }

    /// The typed `hvcC` HEVCDecoderConfigurationRecord ([`HevcConfig`]), if the item carries one.
    ///
    /// Returns `None` when the item has no `hvcC` codec configuration (it has none, or its codec
    /// configuration is a non-HEVC one such as `av1C`), and `Some(Err(..))` when an `hvcC` record is
    /// present but malformed — keeping "absent" distinct from "malformed". See
    /// [`HevcConfig::parse`] (`references/heif` §1).
    #[must_use]
    pub fn hevc_config(&self) -> Option<Result<HevcConfig>> {
        self.codec_configuration()
            .filter(|(kind, _)| *kind == b"hvcC")
            .map(|(_, data)| HevcConfig::parse(data))
    }

    /// The item's transformative properties (`clap`/`irot`/`imir`) in `ipma` association order — the
    /// order a decoder applies them (ISO/IEC 23008-12 §7; MIAF ordering is checked by
    /// [`is_miaf_transform_ordered`](Self::is_miaf_transform_ordered)).
    #[must_use]
    pub fn transformative_properties(&self) -> Vec<TransformativeProperty> {
        self.inner
            .properties
            .iter()
            .filter_map(|p| match p.kind {
                PropertyKind::CleanAperture {
                    width_n,
                    width_d,
                    height_n,
                    height_d,
                    horiz_off_n,
                    horiz_off_d,
                    vert_off_n,
                    vert_off_d,
                } => Some(TransformativeProperty::CleanAperture(CleanAperture {
                    width_n,
                    width_d,
                    height_n,
                    height_d,
                    horiz_off_n,
                    horiz_off_d,
                    vert_off_n,
                    vert_off_d,
                })),
                PropertyKind::Rotation(turns) => Some(TransformativeProperty::Rotation(turns)),
                PropertyKind::Mirror(axis) => Some(TransformativeProperty::Mirror(axis)),
                _ => None,
            })
            .collect()
    }

    /// Whether the item's transformative properties satisfy the MIAF constraint (ISO/IEC 23000-22):
    /// at most one each of `clap`/`irot`/`imir`, and — when more than one is present — applied in
    /// the fixed order clean-aperture → rotation → mirror. Parsing stays permissive; this only
    /// reports conformance.
    #[must_use]
    pub fn is_miaf_transform_ordered(&self) -> bool {
        // Ranks: clap = 0, irot = 1, imir = 2. A strictly increasing sequence enforces both "≤ 1
        // each" and the fixed order in one pass.
        let mut last_rank: Option<u8> = None;
        for tp in self.transformative_properties() {
            let rank = match tp {
                TransformativeProperty::CleanAperture(_) => 0,
                TransformativeProperty::Rotation(_) => 1,
                TransformativeProperty::Mirror(_) => 2,
            };
            if last_rank.is_some_and(|last| rank <= last) {
                return false;
            }
            last_rank = Some(rank);
        }
        true
    }

    /// Whether the item is associated with an *essential* property this reader does not recognise.
    ///
    /// A conforming reader must not render an item whose essential property it cannot understand
    /// (MIAF §7.3.6). This flags an essential [`PropertyKind::Other`] (an unrecognised box, or an
    /// unknown `colr` type). An essential *codec configuration* is not counted — whether the codec
    /// can be decoded is the decode layer's concern.
    #[must_use]
    pub fn has_unsupported_essential_property(&self) -> bool {
        self.inner
            .properties
            .iter()
            .any(|p| p.essential && matches!(p.kind, PropertyKind::Other { .. }))
    }

    /// The target ids of the item's `dimg` reference (its derivation sources), in order; empty if it
    /// has none.
    #[must_use]
    pub fn derivation_target_ids(&self) -> &'a [u32] {
        self.reference_targets(b"dimg").unwrap_or(&[])
    }

    /// The `auxC` auxiliary-type URN, if the item carries one.
    #[must_use]
    pub fn auxiliary_type(&self) -> Option<&'a str> {
        self.inner.properties.iter().find_map(|p| match &p.kind {
            PropertyKind::AuxiliaryType { aux_type, .. } => Some(aux_type.as_str()),
            _ => None,
        })
    }

    /// The target ids of the item's first outgoing reference of type `ref_type`, if any.
    fn reference_targets(&self, ref_type: &[u8; 4]) -> Option<&'a [u32]> {
        self.inner
            .references
            .iter()
            .find(|r| &r.reference_type == ref_type)
            .map(|r| r.to_item_ids.as_slice())
    }

    /// Finds the first property whose kind maps to `Some` under `f`.
    fn find_property<T>(&self, f: impl Fn(&'a PropertyKind) -> Option<T>) -> Option<T> {
        self.inner.properties.iter().find_map(|p| f(&p.kind))
    }
}

/// The semantic kind of a HEIF item, derived from its `item_type`.
///
/// Non-exhaustive: further item types may gain variants without a breaking change — match with a
/// wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ItemKind {
    /// A coded image item carrying compressed pixels via its codec configuration and payload. The
    /// four-character `codec` distinguishes HEVC (`hvc1`/`hev1`) from other codecs a HEIF file may
    /// carry (`av01`, …); use [`is_hevc`](Self::is_hevc). For an HEVC item, the typed configuration
    /// is available via [`HeifItem::hevc_config`](crate::HeifItem::hevc_config).
    CodedImage {
        /// The coded-image item type (`*b"hvc1"`, `*b"hev1"`, `*b"av01"`, …).
        codec: [u8; 4],
    },
    /// A `grid` derived image: a tile matrix reassembled from its `dimg` sources.
    Grid,
    /// An `iovl` derived image: its `dimg` sources composited onto a canvas.
    Overlay,
    /// An `iden` identity derived image: its single `dimg` source with that source's transformative
    /// properties applied.
    Identity,
    /// An `Exif` metadata item.
    Exif,
    /// A `mime` item (XMP when its `content_type` is `application/rdf+xml`).
    Mime,
    /// Any other item type, preserved verbatim.
    Unknown([u8; 4]),
}

impl ItemKind {
    /// Whether this is a coded-image item of any codec.
    #[must_use]
    pub fn is_coded_image(&self) -> bool {
        matches!(self, ItemKind::CodedImage { .. })
    }

    /// Whether this is an HEVC coded-image item (`hvc1` or `hev1`).
    #[must_use]
    pub fn is_hevc(&self) -> bool {
        matches!(self, ItemKind::CodedImage { codec } if codec == b"hvc1" || codec == b"hev1")
    }
}

/// One transformative item property, in `ipma` association (application) order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformativeProperty {
    /// `clap` clean aperture (crop).
    CleanAperture(CleanAperture),
    /// `irot` rotation, in anti-clockwise quarter turns (`0..=3`).
    Rotation(u8),
    /// `imir` mirror axis (`0` exchanges top and bottom, `1` exchanges left and right).
    Mirror(u8),
}

/// The `clap` clean-aperture crop as fractional width/height and centre offsets (ISO/IEC 14496-12
/// §12.1.4). A computed view of [`PropertyKind::CleanAperture`]; the offset numerators are the raw
/// two's-complement bits, interpreted as signed by consumers (MIAF §7.3.6.7).
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
    /// Horizontal centre-offset numerator (signed, as raw two's-complement bits).
    pub horiz_off_n: u32,
    /// Horizontal centre-offset denominator.
    pub horiz_off_d: u32,
    /// Vertical centre-offset numerator (signed, as raw two's-complement bits).
    pub vert_off_n: u32,
    /// Vertical centre-offset denominator.
    pub vert_off_d: u32,
}

/// The `pasp` pixel aspect ratio `h_spacing : v_spacing` (ISO/IEC 14496-12 §12.1.4). A computed view
/// of [`PropertyKind::PixelAspectRatio`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelAspectRatio {
    /// Relative horizontal pixel spacing.
    pub h_spacing: u32,
    /// Relative vertical pixel spacing.
    pub v_spacing: u32,
}

/// The `clli` content light level `MaxCLL`/`MaxPALL`, in cd/m² (ISO/IEC 14496-12; HEVC SEI
/// semantics). A computed view of [`PropertyKind::ContentLightLevel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentLightLevel {
    /// Maximum content light level (`MaxCLL`).
    pub max_content_light_level: u16,
    /// Maximum picture average light level (`MaxPALL`).
    pub max_pic_average_light_level: u16,
}

/// Whether `item_type` names a coded (pixel-bearing) image item rather than a derived, metadata, or
/// unknown item.
fn is_coded_image_type(item_type: &[u8; 4]) -> bool {
    matches!(item_type, b"hvc1" | b"hev1" | b"av01" | b"vvc1" | b"vvi1")
}

/// Whether `aux_type` is one of the alpha URNs (`references/heif` §6).
fn is_alpha_urn(aux_type: &str) -> bool {
    ALPHA_AUX_URNS.contains(&aux_type)
}

/// Whether `aux_type` is one of the depth URNs (`references/heif` §6).
fn is_depth_urn(aux_type: &str) -> bool {
    DEPTH_AUX_URNS.contains(&aux_type)
}

/// Unit tests for the two metadata lenses on [`HeifItem`]: the `exif_tiff_header_offset`
/// arithmetic of `exif_tiff_stream` and the property search of `icc_profile`. They read
/// `HeifImage::new`, which is `pub(crate)`, so they live here.
#[cfg(test)]
mod tests {
    use gamut_core::ErrorKind;
    use gamut_isobmff::{IsoBmffImage, NclxColr, Property};

    use super::*;

    /// A one-item file whose primary is `item`.
    fn image_of(item: Item) -> HeifImage {
        HeifImage::new(IsoBmffImage {
            major_brand: *b"heic",
            minor_version: 0,
            compatible_brands: vec![*b"heic", *b"mif1"],
            primary_item_id: item.id,
            items: vec![item],
            groups: vec![],
        })
        .unwrap()
    }

    /// A bare item of the given type and payload.
    fn item(item_type: [u8; 4], payload: Vec<u8>, properties: Vec<Property>) -> Item {
        Item {
            id: 1,
            item_type,
            name: String::new(),
            content_type: None,
            content_encoding: None,
            hidden: false,
            references: vec![],
            properties,
            payload,
        }
    }

    fn colr(info: ColourInformation) -> Property {
        Property {
            essential: false,
            kind: PropertyKind::Colour(info),
        }
    }

    #[test]
    fn exif_tiff_stream_skips_the_offset_field_and_the_offset() {
        // Offset 0 (the usual case) and a non-zero offset skipping filler bytes.
        for (payload, expected) in [
            (b"\0\0\0\0II*\0".to_vec(), &b"II*\0"[..]),
            (b"\0\0\0\x02\xEE\xEEMM\0*".to_vec(), &b"MM\0*"[..]),
            // An offset landing exactly at the end is an empty stream, not an error.
            (b"\0\0\0\x01\xEE".to_vec(), &b""[..]),
        ] {
            let image = image_of(item(*b"Exif", payload.clone(), vec![]));
            assert_eq!(
                image.primary_item().exif_tiff_stream().unwrap(),
                expected,
                "{payload:?}"
            );
        }
    }

    #[test]
    fn exif_tiff_stream_refuses_the_named_faults() {
        let cases: [(Item, &str); 3] = [
            (
                item(*b"mime", b"\0\0\0\0<x/>".to_vec(), vec![]),
                "HEIF: item is not an Exif item",
            ),
            (
                item(*b"Exif", b"\0\0\0".to_vec(), vec![]),
                "HEIF: Exif item payload is shorter than its tiff-header offset field",
            ),
            (
                item(*b"Exif", b"\0\0\0\x05II*\0".to_vec(), vec![]),
                "HEIF: Exif item tiff-header offset out of range",
            ),
        ];
        for (item, message) in cases {
            let image = image_of(item);
            let err = image.primary_item().exif_tiff_stream().unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{message}");
            assert_eq!(err.static_message(), Some(message));
        }
    }

    #[test]
    fn icc_profile_finds_the_icc_colr_behind_an_nclx_one() {
        let nclx = ColourInformation::Nclx(NclxColr {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 6,
            full_range: true,
        });
        let hvc1 = |props: Vec<Property>| item(*b"hvc1", vec![0xAA], props);

        // nclx first, then `prof`: `colour()` reports the nclx, the lens the profile.
        let image = image_of(hvc1(vec![
            colr(nclx.clone()),
            colr(ColourInformation::UnrestrictedIcc(vec![1, 2, 3])),
        ]));
        let primary = image.primary_item();
        assert!(matches!(primary.colour(), Some(ColourInformation::Nclx(_))));
        assert_eq!(primary.icc_profile(), Some(&[1u8, 2, 3][..]));

        // `rICC` counts too.
        let image = image_of(hvc1(vec![colr(ColourInformation::RestrictedIcc(vec![9]))]));
        assert_eq!(image.primary_item().icc_profile(), Some(&[9u8][..]));

        // nclx alone, or no colr at all: no profile.
        let image = image_of(hvc1(vec![colr(nclx)]));
        assert_eq!(image.primary_item().icc_profile(), None);
        let image = image_of(hvc1(vec![]));
        assert_eq!(image.primary_item().icc_profile(), None);
    }
}
