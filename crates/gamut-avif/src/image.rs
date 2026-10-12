//! The role-typed semantic view over a parsed AVIF still image ([`AvifImage`] / [`AvifItem`]).
//!
//! This layer owns a [`gamut_isobmff::IsoBmffImage`] (the codec-agnostic box-tree model AVIF and
//! HEIC share) and reads *roles* off it — which item is the primary image, which is its alpha
//! plane, its thumbnail, its Exif/XMP metadata, its grid tiles — without duplicating any state.
//! Following the unified-model rule, every datum lives once in the underlying [`IsoBmffImage`];
//! the accessors here are computed lenses over its items, properties, and references (ISO/IEC
//! 23008-12 as profiled by AVIF v1.2.0, see `references/avif`). The typed AV1 configuration
//! record (`av1C`) is reached through [`AvifItem::av1_config`] and the AV1 sequence header through
//! [`AvifImage::av1_sequence_header`]; past that header the coded OBU stream stays opaque here —
//! the decode pipeline around the crate's pluggable decoder seam drives it.

use gamut_av1::SequenceHeader;
use gamut_core::{Dimensions, Error, Result};
use gamut_isobmff::{
    ColourInformation, EntityGroup, ImageGrid, ImageOverlay, IsoBmffImage, Item, PropertyKind,
};

use crate::av1c::Av1Config;

/// The `aux_type` URN that marks an auxiliary image as an **alpha** plane (AVIF v1.2.0 §4 — the
/// MIAF/CICP URN; AVIF defines no format-specific alias the way HEVC does).
pub(crate) const ALPHA_AUX_URN: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha";

/// The `aux_type` URN that marks an auxiliary image as a **depth** map (AVIF v1.2.0 §4).
const DEPTH_AUX_URN: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:depth";

/// The four-character AVIF brands that denote an AV1 still image (AVIF v1.2.0 §8.3): `avif` for a
/// still image file, `avio` for a file whose primary item (or its sources) are intra-only AV1
/// image items. The sequence brand `avis` is deliberately absent — image sequences are out of
/// scope (gamut is image-first).
const AV1_STILL_BRANDS: [[u8; 4]; 2] = [*b"avif", *b"avio"];

/// A role-typed view of a parsed AVIF still image: the brands, the validated primary item, and
/// relationship lenses (thumbnails, auxiliaries, metadata, derivations, groups) over the items.
///
/// It owns the underlying [`IsoBmffImage`]; [`as_isobmff`](Self::as_isobmff) exposes it for
/// callers that want the raw box-tree model. Every accessor is a computed lens — no role is
/// cached into a peer field — so the model stays the single source of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvifImage {
    inner: IsoBmffImage,
    /// Index of the primary item within `inner.items`, resolved and validated at construction.
    primary_index: usize,
}

impl AvifImage {
    /// Wraps a parsed [`IsoBmffImage`], validating the primary item.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if the `pitm` primary-item id names no item, or if that
    /// item is hidden (ISO/IEC 23008-12: the primary item shall not be hidden).
    pub(crate) fn new(inner: IsoBmffImage) -> Result<Self> {
        let primary_index = inner
            .items
            .iter()
            .position(|item| item.id == inner.primary_item_id)
            .ok_or_else(|| {
                Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "AVIF: primary item id names no item",
                )
            })?;
        if inner.items[primary_index].hidden {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: primary item is hidden",
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

    /// The `ftyp` major brand (e.g. `*b"avif"`).
    #[must_use]
    pub fn major_brand(&self) -> [u8; 4] {
        self.inner.major_brand
    }

    /// The `ftyp` compatible brands, in file order.
    #[must_use]
    pub fn compatible_brands(&self) -> &[[u8; 4]] {
        &self.inner.compatible_brands
    }

    /// Whether this file is an AV1 still image.
    ///
    /// True when the major brand or any compatible brand is `avif` or `avio` (AVIF v1.2.0 §8.3),
    /// or — for the generic structural brand `mif1` — when the primary item carries an `av1C`
    /// AV1 configuration. The image-*sequence* brand (`avis`) is out of scope and is not treated
    /// as a still here, but a valid still-image `meta` is not rejected for carrying one alongside
    /// `mif1` ([`gamut_isobmff::read`] already rejects track files).
    #[must_use]
    pub fn is_av1_still(&self) -> bool {
        let has_av1_brand = core::iter::once(&self.inner.major_brand)
            .chain(&self.inner.compatible_brands)
            .any(|brand| AV1_STILL_BRANDS.contains(brand));
        if has_av1_brand {
            return true;
        }
        let mif1 = core::iter::once(&self.inner.major_brand)
            .chain(&self.inner.compatible_brands)
            .any(|brand| brand == b"mif1");
        mif1 && self.primary_item().kind().is_av1()
    }

    /// The primary (displayed) item — validated at parse time to exist and not be hidden.
    #[must_use]
    pub fn primary_item(&self) -> AvifItem<'_> {
        AvifItem {
            inner: &self.inner.items[self.primary_index],
        }
    }

