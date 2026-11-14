//! The role-typed semantic view over a parsed HEIF still image ([`HeifImage`] / [`HeifItem`]).
//!
//! This layer owns a [`gamut_isobmff::IsoBmffImage`] (the codec-agnostic box-tree model AVIF and
//! HEIC share) and reads *roles* off it — which item is the primary image, which is its alpha
//! plane, its thumbnail, its Exif/XMP metadata, its grid tiles — without duplicating any state.
//! Every datum lives once in the underlying [`IsoBmffImage`]; the accessors here are computed
//! lenses over its items, properties, and references (ISO/IEC 23008-12 and its MIAF profile,
//! ISO/IEC 23000-22; see `references/heif`). Nothing here reads a codec configuration record or a
//! coded payload: that is the codec binding's ([`Codestream`](crate::Codestream)).

use gamut_core::{Dimensions, Error, Orientation, RawMetadata, Result};
/// The typed `clap`/`pasp`/`clli`/`mdcv` property values are the container crate's own,
/// re-exported so the accessors here and the [`PropertyKind`] variants share one type (#743).
pub use gamut_isobmff::{CleanAperture, ContentLightLevel, MasteringDisplay, PixelAspectRatio};
use gamut_isobmff::{
    ColourInformation, EntityGroup, ImageGrid, ImageOverlay, IsoBmffImage, Item, PropertyKind,
};

use crate::Codestream;

/// The [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) message
/// [`HeifImage::primary_metadata`] raises for an XMP item that declares a `content_encoding`.
pub const ENCODED_XMP: &str = "HEIF: content-encoded XMP item is not supported";

/// The `aux_type` URNs that mark an auxiliary image as an **alpha** plane: the MIAF/CICP URN
/// (ISO/IEC 23000-22 §7.3.5) and the HEVC one (ISO/IEC 23008-12 §6.5.8). See `references/heif` §6.
const ALPHA_AUX_URNS: [&str; 2] = [
    "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha",
    "urn:mpeg:hevc:2015:auxid:1",
];

/// The `aux_type` URNs that mark an auxiliary image as a **depth** map. See `references/heif` §6.
const DEPTH_AUX_URNS: [&str; 2] = [
    "urn:mpeg:mpegB:cicp:systems:auxiliary:depth",
    "urn:mpeg:hevc:2015:auxid:2",
];