    /// All image items, in `iinf` order.
    pub fn items(&self) -> impl Iterator<Item = AvifItem<'_>> + '_ {
        self.inner.items.iter().map(|inner| AvifItem { inner })
    }

    /// The item with `id`, if present.
    #[must_use]
    pub fn item(&self, id: u32) -> Option<AvifItem<'_>> {
        self.inner
            .items
            .iter()
            .find(|item| item.id == id)
            .map(|inner| AvifItem { inner })
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
    pub fn thumbnails_of(&self, item_id: u32) -> Vec<AvifItem<'_>> {
        self.items_referencing(b"thmb", item_id)
    }

    /// The auxiliary items *of* `item_id`: items carrying an `auxl` reference to it (an `auxl`
    /// reference runs auxiliary → master).
    #[must_use]
    pub fn auxiliaries_of(&self, item_id: u32) -> Vec<AvifItem<'_>> {
        self.items_referencing(b"auxl", item_id)
    }

    /// The alpha-plane auxiliary of `item_id`, if any: an auxiliary whose `auxC` `aux_type` is the
    /// alpha URN (AVIF v1.2.0 §4).
    #[must_use]
    pub fn alpha_auxiliary_of(&self, item_id: u32) -> Option<AvifItem<'_>> {
        self.auxiliaries_of(item_id)
            .into_iter()
            .find(|aux| aux.auxiliary_type() == Some(ALPHA_AUX_URN))
    }

    /// The depth-map auxiliary of `item_id`, if any: an auxiliary whose `auxC` `aux_type` is the
    /// depth URN (AVIF v1.2.0 §4).
    #[must_use]
    pub fn depth_auxiliary_of(&self, item_id: u32) -> Option<AvifItem<'_>> {
        self.auxiliaries_of(item_id)
            .into_iter()
            .find(|aux| aux.auxiliary_type() == Some(DEPTH_AUX_URN))
    }

    /// Whether `item_id`'s colour values are premultiplied by its associated alpha auxiliary.
    ///
    /// A premultiplied colour image carries an outgoing `prem` item reference to its alpha
    /// auxiliary (ISO/IEC 23008-12 §6; the direction used by the crate's libavif oracle — the
    /// colour image is the reference *source*). This returns whether `item_id` is the source of
    /// such a reference.
    #[must_use]
    pub fn is_premultiplied(&self, item_id: u32) -> bool {
        self.item(item_id)
            .is_some_and(|item| item.reference_targets(b"prem").is_some())
    }

    /// The metadata items describing `item_id`: `Exif`/`mime` items carrying a `cdsc` reference
    /// to it (a `cdsc` reference runs metadata → described image).
    #[must_use]
    pub fn metadata_of(&self, item_id: u32) -> Vec<AvifItem<'_>> {
        self.items_referencing(b"cdsc", item_id)
    }

    /// The Exif metadata item describing the primary item, if any. The payload is exposed raw —
    /// including the 4-byte `exif_tiff_header_offset` prefix HEIF/AVIF wraps around the TIFF
    /// stream — via [`AvifItem::as_isobmff_item`].
    #[must_use]
    pub fn exif(&self) -> Option<AvifItem<'_>> {
        self.metadata_of(self.inner.primary_item_id)
            .into_iter()
            .find(|item| matches!(item.kind(), ItemKind::Exif))
    }

    /// The XMP metadata item describing the primary item, if any (a `mime` item whose
    /// `content_type` is `application/rdf+xml`).
    #[must_use]
    pub fn xmp(&self) -> Option<AvifItem<'_>> {
        self.metadata_of(self.inner.primary_item_id)
            .into_iter()
            .find(|item| {
                item.as_isobmff_item().content_type.as_deref() == Some("application/rdf+xml")
            })
    }

    /// The derivation sources of `item_id`: the items its `dimg` reference lists, in order (the
    /// tiles of a `grid`, the inputs of an `iovl`, the single source of an `iden`).
    #[must_use]
    pub fn derivation_sources(&self, item_id: u32) -> Vec<AvifItem<'_>> {
        self.item(item_id)
            .and_then(|item| item.reference_targets(b"dimg"))
            .map(|ids| ids.iter().filter_map(|&id| self.item(id)).collect())
            .unwrap_or_default()
    }

    /// Parses the `ImageGrid` payload of a `grid` item and validates that its `dimg` reference
    /// count equals `rows * columns`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `item_id` names no item or the tile count does not match
    /// `rows * columns`, and propagates [`ImageGrid::parse`] errors for a malformed payload.
    pub fn grid(&self, item_id: u32) -> Result<ImageGrid> {
        let item = self.item(item_id).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "AVIF: grid item not found")
        })?;
        let grid = ImageGrid::parse(&item.as_isobmff_item().payload)?;
        let tiles = usize::from(grid.rows) * usize::from(grid.columns);
        if item.derivation_target_ids().len() != tiles {
            return Err(Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: grid dimg count does not equal rows * columns",
            ));
        }
        Ok(grid)
    }

    /// Parses the `ImageOverlay` payload of an `iovl` item, using the item's `dimg` reference
    /// count (the overlay stores one offset pair per referenced input).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `item_id` names no item, and propagates
    /// [`ImageOverlay::parse`] errors for a payload that does not hold exactly that many offset
    /// pairs.
    pub fn overlay(&self, item_id: u32) -> Result<ImageOverlay> {
        let item = self.item(item_id).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "AVIF: overlay item not found")
        })?;
        ImageOverlay::parse(
            &item.as_isobmff_item().payload,
            item.derivation_target_ids().len(),
        )
    }

    /// Parses the AV1 sequence header (AV1 §5.5) of the coded item `item_id` — its profile,
    /// still-picture flags, level, maximum frame size, and `color_config` (bit depth, chroma
    /// subsampling, CICP code points, range) — through `gamut-av1`'s parser, without decoding any
    /// samples.
    ///
    /// Works on any coded item with an `av1C`: pass [`primary_item`](Self::primary_item)'s id for
    /// the colour image, and [`alpha_auxiliary_of`](Self::alpha_auxiliary_of)'s for its alpha
    /// plane. The header is the one [`Av1Config::sequence_header`] selects (the payload's, else
    /// `configOBUs`'). When the item has no `colr`, this header's `color_config` is what the RGBA
    /// presentation surfaces de-matrix with (see [`decode_item_rgba8`](Self::decode_item_rgba8)).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] if `item_id` names no item or the item carries no `av1C`,
    /// and propagates [`AvifItem::av1_config`]'s and [`Av1Config::sequence_header`]'s errors.
    pub fn av1_sequence_header(&self, item_id: u32) -> Result<SequenceHeader> {
        let item = self.item(item_id).ok_or_else(|| {
            Error::invalid_input(env!("CARGO_PKG_NAME"), "AVIF: item id names no item")
        })?;
        let config = item.av1_config().ok_or_else(|| {
            Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "AVIF: item has no av1C configuration",
            )
        })??;
        config.sequence_header(&item.inner.payload)
    }

    /// Items carrying an outgoing reference of type `ref_type` whose target list contains
    /// `target`.
    fn items_referencing(&self, ref_type: &[u8; 4], target: u32) -> Vec<AvifItem<'_>> {
        self.inner
            .items
            .iter()
            .filter(|item| {
                item.references
                    .iter()
                    .any(|r| &r.reference_type == ref_type && r.to_item_ids.contains(&target))
            })
            .map(|inner| AvifItem { inner })
            .collect()
    }
}