/// A role-typed view of a parsed HEIF still image: the brands, the validated primary item, and
/// relationship lenses (thumbnails, auxiliaries, metadata, derivations, groups) over the items.
///
/// It owns the underlying [`IsoBmffImage`]; [`as_isobmff`](Self::as_isobmff) exposes it for
/// callers that want the raw box-tree model. Every accessor is a computed lens — no role is
/// cached into a peer field — so the model stays the single source of truth.
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
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if the
    /// `pitm` primary-item id names no item, or if that item is hidden (ISO/IEC 23008-12: the
    /// primary item shall not be hidden).
    pub fn new(inner: IsoBmffImage) -> Result<Self> {
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

    /// The `ftyp` major brand (e.g. `*b"heic"`, `*b"avif"`).
    #[must_use]
    pub fn major_brand(&self) -> [u8; 4] {
        self.inner.major_brand
    }

    /// The `ftyp` compatible brands, in file order.
    #[must_use]
    pub fn compatible_brands(&self) -> &[[u8; 4]] {
        &self.inner.compatible_brands
    }

    /// Whether `brand` is the major brand or one of the compatible brands — the one test a codec
    /// crate's "is this an HEVC/AV1 still?" question is built from.
    #[must_use]
    pub fn has_brand(&self, brand: &[u8; 4]) -> bool {
        core::iter::once(&self.inner.major_brand)
            .chain(&self.inner.compatible_brands)
            .any(|b| b == brand)
    }

    /// The primary (displayed) item — validated at construction to exist and not be hidden.
    #[must_use]
    pub fn primary_item(&self) -> HeifItem<'_> {
        HeifItem {
            inner: &self.inner.items[self.primary_index],
        }
    }

    /// All items, in `iinf` order.
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

    /// The alpha-plane auxiliary of `item_id`, if any: an auxiliary whose `auxC` `aux_type` is
    /// either alpha URN — the MIAF/CICP one or the HEVC one (`references/heif` §6).
    #[must_use]
    pub fn alpha_auxiliary_of(&self, item_id: u32) -> Option<HeifItem<'_>> {
        self.auxiliary_of(item_id, &ALPHA_AUX_URNS)
    }

    /// The depth-map auxiliary of `item_id`, if any: an auxiliary whose `auxC` `aux_type` is
    /// either depth URN — the MIAF/CICP one or the HEVC one (`references/heif` §6).
    #[must_use]
    pub fn depth_auxiliary_of(&self, item_id: u32) -> Option<HeifItem<'_>> {
        self.auxiliary_of(item_id, &DEPTH_AUX_URNS)
    }

    /// Whether `item_id`'s colour values are premultiplied by its associated alpha auxiliary.
    ///
    /// A premultiplied colour image carries an outgoing `prem` item reference to its alpha
    /// auxiliary (ISO/IEC 23008-12 §6; the direction libheif and libavif use — the colour image is
    /// the reference *source*). This returns whether `item_id` is the source of such a reference.
    #[must_use]
    pub fn is_premultiplied(&self, item_id: u32) -> bool {
        self.item(item_id)
            .is_some_and(|item| item.reference_targets(b"prem").is_some())
    }

    /// The metadata items describing `item_id`: `Exif`/`mime` items carrying a `cdsc` reference
    /// to it (a `cdsc` reference runs metadata → described image, `references/heif` §9).
    #[must_use]
    pub fn metadata_of(&self, item_id: u32) -> Vec<HeifItem<'_>> {
        self.items_referencing(b"cdsc", item_id)
    }

    /// The Exif metadata item describing the primary item, if any. Its TIFF stream is
    /// [`HeifItem::exif_tiff_stream`].
    #[must_use]
    pub fn exif(&self) -> Option<HeifItem<'_>> {
        self.metadata_of(self.inner.primary_item_id)
            .into_iter()
            .find(|item| item.kind() == ItemKind::Exif)
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

    /// The EXIF, XMP and ICC payloads the file carries for its primary item, verbatim: the `Exif`
    /// item's TIFF stream (its `exif_tiff_header_offset` applied), the XMP `mime` item's packet
    /// and the primary item's ICC `colr` profile, each present only when the file carries it.
    /// HEIF has no IPTC-IIM item type, and its C2PA store is a top-level box, not an item, so
    /// neither is among them. Refusals are raised under `C`'s [`ORIGIN`](Codestream::ORIGIN),
    /// as the decode surfaces' are; no codestream is read.
    ///
    /// # Errors
    ///
    /// An [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error when the `Exif`
    /// item's offset field is short or points past the payload
    /// ([`HeifItem::exif_tiff_stream`]); and an
    /// [`ErrorKind::Unsupported`](gamut_core::ErrorKind::Unsupported) error, [`ENCODED_XMP`],
    /// when the XMP item declares a `content_encoding` (ISO/IEC 14496-12 §8.11.6) — its payload
    /// is then not the raw packet, and this crate decodes no content codings. Any declared coding
    /// is refused, `identity` included, which RFC 9110 §8.4.1 reserves for `Accept-Encoding`.
    pub fn primary_metadata<C: Codestream>(&self) -> Result<RawMetadata> {
        let mut metadata = RawMetadata::default();
        if let Some(exif) = self.exif() {
            metadata.exif = Some(exif.exif_tiff_stream()?.to_vec());
        }
        if let Some(xmp) = self.xmp() {
            let item = xmp.as_isobmff_item();
            if item.content_encoding.is_some() {
                return Err(Error::unsupported(C::ORIGIN, ENCODED_XMP));
            }
            metadata.xmp = Some(item.payload.clone());
        }
        metadata.icc = self.primary_item().icc_profile().map(<[u8]>::to_vec);
        Ok(metadata)
    }

    /// The derivation sources of `item_id`: the items its `dimg` reference lists, in order (the
    /// tiles of a `grid`, the inputs of an `iovl`, the single source of an `iden`). An id that
    /// names no item is skipped.
    #[must_use]
    pub fn derivation_sources(&self, item_id: u32) -> Vec<HeifItem<'_>> {
        self.item(item_id)
            .map(|item| {
                item.derivation_target_ids()
                    .iter()
                    .filter_map(|&id| self.item(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Parses the `ImageGrid` payload of a `grid` item and validates that its `dimg` reference
    /// count equals `rows * columns`.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `item_id` names no item or the tile count does not match `rows * columns`, and propagates
    /// [`ImageGrid::parse`] errors for a malformed payload.
    pub fn grid(&self, item_id: u32) -> Result<ImageGrid> {
        self.grid_for(item_id, env!("CARGO_PKG_NAME"))
    }

    /// [`grid`](Self::grid), raising its own errors under `origin`.
    pub(crate) fn grid_for(&self, item_id: u32, origin: &'static str) -> Result<ImageGrid> {
        let item = self
            .item(item_id)
            .ok_or_else(|| Error::invalid_input(origin, "HEIF: grid item not found"))?;
        let grid = ImageGrid::parse(&item.as_isobmff_item().payload)?;
        let tiles = usize::from(grid.rows) * usize::from(grid.columns);
        if item.derivation_target_ids().len() != tiles {
            return Err(Error::invalid_input(
                origin,
                "HEIF: grid dimg count does not equal rows * columns",
            ));
        }
        Ok(grid)
    }

    /// Parses the `ImageOverlay` payload of an `iovl` item, using the item's `dimg` reference
    /// count (the overlay stores one offset pair per referenced input, `references/heif` §4).
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error if
    /// `item_id` names no item, and propagates [`ImageOverlay::parse`] errors for a payload that
    /// does not hold exactly that many offset pairs.
    pub fn overlay(&self, item_id: u32) -> Result<ImageOverlay> {
        self.overlay_for(item_id, env!("CARGO_PKG_NAME"))
    }

    /// [`overlay`](Self::overlay), raising its own errors under `origin`.
    pub(crate) fn overlay_for(&self, item_id: u32, origin: &'static str) -> Result<ImageOverlay> {
        let item = self
            .item(item_id)
            .ok_or_else(|| Error::invalid_input(origin, "HEIF: overlay item not found"))?;
        ImageOverlay::parse(
            &item.as_isobmff_item().payload,
            item.derivation_target_ids().len(),
        )
    }

    /// The first auxiliary of `item_id` whose `aux_type` is one of `urns`.
    fn auxiliary_of(&self, item_id: u32, urns: &[&str]) -> Option<HeifItem<'_>> {
        self.auxiliaries_of(item_id)
            .into_iter()
            .find(|aux| aux.auxiliary_type().is_some_and(|urn| urns.contains(&urn)))
    }

    /// Items carrying an outgoing reference of type `ref_type` whose target list contains
    /// `target`.
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

/// A single HEIF item, viewed by role. A zero-cost borrow of the underlying
/// [`gamut_isobmff::Item`]; [`as_isobmff_item`](Self::as_isobmff_item) exposes it. Per-item
/// accessors read the item's type and properties; cross-item relationships live on
/// [`HeifImage`].
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
            // Any coded-image item: a HEIF-family file may carry a codec item other than the one
            // its brand names, and which codec decodes it is the binding's question.
            ty if is_coded_image_type(ty) => ItemKind::CodedImage { codec: *ty },
            ty => ItemKind::Unknown(*ty),
        }
    }

    /// For an `Exif` item, the TIFF stream (starting `II`/`MM`) behind the payload's 4-byte
    /// big-endian `exif_tiff_header_offset` — the `ExifDataBlock` of ISO/IEC 23008-12 §A.2.1
    /// (`references/heif` §9). The arithmetic is [`gamut_isobmff::Item::exif_tiff_stream`]'s.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error (raised
    /// by `gamut-isobmff`) if the item is not an `Exif` item, if the payload is shorter than the
    /// offset field, or if the offset points past the payload's end.
    pub fn exif_tiff_stream(&self) -> Result<&'a [u8]> {
        self.inner.exif_tiff_stream()
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

    /// The display orientation the item's `irot` and `imir` properties compose to, applied in
    /// `ipma` order; [`Orientation::Identity`] when it carries neither.
    ///
    /// Every sequence of quarter turns and mirrors is one of the eight EXIF orientations, so this
    /// is exact even for an order the MIAF constraint forbids (check
    /// [`is_miaf_transform_ordered`](Self::is_miaf_transform_ordered)). A `clap` crop is not an
    /// orientation and is not folded in; when one sits between the rotation and the mirror, the
    /// crop window is placed in the frame of whatever precedes it — read the ordered list from
    /// [`transformative_properties`](Self::transformative_properties) to apply them exactly.
    #[must_use]
    pub fn orientation(&self) -> Orientation {
        let mut state = Dihedral::IDENTITY;
        for property in self.transformative_properties() {
            state = match property {
                TransformativeProperty::Rotation(turns) => state.rotate_ccw(turns),
                TransformativeProperty::Mirror(axis) => state.mirror(axis),
                TransformativeProperty::CleanAperture(_) => state,
            };
        }
        state.orientation()
    }

    /// The `clap` clean-aperture crop, if present.
    #[must_use]
    pub fn clean_aperture(&self) -> Option<CleanAperture> {
        self.find_property(|kind| match *kind {
            PropertyKind::CleanAperture(clap) => Some(clap),
            _ => None,
        })
    }

    /// The `pasp` pixel aspect ratio (`h_spacing`, `v_spacing`), if present.
    #[must_use]
    pub fn pixel_aspect_ratio(&self) -> Option<PixelAspectRatio> {
        self.find_property(|kind| match *kind {
            PropertyKind::PixelAspectRatio(pasp) => Some(pasp),
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

    /// The first `colr` property, whatever its type (CICP `nclx` or an embedded ICC profile), if
    /// present. [`presentation_colour`](Self::presentation_colour) is the one the RGBA surfaces
    /// de-matrix with, and [`icc_profile`](Self::icc_profile) the profile regardless of order.
    #[must_use]
    pub fn colour(&self) -> Option<&'a ColourInformation> {
        self.inner.properties.iter().find_map(|p| match &p.kind {
            PropertyKind::Colour(info) => Some(info),
            _ => None,
        })
    }

    /// The `colr` property the YCbCr → RGB presentation reads: the `nclx` one wherever it sits in
    /// association order, else the first `colr` of any type, else `None`.
    ///
    /// An item may carry one `colr` of each `colour_type` (ISO/IEC 14496-12 §12.1.5), and an ICC
    /// profile describes the RGB result rather than the matrix that produced it, so the `nclx`
    /// box is the one that answers the de-matrixing question whichever comes first.
    #[must_use]
    pub fn presentation_colour(&self) -> Option<&'a ColourInformation> {
        let mut first = None;
        for property in &self.inner.properties {
            if let PropertyKind::Colour(info) = &property.kind {
                if matches!(info, ColourInformation::Nclx(_)) {
                    return Some(info);
                }
                first.get_or_insert(info);
            }
        }
        first
    }

    /// The ICC profile carried by the item's first `colr` property of ICC type (`rICC` or
    /// `prof`), if any — the bytes `gamut-icc` parses, whatever order it shares with an `nclx`.
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

    /// The `clli` content light level (`MaxCLL`/`MaxPALL`), if present.
    #[must_use]
    pub fn content_light_level(&self) -> Option<ContentLightLevel> {
        self.find_property(|kind| match *kind {
            PropertyKind::ContentLightLevel(clli) => Some(clli),
            _ => None,
        })
    }

    /// The `mdcv` mastering display colour volume (SMPTE ST 2086 primaries, white point and
    /// luminance range), if present. Values are the raw stored code points, as
    /// [`MasteringDisplay`] documents.
    #[must_use]
    pub fn mastering_display(&self) -> Option<MasteringDisplay> {
        self.find_property(|kind| match *kind {
            PropertyKind::MasteringDisplayColourVolume(mdcv) => Some(mdcv),
            _ => None,
        })
    }

    /// The raw codec-configuration record as `(box type, body)` — the `hvcC`/`av1C`/… bytes, kept
    /// opaque. `None` if the item has no codec configuration. The codec binding types it.
    #[must_use]
    pub fn codec_configuration(&self) -> Option<(&'a [u8; 4], &'a [u8])> {
        self.inner.properties.iter().find_map(|p| match &p.kind {
            PropertyKind::CodecConfiguration { kind, data } => Some((kind, data.as_slice())),
            _ => None,
        })
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
                PropertyKind::CleanAperture(clap) => {
                    Some(TransformativeProperty::CleanAperture(clap))
                }
                PropertyKind::Rotation(turns) => Some(TransformativeProperty::Rotation(turns)),
                PropertyKind::Mirror(axis) => Some(TransformativeProperty::Mirror(axis)),
                _ => None,
            })
            .collect()
    }

    /// Whether the item's transformative properties satisfy the MIAF constraint (ISO/IEC
    /// 23000-22): at most one each of `clap`/`irot`/`imir`, and — when more than one is present —
    /// in the fixed order clean-aperture → rotation → mirror. Parsing stays permissive; this only
    /// reports conformance.
    #[must_use]
    pub fn is_miaf_transform_ordered(&self) -> bool {
        // Ranks: clap = 0, irot = 1, imir = 2. A strictly increasing sequence enforces both "≤ 1
        // each" and the fixed order in one pass.
        let ranks: Vec<u8> = self
            .transformative_properties()
            .iter()
            .map(|tp| match tp {
                TransformativeProperty::CleanAperture(_) => 0,
                TransformativeProperty::Rotation(_) => 1,
                TransformativeProperty::Mirror(_) => 2,
            })
            .collect();
        ranks.windows(2).all(|pair| pair[0] < pair[1])
    }

    /// Whether the item is associated with an *essential* property this reader does not
    /// recognise.
    ///
    /// A conforming reader must not render an item whose essential property it cannot understand
    /// (MIAF §7.3.6). This flags an essential [`PropertyKind::Other`] (an unrecognised box such as
    /// a layered-image `a1lx`/`a1op`, or an unknown `colr` type). An essential *codec
    /// configuration* is not counted — whether the codec can be decoded is the binding's concern.
    #[must_use]
    pub fn has_unsupported_essential_property(&self) -> bool {
        self.inner
            .properties
            .iter()
            .any(|p| p.essential && matches!(p.kind, PropertyKind::Other { .. }))
    }

    /// The target ids of the item's `dimg` reference (its derivation sources), in order; empty
    /// if it has none.
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
    /// four-character `codec` names the coding (`hvc1`/`hev1` HEVC, `av01` AV1, `vvc1`/`vvi1`
    /// VVC); which binding decodes it is [`Codestream::is_coded_item`](crate::Codestream::is_coded_item)'s
    /// answer.
    CodedImage {
        /// The coded-image item type (`*b"hvc1"`, `*b"av01"`, …).
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

/// One transformative item property, in `ipma` association (application) order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformativeProperty {
    /// `clap` clean aperture (crop).
    CleanAperture(CleanAperture),
    /// `irot` rotation, in anti-clockwise quarter turns (`0..=3`).
    Rotation(u8),
    /// `imir` mirror axis (`0` exchanges top and bottom, `1` exchanges left and right; ISO/IEC
    /// 23008-12:2022 §6.5.12 — the reading libheif and libavif implement).
    Mirror(u8),
}

/// Whether `item_type` names a coded (pixel-bearing) image item rather than a derived, metadata,
/// or unknown item.
fn is_coded_image_type(item_type: &[u8; 4]) -> bool {
    matches!(item_type, b"av01" | b"hvc1" | b"hev1" | b"vvc1" | b"vvi1")
}

/// An element of the symmetry group of the square, written as "mirror left↔right if
/// `mirrored`, then rotate `ccw` quarter turns anti-clockwise" — the normal form every sequence
/// of `irot` and `imir` reduces to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dihedral {
    /// Whether the normal form opens with a left↔right mirror.
    mirrored: bool,
    /// Anti-clockwise quarter turns after it, `0..=3`.
    ccw: u8,
}

impl Dihedral {
    /// No transform.
    const IDENTITY: Self = Self {
        mirrored: false,
        ccw: 0,
    };

    /// This transform followed by `turns` anti-clockwise quarter turns.
    fn rotate_ccw(self, turns: u8) -> Self {
        Self {
            ccw: (self.ccw + turns) % 4,
            ..self
        }
    }

    /// This transform followed by an `imir` with `axis`: `1` mirrors left↔right, anything else
    /// top↔bottom (the field is one bit). A left↔right mirror after a rotation by `k` is the
    /// mirror first and a rotation by `-k`; a top↔bottom mirror is a left↔right one followed by a
    /// half turn, so it lands on `2 - k`.
    fn mirror(self, axis: u8) -> Self {
        let half_turn = if axis == 1 { 0 } else { 2 };
        Self {
            mirrored: !self.mirrored,
            ccw: (half_turn + 4 - self.ccw) % 4,
        }
    }