/// A single AVIF item, viewed by role. A zero-cost borrow of the underlying
/// [`gamut_isobmff::Item`]; [`as_isobmff_item`](Self::as_isobmff_item) exposes it. Per-item
/// accessors read the item's type and properties; cross-item relationships live on [`AvifImage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvifItem<'a> {
    inner: &'a Item,
}

impl<'a> AvifItem<'a> {
    /// The underlying box-tree item.
    #[must_use]
    pub fn as_isobmff_item(&self) -> &'a Item {
        self.inner
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
            // Any coded-image item (`av01` for AV1, but also `hvc1`, `vvc1`, …): a HEIF-family
            // file may legitimately carry a non-AV1 codec item. `is_av1`/`is_coded_image`
            // classify it.
            ty if is_coded_image_type(ty) => ItemKind::CodedImage { codec: *ty },
            ty => ItemKind::Unknown(*ty),
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

    /// The `imir` mirror axis (`0` = top↔bottom exchanged, `1` = left↔right exchanged; ISO/IEC
    /// 23008-12:2022 §6.5.12 — the semantics libheif and libavif implement), if present.
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

    /// The per-channel bit depths from the `pixi` property, if present (3 entries for colour, 1
    /// for monochrome).
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

    /// The embedded ICC profile from a `colr` box of type `rICC` or `prof`, if the item carries
    /// one.
    ///
    /// Distinct from [`colour`](Self::colour), which returns the **first** `colr` property
    /// whatever its type. An item may legitimately carry both a CICP `nclx` box and an ICC one
    /// (ISO/IEC 14496-12 §12.1.5 allows one of each `colour_type`), and this crate's encoder writes
    /// exactly that pairing, so the profile needs its own lens to be reachable.
    #[must_use]
    pub fn icc_profile(&self) -> Option<&'a [u8]> {
        self.inner.properties.iter().find_map(|p| match &p.kind {
            PropertyKind::Colour(
                ColourInformation::RestrictedIcc(icc) | ColourInformation::UnrestrictedIcc(icc),
            ) => Some(icc.as_slice()),
            _ => None,
        })
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