    /// The EXIF orientation of this transform.
    fn orientation(self) -> Orientation {
        match (self.mirrored, self.ccw) {
            (false, 0) => Orientation::Identity,
            (false, 1) => Orientation::Rotate90Ccw,
            (false, 2) => Orientation::Rotate180,
            (false, _) => Orientation::Rotate90Cw,
            (true, 0) => Orientation::FlipHorizontal,
            (true, 1) => Orientation::Transpose,
            (true, 2) => Orientation::FlipVertical,
            (true, _) => Orientation::AntiTranspose,
        }
    }
}

#[cfg(test)]
mod tests {
    use gamut_core::{Cicp, ErrorKind};
    use gamut_isobmff::{ItemReference, Property};

    use super::*;
    use crate::invariants::{STUB_ORIGIN, StubCodec};

    /// A one-item file whose primary is `item`.
    fn image_of(item: Item) -> HeifImage {
        image_with(vec![item])
    }

    /// A file whose primary is the first of `items`.
    fn image_with(items: Vec<Item>) -> HeifImage {
        let primary = items[0].id;
        HeifImage::new(IsoBmffImage::new(
            *b"heic",
            vec![*b"heic", *b"mif1"],
            primary,
            items,
        ))
        .unwrap()
    }

    fn coded(id: u32, properties: Vec<Property>) -> Item {
        Item::new(id, *b"hvc1")
            .with_properties(properties)
            .with_payload(vec![0xAA])
    }

    fn property(kind: PropertyKind) -> Property {
        Property::new(false, kind)
    }

    fn nclx(matrix_coefficients: u16) -> ColourInformation {
        ColourInformation::Nclx(Cicp {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients,
            full_range: true,
        })
    }

    fn references(item: Item, reference_type: [u8; 4], to: Vec<u32>) -> Item {
        let mut refs = item.references.clone();
        refs.push(ItemReference::new(reference_type, to));
        item.with_references(refs)
    }

    /// One file exercising every role lens: a primary with a thumbnail, Exif, XMP and another
    /// `mime` item, a premultiplied alpha, a grid with a dangling tile, and two entity groups.
    fn roles() -> HeifImage {
        let primary = references(coded(1, vec![]), *b"prem", vec![9]);
        let mut xmp = references(Item::new(4, *b"mime"), *b"cdsc", vec![1]);
        xmp.content_type = Some("application/rdf+xml".to_owned());
        let mut other_mime = references(Item::new(5, *b"mime"), *b"cdsc", vec![1]);
        other_mime.content_type = Some("text/plain".to_owned());
        let items = vec![
            primary,
            references(coded(2, vec![]), *b"thmb", vec![1]),
            other_mime,
            xmp,
            references(Item::new(3, *b"Exif"), *b"cdsc", vec![1]),
            references(Item::new(6, *b"grid"), *b"dimg", vec![7, 99, 8]),
            coded(7, vec![]),
            coded(8, vec![]),
            references(coded(9, vec![]), *b"auxl", vec![1]),
            // A reference of another type to the primary, and a thumbnail of something else.
            references(coded(10, vec![]), *b"base", vec![1]),
            references(coded(11, vec![]), *b"thmb", vec![7]),
        ];
        let file =
            IsoBmffImage::new(*b"heic", vec![*b"mif1", *b"heic"], 1, items).with_groups(vec![
                EntityGroup::new(*b"ster", 20, vec![7, 8]),
                EntityGroup::new(*b"altr", 21, vec![1, 6]),
            ]);
        HeifImage::new(file).unwrap()
    }

    fn ids(items: Vec<HeifItem<'_>>) -> Vec<u32> {
        items.iter().map(HeifItem::id).collect()
    }

    #[test]
    fn the_role_lenses_read_references_by_type_and_target() {
        let image = roles();
        assert_eq!(image.major_brand(), *b"heic");
        assert_eq!(image.compatible_brands(), [*b"mif1", *b"heic"]);
        assert_eq!(image.items().count(), 11);
        assert_eq!(ids(image.thumbnails_of(1)), [2]);
        assert_eq!(ids(image.auxiliaries_of(1)), [9]);
        assert_eq!(ids(image.metadata_of(1)), [5, 4, 3]);
        assert_eq!(image.exif().map(|i| i.id()), Some(3));
        assert_eq!(image.xmp().map(|i| i.id()), Some(4));
        assert_eq!(ids(image.derivation_sources(6)), [7, 8]);
        assert!(image.derivation_sources(99).is_empty());
        assert!(image.is_premultiplied(1));
        assert!(!image.is_premultiplied(2));
        assert_eq!(image.groups().len(), 2);
        let alternatives: Vec<u32> = image.alternatives().iter().map(|g| g.group_id).collect();
        assert_eq!(alternatives, [21]);
    }