    /// The raw codec-configuration record as `(box type, body)` — the `av1C`/`hvcC` bytes, kept
    /// opaque. `None` if the item has no codec configuration. For the typed AV1 record use
    /// [`av1_config`](Self::av1_config).
    #[must_use]
    pub fn codec_configuration(&self) -> Option<(&'a [u8; 4], &'a [u8])> {
        self.inner.properties.iter().find_map(|p| match &p.kind {
            PropertyKind::CodecConfiguration { kind, data } => Some((kind, data.as_slice())),
            _ => None,
        })
    }

    /// The typed `av1C` AV1CodecConfigurationRecord ([`Av1Config`]), if the item carries one.
    ///
    /// Returns `None` when the item has no `av1C` codec configuration (it has none, or its codec
    /// configuration is a non-AV1 one such as `hvcC`), and `Some(Err(..))` when an `av1C` record
    /// is present but malformed — keeping "absent" distinct from "malformed". See
    /// [`Av1Config::parse`].
    #[must_use]
    pub fn av1_config(&self) -> Option<Result<Av1Config>> {
        self.codec_configuration()
            .filter(|(kind, _)| *kind == b"av1C")
            .map(|(_, data)| Av1Config::parse(data))
    }

    /// The item's transformative properties (`clap`/`irot`/`imir`) in `ipma` association order —
    /// the order a reader applies them (ISO/IEC 23008-12 §7; MIAF ordering is checked by
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