    #[test]
    fn kind_classifies_by_item_type() {
        let kind = |ty: &[u8; 4]| {
            image_with(vec![coded(1, vec![]), Item::new(2, *ty)])
                .item(2)
                .unwrap()
                .kind()
        };
        for codec in [b"av01", b"hvc1", b"hev1", b"vvc1", b"vvi1"] {
            assert_eq!(kind(codec), ItemKind::CodedImage { codec: *codec });
        }
        assert_eq!(kind(b"grid"), ItemKind::Grid);
        assert_eq!(kind(b"iovl"), ItemKind::Overlay);
        assert_eq!(kind(b"iden"), ItemKind::Identity);
        assert_eq!(kind(b"Exif"), ItemKind::Exif);
        assert_eq!(kind(b"mime"), ItemKind::Mime);
        assert_eq!(kind(b"zzzz"), ItemKind::Unknown(*b"zzzz"));
    }

    #[test]
    fn the_property_lenses_read_their_own_property() {
        let clap = CleanAperture {
            width_n: 4,
            width_d: 1,
            height_n: 2,
            height_d: 1,
            horiz_off_n: 0,
            horiz_off_d: 1,
            vert_off_n: 0,
            vert_off_d: 1,
        };
        let pasp = PixelAspectRatio {
            h_spacing: 4,
            v_spacing: 3,
        };
        let clli = ContentLightLevel {
            max_content_light_level: 1000,
            max_pic_average_light_level: 400,
        };
        let mdcv = MasteringDisplay::new([[1, 2], [3, 4], [5, 6]], [7, 8], 9, 10);
        let kinds = vec![
            PropertyKind::ImageSpatialExtents {
                width: 6,
                height: 5,
            },
            PropertyKind::CleanAperture(clap),
            PropertyKind::PixelAspectRatio(pasp),
            PropertyKind::PixelInformation {
                bits_per_channel: vec![10, 10, 10],
            },
            PropertyKind::ContentLightLevel(clli),
            PropertyKind::MasteringDisplayColourVolume(mdcv),
            PropertyKind::Rotation(1),
        ];
        let image = image_of(coded(1, kinds.into_iter().map(property).collect()));
        let item = image.primary_item();
        assert_eq!(item.dimensions(), Dimensions::new(6, 5).ok());
        assert_eq!(item.clean_aperture(), Some(clap));
        assert_eq!(item.pixel_aspect_ratio(), Some(pasp));
        assert_eq!(item.bits_per_channel(), Some(&[10u8, 10, 10][..]));
        assert_eq!(item.content_light_level(), Some(clli));
        assert_eq!(item.mastering_display(), Some(mdcv));
        assert_eq!(
            item.transformative_properties(),
            [
                TransformativeProperty::CleanAperture(clap),
                TransformativeProperty::Rotation(1)
            ]
        );
        // None of them is invented for an item without the property.
        let bare = image_of(coded(1, vec![]));
        let bare = bare.primary_item();
        assert_eq!(
            (
                bare.dimensions(),
                bare.clean_aperture(),
                bare.pixel_aspect_ratio(),
                bare.bits_per_channel(),
                bare.content_light_level(),
                bare.mastering_display(),
            ),
            (None, None, None, None, None, None)
        );
    }

    #[test]
    fn a_missing_or_hidden_primary_is_refused() {
        let missing = HeifImage::new(IsoBmffImage::new(
            *b"heic",
            vec![],
            9,
            vec![coded(1, vec![])],
        ))
        .unwrap_err();
        assert_eq!(
            missing.static_message(),
            Some("HEIF: primary item id names no item")
        );
        let hidden = HeifImage::new(IsoBmffImage::new(
            *b"heic",
            vec![],
            1,
            vec![coded(1, vec![]).with_hidden(true)],
        ))
        .unwrap_err();
        assert_eq!(
            hidden.static_message(),
            Some("HEIF: primary item is hidden")
        );
    }

    #[test]
    fn has_brand_reads_the_major_and_every_compatible_brand() {
        let image = HeifImage::new(IsoBmffImage::new(
            *b"avif",
            vec![*b"mif1", *b"miaf"],
            1,
            vec![coded(1, vec![])],
        ))
        .unwrap();
        assert!(image.has_brand(b"avif"));
        assert!(image.has_brand(b"miaf"));
        assert!(!image.has_brand(b"heic"));
    }