    /// Whether the item's transformative properties satisfy the MIAF constraint (ISO/IEC
    /// 23000-22): at most one each of `clap`/`irot`/`imir`, and — when more than one is present —
    /// applied in the fixed order clean-aperture → rotation → mirror. Parsing stays permissive;
    /// this only reports conformance.
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

    /// Whether the item is associated with an *essential* property this reader does not
    /// recognise.
    ///
    /// A conforming reader must not render an item whose essential property it cannot understand
    /// (MIAF §7.3.6). This flags an essential [`PropertyKind::Other`] (an unrecognised box such
    /// as a layered-image `a1lx`/`a1op`, or an unknown `colr` type). An essential *codec
    /// configuration* is not counted — whether the codec can be decoded is the decode layer's
    /// concern.
    #[must_use]
    pub fn has_unsupported_essential_property(&self) -> bool {
        self.inner
            .properties
            .iter()
            .any(|p| p.essential && matches!(p.kind, PropertyKind::Other { .. }))
    }

    /// The target ids of the item's `dimg` reference (its derivation sources), in order; empty if
    /// it has none.
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

/// The semantic kind of an AVIF item, derived from its `item_type`.
///
/// Non-exhaustive: further item types may gain variants without a breaking change — match with a
/// wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ItemKind {
    /// A coded image item carrying compressed pixels via its codec configuration and payload. The
    /// four-character `codec` distinguishes AV1 (`av01`) from other codecs a HEIF-family file may
    /// carry (`hvc1`, …); use [`is_av1`](Self::is_av1). For an AV1 item, the typed configuration
    /// is available via [`AvifItem::av1_config`](crate::AvifItem::av1_config).
    CodedImage {
        /// The coded-image item type (`*b"av01"`, `*b"hvc1"`, …).
        codec: [u8; 4],
    },
    /// A `grid` derived image: a tile matrix reassembled from its `dimg` sources.
    Grid,
    /// An `iovl` derived image: its `dimg` sources composited onto a canvas.
    Overlay,
    /// An `iden` identity derived image: its single `dimg` source with that source's
    /// transformative properties applied.
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

    /// Whether this is an AV1 coded-image item (`av01`).
    #[must_use]
    pub fn is_av1(&self) -> bool {
        matches!(self, ItemKind::CodedImage { codec } if codec == b"av01")
    }
}

/// One transformative item property, in `ipma` association (application) order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformativeProperty {
    /// `clap` clean aperture (crop).
    CleanAperture(CleanAperture),
    /// `irot` rotation, in anti-clockwise quarter turns (`0..=3`).
    Rotation(u8),
    /// `imir` mirror axis (`0` = top↔bottom, `1` = left↔right; ISO/IEC 23008-12:2022 §6.5.12).
    Mirror(u8),
}

/// The `clap` clean-aperture crop as fractional width/height and centre offsets (ISO/IEC 14496-12
/// §12.1.4). A computed view of [`PropertyKind::CleanAperture`]; the offset numerators are the
/// raw two's-complement bits, interpreted as signed by consumers (MIAF §7.3.6.7).
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

/// The `pasp` pixel aspect ratio `h_spacing : v_spacing` (ISO/IEC 14496-12 §12.1.4). A computed
/// view of [`PropertyKind::PixelAspectRatio`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelAspectRatio {
    /// Relative horizontal pixel spacing.
    pub h_spacing: u32,
    /// Relative vertical pixel spacing.
    pub v_spacing: u32,
}

/// The `clli` content light level `MaxCLL`/`MaxPALL`, in cd/m² (ISO/IEC 14496-12). A computed
/// view of [`PropertyKind::ContentLightLevel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentLightLevel {
    /// Maximum content light level (`MaxCLL`).
    pub max_content_light_level: u16,
    /// Maximum picture average light level (`MaxPALL`).
    pub max_pic_average_light_level: u16,
}

/// Whether `item_type` names a coded (pixel-bearing) image item rather than a derived, metadata,
/// or unknown item.
fn is_coded_image_type(item_type: &[u8; 4]) -> bool {
    matches!(item_type, b"av01" | b"hvc1" | b"hev1" | b"vvc1" | b"vvi1")
}