    #[test]
    fn exif_tiff_stream_delegates_to_the_isobmff_lens() {
        // The arithmetic and its refusals are `gamut_isobmff::Item::exif_tiff_stream`'s and are
        // tested there; this pins only that the lens returns what it returns. A non-zero offset,
        // so the raw payload and the stream differ.
        let item = Item::new(1, *b"Exif").with_payload(b"\0\0\0\x02\xEE\xEEMM\0*".to_vec());
        assert_eq!(
            image_of(item).primary_item().exif_tiff_stream().unwrap(),
            b"MM\0*"
        );
    }

    /// `primary_metadata` of a file whose primary item 1 is described by `metadata`.
    fn primary_metadata(metadata: Vec<Item>) -> Result<RawMetadata> {
        let mut items = vec![coded(1, Vec::new())];
        items.extend(
            metadata
                .into_iter()
                .map(|item| references(item, *b"cdsc", vec![1])),
        );
        image_with(items).primary_metadata::<StubCodec>()
    }

    #[test]
    fn primary_metadata_hands_over_the_tiff_stream_the_packet_and_the_profile() {
        let exif = Item::new(2, *b"Exif").with_payload(b"\0\0\0\0MM\0*".to_vec());
        let mut xmp = Item::new(3, *b"mime").with_payload(b"<x:xmpmeta/>".to_vec());
        xmp.content_type = Some("application/rdf+xml".to_owned());
        let raw = primary_metadata(vec![exif, xmp]).unwrap();
        assert_eq!(raw.exif.as_deref(), Some(&b"MM\0*"[..]));
        assert_eq!(raw.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        assert_eq!(raw.icc, None);
        assert_eq!(
            primary_metadata(Vec::new()).unwrap(),
            RawMetadata::default()
        );

        let icc = coded(
            1,
            vec![property(PropertyKind::Colour(
                ColourInformation::UnrestrictedIcc(b"profile".to_vec()),
            ))],
        );
        let raw = image_of(icc).primary_metadata::<StubCodec>().unwrap();
        assert_eq!(raw.icc.as_deref(), Some(&b"profile"[..]));
    }

    #[test]
    fn primary_metadata_propagates_an_unlocatable_exif_stream() {
        let exif = Item::new(2, *b"Exif").with_payload(b"\0\0\0\x09MM\0*".to_vec());
        let err = primary_metadata(vec![exif]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn primary_metadata_refuses_a_content_encoded_xmp_item_under_the_bindings_origin() {
        let mut xmp = Item::new(2, *b"mime").with_payload(b"compressed".to_vec());
        xmp.content_type = Some("application/rdf+xml".to_owned());
        xmp.content_encoding = Some("deflate".to_owned());
        let err = primary_metadata(vec![xmp]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert_eq!(err.static_message(), Some(ENCODED_XMP));
        assert_eq!(err.origin(), Some(STUB_ORIGIN));
    }

    #[test]
    fn icc_profile_finds_the_icc_colr_behind_an_nclx_one() {
        let colr = |info| property(PropertyKind::Colour(info));
        let image = image_of(coded(
            1,
            vec![
                colr(nclx(6)),
                colr(ColourInformation::UnrestrictedIcc(vec![1, 2, 3])),
            ],
        ));
        let primary = image.primary_item();
        assert!(matches!(primary.colour(), Some(ColourInformation::Nclx(_))));
        assert_eq!(primary.icc_profile(), Some(&[1u8, 2, 3][..]));

        let image = image_of(coded(
            1,
            vec![colr(ColourInformation::RestrictedIcc(vec![9]))],
        ));
        assert_eq!(image.primary_item().icc_profile(), Some(&[9u8][..]));

        let image = image_of(coded(1, vec![colr(nclx(6))]));
        assert_eq!(image.primary_item().icc_profile(), None);
    }

    #[test]
    fn presentation_colour_prefers_nclx_in_either_order() {
        let colr = |info| property(PropertyKind::Colour(info));
        let icc = ColourInformation::UnrestrictedIcc(vec![1]);
        for properties in [
            vec![colr(icc.clone()), colr(nclx(9))],
            vec![colr(nclx(9)), colr(icc.clone())],
        ] {
            let image = image_of(coded(1, properties));
            assert_eq!(image.primary_item().presentation_colour(), Some(&nclx(9)));
        }
        // ICC alone: the first colr; no colr: nothing.
        let image = image_of(coded(1, vec![colr(icc.clone())]));
        assert_eq!(image.primary_item().presentation_colour(), Some(&icc));
        let image = image_of(coded(1, vec![]));
        assert_eq!(image.primary_item().presentation_colour(), None);
    }

    #[test]
    fn both_alpha_urns_mark_an_alpha_auxiliary_and_both_depth_urns_a_depth_map() {
        let aux = |id: u32, urn: &str| {
            Item::new(id, *b"hvc1")
                .with_hidden(true)
                .with_properties(vec![property(PropertyKind::AuxiliaryType {
                    aux_type: urn.to_owned(),
                    aux_subtype: Vec::new(),
                })])
                .with_references(vec![ItemReference::new(*b"auxl", vec![1])])
        };
        for urn in ALPHA_AUX_URNS {
            let image = image_with(vec![coded(1, vec![]), aux(2, urn)]);
            assert_eq!(
                image.alpha_auxiliary_of(1).map(|i| i.id()),
                Some(2),
                "{urn}"
            );
            assert_eq!(image.depth_auxiliary_of(1), None, "{urn}");
        }
        for urn in DEPTH_AUX_URNS {
            let image = image_with(vec![coded(1, vec![]), aux(2, urn)]);
            assert_eq!(
                image.depth_auxiliary_of(1).map(|i| i.id()),
                Some(2),
                "{urn}"
            );
            assert_eq!(image.alpha_auxiliary_of(1), None, "{urn}");
        }
    }

    #[test]
    fn is_miaf_transform_ordered_needs_strictly_rising_ranks() {
        let clap = PropertyKind::CleanAperture(CleanAperture {
            width_n: 1,
            width_d: 1,
            height_n: 1,
            height_d: 1,
            horiz_off_n: 0,
            horiz_off_d: 1,
            vert_off_n: 0,
            vert_off_d: 1,
        });
        let ordered = |kinds: Vec<PropertyKind>| {
            image_of(coded(1, kinds.into_iter().map(property).collect()))
                .primary_item()
                .is_miaf_transform_ordered()
        };
        assert!(ordered(vec![]));
        assert!(ordered(vec![
            clap,
            PropertyKind::Rotation(1),
            PropertyKind::Mirror(0)
        ]));
        assert!(!ordered(vec![
            PropertyKind::Mirror(0),
            PropertyKind::Rotation(1)
        ]));
        assert!(!ordered(vec![
            PropertyKind::Rotation(1),
            PropertyKind::Rotation(1)
        ]));
    }

    #[test]
    fn single_rotations_and_mirrors_name_their_orientation() {
        let orientation = |kinds: Vec<PropertyKind>| {
            image_of(coded(1, kinds.into_iter().map(property).collect()))
                .primary_item()
                .orientation()
        };
        assert_eq!(orientation(vec![]), Orientation::Identity);
        assert_eq!(
            orientation(vec![PropertyKind::Rotation(1)]),
            Orientation::Rotate90Ccw
        );
        assert_eq!(
            orientation(vec![PropertyKind::Rotation(2)]),
            Orientation::Rotate180
        );
        assert_eq!(
            orientation(vec![PropertyKind::Rotation(3)]),
            Orientation::Rotate90Cw
        );
        assert_eq!(
            orientation(vec![PropertyKind::Mirror(1)]),
            Orientation::FlipHorizontal
        );
        assert_eq!(
            orientation(vec![PropertyKind::Mirror(0)]),
            Orientation::FlipVertical
        );
        // MIAF order (rotate, then mirror): a quarter turn anti-clockwise then a left↔right
        // mirror is the anti-transpose; then a top↔bottom mirror, the transpose.
        assert_eq!(
            orientation(vec![PropertyKind::Rotation(1), PropertyKind::Mirror(1)]),
            Orientation::AntiTranspose
        );
        assert_eq!(
            orientation(vec![PropertyKind::Rotation(1), PropertyKind::Mirror(0)]),
            Orientation::Transpose
        );
    }

    /// The pixel mapping of each normal form, drawn on a 3×2 grid of distinct values by the
    /// pipeline's own transforms, must equal the mapping of the property sequence it was derived
    /// from: the composition rule is checked against the code that applies it.
    #[test]
    fn every_two_step_sequence_composes_to_the_transform_it_applies() {
        type Grid = (Vec<u8>, u32, u32);
        fn apply(steps: &[TransformativeProperty], grid: Grid) -> Grid {
            crate::convert::apply_transforms(steps.to_vec(), grid, "test").unwrap()
        }
        let normal_form = |o: Orientation| -> Vec<TransformativeProperty> {
            let (mirrored, ccw) = match o {
                Orientation::Identity => (false, 0),
                Orientation::Rotate90Ccw => (false, 1),
                Orientation::Rotate180 => (false, 2),
                Orientation::Rotate90Cw => (false, 3),
                Orientation::FlipHorizontal => (true, 0),
                Orientation::Transpose => (true, 1),
                Orientation::FlipVertical => (true, 2),
                Orientation::AntiTranspose => (true, 3),
            };
            let mut steps = Vec::new();
            if mirrored {
                steps.push(TransformativeProperty::Mirror(1));
            }
            steps.push(TransformativeProperty::Rotation(ccw));
            steps
        };
        // Four channels per pixel, as the pipeline works on interleaved RGBA.
        let grid: Grid = ((0..24).collect(), 3, 2);
        let step = |i: u8| match i {
            0..=3 => TransformativeProperty::Rotation(i),
            _ => TransformativeProperty::Mirror(i - 4),
        };
        for a in 0..6 {
            for b in 0..6 {
                let steps = [step(a), step(b)];
                let mut state = Dihedral::IDENTITY;
                for s in steps {
                    state = match s {
                        TransformativeProperty::Rotation(t) => state.rotate_ccw(t),
                        TransformativeProperty::Mirror(axis) => state.mirror(axis),
                        TransformativeProperty::CleanAperture(_) => state,
                    };
                }
                assert_eq!(
                    apply(&steps, grid.clone()),
                    apply(&normal_form(state.orientation()), grid.clone()),
                    "{steps:?}"
                );
            }
        }
    }
}
